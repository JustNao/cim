//! Runtime loader + per-pane instance manager for the optional proprietary C++
//! image-processing operators.
//!
//! The two operators live in **separately built** shared libraries, one each:
//! LUT_ALPHA (auto-contrast) and DETAILS_ENHANCED (detail enhancement). cim does
//! **not** link them at build time: each is loaded on demand at startup by its
//! hard-coded file name (see `LUT_ALPHA_LIB` / `DETAILS_LIB`). The **directory**
//! that holds them is configured in Settings (`Config::cpp_lib_dir`) and passed
//! to [`init`]; when it's left empty the bare name is used and the system loader
//! resolves it via its search path (`LD_LIBRARY_PATH`, Linux-only), preserving
//! the old behaviour. Each operator is independent: if its library is missing or
//! its symbols don't resolve, only that operator stays unavailable and its feature
//! is disabled in the UI.
//!
//! **The operators are heavy, size-dependent C++ objects, not stateless
//! functions.** Each library exports a three-symbol lifecycle rather than one
//! entry point:
//!
//! ```c
//! void* cim_<op>_create (size_t width, size_t height);        // build the instance
//! void  cim_<op>_apply  (void* handle, uint16_t* data, size_t len); // per frame, in place
//! void  cim_<op>_destroy(void* handle);                       // free the instance
//! ```
//!
//! **DETAILS_ENHANCED takes a second buffer.** Its `apply` additionally receives
//! the **after-LUT 8-bit** companion of the same frame — the display-tone look —
//! so the operator can key its enhancement off it, not just the raw 16-bit data:
//!
//! ```c
//! void  cim_details_enhanced_apply(void* handle, uint16_t* data,
//!                                  const uint8_t* lut8, size_t len);
//! ```
//!
//! `data` is the raw 16-bit buffer (transformed in place); `lut8` is a read-only
//! `len`-sample 8-bit render of the **current view LUT output** — the pane's own
//! tone as it is shown, i.e. `data` after any LUT_ALPHA (or the linear/clip map)
//! downscaled to 8 bits — built in [`PaneOps::apply`], so it always tracks
//! whichever LUT the view is using.
//!
//! Construction (`create`) is expensive and depends on the image **size** (not
//! its contents), so cim builds an instance **once per (pane, size)** and reuses
//! it across that pane's frames via `apply`, rebuilding only when the dimensions
//! change and destroying it when the pane goes away. [`PaneOps`] holds one pane's
//! instances; it is owned by that pane's render worker thread (see
//! `renderer::Worker`) or its export pane, so a given instance is only ever
//! touched by one thread — the proprietary class need not be reentrant.
//!
//! Both operators receive the frame as a **single-channel 16-bit** buffer
//! (`width * height` u16 samples, one per pixel, row-major) and transform it
//! **in place**, keeping the same dimensions. They are only ever invoked for
//! frames whose native format is **single-channel 16-bit unsigned** (see the
//! `is_op_input` gate in `app::decode::prepare` / `renderer` / `export`), so the
//! operator sees genuine 16-bit precision rather than a value already crushed to
//! 8 bits. cim expands the operator's output back to grey RGBA for display.
//!
//! See `INTEGRATION_CPP.md` for how to build the libraries and the exact ABI.

use std::os::raw::c_void;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, RwLock};

/// The C symbols each operator library exports (see the module docs):
/// `create(width, height) -> handle`, `apply(...)`, `destroy(handle)`.
type CreateFn = unsafe extern "C" fn(usize, usize) -> *mut c_void;
/// LUT_ALPHA's `apply`: raw 16-bit buffer, transformed in place.
type ApplyFn = unsafe extern "C" fn(*mut c_void, *mut u16, usize);
/// DETAILS_ENHANCED's `apply`: the raw 16-bit buffer (in place) **plus** the
/// after-LUT 8-bit companion (read-only), both `len` samples.
type DetailsApplyFn = unsafe extern "C" fn(*mut c_void, *mut u16, *const u8, usize);
type DestroyFn = unsafe extern "C" fn(*mut c_void);

// Hard-coded shared-library file names, one operator each. Resolved inside the
// configured library directory (`Config::cpp_lib_dir`) — which defaults to the
// `LIBS` folder next to the cim executable when unset (see `cpp_lib_dir`) — or,
// only when no directory resolves, via the system loader's search path
// (`LD_LIBRARY_PATH`) by bare name.
// TODO: replace these placeholders with the real distributed file names.
const LUT_ALPHA_LIB: &str = "libcim_lut_alpha.so"; // placeholder
const DETAILS_LIB: &str = "libcim_details_enhanced.so"; // placeholder

/// Resolve a library file name against the optional configured directory. With a
/// directory, load exactly `<dir>/<name>`; without one, pass the bare name so the
/// system loader resolves it via its search path (`LD_LIBRARY_PATH`).
fn resolve(dir: Option<&Path>, name: &str) -> PathBuf {
    match dir {
        Some(d) => d.join(name),
        None => PathBuf::from(name),
    }
}

/// A successfully loaded operator library plus its three resolved entry points.
/// The `Library` is kept alive here because the function pointers borrow from it;
/// it unloads when this slot is cleared.
struct Operator {
    _lib: libloading::Library,
    create: CreateFn,
    /// The `<stem>_apply` symbol. LUT_ALPHA and DETAILS_ENHANCED export different
    /// `apply` signatures ([`ApplyFn`] vs [`DetailsApplyFn`]); it is stored as the
    /// canonical [`ApplyFn`] and the DETAILS call site transmutes it to
    /// [`DetailsApplyFn`] (all fn pointers share a representation, so this is
    /// sound — the resolved symbol address is the same either way).
    apply: ApplyFn,
    destroy: DestroyFn,
}

// The handles are only ever called through `&Operator` behind the `RwLock`, and
// both `Library` and bare `fn` pointers are themselves `Send + Sync`.
unsafe impl Send for Operator {}
unsafe impl Sync for Operator {}

/// The process-wide loaded operators (`None` until loaded / when unavailable).
/// Guarded by an `RwLock` so each pane's worker can read them concurrently to
/// build its own instance.
static LUT_ALPHA: RwLock<Option<Operator>> = RwLock::new(None);
static DETAILS: RwLock<Option<Operator>> = RwLock::new(None);

/// Process-wide lock serialising every operator **`create` and `destroy`** call.
///
/// Per-instance `apply` stays fully parallel (each pane owns its instance on its
/// own worker thread — that is the point of the pool), but **construction and
/// teardown are serialised across all panes**. The proprietary operators are only
/// promised to be safe when a single instance is touched by a single thread; they
/// are *not* promised that two threads may enter `create`/`destroy` at once, and
/// heavy size-dependent constructors routinely touch process-global state on first
/// use — FFTW planner setup, static lookup-table init, one-time library bring-up —
/// none of which is guaranteed reentrant. When several synced panes are switched to
/// LUT_ALPHA / Details in the same frame they each fire a render job at once, so
/// their worker threads call `create` **simultaneously** and race that global init
/// (intermittent segfault); applying the operator to one desynced pane at a time
/// never overlaps two constructions, which is why that path never crashes. This
/// mutex makes the concurrent case behave like the serial one. It is held only for
/// the one-time build/free, not for the per-frame `apply`, so steady-state
/// rendering keeps its per-pane parallelism.
static CONSTRUCT: Mutex<()> = Mutex::new(());

/// Load one operator library and resolve its `create`/`apply`/`destroy` symbols.
/// `stem` is the operator's symbol prefix (e.g. `cim_lut_alpha`), to which
/// `_create` / `_apply` / `_destroy` are appended.
fn load_one(lib_path: &Path, stem: &str) -> anyhow::Result<Operator> {
    // SAFETY: loading a shared library and calling its init routines is
    // inherently unsafe; these are trusted, distributed alongside the binary.
    unsafe {
        let lib = libloading::Library::new(lib_path)?;
        let create: libloading::Symbol<CreateFn> =
            lib.get(format!("{stem}_create\0").as_bytes())?;
        // Resolved as the canonical `ApplyFn`; DETAILS_ENHANCED's call site
        // transmutes it to its own `DetailsApplyFn` (same fn-pointer address).
        let apply: libloading::Symbol<ApplyFn> = lib.get(format!("{stem}_apply\0").as_bytes())?;
        let destroy: libloading::Symbol<DestroyFn> =
            lib.get(format!("{stem}_destroy\0").as_bytes())?;
        Ok(Operator {
            create: *create,
            apply: *apply,
            destroy: *destroy,
            _lib: lib,
        })
    }
}

/// Attempt to load both operator libraries from `dir` (the configured library
/// folder, or `None` to resolve by bare name via `LD_LIBRARY_PATH`). Call once at
/// startup. A library that's missing or lacking a symbol simply leaves that
/// operator unavailable (its feature disabled in the UI); it never fails startup.
pub fn init(dir: Option<&Path>) {
    // A missing or unresolvable library simply leaves that operator unavailable
    // (its feature disabled in the UI) — silently, with no startup log noise.
    let _ = load_missing(dir);
}

/// Load any operator library that **isn't loaded yet** from `dir`, leaving
/// already-loaded operators untouched, and return the resulting
/// `(lut_alpha_loaded, details_loaded)`.
///
/// This is the safe way to apply a newly configured folder **without a restart**:
/// it only ever *adds* a library, never unloads one, so it cannot invalidate the
/// `apply`/`destroy` function pointers copied into live render/export instances
/// (see the module docs — those bypass the `RwLock`). It therefore fills in only
/// operators that failed to load at startup (empty/wrong folder then); repointing
/// an *already-loaded* operator at a different folder still needs a restart.
pub fn load_missing(dir: Option<&Path>) -> (bool, bool) {
    // Hold each slot's write lock only while (re)loading it; scope the guards so
    // the `*_available()` reads below take fresh read locks.
    {
        let mut slot = LUT_ALPHA.write().unwrap();
        if slot.is_none() {
            if let Ok(op) = load_one(&resolve(dir, LUT_ALPHA_LIB), "cim_lut_alpha") {
                *slot = Some(op);
            }
        }
    }
    {
        let mut slot = DETAILS.write().unwrap();
        if slot.is_none() {
            if let Ok(op) = load_one(&resolve(dir, DETAILS_LIB), "cim_details_enhanced") {
                *slot = Some(op);
            }
        }
    }
    (lut_alpha_available(), details_available())
}

/// Whether each operator library **file** is present in `dir` (or, with no
/// directory, resolvable next to the working directory by bare name). Returns
/// `(lut_alpha_present, details_present)`. Used by Settings to show a found /
/// not-found indicator for the configured folder — a pure filesystem check that
/// doesn't load anything, so it can run live as the user edits the path.
pub fn libs_present(dir: Option<&Path>) -> (bool, bool) {
    (
        resolve(dir, LUT_ALPHA_LIB).is_file(),
        resolve(dir, DETAILS_LIB).is_file(),
    )
}

/// Whether the LUT_ALPHA operator is loaded and callable. The UI gates the
/// LUT_ALPHA contrast mode on this.
pub fn lut_alpha_available() -> bool {
    LUT_ALPHA.read().unwrap().is_some()
}

/// Whether the Details (detail-enhancement) operator is loaded and callable. The
/// UI gates the RC/Details toggle on this.
pub fn details_available() -> bool {
    DETAILS.read().unwrap().is_some()
}

/// Whether a proprietary operator actually runs on `frame` for the given tone.
/// The operators only accept a single-channel 16-bit frame (`is_op_input`, and
/// never a mask), and only when the wanted operator's library is loaded —
/// otherwise the render falls back to the plain LUT. This is the one predicate
/// the three render paths (live sync `stage`, the render worker, and export)
/// share, so "when do operators run" is decided in a single place.
pub fn ops_active(frame: &crate::media::FrameData, lut_alpha: bool, details: bool) -> bool {
    frame.is_op_input()
        && !frame.is_mask()
        && ((lut_alpha && lut_alpha_available()) || (details && details_available()))
}

/// One live proprietary operator instance: the opaque C++ handle from `create`,
/// the `(width, height)` it was built for, and the fn pointers to drive/free it.
/// Owned by a single pane's worker thread; `Drop` frees the handle on that thread.
struct Instance {
    dims: (usize, usize),
    handle: *mut c_void,
    apply: ApplyFn,
    destroy: DestroyFn,
}

impl Drop for Instance {
    fn drop(&mut self) {
        // Serialise teardown against other panes' `create`/`destroy` for the same
        // reason those are serialised (see `CONSTRUCT`): the vendor destructor may
        // touch shared global state that construction also mutates. Recover a
        // poisoned guard rather than double-panic while unwinding a drop.
        let _guard = CONSTRUCT.lock().unwrap_or_else(PoisonError::into_inner);
        // SAFETY: `handle` came from the matching `create` and is freed exactly
        // once, here, on the owning worker thread.
        unsafe { (self.destroy)(self.handle) };
    }
}

// An `Instance` is only ever touched through `&mut` on its owning thread; the
// raw handle is opaque and never shared.
unsafe impl Send for Instance {}

/// The proprietary operator instances for **one pane**, owned by that pane's
/// render worker thread (`renderer::Worker`) or its export pane. Each operator's
/// instance is created lazily on first use and rebuilt when the frame dimensions
/// change (construction is heavy and size-dependent), so the heavy work is paid
/// once per size and reused across that pane's frames.
#[derive(Default)]
pub struct PaneOps {
    lut_alpha: Option<Instance>,
    details: Option<Instance>,
}

impl PaneOps {
    /// Apply the tone operators to an already-rendered **single-channel 16-bit**
    /// buffer (`width * height` samples) in place: the optional LUT_ALPHA operator
    /// (when `lut_alpha` is set) followed by the optional details enhancement. Each
    /// stage is a no-op when its library isn't loaded (callers also gate on
    /// `lut_alpha_available` / `details_available`). Reuses this pane's cached
    /// instances, rebuilding one only if `width`/`height` changed since last call.
    ///
    /// DETAILS_ENHANCED additionally receives the **after-LUT 8-bit companion** of
    /// the frame — the current view's tone output. That is exactly `gray` as it
    /// stands here (LUT_ALPHA already applied if this is a LUT_ALPHA pane, and the
    /// linear/clip window already baked into the render) downscaled to 8 bits, i.e.
    /// the very pixels the pane would show without details. It is built here, so the
    /// operator always sees whichever LUT the view is currently using.
    ///
    /// This is the operator step of the render tail; [`PaneOps::render_display`]
    /// wraps it (render the 16-bit input, `apply`, expand to RGBA) and is what the
    /// live render worker and the export worker both call, so the two match
    /// pixel-for-pixel.
    pub fn apply(
        &mut self,
        gray: &mut Vec<u16>,
        width: usize,
        height: usize,
        lut_alpha: bool,
        details: bool,
    ) {
        if lut_alpha && Self::ensure(&mut self.lut_alpha, &LUT_ALPHA, width, height) {
            run(self.lut_alpha.as_ref().unwrap(), gray);
        }
        if details && Self::ensure(&mut self.details, &DETAILS, width, height) {
            // The 8-bit companion is the current view LUT output: `gray` (post
            // LUT_ALPHA if used, else the linear/clip map) downscaled to 8 bits.
            let companion: Vec<u8> = gray.iter().map(|&s| (s >> 8) as u8).collect();
            run_details(self.details.as_ref().unwrap(), gray, &companion);
        }
    }

    /// Build a frame's 8-bit display RGBA into `out`, running the proprietary
    /// operators when they're active for this frame/tone (`ops_active`): render a
    /// single-channel 16-bit buffer at full precision, `apply` the operators in
    /// place, then expand the grey back to RGBA. When they're not active, fall
    /// back to the plain LUT render (`FrameData::render_into`).
    ///
    /// Returns `(lut_time, ops_time)` for the `CIM_DEBUG` profiler (`ops_time`
    /// is zero on the plain path). This is the **one** implementation of the
    /// heavy render tail, shared by the live render worker
    /// (`renderer::Worker::render`) and export (`export::ExportPane::render`), so
    /// the two produce identical pixels.
    pub fn render_display(
        &mut self,
        frame: &crate::media::FrameData,
        lo: f32,
        hi: f32,
        lut_alpha: bool,
        details: bool,
        lut: &mut crate::media::ToneLut,
        out: &mut Vec<u8>,
    ) -> (std::time::Duration, std::time::Duration) {
        use std::time::{Duration, Instant};
        if !ops_active(frame, lut_alpha, details) {
            let t = Instant::now();
            frame.render_into_lut(lo, hi, lut, out);
            return (t.elapsed(), Duration::ZERO);
        }
        let [w, h] = frame.size;
        let mut gray = Vec::new();
        let t = Instant::now();
        frame.render_into_gray_u16_lut(lo, hi, lut, &mut gray);
        let lut_time = t.elapsed();
        let t = Instant::now();
        self.apply(&mut gray, w, h, lut_alpha, details);
        let ops_time = t.elapsed();
        // Expand the processed grey back to 8-bit RGBA for the texture.
        out.clear();
        out.resize(gray.len() * 4, 255);
        for (i, &s) in gray.iter().enumerate() {
            let g = (s >> 8) as u8;
            let o = i * 4;
            out[o] = g;
            out[o + 1] = g;
            out[o + 2] = g;
        }
        (lut_time, ops_time)
    }

    /// Ensure `slot` holds an instance of `op` built for `(w, h)`, creating it (or
    /// rebuilding after a size change) as needed. Returns whether a usable instance
    /// is present — `false` if the library is absent or `create` returned null.
    fn ensure(
        slot: &mut Option<Instance>,
        op: &RwLock<Option<Operator>>,
        w: usize,
        h: usize,
    ) -> bool {
        if slot.as_ref().map(|i| i.dims) != Some((w, h)) {
            // Drop the old instance first (frees it on this thread) so a heavy
            // rebuild never holds two instances at once.
            *slot = None;
            if let Some(operator) = op.read().unwrap().as_ref() {
                // Serialise construction across all panes: two worker threads must
                // not enter a vendor `create` at once (see `CONSTRUCT`). This is
                // the one-time, size-dependent build, not the per-frame `apply`, so
                // parallel steady-state rendering is unaffected.
                let _guard = CONSTRUCT.lock().unwrap_or_else(PoisonError::into_inner);
                // SAFETY: `create` per the documented ABI; the returned handle is
                // freed exactly once in `Instance::drop`.
                let handle = unsafe { (operator.create)(w, h) };
                if !handle.is_null() {
                    *slot = Some(Instance {
                        dims: (w, h),
                        handle,
                        apply: operator.apply,
                        destroy: operator.destroy,
                    });
                }
            }
        }
        slot.is_some()
    }
}

/// Run one instance's LUT_ALPHA-style operator over `gray` in place.
fn run(inst: &Instance, gray: &mut [u16]) {
    // SAFETY: `gray` is a valid `len`-element buffer; the callee only reads/writes
    // within it and keeps the dimensions (per the ABI). `handle` matches `apply`.
    unsafe { (inst.apply)(inst.handle, gray.as_mut_ptr(), gray.len()) };
}

/// Run DETAILS_ENHANCED over `gray` in place, passing the read-only after-LUT
/// 8-bit `companion` of the same frame (same length) as a second buffer.
fn run_details(inst: &Instance, gray: &mut [u16], companion: &[u8]) {
    // SAFETY: `gray` and `companion` are valid buffers of the same `len`; the
    // callee writes only `gray` (in place, keeping dimensions) and reads only
    // `companion`, per the DETAILS_ENHANCED ABI. `handle` matches `apply`. The
    // stored `apply` is the same symbol address for either signature; DETAILS was
    // loaded from a library exporting the `DetailsApplyFn` shape, so this
    // transmute recovers the correct type.
    unsafe {
        let apply: DetailsApplyFn = std::mem::transmute(inst.apply);
        apply(
            inst.handle,
            gray.as_mut_ptr(),
            companion.as_ptr(),
            gray.len(),
        );
    }
}

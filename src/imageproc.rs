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
pub fn ops_active(frame: &crate::media::FrameData, ops: Ops) -> bool {
    frame.is_op_input()
        && !frame.is_mask()
        && ((ops.lut_alpha && lut_alpha_available()) || (ops.details && details_available()))
}

/// How to turn a frame's samples into display pixels: the window, the optional
/// Colormap palette, and which proprietary operators to run.
///
/// One value rather than four loose parameters because every render path needs
/// the whole set and they must stay together — `palette` going missing on the
/// way to the render pool is exactly how a Colormap pane came back grey.
#[derive(Clone, Copy, Debug)]
pub struct Display {
    /// Display bounds `[lo, hi] -> [0, 255]`, computed on the UI thread.
    pub lo: f32,
    pub hi: f32,
    /// The Colormap tone: false-colour through this palette instead of grey.
    /// Mutually exclusive with the operators in practice — `uses_colormap` and
    /// `ops_active` can't both hold for one pane — and [`PaneOps::render_display`]
    /// takes the palette branch first regardless.
    pub palette: Option<crate::palette::Palette>,
    pub ops: Ops,
}

/// Which proprietary operators a render should run. The two travel together
/// everywhere — the pane's tone decides both, `ops_active` weighs both, and a
/// render job carries both — so they travel as one value rather than as a pair
/// of bare bools that can be swapped at a call site without complaint.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Ops {
    /// Run LUT_ALPHA (only a LUT_ALPHA-tone pane sets it; masks never do).
    pub lut_alpha: bool,
    /// Run the detail enhancement.
    pub details: bool,
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

/// How many differently-sized instances of one operator a pane keeps alive at
/// once. Adaptive rendering makes a pane alternate between two input sizes —
/// its decimated whole-image base and its viewport region — every frame; with a
/// single slot that alternation would destroy/create the heavy C++ object twice
/// per frame. Small, because instances are heavy: enough for base + region +
/// one transitional size, evicted least-recently-used.
const INSTANCES_PER_OP: usize = 3;

/// The proprietary operator instances for **one pane**, owned by that pane's
/// render worker thread (`renderer::Worker`) or its export pane. Each operator
/// keeps a small most-recent-first list of instances keyed by input size
/// ([`INSTANCES_PER_OP`]): one is created lazily the first time a size is
/// rendered and reused across that pane's frames, so the heavy size-dependent
/// construction is paid once per size — and a pane alternating between its
/// adaptive base and region sizes reuses both instead of rebuilding.
#[derive(Default)]
pub struct PaneOps {
    lut_alpha: Vec<Instance>,
    details: Vec<Instance>,
}

impl PaneOps {
    /// Apply the tone operators to an already-rendered **single-channel 16-bit**
    /// buffer (`width * height` samples) in place: the optional LUT_ALPHA operator
    /// (when `lut_alpha` is set) followed by the optional details enhancement. Each
    /// stage is a no-op when its library isn't loaded (callers also gate on
    /// `lut_alpha_available` / `details_available`). Reuses this pane's cached
    /// instances, building one only for a `(width, height)` it doesn't hold.
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
    pub fn apply(&mut self, gray: &mut [u16], width: usize, height: usize, ops: Ops) {
        if ops.lut_alpha {
            if let Some(inst) = Self::ensure(&mut self.lut_alpha, &LUT_ALPHA, width, height) {
                run(inst, gray);
            }
        }
        if ops.details {
            if let Some(inst) = Self::ensure(&mut self.details, &DETAILS, width, height) {
                // The 8-bit companion is the current view LUT output: `gray`
                // (post LUT_ALPHA if used, else the linear/clip map) downscaled
                // to 8 bits.
                let companion: Vec<u8> = gray.iter().map(|&s| (s >> 8) as u8).collect();
                run_details(inst, gray, &companion);
            }
        }
    }

    /// Build the 8-bit display RGBA of `region` into `out` for the display
    /// `tone`, running the proprietary operators when they're active for this
    /// frame/tone (`ops_active`): render a single-channel 16-bit buffer at full
    /// precision, `apply` the operators in place, then expand the grey back to
    /// RGBA. A Colormap `tone` false-colours through its palette; otherwise, with
    /// no operator active, this is the plain LUT render.
    ///
    /// Returns `(lut_time, ops_time)` for the `CIM_DEBUG` profiler (`ops_time`
    /// is zero on the plain path). This is the **one** implementation of the
    /// heavy render tail, shared by the live render worker
    /// (`renderer::Worker::render`) and export (`export::ExportPane::render`), so
    /// the two produce identical pixels.
    ///
    /// The crop and decimation happen **before** the operators run, so under
    /// adaptive rendering they see the visible region rather than the whole
    /// image and their output (e.g. LUT_ALPHA's auto-contrast) adapts to the
    /// view. Their instance is keyed on `region.out`, which the region geometry
    /// deliberately keeps stable while panning (see `app::roi`). Export renders
    /// whole-image regions, so the two are expected to differ under adaptive
    /// rendering — the Settings hover says so.
    pub fn render_display<S: crate::media::RgbaSink>(
        &mut self,
        frame: &crate::media::FrameData,
        tone: Display,
        region: crate::media::Region,
        lut: &mut crate::media::ToneLut,
        out: &mut S,
    ) -> (std::time::Duration, std::time::Duration) {
        use std::time::{Duration, Instant};
        let Display { lo, hi, ops, .. } = tone;
        // Colormap first: it is a display-only tone, so no operator runs under
        // it. The caller has already checked `uses_colormap` (mono, non-mask),
        // which is what `render_cmap` requires.
        if let Some(pal) = tone.palette {
            let t = Instant::now();
            frame.render_cmap(lo, hi, region, pal, lut, out);
            return (t.elapsed(), Duration::ZERO);
        }
        if !ops_active(frame, ops) {
            let t = Instant::now();
            frame.render_lut(lo, hi, region, lut, out);
            return (t.elapsed(), Duration::ZERO);
        }
        let [w, h] = region.out;
        let mut gray = Vec::new();
        let t = Instant::now();
        frame.render_gray_u16_lut(lo, hi, region, lut, &mut gray);
        let lut_time = t.elapsed();
        let t = Instant::now();
        self.apply(&mut gray, w, h, ops);
        let ops_time = t.elapsed();
        // Expand the processed grey back to 8-bit RGBA for the texture.
        out.begin(gray.len());
        for &s in &gray {
            out.push_gray((s >> 8) as u8);
        }
        (lut_time, ops_time)
    }

    /// The instance of `op` built for `(w, h)`, reusing a cached one (moved to
    /// the front, most recently used) or creating it — evicting the
    /// least-recently-used instance beyond [`INSTANCES_PER_OP`] *before* the
    /// build, so a heavy rebuild never holds an extra instance. `None` if the
    /// library is absent or `create` returned null.
    fn ensure<'a>(
        slot: &'a mut Vec<Instance>,
        op: &RwLock<Option<Operator>>,
        w: usize,
        h: usize,
    ) -> Option<&'a Instance> {
        if let Some(pos) = slot.iter().position(|i| i.dims == (w, h)) {
            // Cache hit: move to the front (most recently used).
            let inst = slot.remove(pos);
            slot.insert(0, inst);
            return slot.first();
        }
        if let Some(operator) = op.read().unwrap().as_ref() {
            // Make room first (frees on this thread) so the build below never
            // holds more than the cap's worth of heavy instances.
            slot.truncate(INSTANCES_PER_OP.saturating_sub(1));
            // Serialise construction across all panes: two worker threads must
            // not enter a vendor `create` at once (see `CONSTRUCT`). This is
            // the one-time, size-dependent build, not the per-frame `apply`, so
            // parallel steady-state rendering is unaffected.
            let _guard = CONSTRUCT.lock().unwrap_or_else(PoisonError::into_inner);
            // SAFETY: `create` per the documented ABI; the returned handle is
            // freed exactly once in `Instance::drop`.
            let handle = unsafe { (operator.create)(w, h) };
            if !handle.is_null() {
                slot.insert(
                    0,
                    Instance {
                        dims: (w, h),
                        handle,
                        apply: operator.apply,
                        destroy: operator.destroy,
                    },
                );
                return slot.first();
            }
        }
        None
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

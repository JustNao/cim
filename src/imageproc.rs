//! Runtime loader for the optional proprietary C++ image-processing operators.
//!
//! The two operators live in **separately built** shared libraries, one each:
//! `cim_lut_alpha` (auto-contrast) and `cim_details_enhanced` (detail
//! enhancement). cim does **not** link them at build time: each is loaded on
//! demand at startup by its hard-coded file name (see `LUT_ALPHA_LIB` /
//! `DETAILS_LIB`), resolved through the system loader's search path — set
//! `LD_LIBRARY_PATH` (Linux) to point at the directory that holds them. Each
//! operator is independent: if its library is missing or its symbol doesn't
//! resolve, only that operator stays **unavailable** and its feature is disabled
//! in the UI — the rest of the viewer (and the other operator) work unchanged.
//!
//! Both operators receive the frame as an interleaved **16-bit** RGBA buffer
//! (`width * height * 4` u16 samples,
//! row-major) and transform it **in place**, keeping the same dimensions and
//! leaving the alpha sample (every 4th) untouched. They are only ever invoked
//! for frames whose native format is 16-bit unsigned (see the U16 gate in
//! `app::decode::prepare` / `export`), so the operator always sees genuine
//! 16-bit precision rather than a value already crushed to 8 bits.
//!
//! See `INTEGRATION_CPP.md` for how to build the libraries and the exact ABI.

use std::sync::RwLock;

/// The C ABI every operator export must match:
/// `void cim_lut_alpha(uint16_t* data, size_t len, size_t width, size_t height)`
/// where `len` is the number of u16 samples (`width * height * 4`).
type OpFn = unsafe extern "C" fn(*mut u16, usize, usize, usize);

// Hard-coded shared-library file names, one operator each. Resolved via the
// system loader's search path (`LD_LIBRARY_PATH`), not an absolute path.
// TODO: replace these placeholders with the real distributed file names.
const LUT_ALPHA_LIB: &str = "libcim_lut_alpha.so"; // placeholder
const DETAILS_LIB: &str = "libcim_details_enhanced.so"; // placeholder

/// A successfully loaded library plus its single resolved entry point. The
/// `Library` is kept alive here because the function pointer borrows from it; it
/// unloads when this slot is cleared.
struct Op {
    _lib: libloading::Library,
    func: OpFn,
}

// The handle is only ever called through `&Op` behind the `RwLock`, and both
// `Library` and bare `fn` pointers are themselves `Send + Sync`.
unsafe impl Send for Op {}
unsafe impl Sync for Op {}

/// The process-wide loaded operators (`None` until loaded / when unavailable).
/// Guarded by an `RwLock` so the render pool / export worker can read them
/// concurrently.
static LUT_ALPHA: RwLock<Option<Op>> = RwLock::new(None);
static DETAILS: RwLock<Option<Op>> = RwLock::new(None);

/// Load one operator library by name and resolve its symbol.
fn load_one(lib_name: &str, symbol: &[u8]) -> anyhow::Result<Op> {
    // SAFETY: loading a shared library and calling its init routines is
    // inherently unsafe; these are trusted, distributed alongside the binary.
    unsafe {
        let lib = libloading::Library::new(lib_name)?;
        let func: libloading::Symbol<OpFn> = lib.get(symbol)?;
        Ok(Op {
            func: *func,
            _lib: lib,
        })
    }
}

/// Attempt to load both operator libraries by their hard-coded names. Call once
/// at startup. A library that's missing or lacking its symbol simply leaves that
/// operator unavailable (its feature disabled in the UI); it never fails startup.
pub fn init() {
    match load_one(LUT_ALPHA_LIB, b"cim_lut_alpha\0") {
        Ok(op) => *LUT_ALPHA.write().unwrap() = Some(op),
        Err(e) => eprintln!("cim: LUT_ALPHA operator unavailable ({LUT_ALPHA_LIB}): {e}"),
    }
    match load_one(DETAILS_LIB, b"cim_details_enhanced\0") {
        Ok(op) => *DETAILS.write().unwrap() = Some(op),
        Err(e) => eprintln!("cim: Details operator unavailable ({DETAILS_LIB}): {e}"),
    }
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

/// Apply LUT_ALPHA auto-contrast to an RGBA16 buffer in place (no-op if the
/// library isn't loaded). `rgba` must be `width * height * 4` samples.
fn lut_alpha(rgba: &mut [u16], width: usize, height: usize) {
    if let Some(op) = LUT_ALPHA.read().unwrap().as_ref() {
        // SAFETY: `rgba` is a valid `len`-element buffer; the callee only reads/
        // writes within it and keeps the dimensions (per the documented ABI).
        unsafe { (op.func)(rgba.as_mut_ptr(), rgba.len(), width, height) };
    }
}

/// Apply DETAILS_ENHANCED detail enhancement to an RGBA16 buffer in place
/// (no-op if the library isn't loaded). `rgba` must be `width * height * 4`.
fn details_enhanced(rgba: &mut [u16], width: usize, height: usize) {
    if let Some(op) = DETAILS.read().unwrap().as_ref() {
        // SAFETY: see `lut_alpha`.
        unsafe { (op.func)(rgba.as_mut_ptr(), rgba.len(), width, height) };
    }
}

/// Apply the post-render tone operators to an already-rendered RGBA16 buffer in
/// place: optional LUT_ALPHA (mixed back toward the linear image by `1 - blend`
/// when `lut_blend = Some(blend)`; `None` skips it) followed by the optional
/// details enhancement. This is the one shared tail of the render pipeline, run
/// both off-thread for the live view (`renderer::render`) and by the export
/// worker (`export::ExportPane::ensure_frame`), so the two match pixel-for-pixel.
/// Each stage is a no-op when its library isn't loaded (the callers also gate on
/// `lut_alpha_available` / `details_available`, so the buffer is simply the plain
/// linear render in that case).
pub fn apply_operators(rgba: &mut Vec<u16>, width: usize, height: usize, lut_blend: Option<f32>, details: bool) {
    if let Some(blend) = lut_blend {
        let blend = blend.clamp(0.0, 1.0);
        if blend >= 1.0 {
            lut_alpha(rgba, width, height);
        } else {
            // Mix the operator's output back toward the plain linear image.
            let base = rgba.clone();
            lut_alpha(rgba, width, height);
            blend_rgba16(rgba, &base, blend);
        }
    }
    if details {
        details_enhanced(rgba, width, height);
    }
}

/// Blend `out` toward `base` by `1 - t`: `out = t·out + (1 - t)·base` per sample.
fn blend_rgba16(out: &mut [u16], base: &[u16], t: f32) {
    let t = t.clamp(0.0, 1.0);
    for (o, &b) in out.iter_mut().zip(base) {
        *o = (b as f32 * (1.0 - t) + *o as f32 * t).round().clamp(0.0, 65535.0) as u16;
    }
}

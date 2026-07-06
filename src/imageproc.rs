//! Runtime loader for the optional proprietary C++ image-processing library.
//!
//! The two operators (LUT_ALPHA auto-contrast, DETAILS_ENHANCED detail
//! enhancement) live in a **separately built** shared library (`.so` on Linux,
//! `.dll` on Windows). cim does **not** link it at build time: it's loaded on
//! demand at runtime from a path in Settings (`Config::ops_library_path`). If no
//! path is set, the file is missing, or its symbols don't resolve, the operators
//! stay **unavailable** and the LUT_ALPHA / Details features are disabled in the
//! UI — the rest of the viewer works unchanged.
//!
//! Both operators receive the frame as an interleaved **16-bit** RGBA buffer
//! (`width * height * 4` u16 samples,
//! row-major) and transform it **in place**, keeping the same dimensions and
//! leaving the alpha sample (every 4th) untouched. They are only ever invoked
//! for frames whose native format is 16-bit unsigned (see the U16 gate in
//! `app::decode::prepare` / `export`), so the operator always sees genuine
//! 16-bit precision rather than a value already crushed to 8 bits.
//!
//! See `INTEGRATION_CPP.md` for how to build the library and the exact ABI.

use std::path::Path;
use std::sync::RwLock;

/// The C ABI every operator export must match:
/// `void cim_lut_alpha(uint16_t* data, size_t len, size_t width, size_t height)`
/// where `len` is the number of u16 samples (`width * height * 4`).
type OpFn = unsafe extern "C" fn(*mut u16, usize, usize, usize);

/// A successfully loaded library plus its two resolved entry points. The
/// `Library` is kept alive here because the function pointers borrow from it; it
/// unloads when this is replaced (`load`) or cleared (`unload`).
struct Ops {
    _lib: libloading::Library,
    lut_alpha: OpFn,
    details_enhanced: OpFn,
}

// The handles are only ever called through `&Ops` behind the `RwLock`, and both
// `Library` and bare `fn` pointers are themselves `Send + Sync`.
unsafe impl Send for Ops {}
unsafe impl Sync for Ops {}

/// The process-wide loaded operators (`None` until a library is loaded). Guarded
/// by an `RwLock` so the render pool / export worker can read it concurrently.
static OPS: RwLock<Option<Ops>> = RwLock::new(None);

/// Load the proprietary library from `path`, resolving both operator symbols.
/// Replaces any previously loaded library on success; on failure the previous
/// state is left untouched and the error is returned for the UI to surface.
pub fn load(path: &Path) -> anyhow::Result<()> {
    // SAFETY: loading an arbitrary shared library and calling its init routines
    // is inherently unsafe; the user vouches for the file they point us at.
    unsafe {
        let lib = libloading::Library::new(path)?;
        let lut_alpha: libloading::Symbol<OpFn> = lib.get(b"cim_lut_alpha\0")?;
        let details_enhanced: libloading::Symbol<OpFn> = lib.get(b"cim_details_enhanced\0")?;
        let ops = Ops {
            lut_alpha: *lut_alpha,
            details_enhanced: *details_enhanced,
            _lib: lib,
        };
        *OPS.write().unwrap() = Some(ops);
    }
    Ok(())
}

/// Drop the loaded library, disabling the operators again.
pub fn unload() {
    *OPS.write().unwrap() = None;
}

/// Whether the proprietary operators are currently loaded and callable. The UI
/// gates the LUT_ALPHA mode and the Details toggle on this.
pub fn is_available() -> bool {
    OPS.read().unwrap().is_some()
}

/// Apply LUT_ALPHA auto-contrast to an RGBA16 buffer in place (no-op if the
/// library isn't loaded). `rgba` must be `width * height * 4` samples.
fn lut_alpha(rgba: &mut [u16], width: usize, height: usize) {
    if let Some(ops) = OPS.read().unwrap().as_ref() {
        // SAFETY: `rgba` is a valid `len`-element buffer; the callee only reads/
        // writes within it and keeps the dimensions (per the documented ABI).
        unsafe { (ops.lut_alpha)(rgba.as_mut_ptr(), rgba.len(), width, height) };
    }
}

/// Apply DETAILS_ENHANCED detail enhancement to an RGBA16 buffer in place
/// (no-op if the library isn't loaded). `rgba` must be `width * height * 4`.
fn details_enhanced(rgba: &mut [u16], width: usize, height: usize) {
    if let Some(ops) = OPS.read().unwrap().as_ref() {
        // SAFETY: see `lut_alpha`.
        unsafe { (ops.details_enhanced)(rgba.as_mut_ptr(), rgba.len(), width, height) };
    }
}

/// Apply the post-render tone operators to an already-rendered RGBA16 buffer in
/// place: optional LUT_ALPHA (mixed back toward the linear image by `1 - blend`
/// when `lut_blend = Some(blend)`; `None` skips it) followed by the optional
/// details enhancement. This is the one shared tail of the render pipeline, run
/// both off-thread for the live view (`renderer::render`) and by the export
/// worker (`export::ExportPane::ensure_frame`), so the two match pixel-for-pixel.
/// A whole no-op when the library isn't loaded (the callers also gate on
/// `is_available`, so the buffer is simply the plain linear render in that case).
pub fn apply_operators(rgba: &mut Vec<u16>, width: usize, height: usize, lut_blend: Option<f32>, details: bool) {
    if !is_available() {
        return;
    }
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

//! FFI bridge to the proprietary C++ image-processing functions.
//!
//! The heavy lifting lives in C++ (`cpp/imageproc.{h,cpp}`); this module is just
//! the [`cxx`] bridge plus thin safe wrappers the render pipeline calls. Both
//! operators take an interleaved 8-bit **RGBA** buffer and transform it in
//! place (same dimensions), so they slot straight onto the RGBA that
//! `FrameData::render_into` produces just before texture upload.
//!
//! See `INTEGRATION_CPP.md` for how to drop the real proprietary code in.

#[cxx::bridge(namespace = "cim")]
mod ffi {
    unsafe extern "C++" {
        include!("cpp/imageproc.h");

        /// Auto-contrast tone mapping (the alternative to the 0.01% clip).
        fn lut_alpha(data: &mut [u8], width: usize, height: usize);

        /// Local detail / sharpness enhancement.
        fn details_enhanced(data: &mut [u8], width: usize, height: usize);
    }
}

/// Apply LUT_ALPHA auto-contrast to an RGBA8 buffer in place.
/// `rgba` must be `width * height * 4` bytes.
pub fn lut_alpha(rgba: &mut [u8], width: usize, height: usize) {
    ffi::lut_alpha(rgba, width, height);
}

/// Apply DETAILS_ENHANCED detail enhancement to an RGBA8 buffer in place.
/// `rgba` must be `width * height * 4` bytes.
pub fn details_enhanced(rgba: &mut [u8], width: usize, height: usize) {
    ffi::details_enhanced(rgba, width, height);
}

/// Apply the post-LUT tone operators to an already-rendered RGBA8 buffer in
/// place: optional LUT_ALPHA (mixed back toward the linear image by `1 - blend`
/// when `lut_blend = Some(blend)`; `None` skips it) followed by the optional
/// details enhancement. This is the one shared tail of the render pipeline, run
/// both off-thread for the live view (`renderer::render`) and by the export
/// worker (`export::ExportPane::ensure_frame`), so the two match pixel-for-pixel.
pub fn apply_operators(rgba: &mut Vec<u8>, width: usize, height: usize, lut_blend: Option<f32>, details: bool) {
    if let Some(blend) = lut_blend {
        let blend = blend.clamp(0.0, 1.0);
        if blend >= 1.0 {
            lut_alpha(rgba, width, height);
        } else {
            // Mix the operator's output back toward the plain linear image.
            let base = rgba.clone();
            lut_alpha(rgba, width, height);
            blend_rgba(rgba, &base, blend);
        }
    }
    if details {
        details_enhanced(rgba, width, height);
    }
}

/// Blend `out` toward `base` by `1 - t`: `out = t·out + (1 - t)·base` per byte.
fn blend_rgba(out: &mut [u8], base: &[u8], t: f32) {
    let t = t.clamp(0.0, 1.0);
    for (o, &b) in out.iter_mut().zip(base) {
        *o = (b as f32 * (1.0 - t) + *o as f32 * t).round().clamp(0.0, 255.0) as u8;
    }
}

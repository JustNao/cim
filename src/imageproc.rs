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

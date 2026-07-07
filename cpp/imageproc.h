// C ABI exposed by the optional proprietary image-processing libraries.
//
// cim loads these libraries at **runtime** (not at link time) via `libloading`
// (see src/imageproc.rs) by hard-coded file name, resolved through the loader
// search path (`LD_LIBRARY_PATH`; Linux-only). Each operator lives in its **own**
// shared library (`libcim_lut_alpha.so` / `libcim_details_enhanced.so`) that
// exports the three C symbols for that operator. cim resolves them by these exact
// names, so they must be `extern "C"` (unmangled).
//
// ---------- Why three symbols per operator (create / apply / destroy) ----------
// Each operator is a proprietary C++ class whose construction is HEAVY and
// depends on the image SIZE (not its contents). cim therefore does not call a
// stateless function per frame; it drives a lifecycle instead:
//
//     handle = cim_<op>_create(width, height);      // once per (pane, size)
//     cim_<op>_apply(handle, data, len);            // per frame, transforms in place
//     cim_<op>_destroy(handle);                     // on close / reload / resize
//
// cim keeps **one instance per pane** and reuses it across that pane's frames,
// rebuilding only when the dimensions change (see PaneOps in src/imageproc.rs).
// Each pane's instance lives on that pane's own worker thread (src/renderer.rs),
// so a given instance is only ever touched by one thread — the proprietary class
// need not be thread-safe / reentrant.
//
// ---------- Data contract (do not change without updating src/imageproc.rs) -----
//   * `data` is a single-channel 16-bit buffer, `len == width * height` u16
//     samples, one per pixel, row-major (`len` is passed so you can bounds-check).
//   * `apply` transforms it IN PLACE and keeps the same dimensions.
//   * The operators are only ever called for single-channel 16-bit images; cim
//     expands the result back to grey RGBA afterwards.
//   * `create` returns an opaque handle, or NULL on failure — cim then treats the
//     operator as unavailable for that pane and falls back to the plain render.
//   * Only plain C crosses this boundary. Vendor C++ types (image classes, pixel
//     formats, …) stay inside the .cpp; if a vendor value must reach cim, add a
//     plain C enum/struct here and mirror it in src/imageproc.rs.
#pragma once

#include <cstddef>
#include <cstdint>

#if defined(_WIN32)
#define CIM_EXPORT __declspec(dllexport)
#else
#define CIM_EXPORT __attribute__((visibility("default")))
#endif

extern "C" {

// ---- LUT_ALPHA: auto-contrast tone mapping (libcim_lut_alpha.so) -----------
CIM_EXPORT void* cim_lut_alpha_create(std::size_t width, std::size_t height);
CIM_EXPORT void cim_lut_alpha_apply(void* handle, std::uint16_t* data, std::size_t len);
CIM_EXPORT void cim_lut_alpha_destroy(void* handle);

// ---- DETAILS_ENHANCED: local detail / sharpness (libcim_details_enhanced.so) --
CIM_EXPORT void* cim_details_enhanced_create(std::size_t width, std::size_t height);
CIM_EXPORT void cim_details_enhanced_apply(void* handle, std::uint16_t* data, std::size_t len);
CIM_EXPORT void cim_details_enhanced_destroy(void* handle);

} // extern "C"

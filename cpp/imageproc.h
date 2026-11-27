// C ABI exposed by the optional proprietary image-processing libraries.
//
// cim loads these libraries at **runtime** (not at link time) via `libloading`
// (see src/imageproc.rs) by hard-coded file name, resolved through the loader
// search path (`LD_LIBRARY_PATH`; Linux-only). Each operator lives in its **own**
// shared library (`libcim_lut_alpha.so` / `libcim_boost.so` /
// `libcim_details_enhanced.so`) that exports the three C symbols for that operator. cim resolves them by these exact
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
//   * DETAILS_ENHANCED's `apply` takes a SECOND buffer, `lut8`: the after-LUT
//     8-bit companion of the same frame — the CURRENT VIEW LUT output, i.e. the
//     exact grayscale the pane is showing (LUT_ALPHA / Boost applied if that's the
//     active tone, else the linear/clip map). `len` samples, one per pixel, row-major,
//     READ-ONLY (const) context; write your result into `data`, not `lut8`.
//   * The operators are only ever called for single-channel 16-bit images; cim
//     expands the result back to grey RGBA afterwards.
//   * `create` returns an opaque handle, or NULL on failure — cim then treats the
//     operator as unavailable for that pane and falls back to the plain render.
//   * Only plain C crosses this boundary. Vendor C++ types (image classes, pixel
//     formats, …) stay inside the .cpp; if a vendor value must reach cim, add a
//     plain C enum/struct here and mirror it in src/imageproc.rs.
//
// ---------- Optional: tunable construction parameters + presets ---------------
// An operator whose construction takes parameters (read from a preset file next
// to the library, say) may ALSO export the seven `cim_<op>_param_*` /
// `cim_<op>_preset_*` / `cim_<op>_create_with` symbols below. cim then offers the
// library's presets as starting points and every parameter for editing (the ⚙
// button beside the tone, in the Transformations panel). All seven or none: a
// library missing any of them loads exactly as before, with no parameter UI.
//
//   * A parameter is a `len`-long list of numbers (`len == 1` for a scalar),
//     float or int (`kind`). `min`/`max` bound the editor; NaN = unbounded.
//   * Values cross as ONE flat `double` array: the parameters in index order,
//     each contributing its `len` values (`n == sum of len`). Ints are whole
//     doubles.
//   * `preset_default` is the preset plain `create` uses; `create_with` builds
//     the instance from explicit values instead. All parameters are construction
//     parameters: cim builds a fresh instance whenever they change.
//   * Strings (`key`, preset names) must stay valid while the library is loaded;
//     cim copies them once, at load.
//   * The info / values calls return 0 on success, non-zero on a bad index.
#pragma once

#include <cstddef>
#include <cstdint>

#if defined(_WIN32)
#define CIM_EXPORT __declspec(dllexport)
#else
#define CIM_EXPORT __attribute__((visibility("default")))
#endif

extern "C" {

// One tunable parameter (see "Optional: tunable construction parameters").
typedef struct {
    const char* key;  // stable name, shown in the UI and saved in presets
    int kind;         // CIM_PARAM_FLOAT or CIM_PARAM_INT
    std::size_t len;  // number of values (1 = scalar, n = a list "[x1 ... xn]")
    double min;       // NaN = unbounded
    double max;       // NaN = unbounded
} cim_param_info;

enum { CIM_PARAM_FLOAT = 0, CIM_PARAM_INT = 1 };

// ---- LUT_ALPHA: auto-contrast tone mapping (libcim_lut_alpha.so) -----------
CIM_EXPORT void* cim_lut_alpha_create(std::size_t width, std::size_t height);
CIM_EXPORT void cim_lut_alpha_apply(void* handle, std::uint16_t* data, std::size_t len);
CIM_EXPORT void cim_lut_alpha_destroy(void* handle);
// Optional (all or none) — LUT_ALPHA's tunable parameters and presets. Boost may
// export the same set under its own `cim_boost_` prefix.
CIM_EXPORT std::size_t cim_lut_alpha_param_count(void);
CIM_EXPORT int cim_lut_alpha_param_info(std::size_t i, cim_param_info* out);
CIM_EXPORT std::size_t cim_lut_alpha_preset_count(void);
CIM_EXPORT const char* cim_lut_alpha_preset_name(std::size_t preset);
CIM_EXPORT std::size_t cim_lut_alpha_preset_default(void);
CIM_EXPORT int cim_lut_alpha_preset_values(std::size_t preset, double* out, std::size_t n);
CIM_EXPORT void* cim_lut_alpha_create_with(std::size_t width, std::size_t height,
                                           const double* values, std::size_t n);

// ---- Boost: alternative auto-contrast tone mapping (libcim_boost.so) --------
// Same contract as LUT_ALPHA (full native range in, its own contrast out); a
// pane uses one or the other, never both.
CIM_EXPORT void* cim_boost_create(std::size_t width, std::size_t height);
CIM_EXPORT void cim_boost_apply(void* handle, std::uint16_t* data, std::size_t len);
CIM_EXPORT void cim_boost_destroy(void* handle);

// ---- DETAILS_ENHANCED: local detail / sharpness (libcim_details_enhanced.so) --
// `apply` additionally receives `lut8`, the after-LUT 8-bit companion of the same
// frame (read-only). Transform `data` (16-bit) in place; `lut8` is context only.
CIM_EXPORT void* cim_details_enhanced_create(std::size_t width, std::size_t height);
CIM_EXPORT void cim_details_enhanced_apply(void* handle, std::uint16_t* data,
                                           const std::uint8_t* lut8, std::size_t len);
CIM_EXPORT void cim_details_enhanced_destroy(void* handle);

} // extern "C"

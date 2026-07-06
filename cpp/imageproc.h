// C ABI exposed by the optional proprietary image-processing library.
//
// cim loads this library at **runtime** (not at link time) via `libloading`
// (see src/imageproc.rs) from the path set in Settings. Build it as a shared
// library (`.so` / `.dll`) — for example with CMake `add_library(cim_ops SHARED …)`
// — that exports these two C symbols. cim resolves them by these exact names,
// so they must be `extern "C"` (unmangled) and, on Windows, exported.
//
// Each operator receives the frame as an interleaved **16-bit RGBA** buffer
// (`len == width * height * 4` u16 samples, row-major) and transforms it **in
// place**, keeping the dimensions and leaving the alpha sample (every 4th)
// untouched. They are only ever called for genuinely 16-bit images.
#pragma once

#include <cstddef>
#include <cstdint>

#if defined(_WIN32)
#define CIM_EXPORT __declspec(dllexport)
#else
#define CIM_EXPORT __attribute__((visibility("default")))
#endif

extern "C" {

// Auto-contrast tone mapping (replaces the old 0.01% percentile clip).
CIM_EXPORT void cim_lut_alpha(std::uint16_t* data, std::size_t len, std::size_t width, std::size_t height);

// Local detail / sharpness enhancement.
CIM_EXPORT void cim_details_enhanced(std::uint16_t* data, std::size_t len, std::size_t width, std::size_t height);

} // extern "C"

// C ABI exposed by the optional proprietary image-processing library.
//
// cim loads these libraries at **runtime** (not at link time) via `libloading`
// (see src/imageproc.rs) by hard-coded file name, resolved through the loader
// search path (`LD_LIBRARY_PATH`). Each operator lives in its own shared library
// (`.so` / `.dll`) — for example with CMake `add_library(... SHARED …)` — that
// exports its one C symbol. cim resolves them by these exact names, so they must
// be `extern "C"` (unmangled) and, on Windows, exported.
//
// Each operator receives the frame as a **single-channel 16-bit** buffer
// (`len == width * height` u16 samples, one per pixel, row-major) and transforms
// it **in place**, keeping the dimensions. They are only ever called for
// single-channel 16-bit images; cim expands the result back to grey RGBA.
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

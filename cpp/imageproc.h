// Thin C++ wrapper layer between cim's cxx bridge (src/imageproc.rs) and the
// proprietary image-processing library.
//
// The bridge calls these free functions. Each receives the frame as an
// interleaved 8-bit **RGBA** buffer (4 bytes/pixel, `width * height * 4` long)
// and transforms it **in place**. This is the seam where the proprietary
// classes get called — see cpp/imageproc.cpp and INTEGRATION_CPP.md.
#pragma once

#include "rust/cxx.h"

#include <cstddef>
#include <cstdint>

namespace cim {

// Auto-contrast tone mapping (replaces the old 0.01% percentile clip).
void lut_alpha(rust::Slice<std::uint8_t> data, std::size_t width, std::size_t height);

// Local detail / sharpness enhancement.
void details_enhanced(rust::Slice<std::uint8_t> data, std::size_t width, std::size_t height);

} // namespace cim

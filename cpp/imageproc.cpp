// Implementation of the cim <-> proprietary C++ bridge wrappers.
//
// ============================ INTEGRATION POINT ============================
// The bodies below are PLACEHOLDERS so the project builds and runs before the
// proprietary sources are dropped in. Replace each body with a call into your
// real classes. The only contract the Rust side depends on is:
//
//   * `data` is interleaved 8-bit RGBA, `width * height * 4` bytes, row-major.
//   * You transform it IN PLACE and keep the same dimensions.
//   * Alpha (every 4th byte) should be left as-is (cim keeps it at 255).
//
// If your library works on RGB (3 ch), planar, or its own Image class, adapt
// here: copy R/G/B out with stride 4, run your algorithm, copy back. Example:
//
//     MyImage img(width, height);
//     for (size_t i = 0; i < width * height; ++i) {
//         img.r[i] = data[i*4+0]; img.g[i] = data[i*4+1]; img.b[i] = data[i*4+2];
//     }
//     proprietary::LutAlpha().apply(img);           // <-- your class
//     for (size_t i = 0; i < width * height; ++i) {
//         data[i*4+0] = img.r[i]; data[i*4+1] = img.g[i]; data[i*4+2] = img.b[i];
//     }
// ==========================================================================
#include "cpp/imageproc.h"

#include <algorithm>
#include <cstdint>

namespace cim {

// PLACEHOLDER: a plain per-image min/max contrast stretch, standing in for the
// proprietary LUT_ALPHA auto-contrast until the real code is linked in.
void lut_alpha(rust::Slice<std::uint8_t> data, std::size_t width, std::size_t height) {
    const std::size_t px = width * height;
    if (px == 0 || data.size() < px * 4) {
        return;
    }

    std::uint8_t lo = 255, hi = 0;
    for (std::size_t i = 0; i < px; ++i) {
        for (int c = 0; c < 3; ++c) {
            std::uint8_t v = data[i * 4 + c];
            lo = std::min(lo, v);
            hi = std::max(hi, v);
        }
    }
    if (hi <= lo) {
        return; // flat image, nothing to stretch
    }

    const float scale = 255.0f / static_cast<float>(hi - lo);
    for (std::size_t i = 0; i < px; ++i) {
        for (int c = 0; c < 3; ++c) {
            float v = (static_cast<float>(data[i * 4 + c]) - lo) * scale;
            data[i * 4 + c] = static_cast<std::uint8_t>(std::clamp(v, 0.0f, 255.0f));
        }
    }
}

// PLACEHOLDER: identity. Swap in the proprietary DETAILS_ENHANCED here.
void details_enhanced(rust::Slice<std::uint8_t> data, std::size_t width, std::size_t height) {
    (void)data;
    (void)width;
    (void)height;
}

} // namespace cim

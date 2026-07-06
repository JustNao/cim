// Reference implementation of the cim image-processing operators.
//
// ============================ INTEGRATION POINT ============================
// This file (plus your proprietary sources) is compiled into a **standalone
// shared library** (`.so` / `.dll`) — see INTEGRATION_CPP.md — which cim loads
// at runtime. It is NOT compiled into cim itself. The bodies below are
// PLACEHOLDERS; replace each with a call into your real classes. The contract
// the Rust side depends on is:
//
//   * `data` is a single-channel 16-bit buffer, `len == width * height` samples,
//     one per pixel, row-major (`len` is provided so you can bounds-check).
//   * You transform it IN PLACE and keep the same dimensions.
//   * These are only ever called for single-channel 16-bit images.
//
// If your library works on its own Image class, adapt here: copy the samples
// out, run your algorithm, copy back. Example:
//
//     MyImage img(width, height);
//     for (size_t i = 0; i < width * height; ++i) img.gray[i] = data[i];
//     proprietary::LutAlpha().apply(img);           // <-- your class
//     for (size_t i = 0; i < width * height; ++i) data[i] = img.gray[i];
// ==========================================================================
#include "imageproc.h"

#include <algorithm>
#include <cstddef>
#include <cstdint>

// PLACEHOLDER: a plain per-image min/max contrast stretch, standing in for the
// proprietary LUT_ALPHA auto-contrast until the real code is linked in.
extern "C" void cim_lut_alpha(std::uint16_t* data, std::size_t len, std::size_t width, std::size_t height) {
    const std::size_t px = width * height;
    if (px == 0 || len < px) {
        return;
    }

    std::uint16_t lo = 65535, hi = 0;
    for (std::size_t i = 0; i < px; ++i) {
        lo = std::min(lo, data[i]);
        hi = std::max(hi, data[i]);
    }
    if (hi <= lo) {
        return; // flat image, nothing to stretch
    }

    const float scale = 65535.0f / static_cast<float>(hi - lo);
    for (std::size_t i = 0; i < px; ++i) {
        float v = (static_cast<float>(data[i]) - lo) * scale;
        data[i] = static_cast<std::uint16_t>(std::clamp(v, 0.0f, 65535.0f));
    }
}

// PLACEHOLDER: identity. Swap in the proprietary DETAILS_ENHANCED here.
extern "C" void cim_details_enhanced(std::uint16_t* data, std::size_t len, std::size_t width, std::size_t height) {
    (void)data;
    (void)len;
    (void)width;
    (void)height;
}

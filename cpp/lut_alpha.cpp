// LUT_ALPHA operator — compiled into libcim_lut_alpha.so (see ../INTEGRATION_CPP.md).
//
// ============================ INTEGRATION POINT ============================
// This file is NOT compiled into cim. It is built into a standalone shared
// library that cim loads at runtime and resolves by symbol name. Replace the
// PLACEHOLDER `LutAlpha` below with your proprietary auto-contrast class.
//
// cim drives the create/apply/destroy lifecycle documented in imageproc.h:
//   * cim_lut_alpha_create(w, h)  — do the HEAVY, size-dependent construction
//                                    here; it runs once per (pane, image size).
//   * cim_lut_alpha_apply(h, ...) — the per-frame call; transform the buffer in
//                                    place, reusing the instance from `create`.
//   * cim_lut_alpha_destroy(h)    — free it (pane closed / reloaded / resized).
//
// ---- Wiring in your proprietary code ----------------------------------------
// Your operator comes from several .so in one vendor directory (non-colliding).
// In THIS file you may freely #include the vendor headers and use their types
// (image classes, pixel-format enums, …) — they stay on this side of the C
// boundary; only plain C (pointers, sizes) crosses to cim. Point the CMake
// target at that directory's headers + entry library (see ../cpp/CMakeLists.txt).
// A real implementation typically looks like:
//
//     #include <vendor_alpha/auto_contrast.h>   // <-- your real header(s)
//     #include <vendor_alpha/image.h>           // Image, PixelFormat, …
//
//     extern "C" void* cim_lut_alpha_create(std::size_t w, std::size_t h) {
//         return new vendor_alpha::AutoContrast(w, h);          // heavy, once
//     }
//     extern "C" void cim_lut_alpha_apply(void* handle, std::uint16_t* data, std::size_t len) {
//         auto* op = static_cast<vendor_alpha::AutoContrast*>(handle);
//         // Wrap cim's raw single-channel 16-bit buffer as the vendor image type
//         // (a zero-copy view if the API allows, else copy in, run, copy back):
//         vendor_alpha::Image img(data, len, vendor_alpha::PixelFormat::Gray16);
//         op->run(img);                                         // transforms in place
//     }
//     extern "C" void cim_lut_alpha_destroy(void* handle) {
//         delete static_cast<vendor_alpha::AutoContrast*>(handle);
//     }
//
// If cim itself must choose/report a vendor value (e.g. a pixel format), do NOT
// leak the vendor type across the boundary: add a plain C enum to imageproc.h,
// map it to the vendor enum here, and mirror the C enum in src/imageproc.rs.
// ==========================================================================
#include "imageproc.h"

#include <algorithm>
#include <cstddef>
#include <cstdint>

namespace {

// PLACEHOLDER for the proprietary LUT_ALPHA class. It keeps the image size (as a
// real size-dependent operator would) and does a per-image min/max contrast
// stretch, so the library builds and cim's whole pipeline — the create/apply/
// destroy lifecycle, per-pane instance reuse, the off-thread render — can be
// exercised end-to-end before the proprietary code is available. Swap it out.
struct LutAlpha {
    std::size_t width;
    std::size_t height;

    LutAlpha(std::size_t w, std::size_t h) : width(w), height(h) {}

    void apply(std::uint16_t* data, std::size_t len) const {
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
            const float v = (static_cast<float>(data[i]) - lo) * scale;
            data[i] = static_cast<std::uint16_t>(std::clamp(v, 0.0f, 65535.0f));
        }
    }
};

} // namespace

extern "C" void* cim_lut_alpha_create(std::size_t width, std::size_t height) {
    return new LutAlpha(width, height);
}

extern "C" void cim_lut_alpha_apply(void* handle, std::uint16_t* data, std::size_t len) {
    static_cast<LutAlpha*>(handle)->apply(data, len);
}

extern "C" void cim_lut_alpha_destroy(void* handle) {
    delete static_cast<LutAlpha*>(handle);
}

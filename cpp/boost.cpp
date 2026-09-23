// Boost operator — compiled into libcim_boost.so (see ../INTEGRATION_CPP.md).
//
// ============================ INTEGRATION POINT ============================
// This file is NOT compiled into cim. It is built into a standalone shared
// library that cim loads at runtime and resolves by symbol name. Replace the
// PLACEHOLDER `Boost` below with your proprietary auto-contrast class.
//
// Boost is a tone, like LUT_ALPHA, and follows the same contract: cim hands it
// the frame over its FULL native range (no clip / Share clip / region window),
// and the operator computes its own contrast. It is an alternative to
// LUT_ALPHA, never chained after it; DETAILS_ENHANCED, when on, runs after it.
//
// cim drives the create/apply/destroy lifecycle documented in imageproc.h:
//   * cim_boost_create(w, h)  — do the HEAVY, size-dependent construction here;
//                                it runs once per (pane, image size).
//   * cim_boost_apply(h, ...) — the per-frame call; transform the buffer in
//                                place, reusing the instance from `create`.
//   * cim_boost_destroy(h)    — free it (pane closed / reloaded / resized).
//
// ---- Wiring in your proprietary code ----------------------------------------
// Exactly as for LUT_ALPHA (see the worked example at the top of lut_alpha.cpp):
// #include the vendor headers here, keep vendor types on this side of the C
// boundary, and point the `cim_boost` CMake target at the vendor headers +
// entry library (see ../cpp/CMakeLists.txt).
// ==========================================================================
#include "imageproc.h"

#include <algorithm>
#include <cmath>
#include <cstddef>
#include <cstdint>

namespace {

// PLACEHOLDER for the proprietary Boost class. It keeps the image size (as a
// real size-dependent operator would) and applies a min/max stretch followed by
// a gamma lift, so it is visibly different from the LUT_ALPHA placeholder while
// letting cim's whole pipeline — the create/apply/destroy lifecycle, per-pane
// instance reuse, the off-thread render — be exercised end-to-end before the
// proprietary code is available. Swap it out.
struct Boost {
    std::size_t width;
    std::size_t height;

    Boost(std::size_t w, std::size_t h) : width(w), height(h) {}

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
        const float range = static_cast<float>(hi - lo);
        for (std::size_t i = 0; i < px; ++i) {
            const float t = (static_cast<float>(data[i]) - lo) / range; // 0..1
            const float v = std::pow(t, 0.5f) * 65535.0f;              // lift the darks
            data[i] = static_cast<std::uint16_t>(std::clamp(v, 0.0f, 65535.0f));
        }
    }
};

} // namespace

extern "C" void* cim_boost_create(std::size_t width, std::size_t height) {
    return new Boost(width, height);
}

extern "C" void cim_boost_apply(void* handle, std::uint16_t* data, std::size_t len) {
    static_cast<Boost*>(handle)->apply(data, len);
}

extern "C" void cim_boost_destroy(void* handle) {
    delete static_cast<Boost*>(handle);
}

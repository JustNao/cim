// DETAILS_ENHANCED operator — compiled into libcim_details_enhanced.so
// (see ../INTEGRATION_CPP.md).
//
// ============================ INTEGRATION POINT ============================
// This file is NOT compiled into cim. It is built into a standalone shared
// library that cim loads at runtime and resolves by symbol name. Replace the
// PLACEHOLDER `DetailsEnhanced` below with your proprietary detail/sharpening
// class. See lut_alpha.cpp for the fully worked wiring example — the pattern is
// identical, only the vendor header/class and library differ.
//
// cim drives the create/apply/destroy lifecycle documented in imageproc.h:
//   * cim_details_enhanced_create(w, h)  — HEAVY, size-dependent construction,
//                                          once per (pane, image size).
//   * cim_details_enhanced_apply(h, ...) — per frame; transform `data` (16-bit)
//                                          in place, using `lut8` (the after-LUT
//                                          8-bit companion) as read-only context.
//   * cim_details_enhanced_destroy(h)    — free it.
//
// You may #include your vendor headers and use their types here; only plain C
// crosses to cim. Point this operator's CMake target at its own vendor directory
// (headers + entry library) — it is independent of the LUT_ALPHA subsystem.
// ==========================================================================
#include "imageproc.h"

#include <cstddef>
#include <cstdint>

namespace {

// PLACEHOLDER for the proprietary DETAILS_ENHANCED class: an identity transform
// that keeps the image size, so the library builds and the lifecycle can be
// exercised before the real code lands. Swap it out.
struct DetailsEnhanced {
    std::size_t width;
    std::size_t height;

    DetailsEnhanced(std::size_t w, std::size_t h) : width(w), height(h) {}

    // `data`  — raw 16-bit grayscale, transformed IN PLACE (kept same size).
    // `lut8`  — the after-LUT 8-bit companion of the same frame (read-only): the
    //           current view LUT output — exactly what the pane shows (LUT_ALPHA
    //           if that's the active tone, else the linear/clip map). Use it to
    //           steer the enhancement; do not write to it.
    void apply(std::uint16_t* data, const std::uint8_t* lut8, std::size_t len) const {
        (void)data;
        (void)lut8;
        (void)len;
        // Identity: leave the buffer unchanged.
    }
};

} // namespace

extern "C" void* cim_details_enhanced_create(std::size_t width, std::size_t height) {
    return new DetailsEnhanced(width, height);
}

extern "C" void cim_details_enhanced_apply(void* handle, std::uint16_t* data,
                                           const std::uint8_t* lut8, std::size_t len) {
    static_cast<DetailsEnhanced*>(handle)->apply(data, lut8, len);
}

extern "C" void cim_details_enhanced_destroy(void* handle) {
    delete static_cast<DetailsEnhanced*>(handle);
}

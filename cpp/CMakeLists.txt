# Example CMake build for the optional cim image-processing operators.
#
# Builds TWO shared libraries — one per operator — under the exact names cim
# loads at runtime (see src/imageproc.rs: LUT_ALPHA_LIB / DETAILS_LIB):
#
#     libcim_lut_alpha.so         <- lut_alpha.cpp        + your vendor alpha subsystem
#     libcim_details_enhanced.so  <- details_enhanced.cpp + your vendor detail subsystem
#
# Each library exports only its own operator's cim_* symbols and links only its
# own vendor code, so a missing library disables just that feature in cim. This
# file is built SEPARATELY from cim (cim's `cargo build` compiles no C++):
#
#     cmake -S cpp -B build -DCMAKE_BUILD_TYPE=Release
#     cmake --build build
#     # -> build/libcim_lut_alpha.so, build/libcim_details_enhanced.so
#
# Then put both libraries AND their vendor .so dependencies together in one
# directory on cim's loader path and launch cim (Linux-only):
#
#     LD_LIBRARY_PATH=/path/to/lib ./cim

cmake_minimum_required(VERSION 3.15)
project(cim_ops LANGUAGES CXX)

# One SHARED target per operator. SHARED is the key: it emits a .so, not a .a.
# CMake adds the `lib` prefix and `.so` suffix, so the target name is the stem:
# target `cim_lut_alpha` -> `libcim_lut_alpha.so`, exactly what cim resolves.
add_library(cim_lut_alpha SHARED lut_alpha.cpp)
add_library(cim_details_enhanced SHARED details_enhanced.cpp)

foreach(op cim_lut_alpha cim_details_enhanced)
    target_compile_features(${op} PRIVATE cxx_std_17)
    set_target_properties(${op} PROPERTIES
        # A shared library needs PIC; setting it explicitly also forces any static
        # vendor archives you link in to be built/linked PIC (else the linker
        # rejects them — see INTEGRATION_CPP.md).
        POSITION_INDEPENDENT_CODE ON
        # Resolve sibling vendor .so next to this library at runtime, so dropping
        # the whole lib/ folder on the loader path "just works".
        BUILD_RPATH "$ORIGIN"
        INSTALL_RPATH "$ORIGIN"
    )
endforeach()

# --- Wire in the proprietary subsystems (one per operator) ------------------
# Each operator's vendor code lives in its own directory of non-colliding .so.
# Point each target at its headers and link its entry library; the OS dynamic
# loader pulls in the rest of that subsystem transitively (via the vendor libs'
# own NEEDED entries + the $ORIGIN rpath above). Uncomment and set the paths:
#
#   target_include_directories(cim_lut_alpha PRIVATE /path/to/vendor_alpha/include)
#   target_link_libraries(cim_lut_alpha PRIVATE /path/to/vendor_alpha/lib/libvendor_alpha.so)
#
#   target_include_directories(cim_details_enhanced PRIVATE /path/to/vendor_details/include)
#   target_link_libraries(cim_details_enhanced PRIVATE /path/to/vendor_details/lib/libvendor_details.so)
#
# To fold in a static archive instead of a vendor .so, build it -fPIC and force
# all its objects into the library (else objects reached only indirectly are
# dropped and your symbols come up empty):
#
#   target_link_libraries(cim_lut_alpha PRIVATE
#       -Wl,--whole-archive /path/to/vendor_alpha/libvendor_alpha.a -Wl,--no-whole-archive)

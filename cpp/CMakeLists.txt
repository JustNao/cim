# Example CMake build for the optional cim image-processing library.
#
# Produces a shared library (libcim_ops.so / cim_ops.dll) exporting the two C
# entry points cim loads at runtime (see imageproc.h and ../INTEGRATION_CPP.md).
# Point cim's Settings → "Image processing" → Library at the built file.
#
#   cmake -S cpp -B build -DCMAKE_BUILD_TYPE=Release
#   cmake --build build
#
# To fold in your proprietary project, either add its sources to the target or
# link its (PIC-built) static archive — see INTEGRATION_CPP.md.

cmake_minimum_required(VERSION 3.15)
project(cim_ops LANGUAGES CXX)

# SHARED is the key: it makes a .so/.dll, not a .a. (Equivalently, configure the
# whole tree with -DBUILD_SHARED_LIBS=ON for untyped add_library targets.)
add_library(cim_ops SHARED imageproc.cpp)

target_compile_features(cim_ops PRIVATE cxx_std_17)

# A shared library needs position-independent code. CMake adds -fPIC for SHARED
# targets automatically; set it explicitly so any static deps you link in are
# built PIC too (else the linker rejects them — see INTEGRATION_CPP.md).
set_target_properties(cim_ops PROPERTIES
    POSITION_INDEPENDENT_CODE ON
    # On Windows, export every symbol so the C entry points land in the .dll's
    # export table without per-function __declspec (imageproc.h also marks them).
    WINDOWS_EXPORT_ALL_SYMBOLS ON
)

# --- Linking the proprietary project ---------------------------------------
# Option A — add its sources:
#   target_sources(cim_ops PRIVATE proprietary/lut_alpha.cpp proprietary/details.cpp)
#   target_include_directories(cim_ops PRIVATE proprietary/include)
#
# Option B — link its prebuilt static archive (must be built with -fPIC).
# --whole-archive pulls in objects even if only reached indirectly:
#   target_link_libraries(cim_ops PRIVATE
#       -Wl,--whole-archive ${CMAKE_SOURCE_DIR}/proprietary/libproprietary.a -Wl,--no-whole-archive)

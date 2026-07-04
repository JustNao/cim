# Integrating the proprietary C++ image functions

cim calls two proprietary C++ operators through a [`cxx`](https://cxx.rs) bridge.
The wiring is already in place with **placeholder** implementations; this doc is
the checklist for dropping in the real sources.

## Where the pieces live

| File | Role |
|------|------|
| `src/imageproc.rs` | The `#[cxx::bridge]` module + safe Rust wrappers (`lut_alpha`, `details_enhanced`). |
| `cpp/imageproc.h` | C++ declarations the bridge sees. |
| `cpp/imageproc.cpp` | **The integration point** — thin wrappers that call your classes. Currently placeholders. |
| `build.rs` | Compiles the bridge + `cpp/*.cpp` into a static lib via `cxx_build`. |
| `src/app/decode.rs` | Live view: applies the operators to the rendered RGBA before texture upload. |
| `src/export.rs` | Export: applies the same operators so an export matches the screen. |

## The data contract (do not change without updating both sides)

Each operator is called as:

```cpp
void cim::lut_alpha(rust::Slice<uint8_t> data, size_t width, size_t height);
void cim::details_enhanced(rust::Slice<uint8_t> data, size_t width, size_t height);
```

- `data` is **interleaved 8-bit RGBA**, `width * height * 4` bytes, row-major.
- Transform **in place**, keep the same dimensions.
- Leave the alpha byte (every 4th) untouched — cim keeps it at 255.

## Steps to drop in the real code

1. **Add your sources.** Put the proprietary `.cpp`/`.h` under `cpp/` (or a
   subdir). List every `.cpp` in `build.rs`:

   ```rust
   cxx_build::bridge("src/imageproc.rs")
       .file("cpp/imageproc.cpp")
       .file("cpp/lut_alpha.cpp")        // <- your files
       .file("cpp/details_enhanced.cpp")
       .include(&manifest)
       .std("c++17")                     // bump if your code needs c++20
       .compile("cim_imageproc");
   ```
   Add `.rerun-if-changed` lines for them too. For header-only deps, add an
   include dir with `.include("cpp/vendor/include")`.

2. **Fill in the wrappers** in `cpp/imageproc.cpp`. Replace each placeholder
   body with a call into your classes, converting the RGBA buffer to/from
   whatever your API expects (see the worked RGB example in that file's header
   comment).

3. **Prebuilt `.lib`/`.dll` instead of source?** You still need the tiny wrapper
   `.cpp` (it's what the bridge links against). Point the linker at the blob
   from `build.rs`:

   ```rust
   println!("cargo:rustc-link-search=native=C:/path/to/lib");
   println!("cargo:rustc-link-lib=static=proprietary");   // or dylib=
   ```
   For a DLL, ship the `.dll` next to `cim.exe`.

4. `cargo build` / `cargo run`.

## Notes & gotchas

- **A C++ compiler is required to build** now: MSVC (Build Tools) on Windows,
  gcc/clang on Linux. The Linux CI image (`ci/build-linux-glibc228.sh`, Debian
  buster) already has gcc; confirm your code compiles under that toolchain's
  C++ standard.
- **8-bit precision.** The operators receive an image already mapped to 8-bit
  full range. For a 16-bit source that means detail below 1/256 is gone before
  `lut_alpha` sees it. If the proprietary auto-contrast needs the native 16-bit
  data, widen the bridge to pass `u16` samples + bit depth and render *after*
  the operator instead — happy to wire that variant if needed.
- **Threading.** Today the operators run on the UI thread inside `prepare`
  (live) and one-frame-per-tick during export. If a call is heavy, that will
  stutter the UI; the fix is the already-planned "threaded export" and/or moving
  the live operator onto the decode pool. Make sure the C++ is thread-safe (or
  guard it) before parallelising.
- **Determinism / caching.** cim only re-runs an operator when a frame's texture
  is stale (frame changed, or the user toggled the mode — which sets
  `pane.tex = None`). The operators must be pure functions of their input for
  that cache to stay correct.

# Integrating the proprietary C++ image functions

cim calls two proprietary C++ operators — **LUT_ALPHA** (auto-contrast) and
**DETAILS_ENHANCED** (detail/sharpening). Each lives in its **own separately
built shared library** (`.so` on Linux, `.dll` on Windows) that cim loads **at
runtime**, not at build time. There is no C++ compiler or `cxx` dependency in
cim's own build anymore.

Because the libraries are loaded dynamically:

- cim builds and runs with **no** proprietary code present.
- cim loads each library at startup by its **hard-coded file name**
  (`LUT_ALPHA_LIB` / `DETAILS_LIB` in `src/imageproc.rs` — currently
  placeholders), resolved through the OS loader's search path. Put the libraries
  on that path with `LD_LIBRARY_PATH` (Linux) when launching cim.
- Each operator is **independent**: if its library is missing or its symbol
  doesn't resolve, only **that** operator's feature is disabled in the UI (the
  LUT_ALPHA mode, or the Details toggle); the other keeps working and cim
  otherwise behaves as a plain viewer.

## Where the pieces live

| File | Role |
|------|------|
| `src/imageproc.rs` | Runtime loader (`libloading`): hard-coded library names, `init`/`lut_alpha_available`/`details_available` + the `apply_operators` tail called by the render pool and export. |
| `cpp/imageproc.h` | The **C ABI** cim resolves by name (`cim_lut_alpha`, `cim_details_enhanced`). |
| `cpp/imageproc.cpp` | **The integration point** — placeholder bodies to replace with calls into your classes. |
| `cpp/CMakeLists.txt` | Example build producing the operator `.so` / `.dll`. |
| `src/renderer.rs` / `src/export.rs` | Apply the operators (live view off-thread; export on its worker) so the two match pixel-for-pixel. |

## The data contract (do not change without updating both sides)

cim resolves and calls these two exact C symbols:

```cpp
extern "C" void cim_lut_alpha(uint16_t* data, size_t len, size_t width, size_t height);
extern "C" void cim_details_enhanced(uint16_t* data, size_t len, size_t width, size_t height);
```

- `data` is a **single-channel 16-bit** buffer, `len == width * height` samples
  (one per pixel), row-major (`len` is passed so you can bounds-check).
- Transform **in place**, keep the same dimensions.
- **Single-channel 16-bit only.** cim invokes the operators *only* for images
  whose native format is single-channel (grayscale) 16-bit unsigned, rendering to
  a single-channel 16-bit buffer first so you see full precision; the result is
  expanded back to grey RGBA and downscaled to 8 bits for display *after* your
  operator runs. For multi-channel, 8-bit, or float images the operators are
  never called and the UI disables them.

The symbols must be **`extern "C"`** (so the names aren't C++-mangled) and, on
Windows, **exported** from the DLL (`__declspec(dllexport)` — `imageproc.h` does
this — or CMake's `WINDOWS_EXPORT_ALL_SYMBOLS`).

## Building the library

### With the example CMake

```sh
cmake -S cpp -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
# → build/libcim_ops.so  (or build/cim_ops.dll)
```

The one flag that matters is **`SHARED`** in `add_library(cim_ops SHARED …)` —
that is what emits a `.so`/`.dll` instead of a static `.a`. (Configuring the
tree with `-DBUILD_SHARED_LIBS=ON` does the same for untyped `add_library`
targets, but does **not** override targets that explicitly say `STATIC`.)

### Folding in your proprietary project

You said the proprietary code is a large, slow-to-compile project shipped as a
static `.a`. Two ways to combine it:

1. **Add its sources** to the `cim_ops` target (`target_sources` /
   `target_include_directories`).
2. **Link its prebuilt `.a`.** A `.a` is just an archive of `.o` objects, so the
   linker can pull them into a `.so` **without recompiling the sources** — but
   the objects must be **position-independent** (`-fPIC`). Rebuild the project
   once with `-DCMAKE_POSITION_INDEPENDENT_CODE=ON` (or `add_library(... SHARED)`
   for its own libraries), then:

   ```cmake
   target_link_libraries(cim_ops PRIVATE
       -Wl,--whole-archive ${PROP}/libproprietary.a -Wl,--no-whole-archive)
   ```

   `--whole-archive` forces every object into the `.so` (otherwise the linker
   drops objects nothing references yet, and your exported functions come up
   empty). On MSVC the equivalent is `/WHOLEARCHIVE:proprietary.lib`.

   *If you see* `relocation R_X86_64_32 … can not be used when making a shared
   object; recompile with -fPIC`, the archive wasn't built PIC — rebuild the
   proprietary project with `-fPIC`. (Windows has no `-fPIC` requirement.)

### Dependency libraries at runtime

cim loads exactly **two** entry libraries — the ones whose hard-coded names are
`LUT_ALPHA_LIB` / `DETAILS_LIB` in `src/imageproc.rs`, each exporting its single
`cim_*` symbol. Any further shared libraries are their dependencies, pulled in by
the OS dynamic loader when an entry library loads, **as long as it can find
them**. Keep everything together in a `lib/` folder and put that folder on the
loader's search path when launching cim:

```sh
LD_LIBRARY_PATH=/path/to/lib ./cim        # Linux
```

The entry libraries themselves must also be findable under that same
`LD_LIBRARY_PATH` (cim loads them by bare name, not absolute path).
`ldd <lib>.so` lists what each needs — if cim logs a "cannot open shared object"
error at startup, either an entry library or one of its dependencies wasn't on
the path.

## Filling in the operators

Replace the placeholder bodies in `cpp/imageproc.cpp` with calls into your
classes, converting the single-channel 16-bit buffer to/from whatever your API expects
(there's a worked RGB example in that file's header comment).

Put the built libraries on the loader path under the names cim expects
(`LUT_ALPHA_LIB` / `DETAILS_LIB`) and you're done — no rebuild of cim needed to
swap a library later.

## Notes & gotchas

- **Threading.** The live operators run **off the UI thread** on the render pool
  (`src/renderer.rs`), which spawns **one worker thread per pane** (keyed by the
  stable pane `id`). A given pane's renders all run on that one thread, so its
  operator instances are only ever touched by a single thread — **serialised per
  pane, no reliance on the proprietary code being reentrant** — while *different*
  panes render in parallel. The per-pane worker (`renderer::Worker`) is the sole
  owner of that pane's operator instances: build them lazily on the first job that
  needs them and rebuild when a frame's dimensions change (instantiation is
  media-specific and heavy), and they're destroyed on that thread when the pane is
  closed/reloaded (`RenderPool::forget`). Export runs the operators on its own
  worker thread. There is no shared operator state across panes to guard.
- **Determinism / caching.** cim only re-runs an operator when a frame's texture
  is stale (frame changed, or the user toggled the mode). The operators must be
  pure functions of their input for that cache to stay correct.
- **Precision.** The operators receive full 16-bit precision. The Rust-side
  `blend` (LUT_ALPHA mode) and the 8-bit downscale happen *after* your operator,
  so it never sees a pre-crushed image.

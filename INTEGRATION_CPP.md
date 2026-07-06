# Integrating the proprietary C++ image functions

cim calls two proprietary C++ operators — **LUT_ALPHA** (auto-contrast) and
**DETAILS_ENHANCED** (detail/sharpening). They live in a **separately built
shared library** (`.so` on Linux, `.dll` on Windows) that cim loads **at
runtime**, not at build time. There is no C++ compiler or `cxx` dependency in
cim's own build anymore.

Because the library is loaded dynamically:

- cim builds and runs with **no** proprietary code present.
- You point cim at the library file in **Settings → Image processing → Library**
  (persisted as `ops_library_path`; also loaded at startup).
- If the path is unset, the file is missing, or a symbol doesn't resolve, the
  **LUT_ALPHA mode and the Details toggle are disabled** in the UI and cim
  behaves as a plain viewer.

## Where the pieces live

| File | Role |
|------|------|
| `src/imageproc.rs` | Runtime loader (`libloading`): `load`/`unload`/`is_available` + the `apply_operators` tail called by the render pool and export. |
| `cpp/imageproc.h` | The **C ABI** cim resolves by name (`cim_lut_alpha`, `cim_details_enhanced`). |
| `cpp/imageproc.cpp` | **The integration point** — placeholder bodies to replace with calls into your classes. |
| `cpp/CMakeLists.txt` | Example build producing `libcim_ops.so` / `cim_ops.dll`. |
| `src/settings.rs` | `Config.ops_library_path`; `src/app/panels.rs` draws the picker. |
| `src/renderer.rs` / `src/export.rs` | Apply the operators (live view off-thread; export on its worker) so the two match pixel-for-pixel. |

## The data contract (do not change without updating both sides)

cim resolves and calls these two exact C symbols:

```cpp
extern "C" void cim_lut_alpha(uint16_t* data, size_t len, size_t width, size_t height);
extern "C" void cim_details_enhanced(uint16_t* data, size_t len, size_t width, size_t height);
```

- `data` is **interleaved 16-bit RGBA**, `len == width * height * 4` samples,
  row-major (`len` is passed so you can bounds-check).
- Transform **in place**, keep the same dimensions.
- Leave the alpha sample (every 4th) untouched — cim keeps it at 65535.
- **16-bit only.** cim invokes the operators *only* for images whose native
  format is 16-bit unsigned, rendering to a 16-bit RGBA buffer first so you see
  full precision; the result is downscaled to 8 bits for display *after* your
  operator runs. For 8-bit / float images the operators are never called and the
  UI disables them.

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

If your build produces **many** shared libraries, you do **not** link or register
all of them with cim. cim loads exactly **one** — the entry library
(`libcim_ops.so`) exporting the two `cim_*` symbols. The rest are its
dependencies, pulled in by the OS dynamic loader when the entry library loads,
**as long as it can find them**. Keep them together in a `lib/` folder and put
that folder on the loader's search path when launching cim:

```sh
LD_LIBRARY_PATH=/path/to/lib ./cim        # Linux
```

Point cim's **Settings → Image processing → Library** at the entry file itself
(`/path/to/lib/libcim_ops.so`); the dependencies resolve via `LD_LIBRARY_PATH`.
`ldd libcim_ops.so` lists what it needs — if cim's load fails with a "cannot open
shared object" error, a dependency wasn't on the path.

## Filling in the operators

Replace the placeholder bodies in `cpp/imageproc.cpp` with calls into your
classes, converting the 16-bit RGBA buffer to/from whatever your API expects
(there's a worked RGB example in that file's header comment).

Point cim at the built library and you're done — no rebuild of cim needed to
swap the library later.

## Notes & gotchas

- **Threading.** The live operators run **off the UI thread** on a dedicated
  render pool (`src/renderer.rs`), created with **one** worker
  (`RenderPool::new(1)` in `CimApp::new`), so operator calls are serialised —
  safe even if the proprietary code isn't reentrant. Export runs them on its own
  worker thread. **If (and only if) the operators are thread-safe, raise that
  worker count** to render several panes in parallel; otherwise leave it at 1.
- **Determinism / caching.** cim only re-runs an operator when a frame's texture
  is stale (frame changed, or the user toggled the mode). The operators must be
  pure functions of their input for that cache to stay correct.
- **Precision.** The operators receive full 16-bit precision. The Rust-side
  `blend` (LUT_ALPHA mode) and the 8-bit downscale happen *after* your operator,
  so it never sees a pre-crushed image.

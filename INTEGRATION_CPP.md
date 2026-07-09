# Integrating the proprietary C++ image functions

cim calls two proprietary C++ operators — **LUT_ALPHA** (auto-contrast) and
**DETAILS_ENHANCED** (detail/sharpening). Each lives in its **own separately
built shared library** (`.so`) that cim loads **at runtime**, not at build time.
This is a **Linux-only** feature. There is no C++ compiler or `cxx` dependency in
cim's own build — `cargo build` compiles no C++ at all.

Because the libraries are loaded dynamically:

- cim builds and runs with **no** proprietary code present.
- cim loads each library at startup by its **hard-coded file name**
  (`LUT_ALPHA_LIB` / `DETAILS_LIB` in `src/imageproc.rs` — currently
  placeholders), resolved through the OS loader's search path. Put the libraries
  on that path with `LD_LIBRARY_PATH` when launching cim. A missing library is
  **silently** ignored (no startup log).
- Each operator is **independent**: if its library is missing or a symbol doesn't
  resolve, only **that** operator's feature is disabled in the UI (the LUT_ALPHA
  mode, or the Details toggle); the other keeps working and cim otherwise behaves
  as a plain viewer.

## Where the pieces live

| File | Role |
|------|------|
| `src/imageproc.rs` | Runtime loader (`libloading`): hard-coded library names, `init` / `lut_alpha_available` / `details_available`, and **`PaneOps`** — one pane's operator instances (create/apply/destroy lifecycle) driven by the render pool and export. |
| `cpp/imageproc.h` | The **C ABI** cim resolves by name — the `create`/`apply`/`destroy` triple per operator, plus the full rationale. |
| `cpp/lut_alpha.cpp` | **Integration point** for LUT_ALPHA → `libcim_lut_alpha.so`. Placeholder class to replace with your auto-contrast class; worked wiring example in the header comment. |
| `cpp/details_enhanced.cpp` | **Integration point** for DETAILS_ENHANCED → `libcim_details_enhanced.so`. |
| `cpp/CMakeLists.txt` | Example build producing the two operator `.so`. |
| `src/renderer.rs` / `src/export.rs` | Each holds a `PaneOps` and applies the operators (live view off-thread per pane; export on its worker) so the two match pixel-for-pixel. |

## The data contract (do not change without updating both sides)

The operators are **heavy, size-dependent C++ objects**, not stateless functions.
Each library exports a **three-symbol lifecycle**, and cim resolves these exact
names (`extern "C"`, unmangled):

```cpp
// libcim_lut_alpha.so
extern "C" void* cim_lut_alpha_create(size_t width, size_t height);
extern "C" void  cim_lut_alpha_apply(void* handle, uint16_t* data, size_t len);
extern "C" void  cim_lut_alpha_destroy(void* handle);

// libcim_details_enhanced.so
extern "C" void* cim_details_enhanced_create(size_t width, size_t height);
extern "C" void  cim_details_enhanced_apply(void* handle, uint16_t* data,
                                            const uint8_t* lut8, size_t len);
extern "C" void  cim_details_enhanced_destroy(void* handle);
```

**DETAILS_ENHANCED's `apply` takes a second buffer, `lut8`** — the **after-LUT
8-bit** companion of the same frame: the **current view LUT output**, i.e. the
exact grayscale the pane is showing. Whatever LUT the view is using is the 8-bit
input — **LUT_ALPHA** when that's the active tone, otherwise the linear/clip map.
`len` samples, one per pixel, row-major, **read-only**. cim builds it in
`PaneOps::apply` (`src/imageproc.rs`) as the 16-bit `data` after any LUT_ALPHA,
downscaled to 8 bits. Transform the 16-bit `data` in place using `lut8` as
context; never write `lut8`.

- **`create(width, height)`** builds the instance. This is where the **heavy,
  size-dependent construction** goes; cim calls it **once per (pane, image size)**.
  Return an opaque handle, or `NULL` on failure (cim then treats the operator as
  unavailable for that pane and falls back to the plain render).
- **`apply(handle, data, len)`** runs per frame. `data` is a **single-channel
  16-bit** buffer, `len == width * height` samples (one per pixel), row-major.
  Transform it **in place**, keep the same dimensions. cim reuses the instance
  across the pane's frames. **DETAILS_ENHANCED's `apply` additionally takes
  `lut8`** — a read-only `len`-sample 8-bit companion (the after-LUT look) —
  before `len` (see the ABI block above).
- **`destroy(handle)`** frees the instance (pane closed / reloaded / resized).

cim keeps **one instance per pane** in `PaneOps`, rebuilding it only when the
frame dimensions change — so an image sequence with varying page sizes is handled
by transparently reconstructing the operator for the new size. Each pane's
instance lives on that pane's **own worker thread**, so it is only ever touched by
one thread (no thread-safety requirement on the proprietary class).

**Single-channel 16-bit only.** cim invokes the operators *only* for images whose
native format is single-channel (grayscale) 16-bit unsigned, rendering to a
single-channel 16-bit buffer first so you see full precision; the result is
expanded back to grey RGBA and downscaled to 8 bits for display *after* your
operator runs. For multi-channel, 8-bit, or float images the operators are never
called and the UI disables them.

**Only plain C crosses the boundary.** Inside the `.cpp` you may use any vendor
C++ types (image classes, pixel-format enums, …). If a vendor value must reach cim,
do not leak the vendor type: add a plain C enum/struct to `imageproc.h` and mirror
it in `src/imageproc.rs`. cim passes a raw `uint16_t*` (plus, for
DETAILS_ENHANCED, a read-only `const uint8_t*` companion), so most format/type
handling stays entirely inside the shim.

## Building the library — cim and the shim build separately

Yes: since the C++ build was removed from cim, **you build the shim yourself**,
once, whenever the shim or vendor code changes. cim's `cargo build` never touches
it. The two are fully decoupled — that is the point of runtime loading (swap a
library without rebuilding cim).

```sh
cmake -S cpp -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build
# → build/libcim_lut_alpha.so
# → build/libcim_details_enhanced.so
```

The key in `CMakeLists.txt` is **`SHARED`** in `add_library(<op> SHARED …)` —
that is what emits a `.so` instead of a static `.a`. There is **one target per
operator**, each exporting only its own `cim_*` symbols and linking only its own
vendor subsystem.

### Where does the compiled code go?

The `.so` are **not** placed anywhere in cim's build tree (`target/…`); they are a
**runtime** dependency. cim loads them by **bare file name** via the OS loader, so
they just need to be on `LD_LIBRARY_PATH` when cim launches. Keep both entry
libraries **and all their vendor `.so` dependencies together in one directory**
(e.g. a `lib/` folder next to the binary) and point the loader at it:

```sh
LD_LIBRARY_PATH=/path/to/lib ./cim
```

The entry libraries must be findable there under the exact names cim expects
(`libcim_lut_alpha.so` / `libcim_details_enhanced.so`). `ldd libcim_lut_alpha.so`
lists what each pulls in — if a dependency is missing, the entry library fails to
load and that operator is silently unavailable.

### Folding in your proprietary project

Each operator's vendor code lives in its own directory of shared libraries. Point
that operator's target at its headers and entry library (see the commented block
in `CMakeLists.txt`); the OS loader pulls in the rest of the subsystem
transitively, helped by the `$ORIGIN` rpath the example sets so sibling vendor
`.so` resolve at runtime.

To fold in a prebuilt **static** archive instead, a `.a` is just an archive of
`.o` objects, so the linker can pull them into a `.so` **without recompiling** —
but the objects must be **position-independent** (`-fPIC`). Rebuild the vendor
project once with `-DCMAKE_POSITION_INDEPENDENT_CODE=ON`, then:

```cmake
target_link_libraries(cim_lut_alpha PRIVATE
    -Wl,--whole-archive /path/to/vendor_alpha/libvendor_alpha.a -Wl,--no-whole-archive)
```

`--whole-archive` forces every object into the `.so` (otherwise the linker drops
objects nothing references yet, and your exported functions come up empty).

*If you see* `relocation R_X86_64_32 … can not be used when making a shared
object; recompile with -fPIC`, the archive wasn't built PIC — rebuild the vendor
project with `-fPIC`.

### Dependency libraries and namespaces

The two subsystems are **non-colliding** and share `.so` from the same vendor
directory, so plain `dlopen` of the two entry libraries is all cim does — no
separate linker namespaces (`dlmopen`) are needed. If two subsystems ever *did*
carry the same dependency SONAME at incompatible versions, the in-process fix
would be to load each entry library into its own link-map namespace via
`dlmopen(LM_ID_NEWLM)`; that is not the case here.

## Filling in the operators

Replace the placeholder classes in `cpp/lut_alpha.cpp` / `cpp/details_enhanced.cpp`
with your real ones: do the heavy construction in `create`, convert cim's
single-channel 16-bit buffer to/from your API in `apply`, free in `destroy`. Each
file has a worked wiring example in its header comment. Then build (above) and put
the `.so` on the loader path — no rebuild of cim needed to swap a library later.

## Notes & gotchas

- **Threading.** The live operators run **off the UI thread** on the render pool
  (`src/renderer.rs`), which spawns **one worker thread per pane** (keyed by the
  stable pane `id`). A given pane's renders all run on that one thread, so its
  operator instances are only ever touched by a single thread — **serialised per
  pane, no reliance on the proprietary code being reentrant** — while *different*
  panes render in parallel. The per-pane worker (`renderer::Worker`) owns a
  `PaneOps`: instances are built lazily on the first job that needs them, rebuilt
  when a frame's dimensions change, and destroyed on that thread when the pane is
  closed/reloaded (`RenderPool::forget`). Export runs the operators on its own
  worker thread with its own `PaneOps`. There is no shared operator state across
  panes to guard.
- **Determinism / caching.** cim only re-runs an operator when a frame's texture
  is stale (frame changed, or the user toggled the mode). `apply` must be a pure
  function of its input (given a fixed size) for that cache to stay correct.
- **Precision.** The operators receive full 16-bit precision. The 8-bit downscale
  happens *after* your operator, so it never sees a pre-crushed image.

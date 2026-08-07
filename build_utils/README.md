# build_utils — offline Docker build environment

Builds the `cim` release binaries for **Linux** and **Windows** inside Docker
images that work on a machine with **no internet access**. It mirrors the CI
(`.github/workflows/build.yml` + `ci/build-linux-glibc228.sh`): the Linux binary
is built against **glibc 2.28** in `debian:buster`; the Windows binary is
**cross-compiled** with `mingw-w64`.

```
build_utils/
  Dockerfile.linux    offline image for the Linux binary   (debian:buster)
  Dockerfile.windows  offline image for the Windows binary (debian:bookworm)
  build.sh            builds the project for linux / windows / both
  README.md           this file
```

## Why two images

The two targets want **opposite** toolchains:

- the Linux binary must link against an **old glibc** (2.28), which is what
  keeps it on the EOL `debian:buster` base;
- the Windows cross-link needs a **recent mingw-w64 and binutils**. Buster's
  pair (mingw-w64 6.0.0, binutils 2.31) cannot link this project any more: since
  eframe's `wgpu` feature pulled in wgpu/naga, `naga`'s `f64::atan` call hits
  mingw 6.0.0's ``multiple definition of `__imp_atan'`` bug (its `libntdll.a`
  duplicates the CRT math import stubs `libmsvcrt.a` defines; fixed upstream in
  mingw-w64 8.0.0), and binutils 2.31's `ld` then dies with **SIGSEGV** on the
  much larger object set those crates add.

A Windows binary has no glibc constraint, so the Windows image moves forward to
`debian:bookworm` (mingw-w64 10.0.0, binutils 2.40) while the Linux one stays
put. They were one image until that link broke.

## How the offline part works

Each image is **built once on an internet-connected machine**. During that build
it pulls and bakes in everything a compile needs:

- its Debian build packages (GTK/X11/GL for eframe on Linux, `mingw-w64` for
  Windows),
- the pinned Rust toolchain (`1.96.0`), plus the `x86_64-pc-windows-gnu` target
  in the Windows image,
- **every crate from `Cargo.lock`** (`cargo fetch` for all target platforms, into
  `CARGO_HOME` inside the image).

You then transfer the image to the air-gapped machine and build there with
`--offline`. No crates, toolchains, or packages are downloaded at build time —
only your source tree is mounted in.

## 1. Build the images (online machine)

From the **repo root** (the build context must be the repo so `Cargo.toml` /
`Cargo.lock` are visible to `COPY`):

```sh
build_utils/build.sh image            # both
build_utils/build.sh image linux      # Linux only
build_utils/build.sh image windows    # Windows only
# equivalently:
docker build -f build_utils/Dockerfile.linux   -t cim-build-linux:latest   .
docker build -f build_utils/Dockerfile.windows -t cim-build-windows:latest .
```

## 2. Export and transfer

```sh
docker save cim-build-linux:latest   | gzip > cim-build-linux.tar.gz
docker save cim-build-windows:latest | gzip > cim-build-windows.tar.gz
# copy them to the air-gapped machine by whatever means allowed
```

Only the image for the target you actually build there is needed.

## 3. Load and build (air-gapped machine)

```sh
docker load < cim-build-linux.tar.gz
docker load < cim-build-windows.tar.gz

# from anywhere in the repo:
build_utils/build.sh            # both targets
build_utils/build.sh linux      # Linux only
build_utils/build.sh windows    # Windows only
```

Outputs land under the repo (kept separate from your host `target/` so they
never clobber a local dev build):

| Target  | Path                                                        |
|---------|-------------------------------------------------------------|
| Linux   | `target/docker/linux/release/cim`                           |
| Windows | `target/docker/windows/x86_64-pc-windows-gnu/release/cim.exe` |

Editing code and re-running `build.sh` recompiles offline; only changed crates
are rebuilt (the `target/docker/…` dirs persist between runs).

The image names can be overridden with `CIM_BUILD_IMAGE_LINUX` /
`CIM_BUILD_IMAGE_WINDOWS`.

## When to rebuild an image

An image snapshots the dependency set, so rebuild it (online) only when:

- **`Cargo.lock` changes** (added/updated/removed crates) — otherwise the
  offline `--locked --offline` build fails on a missing crate, or
- you **bump the Rust toolchain** (change `1.96.0` in the Dockerfiles).

Plain source edits do **not** need an image rebuild.

## Notes / caveats

- **Windows is a GNU (mingw) cross-build**, whereas CI builds Windows natively
  with **MSVC**. The produced `cim.exe` is a valid x86_64 Windows binary but uses
  the GNU ABI; it may reference the mingw runtime DLLs `libgcc_s_seh-1.dll` and
  `libwinpthread-1.dll` (found in the image under
  `/usr/lib/gcc/x86_64-w64-mingw32/*/` and `/usr/x86_64-w64-mingw32/lib/`) — copy
  them next to the `.exe` if the target lacks them. A true MSVC binary cannot be
  produced from a Linux container; use the CI job or a native Windows build for
  that. None of the mingw trouble above ever affected the MSVC build.
- **`.drectve '-exclude-symbols:…' unrecognized` warnings** during the Windows
  link are LLVM directives GNU `ld` doesn't parse. They are harmless, and there
  are thousands of them — filter with `grep -v '^Warning: .drectve'` when reading
  a build log, or they bury the real error.
- **Runs as root**, so files written into `target/docker/` are root-owned on a
  Linux host. Add `--user "$(id -u):$(id -g)"` to the `docker run` in `build.sh`
  if that matters for your setup (leave it off on Docker Desktop for Windows).
- **Windows host:** run `build.sh` from Git Bash and make sure the repo drive is
  shared with Docker Desktop. If volume mounting complains about the path, mount
  the Windows path form instead (e.g. `-v "C:\path\to\cim":/work`).
- The images only need to be rebuilt on the **online** machine; the air-gapped
  machine just needs Docker and the loaded images.

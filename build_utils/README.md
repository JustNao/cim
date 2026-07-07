# build_utils — offline Docker build environment

Builds the `cim` release binaries for **Linux** and **Windows** inside one Docker
image that works on a machine with **no internet access**. It mirrors the CI
(`.github/workflows/build.yml` + `ci/build-linux-glibc228.sh`): the Linux binary
is built against **glibc 2.28** in `debian:buster`; the Windows binary is
**cross-compiled** with `mingw-w64`.

```
build_utils/
  Dockerfile   the offline build image (toolchains + system libs + vendored crates)
  build.sh     builds the project for linux / windows / both using that image
  README.md    this file
```

## How the offline part works

The image is **built once on an internet-connected machine**. During that build
it pulls and bakes in everything a compile needs:

- the Debian build packages (GTK/X11/GL for eframe, `mingw-w64` for Windows),
- the pinned Rust toolchain (`1.96.0`) plus the `x86_64-pc-windows-gnu` target,
- **every crate from `Cargo.lock`** (`cargo fetch` for both targets, into
  `CARGO_HOME` inside the image).

You then transfer the image to the air-gapped machine and build there with
`--offline`. No crates, toolchains, or packages are downloaded at build time —
only your source tree is mounted in.

## 1. Build the image (online machine)

From the **repo root** (the build context must be the repo so `Cargo.toml` /
`Cargo.lock` are visible to `COPY`):

```sh
build_utils/build.sh image
# equivalently:
docker build -f build_utils/Dockerfile -t cim-build:latest .
```

## 2. Export and transfer

```sh
docker save cim-build:latest | gzip > cim-build.tar.gz
# copy cim-build.tar.gz to the air-gapped machine by whatever means allowed
```

## 3. Load and build (air-gapped machine)

```sh
docker load < cim-build.tar.gz

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

## When to rebuild the image

The image snapshots the dependency set, so rebuild it (online) only when:

- **`Cargo.lock` changes** (added/updated/removed crates) — otherwise the
  offline `--locked --offline` build fails on a missing crate, or
- you **bump the Rust toolchain** (change `1.96.0` in the `Dockerfile`).

Plain source edits do **not** need an image rebuild.

## Notes / caveats

- **Windows is a GNU (mingw) cross-build**, whereas CI builds Windows natively
  with **MSVC**. The produced `cim.exe` is a valid x86_64 Windows binary but uses
  the GNU ABI; it may reference the mingw runtime DLLs `libgcc_s_seh-1.dll` and
  `libwinpthread-1.dll` (found in the image under
  `/usr/lib/gcc/x86_64-w64-mingw32/*/` and `/usr/x86_64-w64-mingw32/lib/`) — copy
  them next to the `.exe` if the target lacks them. A true MSVC binary cannot be
  produced from a Linux container; use the CI job or a native Windows build for
  that.
- **Runs as root**, so files written into `target/docker/` are root-owned on a
  Linux host. Add `--user "$(id -u):$(id -g)"` to the `docker run` in `build.sh`
  if that matters for your setup (leave it off on Docker Desktop for Windows).
- **Windows host:** run `build.sh` from Git Bash and make sure the repo drive is
  shared with Docker Desktop. If volume mounting complains about the path, mount
  the Windows path form instead (e.g. `-v "C:\path\to\cim":/work`).
- The image only needs to be rebuilt on the **online** machine; the air-gapped
  machine just needs Docker and the loaded image.

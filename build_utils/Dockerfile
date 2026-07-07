#
# Offline build environment for cim. One image builds BOTH release targets:
#   * Linux   x86_64, glibc 2.28  (native — matches ci/build-linux-glibc228.sh)
#   * Windows x86_64               (cross-compiled via mingw-w64, GNU ABI)
#
# Everything (Debian packages, the pinned Rust toolchain + targets, and every
# crate from Cargo.lock) is pulled while BUILDING this image, so the resulting
# image builds the project with NO network access. The image is meant to be
# built once on an internet-connected machine, exported with `docker save`, and
# loaded on the air-gapped machine. See build_utils/README.md.
#
# Build (needs internet), FROM THE REPO ROOT so the COPY paths resolve:
#   docker build -f build_utils/Dockerfile -t cim-build:latest .
#

FROM debian:buster

# Debian buster is EOL: point apt at the archive and skip the Valid-Until check
# (same as ci/build-linux-glibc228.sh). glibc in buster is 2.28, so the Linux
# binary runs on RHEL 8 / Debian 10 / Ubuntu 18.04 and newer.
RUN printf 'deb http://archive.debian.org/debian buster main\ndeb http://archive.debian.org/debian-security buster/updates main\n' \
      > /etc/apt/sources.list \
 && printf 'Acquire::Check-Valid-Until "false";\n' > /etc/apt/apt.conf.d/99no-check-valid

# Native Linux build deps (eframe + rfd/GTK, X11/Wayland/GL — the same set CI
# installs), the Windows cross toolchain (mingw-w64), and helpers used by the
# build script (objdump from binutils, file).
RUN apt-get update -qq \
 && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
      curl ca-certificates git build-essential pkg-config file binutils \
      libgtk-3-dev libx11-dev libxcursor-dev libxrandr-dev libxi-dev \
      libgl1-mesa-dev libxkbcommon-dev libwayland-dev \
      mingw-w64 \
 && rm -rf /var/lib/apt/lists/*

# Pinned Rust (matches CI), isolated from any host install, with the Linux host
# target and the Windows-GNU cross target.
ENV RUSTUP_HOME=/opt/rustup \
    CARGO_HOME=/opt/cargo \
    PATH=/opt/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
      | sh -s -- -y --profile minimal --default-toolchain 1.96.0 \
        --target x86_64-pc-windows-gnu \
 && rustc --version

# Link the Windows target with the mingw-w64 gcc (its dlltool/etc. are found on
# PATH). rustc uses its own bundled archiver, so only the linker is set here.
RUN printf '[target.x86_64-pc-windows-gnu]\nlinker = "x86_64-w64-mingw32-gcc"\n' \
      > /opt/cargo/config.toml

# Pre-fetch every crate from the pinned lockfile into CARGO_HOME so builds run
# fully offline. Only the manifest + lock are needed to resolve the graph (a
# dummy target keeps cargo happy); the real sources are bind-mounted at run
# time. Plain `cargo fetch` (no --target) fetches the closure for **all** target
# platforms — Windows-only deps (winapi / windows-sys / …) and platform-gated
# transitive deps (e.g. zbus → async-recursion) alike; a `--target` filter would
# drop part of the graph and break the offline build for the other target.
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs \
 && cargo fetch --locked \
 && cd / && rm -rf /build

# The project source is bind-mounted here at run time (see build_utils/build.sh).
WORKDIR /work

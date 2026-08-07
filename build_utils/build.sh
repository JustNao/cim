#!/usr/bin/env bash
#
# Build cim for Linux (glibc 2.28) and/or Windows (x86_64) inside the offline
# Docker images produced from build_utils/Dockerfile.linux and
# build_utils/Dockerfile.windows.
#
# Usage (run from anywhere in the repo):
#   build_utils/build.sh [all|linux|windows]         build the target(s)  (default: all)
#   build_utils/build.sh image [all|linux|windows]   (re)build the image(s) (needs internet)
#
# There is **one image per target**: they need opposite toolchains (an old
# glibc for Linux, a recent mingw-w64/binutils for Windows), so they sit on
# different Debian bases — see the header of each Dockerfile.
#
# Each image bakes in its toolchain, system libraries and every crate from the
# pinned Cargo.lock, so the build steps below run with NO network access — they
# just mount the working tree and compile it. Rebuild an image (online) only
# when Cargo.lock or the toolchain changes. See build_utils/README.md for the
# full online-build -> docker save -> transfer -> docker load -> build flow.
#
# Outputs (under the repo's target/docker/ so they never clobber the host build):
#   Linux    target/docker/linux/release/cim
#   Windows  target/docker/windows/x86_64-pc-windows-gnu/release/cim.exe
#
set -euo pipefail

IMAGE_LINUX="${CIM_BUILD_IMAGE_LINUX:-cim-build-linux:latest}"
IMAGE_WINDOWS="${CIM_BUILD_IMAGE_WINDOWS:-cim-build-windows:latest}"

# Repo root = the parent of this script's directory, resolved regardless of the
# caller's working directory.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"

# Git Bash on Windows rewrites `/work`-style arguments into host paths; disable
# that so the in-container paths pass through verbatim. (No effect on Linux/mac.)
export MSYS_NO_PATHCONV=1

# require_image <image> <target>: fail with the command that would create it.
require_image() {
  if ! docker image inspect "$1" >/dev/null 2>&1; then
    echo "error: image '$1' not found." >&2
    echo "       Build it online with '$0 image $2', or 'docker load' the exported tar." >&2
    exit 1
  fi
}

# build_image <image> <dockerfile>
build_image() {
  echo ">> building image $1 from $2 (needs internet)"
  docker build -f "$here/$2" -t "$1" "$repo"
}

# run <image> <cargo-target-dir> <bash-snippet>: compile inside the image with
# the source tree mounted at /work. The snippet is expanded IN THE CONTAINER
# (single-quoted by the caller), so $CARGO_TARGET_DIR etc. resolve there.
run() {
  docker run --rm \
    -v "$repo":/work -w /work \
    -e CARGO_TARGET_DIR="$2" \
    "$1" bash -euo pipefail -c "$3"
}

build_linux() {
  require_image "$IMAGE_LINUX" linux
  echo ">> Linux (x86_64, glibc 2.28)"
  run "$IMAGE_LINUX" /work/target/docker/linux '
    cargo build --release --locked --offline
    bin="$CARGO_TARGET_DIR/release/cim"
    echo "=== built $bin ==="
    file "$bin"
    echo "highest glibc symbol required:"
    { objdump -T "$bin" | grep -oE "GLIBC_[0-9.]+" | sort -uV | tail -1; } || true
  '
  echo "   -> target/docker/linux/release/cim"
}

build_windows() {
  require_image "$IMAGE_WINDOWS" windows
  echo ">> Windows (x86_64, mingw-w64 / GNU ABI)"
  run "$IMAGE_WINDOWS" /work/target/docker/windows '
    cargo build --release --locked --offline --target x86_64-pc-windows-gnu
    bin="$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/cim.exe"
    echo "=== built $bin ==="
    file "$bin"
  '
  echo "   -> target/docker/windows/x86_64-pc-windows-gnu/release/cim.exe"
}

case "${1:-all}" in
  image)
    case "${2:-all}" in
      linux)   build_image "$IMAGE_LINUX" Dockerfile.linux ;;
      windows) build_image "$IMAGE_WINDOWS" Dockerfile.windows ;;
      all)     build_image "$IMAGE_LINUX" Dockerfile.linux
               build_image "$IMAGE_WINDOWS" Dockerfile.windows ;;
      *) echo "usage: $0 image [all|linux|windows]" >&2; exit 2 ;;
    esac ;;
  linux)   build_linux ;;
  windows) build_windows ;;
  all)     build_linux; build_windows ;;
  -h|--help|help) awk 'NR==1{next} /^#/{sub(/^# ?/,"");print;next} {exit}' "${BASH_SOURCE[0]}" ;;
  *) echo "usage: $0 [all|linux|windows|image]" >&2; exit 2 ;;
esac

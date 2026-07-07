#!/usr/bin/env bash
#
# Build cim for Linux (glibc 2.28) and/or Windows (x86_64) inside the offline
# Docker image produced from build_utils/Dockerfile.
#
# Usage (run from anywhere in the repo):
#   build_utils/build.sh [all|linux|windows]   build the target(s)   (default: all)
#   build_utils/build.sh image                 (re)build the image    (needs internet)
#
# The image bakes in the toolchains, system libraries and every crate from the
# pinned Cargo.lock, so the build steps below run with NO network access — they
# just mount the working tree and compile it. Rebuild the image (online) only
# when Cargo.lock or the toolchain changes. See build_utils/README.md for the
# full online-build -> docker save -> transfer -> docker load -> build flow.
#
# Outputs (under the repo's target/docker/ so they never clobber the host build):
#   Linux    target/docker/linux/release/cim
#   Windows  target/docker/windows/x86_64-pc-windows-gnu/release/cim.exe
#
set -euo pipefail

IMAGE="${CIM_BUILD_IMAGE:-cim-build:latest}"

# Repo root = the parent of this script's directory, resolved regardless of the
# caller's working directory.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"

# Git Bash on Windows rewrites `/work`-style arguments into host paths; disable
# that so the in-container paths pass through verbatim. (No effect on Linux/mac.)
export MSYS_NO_PATHCONV=1

require_image() {
  if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "error: image '$IMAGE' not found." >&2
    echo "       Build it online with '$0 image', or 'docker load' the exported tar." >&2
    exit 1
  fi
}

build_image() {
  echo ">> building image $IMAGE (needs internet)"
  docker build -f "$here/Dockerfile" -t "$IMAGE" "$repo"
}

# run <cargo-target-dir> <bash-snippet>: compile inside the image with the source
# tree mounted at /work. The snippet is expanded IN THE CONTAINER (single-quoted
# by the caller), so $CARGO_TARGET_DIR etc. resolve there.
run() {
  docker run --rm \
    -v "$repo":/work -w /work \
    -e CARGO_TARGET_DIR="$1" \
    "$IMAGE" bash -euo pipefail -c "$2"
}

build_linux() {
  require_image
  echo ">> Linux (x86_64, glibc 2.28)"
  run /work/target/docker/linux '
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
  require_image
  echo ">> Windows (x86_64, mingw-w64 / GNU ABI)"
  run /work/target/docker/windows '
    cargo build --release --locked --offline --target x86_64-pc-windows-gnu
    bin="$CARGO_TARGET_DIR/x86_64-pc-windows-gnu/release/cim.exe"
    echo "=== built $bin ==="
    file "$bin"
  '
  echo "   -> target/docker/windows/x86_64-pc-windows-gnu/release/cim.exe"
}

case "${1:-all}" in
  image)   build_image ;;
  linux)   build_linux ;;
  windows) build_windows ;;
  all)     build_linux; build_windows ;;
  -h|--help|help) awk 'NR==1{next} /^#/{sub(/^# ?/,"");print;next} {exit}' "${BASH_SOURCE[0]}" ;;
  *) echo "usage: $0 [all|linux|windows|image]" >&2; exit 2 ;;
esac

#!/usr/bin/env bash
# Build the Linux release binary against glibc 2.28 (RHEL 8 / Debian 10 era).
# Runs inside a `debian:buster` container so the resulting ELF only references
# symbols up to GLIBC_2.28. Invoked by CI and usable locally via:
#
#   docker run --rm -v "$PWD":/work -w /work \
#     -e CARGO_TARGET_DIR=/work/target/linux \
#     debian:buster bash ci/build-linux-glibc228.sh
set -euo pipefail

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-target}"

# Buster is EOL: point apt at the archive and skip the Valid-Until check.
cat > /etc/apt/sources.list <<'EOF'
deb http://archive.debian.org/debian buster main
deb http://archive.debian.org/debian-security buster/updates main
EOF
echo 'Acquire::Check-Valid-Until "false";' > /etc/apt/apt.conf.d/99no-check-valid

apt-get update -qq
DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  curl ca-certificates build-essential pkg-config \
  libgtk-3-dev libx11-dev libxcursor-dev libxrandr-dev \
  libxi-dev libgl1-mesa-dev libxkbcommon-dev libwayland-dev >/dev/null

# Pinned Rust toolchain, minimal profile, isolated from any host install.
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
  | sh -s -- -y --default-toolchain 1.96.0 --profile minimal
export PATH="$CARGO_HOME/bin:$PATH"

rustc --version
# Informational only. `… | head -1` closes the pipe early, so the writer gets
# SIGPIPE (exit 141) — under `set -o pipefail` + `set -e` that would abort the
# build, so swallow it. The `|| true` must be outside the pipeline.
{ ldd --version || true; } | head -1 || true

cargo build --release --locked

BIN="$CARGO_TARGET_DIR/release/cim"
echo "=== highest glibc symbol required by the binary ==="
# Also informational; never let it fail the build (SIGPIPE / no match / etc.).
{ objdump -T "$BIN" | grep -oE 'GLIBC_[0-9.]+' | sort -uV | tail -1; } || true
file "$BIN"

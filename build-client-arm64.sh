#!/bin/bash

set -euo pipefail

ARTIFACT_PATH="releases/aivpn-client-linux-arm64"

is_msys_shell() {
  [[ "${OSTYPE:-}" == msys* || "${OSTYPE:-}" == cygwin* || "${MSYSTEM:-}" == MINGW* || "${MSYSTEM:-}" == MSYS* ]]
}

docker_host_path() {
  local path="$1"
  if is_msys_shell && command -v cygpath >/dev/null 2>&1; then
    cygpath -am "$path"
  else
    printf '%s\n' "$path"
  fi
}

docker_cmd() {
  if is_msys_shell; then
    MSYS_NO_PATHCONV=1 MSYS2_ARG_CONV_EXCL='*' docker "$@"
  else
    docker "$@"
  fi
}

echo "=== Building Linux client arm64 release ==="
echo ""
echo "Building Docker builder image with cross-compilation..."

DOCKER_PWD="$(docker_host_path "$(pwd)")"

# Use an official Rust image so cargo/rustup are already present in the container.
docker_cmd run --rm -v "${DOCKER_PWD}:/aivpn" -w /aivpn \
  -e CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  -e CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
  -e OPENSSL_NO_VENDOR=1 \
  -e PKG_CONFIG_ALLOW_CROSS=1 \
  -e PKG_CONFIG_PATH=/usr/lib/aarch64-linux-gnu/pkgconfig \
  -e DEBIAN_FRONTEND=noninteractive \
  rust:1.86-bookworm bash -c "
    dpkg --add-architecture arm64 &&
    apt-get update &&
    apt-get install -y curl build-essential gcc-aarch64-linux-gnu pkg-config libssl-dev:arm64 crossbuild-essential-arm64 &&
    rustup target add aarch64-unknown-linux-gnu &&
    cargo build --release -p aivpn-client --target aarch64-unknown-linux-gnu
  "

mkdir -p releases
cp target/aarch64-unknown-linux-gnu/release/aivpn-client "$ARTIFACT_PATH"
chmod +x "$ARTIFACT_PATH"

echo ""
echo "=== Artifact Ready ==="
ls -lh "$ARTIFACT_PATH"
file "$ARTIFACT_PATH" || true

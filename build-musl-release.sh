#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

PACKAGE_KIND="${1:-}"
TARGET="${2:-}"

usage() {
    cat <<'EOF'
Usage: ./build-musl-release.sh <server|client> <target>

Supported targets:
  - armv7-unknown-linux-musleabihf
  - mipsel-unknown-linux-musl

Examples:
  ./build-musl-release.sh server armv7-unknown-linux-musleabihf
  ./build-musl-release.sh client mipsel-unknown-linux-musl
EOF
}

if [ -z "$PACKAGE_KIND" ] || [ -z "$TARGET" ]; then
    usage
    exit 1
fi

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        echo "Error: required command '$1' is not installed" >&2
        exit 1
    fi
}

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

case "$PACKAGE_KIND" in
    server)
        CRATE_NAME="aivpn-server"
        BINARY_NAME="aivpn-server"
        ;;
    client)
        CRATE_NAME="aivpn-client"
        BINARY_NAME="aivpn-client"
        ;;
    *)
        echo "Error: unsupported package kind '$PACKAGE_KIND'" >&2
        usage
        exit 1
        ;;
esac

case "$TARGET" in
    armv7-unknown-linux-musleabihf)
        IMAGE_TAG="armv7-musleabihf"
        ARTIFACT_PATH="releases/${BINARY_NAME}-linux-armv7-musleabihf"
        ;;
    mipsel-unknown-linux-musl)
        IMAGE_TAG="mipsel-musl"
        ARTIFACT_PATH="releases/${BINARY_NAME}-linux-mipsel-musl"
        ;;
    *)
        echo "Error: unsupported target '$TARGET'" >&2
        usage
        exit 1
        ;;
esac

require_command docker

DOCKER_SRC_DIR="$(docker_host_path "$SCRIPT_DIR")"

echo "=== Building ${BINARY_NAME} for ${TARGET} (musl static) ==="
echo ""
echo "Using Docker image: messense/rust-musl-cross:${IMAGE_TAG}"

mkdir -p releases

docker_cmd run --rm \
    -v "${DOCKER_SRC_DIR}:/app" \
    -w /app \
    "messense/rust-musl-cross:${IMAGE_TAG}" \
    cargo build --release --target "$TARGET" -p "$CRATE_NAME" --bin "$BINARY_NAME"

cp "target/${TARGET}/release/${BINARY_NAME}" "$ARTIFACT_PATH"
chmod +x "$ARTIFACT_PATH"

echo ""
echo "=== Artifact Ready ==="
ls -lh "$ARTIFACT_PATH"
file "$ARTIFACT_PATH" || true

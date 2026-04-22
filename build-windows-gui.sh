#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

TARGET="x86_64-pc-windows-gnu"
RELEASE_DIR="target/${TARGET}/release"
PACKAGE_DIR="aivpn-windows-gui-package"
ZIP_NAME="aivpn-windows-gui.zip"

is_msys_shell() {
    [[ "${OSTYPE:-}" == msys* || "${OSTYPE:-}" == cygwin* || "${MSYSTEM:-}" == MINGW* || "${MSYSTEM:-}" == MSYS* ]]
}

has_command() {
    command -v "$1" >/dev/null 2>&1
}

docker_host_path() {
    local path="$1"
    if is_msys_shell && has_command cygpath; then
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

build_with_local_toolchain() {
    echo "=== Building AIVPN Windows GUI (local toolchain) ==="

    if ! rustup target list --installed | grep -q "$TARGET"; then
        echo "Installing target ${TARGET}..."
        rustup target add "$TARGET"
    fi

    echo "Building aivpn-client.exe..."
    cargo build --release --target "$TARGET" -p aivpn-client --bin aivpn-client

    echo "Building aivpn.exe (GUI)..."
    cargo build --release --target "$TARGET" -p aivpn-windows --bin aivpn

    echo "Creating package..."
    rm -rf "$PACKAGE_DIR"
    mkdir -p "$PACKAGE_DIR"

    cp "${RELEASE_DIR}/aivpn.exe" "$PACKAGE_DIR/"
    cp "${RELEASE_DIR}/aivpn-client.exe" "$PACKAGE_DIR/"

    local wintun_dll="$PACKAGE_DIR/wintun.dll"
    if [[ ! -f "$wintun_dll" ]]; then
        echo "Downloading wintun.dll..."
        local wintun_zip="/tmp/wintun-0.14.1.zip"
        if [[ ! -f "$wintun_zip" ]]; then
            curl -L -o "$wintun_zip" "https://www.wintun.net/builds/wintun-0.14.1.zip"
        fi
        unzip -o "$wintun_zip" "wintun/bin/amd64/wintun.dll" -d /tmp/
        cp /tmp/wintun/bin/amd64/wintun.dll "$wintun_dll"
    fi

    rm -f "$ZIP_NAME"
    (
        cd "$PACKAGE_DIR"
        zip -r "../${ZIP_NAME}" ./*
    )
}

build_with_docker() {
    echo "=== Building AIVPN Windows GUI (Docker fallback) ==="

    has_command docker || {
        echo "Error: docker is required for Windows fallback build" >&2
        exit 1
    }

    local docker_src_dir
    docker_src_dir="$(docker_host_path "$SCRIPT_DIR")"

    docker_cmd run --rm \
        -e DEBIAN_FRONTEND=noninteractive \
        -e CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc \
        -e CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++ \
        -v "${docker_src_dir}:/app" \
        -w /app \
        rust:latest bash -c "
            set -euo pipefail
            apt-get update
            apt-get install -y pkg-config libssl-dev clang llvm mingw-w64 curl unzip zip
            rustup target add ${TARGET}
            cargo build --release --target ${TARGET} -p aivpn-client --bin aivpn-client
            cargo build --release --target ${TARGET} -p aivpn-windows --bin aivpn
            rm -rf ${PACKAGE_DIR}
            mkdir -p ${PACKAGE_DIR}
            cp ${RELEASE_DIR}/aivpn.exe ${PACKAGE_DIR}/
            cp ${RELEASE_DIR}/aivpn-client.exe ${PACKAGE_DIR}/
            curl -L -o /tmp/wintun-0.14.1.zip https://www.wintun.net/builds/wintun-0.14.1.zip
            unzip -o /tmp/wintun-0.14.1.zip 'wintun/bin/amd64/wintun.dll' -d /tmp/
            cp /tmp/wintun/bin/amd64/wintun.dll ${PACKAGE_DIR}/wintun.dll
            rm -f ${ZIP_NAME}
            cd ${PACKAGE_DIR}
            zip -r ../${ZIP_NAME} ./*
        "
}

echo "=== Building AIVPN Windows package with native GUI ==="

if has_command cargo && has_command rustup; then
    build_with_local_toolchain
else
    build_with_docker
fi

echo ""
echo "=== Build complete ==="
echo "Package: ${ZIP_NAME}"
echo "Contents:"
ls -lh "$PACKAGE_DIR/"
echo ""
echo "Total size:"
du -sh "$PACKAGE_DIR"

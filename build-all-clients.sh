#!/usr/bin/env bash

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$SCRIPT_DIR"
RELEASES_DIR="$REPO_ROOT/releases"
DEFAULT_BASE_URL="http://185.232.170.186:8080"

BASE_URL="${AIVPN_UPDATE_BASE_URL:-$DEFAULT_BASE_URL}"
VERSION_OVERRIDE=""
BUILD_WINDOWS="${AIVPN_BUILD_WINDOWS:-1}"
BUILD_MACOS="${AIVPN_BUILD_MACOS:-auto}"
BUILD_ANDROID="${AIVPN_BUILD_ANDROID:-auto}"
LOG_FILE=""

usage() {
    cat <<'EOF'
Usage: ./build-all-clients.sh [options]

Builds available AIVPN client artifacts, copies them into releases/,
and generates releases/version.json with sha256 sums.

Options:
  --version <semver>      Override version in generated version.json
  --base-url <url>        Base URL for asset links in version.json
  --skip-windows          Do not build Windows artifacts
  --skip-macos            Do not build macOS artifacts
  --skip-android          Do not build Android artifact
  --help                  Show this help

Environment:
  AIVPN_UPDATE_BASE_URL   Same as --base-url
  AIVPN_BUILD_WINDOWS     1/0
  AIVPN_BUILD_MACOS       auto/1/0
  AIVPN_BUILD_ANDROID     auto/1/0
EOF
}

log() {
    echo "==> $*"
}

warn() {
    echo "WARNING: $*" >&2
}

die() {
    echo "ERROR: $*" >&2
    exit 1
}

is_msys_shell() {
    [[ "${OSTYPE:-}" == msys* || "${OSTYPE:-}" == cygwin* || "${MSYSTEM:-}" == MINGW* || "${MSYSTEM:-}" == MSYS* ]]
}

has_command() {
    command -v "$1" >/dev/null 2>&1
}

require_command() {
    local cmd="$1"
    has_command "$cmd" || die "required command '$cmd' is not installed"
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

on_error() {
    local exit_code="$?"
    local line_no="$1"
    local command="$2"

    {
        echo
        echo "ERROR: build-all-clients.sh failed"
        echo "  line: $line_no"
        echo "  command: $command"
        if [[ -n "$LOG_FILE" ]]; then
            echo "  log: $LOG_FILE"
        fi
    } >&2

    if [[ -t 0 && -t 1 ]]; then
        read -r -p "Press Enter to close..." _ || true
    fi

    exit "$exit_code"
}

workspace_version() {
    awk -F'"' '
        /^\[workspace.package\]$/ { in_section = 1; next }
        /^\[/ && $0 !~ /^\[workspace.package\]$/ { in_section = 0 }
        in_section && /^version = / { print $2; exit }
    ' "$REPO_ROOT/Cargo.toml"
}

sha256_file() {
    local file="$1"

    if has_command sha256sum; then
        sha256sum "$file" | awk '{print $1}'
        return
    fi

    if has_command shasum; then
        shasum -a 256 "$file" | awk '{print $1}'
        return
    fi

    if has_command openssl; then
        openssl dgst -sha256 "$file" | awk '{print $NF}'
        return
    fi

    die "sha256 tool not found; install sha256sum, shasum, or openssl"
}

file_size_bytes() {
    local file="$1"
    wc -c < "$file" | tr -d '[:space:]'
}

json_escape() {
    local value="$1"
    value="${value//\\/\\\\}"
    value="${value//\"/\\\"}"
    value="${value//$'\n'/\\n}"
    value="${value//$'\r'/\\r}"
    value="${value//$'\t'/\\t}"
    printf '%s' "$value"
}

normalized_url() {
    local path="$1"
    printf '%s/%s' "${BASE_URL%/}" "${path#./}"
}

build_linux_x86_64() {
    require_command docker

    local image_tag="aivpn-client-builder:release"
    local container_name="aivpn-client-release-$RANDOM-$RANDOM"
    local artifact_path="$RELEASES_DIR/aivpn-client-linux-x86_64"
    local artifact_path_host
    artifact_path_host="$(docker_host_path "$artifact_path")"

    log "Building Linux x86_64 client"
    docker_cmd build --target builder -t "$image_tag" -f Dockerfile.client .
    docker_cmd create --name "$container_name" "$image_tag" >/dev/null
    docker_cmd cp "$container_name:/app/target/release/aivpn-client" "$artifact_path_host"
    docker_cmd rm -f "$container_name" >/dev/null 2>&1 || true
    chmod +x "$artifact_path"
}

build_linux_arm64() {
    require_command docker

    log "Building Linux arm64 client"
    bash "$REPO_ROOT/build-client-arm64.sh"
}

build_openwrt_musl() {
    require_command docker

    local artifact_path="$RELEASES_DIR/aivpn-client-openwrt-musl"
    local src_dir="$REPO_ROOT"
    local docker_src_dir
    docker_src_dir="$(docker_host_path "$src_dir")"

    log "Building OpenWrt x86_64 musl client"
    docker_cmd build -t aivpn-musl-builder -f Dockerfile.musl-builder .
    docker_cmd run --rm \
        -v "${docker_src_dir}:/home/rust/src" \
        --workdir /home/rust/src \
        aivpn-musl-builder \
        cargo build --release -p aivpn-client --bin aivpn-client --target x86_64-unknown-linux-musl

    cp "$REPO_ROOT/target/x86_64-unknown-linux-musl/release/aivpn-client" "$artifact_path"
    chmod +x "$artifact_path"
}

build_linux_armv7_musl() {
    require_command docker

    log "Building Linux armv7 musl client"
    bash "$REPO_ROOT/build-musl-release.sh" client armv7-unknown-linux-musleabihf
}

build_linux_mipsel_musl() {
    require_command docker

    log "Building Linux mipsel musl client"
    bash "$REPO_ROOT/build-musl-release.sh" client mipsel-unknown-linux-musl
}

build_windows_artifacts() {
    if [[ "$BUILD_WINDOWS" == "0" ]]; then
        warn "Skipping Windows build by request"
        return
    fi

    log "Building Windows client package"
    bash "$REPO_ROOT/build-windows-gui.sh"

    local exe_src="$REPO_ROOT/target/x86_64-pc-windows-gnu/release/aivpn-client.exe"
    local zip_src="$REPO_ROOT/aivpn-windows-gui.zip"
    local exe_dst="$REPO_ROOT/releases/aivpn-client.exe"
    local zip_dst="$REPO_ROOT/releases/aivpn-windows-package.zip"

    [[ -f "$exe_src" ]] || die "Windows client executable not found: $exe_src"
    cp "$exe_src" "$exe_dst"

    if [[ -f "$zip_src" ]]; then
        cp "$zip_src" "$zip_dst"
    else
        warn "Windows package zip not found after build: $zip_src"
    fi
}

build_macos_artifacts() {
    case "$BUILD_MACOS" in
        0)
            warn "Skipping macOS build by request"
            return
            ;;
        auto)
            if [[ "$(uname -s)" != "Darwin" ]]; then
                warn "Skipping macOS build: host is not macOS"
                return
            fi
            ;;
        1)
            ;;
        *)
            die "Unsupported AIVPN_BUILD_MACOS value: $BUILD_MACOS"
            ;;
    esac

    require_command cargo
    require_command rustup
    require_command lipo
    require_command swiftc
    require_command pkgbuild
    require_command hdiutil

    log "Building macOS client binaries"
    rustup target add x86_64-apple-darwin aarch64-apple-darwin >/dev/null
    cargo build --release -p aivpn-client --target x86_64-apple-darwin
    cargo build --release -p aivpn-client --target aarch64-apple-darwin

    local x86_bin="$REPO_ROOT/target/x86_64-apple-darwin/release/aivpn-client"
    local arm_bin="$REPO_ROOT/target/aarch64-apple-darwin/release/aivpn-client"
    local universal_bin="$RELEASES_DIR/aivpn-client-macos-universal"
    local legacy_bin="$RELEASES_DIR/aivpn-client-macos"

    [[ -f "$x86_bin" ]] || die "macOS x86_64 client binary not found: $x86_bin"
    [[ -f "$arm_bin" ]] || die "macOS arm64 client binary not found: $arm_bin"

    cp "$x86_bin" "$legacy_bin"
    chmod +x "$legacy_bin"

    lipo -create "$x86_bin" "$arm_bin" -output "$universal_bin"
    chmod +x "$universal_bin"

    log "Packaging macOS app"
    bash "$REPO_ROOT/aivpn-macos/build.sh"
}

build_android_artifact() {
    case "$BUILD_ANDROID" in
        0)
            warn "Skipping Android build by request"
            return
            ;;
        auto)
            if ! has_command cargo-ndk; then
                warn "Skipping Android build: cargo-ndk not found"
                return
            fi
            ;;
        1)
            ;;
        *)
            die "Unsupported AIVPN_BUILD_ANDROID value: $BUILD_ANDROID"
            ;;
    esac

    [[ -d "$REPO_ROOT/aivpn-android" ]] || die "Android project directory not found"
    log "Building Android APK"
    (
        cd "$REPO_ROOT/aivpn-android"
        bash ./build-rust-android.sh release
    )
}

asset_path_for_key() {
    case "$1" in
        linux-x86_64) printf '%s\n' "$RELEASES_DIR/aivpn-client-linux-x86_64" ;;
        openwrt-musl) printf '%s\n' "$RELEASES_DIR/aivpn-client-openwrt-musl" ;;
        linux-arm64) printf '%s\n' "$RELEASES_DIR/aivpn-client-linux-arm64" ;;
        linux-armv7) printf '%s\n' "$RELEASES_DIR/aivpn-client-linux-armv7-musleabihf" ;;
        linux-mipsel) printf '%s\n' "$RELEASES_DIR/aivpn-client-linux-mipsel-musl" ;;
        windows-x86_64) printf '%s\n' "$RELEASES_DIR/aivpn-client.exe" ;;
        windows-package) printf '%s\n' "$RELEASES_DIR/aivpn-windows-package.zip" ;;
        macos-universal) printf '%s\n' "$RELEASES_DIR/aivpn-client-macos-universal" ;;
        macos-pkg) printf '%s\n' "$RELEASES_DIR/aivpn-macos.pkg" ;;
        macos-dmg) printf '%s\n' "$RELEASES_DIR/aivpn-macos.dmg" ;;
        android-apk) printf '%s\n' "$RELEASES_DIR/aivpn-client.apk" ;;
        *) return 1 ;;
    esac
}

generate_version_json() {
    local version="$1"
    local output_path="$RELEASES_DIR/version.json"
    local generated_at
    generated_at="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

    local -a ordered_keys=(
        "linux-x86_64"
        "openwrt-musl"
        "linux-arm64"
        "linux-armv7"
        "linux-mipsel"
        "windows-x86_64"
        "windows-package"
        "macos-universal"
        "macos-pkg"
        "macos-dmg"
        "android-apk"
    )

    {
        echo "{"
        echo "  \"version\": \"$(json_escape "$version")\","
        echo "  \"generated_at\": \"$(json_escape "$generated_at")\","
        echo "  \"assets\": {"

        local first=1
        local key file file_name sha256 size url
        for key in "${ordered_keys[@]}"; do
            file="$(asset_path_for_key "$key")"
            [[ -f "$file" ]] || continue

            file_name="$(basename "$file")"
            sha256="$(sha256_file "$file")"
            size="$(file_size_bytes "$file")"
            url="$(normalized_url "$file_name")"

            if [[ $first -eq 0 ]]; then
                echo ","
            fi
            first=0

            printf '    "%s": {\n' "$(json_escape "$key")"
            printf '      "file": "%s",\n' "$(json_escape "$file_name")"
            printf '      "url": "%s",\n' "$(json_escape "$url")"
            printf '      "sha256": "%s",\n' "$(json_escape "$sha256")"
            printf '      "size": %s\n' "$size"
            printf '    }'
        done

        echo
        echo "  }"
        echo "}"
    } > "$output_path"

    log "Generated version manifest: $output_path"
    cat "$output_path"
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            shift
            [[ $# -gt 0 ]] || die "--version requires a value"
            VERSION_OVERRIDE="$1"
            ;;
        --base-url)
            shift
            [[ $# -gt 0 ]] || die "--base-url requires a value"
            BASE_URL="$1"
            ;;
        --skip-windows)
            BUILD_WINDOWS="0"
            ;;
        --skip-macos)
            BUILD_MACOS="0"
            ;;
        --skip-android)
            BUILD_ANDROID="0"
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            die "Unknown argument: $1"
            ;;
    esac
    shift
done

mkdir -p "$RELEASES_DIR"
LOG_FILE="$RELEASES_DIR/build-all-clients.log"
: > "$LOG_FILE"
exec > >(tee -a "$LOG_FILE") 2>&1
trap 'on_error "$LINENO" "$BASH_COMMAND"' ERR

VERSION="${VERSION_OVERRIDE:-$(workspace_version)}"
[[ -n "$VERSION" ]] || die "Could not determine workspace version"

cd "$REPO_ROOT"

build_linux_x86_64
build_linux_arm64
build_openwrt_musl
build_linux_armv7_musl
build_linux_mipsel_musl
build_windows_artifacts
build_macos_artifacts
build_android_artifact

echo
generate_version_json "$VERSION"

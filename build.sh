#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$project_root"

target="${CARGO_BUILD_TARGET:-}"
macos_universal=false

usage() {
    cat <<'EOF'
Usage: ./build.sh [--target <target-triple> | --macos-universal]

Builds the stripped release binary using Cargo.lock.

Examples:
  ./build.sh
  ./build.sh --macos-universal
  ./build.sh --target x86_64-unknown-linux-musl
  ./build.sh --target x86_64-pc-windows-msvc
EOF
}

while (($# > 0)); do
    case "$1" in
        --target)
            if (($# < 2)); then
                echo "error: --target requires a target triple" >&2
                exit 2
            fi
            target="$2"
            shift 2
            ;;
        --macos-universal)
            macos_universal=true
            shift
            ;;
        -h | --help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

describe_binary() {
    local binary="$1"
    echo "release binary: $binary"
    du -h "$binary"
    if command -v file >/dev/null 2>&1; then
        file "$binary"
    fi
}

if [[ "$macos_universal" == true ]]; then
    if [[ -n "$target" ]]; then
        echo "error: --target and --macos-universal cannot be used together" >&2
        exit 2
    fi
    if [[ "$(uname -s)" != "Darwin" ]]; then
        echo "error: universal macOS binaries must be built on macOS" >&2
        exit 1
    fi
    if ! command -v lipo >/dev/null 2>&1; then
        echo "error: lipo is required; install the Xcode command-line tools" >&2
        exit 1
    fi

    macos_targets=(aarch64-apple-darwin x86_64-apple-darwin)
    for macos_target in "${macos_targets[@]}"; do
        if ! rustup target list --installed | grep -qx "$macos_target"; then
            echo "error: missing Rust target $macos_target" >&2
            echo "install it with: rustup target add $macos_target" >&2
            exit 1
        fi
        cargo build --locked --release --target "$macos_target"
    done

    universal_dir="target/universal-apple-darwin/release"
    mkdir -p "$universal_dir"
    universal_binary="$universal_dir/updater"
    lipo -create \
        "target/aarch64-apple-darwin/release/updater" \
        "target/x86_64-apple-darwin/release/updater" \
        -output "$universal_binary"
    describe_binary "$universal_binary"
    exit 0
fi

cargo_command=(cargo build)
cargo_args=(--locked --release)
artifact_dir="target/release"
if [[ -n "$target" ]]; then
    cargo_args+=(--target "$target")
    artifact_dir="target/$target/release"
fi
if [[ "$target" == *linux* && "$(uname -s)" != "Linux" ]]; then
    if command -v cargo-zigbuild >/dev/null 2>&1; then
        cargo_command=(cargo zigbuild)
    else
        echo "error: cross-compiling Linux from this host requires cargo-zigbuild and Zig" >&2
        echo "install it with: cargo install cargo-zigbuild --locked" >&2
        exit 1
    fi
fi

"${cargo_command[@]}" "${cargo_args[@]}"

binary_name="updater"
if [[ "$target" == *windows* || "${OS:-}" == "Windows_NT" ]]; then
    binary_name="updater.exe"
fi
binary="$artifact_dir/$binary_name"

if [[ ! -f "$binary" ]]; then
    echo "error: Cargo succeeded but $binary was not created" >&2
    exit 1
fi

describe_binary "$binary"

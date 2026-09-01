#!/usr/bin/env bash
set -euo pipefail

project_root="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
cd "$project_root"

target="${CARGO_BUILD_TARGET:-}"

usage() {
    cat <<'EOF'
Usage: ./build.sh [--target <target-triple>]

Builds the stripped release binary using Cargo.lock.

Examples:
  ./build.sh
  ./build.sh --target x86_64-unknown-linux-musl
  ./build.sh --target aarch64-unknown-linux-musl
  ./build.sh --target x86_64-pc-windows-msvc
  ./build.sh --target aarch64-pc-windows-msvc
  ./build.sh --target x86_64-apple-darwin
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

cargo_command=(cargo build)
cargo_args=(--locked --release)
artifact_dir="target/release"
if [[ -n "$target" ]]; then
    cargo_args+=(--target "$target")
    artifact_dir="target/$target/release"
fi
host_os="$(uname -s)"

if [[ "$target" == "aarch64-unknown-linux-musl" || ("$target" == *linux* && "$host_os" != "Linux") ]]; then
    if command -v cargo-zigbuild >/dev/null 2>&1; then
        cargo_command=(cargo zigbuild)
    else
        echo "error: this cross-compilation target requires cargo-zigbuild and Zig" >&2
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

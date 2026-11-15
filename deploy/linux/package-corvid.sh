#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd -- "$repo"
case "$(uname -s):$(uname -m)" in
    Linux:x86_64) target=x86_64-unknown-linux-musl ;;
    Linux:aarch64|Linux:arm64) target=aarch64-unknown-linux-musl ;;
    *) echo 'Build the Linux runtime on a Linux x64 or ARM64 host.' >&2; exit 1 ;;
esac
export CC=musl-gcc
target_key=${target^^}
target_key=${target_key//-/_}
export "CARGO_TARGET_${target_key}_LINKER"=musl-gcc
build_dir=${CARGO_TARGET_DIR:-target}
rustup target add "$target"
cargo build --locked --release --target "$target" --target-dir "$build_dir" -p corvid-tui
python3 deploy/package.py runtime --target "$target" --binary "$build_dir/$target/release/corvid" --output "$build_dir/linux-dist/$target"

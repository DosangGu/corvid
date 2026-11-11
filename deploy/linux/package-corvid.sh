#!/usr/bin/env bash
set -euo pipefail
repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd -- "$repo"
target=x86_64-unknown-linux-musl
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
    echo 'Build the Linux x64 runtime on a Linux x86_64 host.' >&2
    exit 1
fi
build_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked --release --target "$target" --target-dir "$build_dir" -p corvid-tui
python3 deploy/package.py runtime --target "$target" --binary "$build_dir/$target/release/corvid" --output "$build_dir/linux-dist/$target"

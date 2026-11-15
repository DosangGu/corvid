#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/../.."
if [[ $(uname -s) != Darwin || $(uname -m) != arm64 || $# != 4 ]]; then
    echo 'Usage (on Apple Silicon): bash deploy/macos/package-corvid.sh LINUX_X64_RUNTIME WINDOWS_X64_RUNTIME LINUX_ARM64_RUNTIME WINDOWS_ARM64_RUNTIME' >&2
    exit 1
fi
export MACOSX_DEPLOYMENT_TARGET=15.0
# Rust 1.93 stripping can misalign proc-macro dylibs on macOS 27.
export CARGO_PROFILE_RELEASE_BUILD_OVERRIDE_STRIP=none
target=aarch64-apple-darwin
build=${CARGO_TARGET_DIR:-target}
release="$build/$target/release"
runtime="$build/runtime/$target"
output="$build/macos-dist"
cargo build --locked --release --target "$target" -p corvid-tui -p corvid-desktop -p corvid-agent-deck -p corvid-deck --features corvid-deck/stream-deck
python3 deploy/package.py runtime --target "$target" --binary "$release/corvid" --output "$runtime"
python3 deploy/package.py package --product tui --target "$target" --runtime "$runtime" --build "$release" --output "$output"
python3 deploy/package.py package --product desktop --target "$target" --runtime "$runtime" --linux-runtime "$1" --windows-runtime "$2" --linux-arm64-runtime "$3" --windows-arm64-runtime "$4" --macos-runtime "$runtime" --build "$release" --output "$output"
python3 deploy/macos_package.py --archive "$output/corvid-desktop-"*"-$target.tar.gz" --output "$output"

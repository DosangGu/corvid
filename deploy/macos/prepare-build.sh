#!/bin/bash
# Run before Cargo in GitHub Actions; Xcode 26 separates the Metal toolchain.
set -euo pipefail
mkdir -p "$HOME/.corvid-tests"
chmod 700 "$HOME/.corvid-tests"
echo "TMPDIR=$HOME/.corvid-tests" >> "$GITHUB_ENV"
if ! xcrun -sdk macosx metal --version; then
    xcodebuild -downloadComponent MetalToolchain
    if ! xcrun -sdk macosx metal --version; then
        for toolchain in "$HOME/Library/Developer/DVTDownloads/MetalToolchain/mounts"/*/Metal.xctoolchain /Library/Developer/Toolchains/Metal.xctoolchain; do
            if [ -f "$toolchain/ToolchainInfo.plist" ]; then
                export TOOLCHAINS
                TOOLCHAINS=$(/usr/libexec/PlistBuddy -c 'Print :Identifier' "$toolchain/ToolchainInfo.plist")
                xcrun -sdk macosx metal --version
                echo "TOOLCHAINS=$TOOLCHAINS" >> "$GITHUB_ENV"
                break
            fi
        done
    fi
fi
xcrun -sdk macosx metal --version

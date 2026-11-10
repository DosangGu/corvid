#!/usr/bin/env bash
set -euo pipefail

repo=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
cd -- "$repo"

target=x86_64-unknown-linux-musl
if [[ $(uname -s) != Linux || $(uname -m) != x86_64 ]]; then
    echo 'Build the Linux x64 daemon on a Linux x86_64 host.' >&2
    exit 1
fi

revision=$(git rev-parse HEAD)
changes=$(git -c core.safecrlf=false status --porcelain --untracked-files=normal -- \
    Cargo.lock Cargo.toml rust-toolchain.toml crates plugins spikes)
if [[ -n $changes ]]; then
    echo 'Commit the build inputs before packaging the Linux daemon.' >&2
    exit 1
fi

build_dir=${CARGO_TARGET_DIR:-target}
cargo build --locked --release --target "$target" --target-dir "$build_dir" -p corvid-tui
binary="$build_dir/$target/release/corvid"

headers=$(readelf --wide --program-headers "$binary")
dynamic=$(readelf --wide --dynamic "$binary")
if grep -Eq '^[[:space:]]+INTERP[[:space:]]' <<< "$headers" || grep -Fq '(NEEDED)' <<< "$dynamic"; then
    echo 'The Linux daemon bundle requires a statically linked executable.' >&2
    exit 1
fi

version_output=$("$binary" --version)
if [[ ! $version_output =~ ^corvid\ ([0-9A-Za-z.+-]+)$ ]]; then
    echo 'Could not read the Linux daemon version.' >&2
    exit 1
fi
version=${BASH_REMATCH[1]}

dist="$build_dir/linux-dist/$target"
mkdir -p -- "$dist"
stage=$(mktemp -d "$dist/.corvid.XXXXXXXX")
trap 'rm -f -- "$stage/corvid" "$stage/manifest.json"; rmdir -- "$stage"' EXIT
install -m 0755 -- "$binary" "$stage/corvid"
size=$(stat -c %s -- "$stage/corvid")
digest=$(sha256sum < "$stage/corvid")
sha256=${digest%% *}

cat > "$stage/manifest.json" <<EOF
{
  "schema_version": 1,
  "target": "$target",
  "source_revision": "$revision",
  "version": "$version",
  "size": $size,
  "sha256": "$sha256"
}
EOF

mv -fT -- "$stage/corvid" "$dist/corvid"
mv -fT -- "$stage/manifest.json" "$dist/manifest.json"
printf 'Linux daemon bundle: %s\n' "$dist"

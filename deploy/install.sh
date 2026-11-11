#!/bin/sh
# Corvid's user-scoped installer. Re-running this script updates the installation.
set -eu
umask 077

repository=DosangGu/corvid
release_version=
product=tui
install_root=
bin_dir=
source_dir=
expected_hash=

fail() { printf 'corvid: %s\n' "$*" >&2; exit 1; }
usage() {
    echo 'Usage: install.sh [--version VERSION] [--product tui|desktop] [--install-root DIR] [--bin-dir DIR] [--source-dir DIR]'
}
while [ "$#" -gt 0 ]; do
    case "$1" in
        --help|-h) usage; exit 0 ;;
        --version|--product|--install-root|--bin-dir|--source-dir|--archive-sha256)
            [ "$#" -ge 2 ] || fail "Missing value for $1"
            option=$1; value=$2; shift 2
            case "$option" in
                --version) release_version=${value#v} ;;
                --product) product=$value ;;
                --install-root) install_root=$value ;;
                --bin-dir) bin_dir=$value ;;
                --source-dir) source_dir=$value ;;
                --archive-sha256) expected_hash=$value ;;
            esac ;;
        *) fail "Unknown option: $1" ;;
    esac
done
[ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || fail 'Supported platform: Linux x86_64 (including WSL).'
case "$product" in tui) target=x86_64-unknown-linux-musl ;; desktop) target=x86_64-unknown-linux-gnu ;; *) fail 'Unknown product' ;; esac
for tool in tar sha256sum mktemp awk sed grep; do command -v "$tool" >/dev/null || fail "Install $tool first"; done
if [ -z "$source_dir" ]; then command -v curl >/dev/null || fail 'Install curl first'; fi
fetch() {
    if [ -n "$source_dir" ]; then
        cp -- "$source_dir/$1" "$2"
    else
        curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
            --connect-timeout 15 --max-time 300 --retry 2 --max-filesize 1073741824 \
            "$base/$1" --output "$2"
    fi
}
if [ -z "$release_version" ]; then
    [ -z "$source_dir" ] || fail '--source-dir requires --version'
    metadata=$(curl --fail --silent --show-error --location --proto '=https' --proto-redir '=https' \
        --connect-timeout 15 --max-time 30 --max-filesize 1048576 "https://api.github.com/repos/$repository/releases/latest")
    release_version=$(printf '%s' "$metadata" | sed -n 's/.*"tag_name":[[:space:]]*"v\([^"]*\)".*/\1/p')
fi
printf '%s\n' "$release_version" | grep -Eq '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$' || fail 'Invalid release version'
base="https://github.com/$repository/releases/download/v$release_version"
install_root=${install_root:-${XDG_DATA_HOME:-$HOME/.local/share}/corvid/$product}
bin_dir=${bin_dir:-$HOME/.local/bin}
for path in "$install_root" "$bin_dir"; do
    case "$path" in /*) ;; *) fail 'Installation paths must be absolute' ;; esac
    case "$path" in *'
'*) fail 'Installation paths cannot contain newlines' ;; esac
done
mkdir -p -- "$install_root" "$bin_dir"
install_root=$(cd -- "$install_root" && pwd -P)
bin_dir=$(cd -- "$bin_dir" && pwd -P)
mkdir -- "$install_root/.install-lock" 2>/dev/null || fail "Another installation is running. If it was interrupted, remove $install_root/.install-lock and retry."
stage=
cleanup() {
    if [ -n "$stage" ]; then rm -rf -- "$stage"; fi
    rmdir -- "$install_root/.install-lock"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
receipt="{\"schema_version\":1,\"product\":\"$product\"}"
if [ -f "$install_root/install.json" ]; then
    [ "$(cat "$install_root/install.json")" = "$receipt" ] || fail 'This directory belongs to another installation'
    [ "$(cat "$install_root/bin-dir")" = "$bin_dir" ] || fail 'Use the original --bin-dir for this installation'
else
    [ ! -e "$install_root/current" ] && [ ! -e "$install_root/versions" ] || fail 'Unrecognized installation directory'
fi
commands=corvid
if [ "$product" = desktop ]; then commands='corvid corvid-desktop'; fi
for name in $commands; do
    destination="$bin_dir/$name"
    if [ -e "$destination" ] || [ -L "$destination" ]; then
        [ -L "$destination" ] && [ "$(readlink "$destination")" = "$install_root/current/$name" ] || fail "$destination is managed by another installation. Choose another --bin-dir."
    fi
done
stage=$(mktemp -d "$install_root/.stage.XXXXXXXX")
archive="corvid-$product-$release_version-$target.tar.gz"
fetch "$archive.sha256" "$stage/checksum"
digest=$(awk 'NR == 1 {print $1}' "$stage/checksum")
printf '%s\n' "$digest" | grep -Eq '^[0-9a-f]{64}$' || fail 'Invalid archive checksum'
[ -z "$expected_hash" ] || [ "$digest" = "$expected_hash" ] || fail 'Release manifest and archive checksum disagree'
fetch "$archive" "$stage/archive.tar.gz"
printf '%s  %s\n' "$digest" archive.tar.gz > "$stage/check"
(cd -- "$stage" && sha256sum --status -c check) || fail 'Archive SHA-256 mismatch'
tar -tzf "$stage/archive.tar.gz" > "$stage/entries"
if grep -Eq '(^/|(^|/)\.\.(/|$))' "$stage/entries"; then fail 'Unsafe archive path'; fi
tar -tvzf "$stage/archive.tar.gz" > "$stage/types"
awk 'substr($0,1,1) != "-" && substr($0,1,1) != "d" {exit 1}' "$stage/types" || fail 'Archive links are not supported'
mkdir -- "$stage/package"
tar -xzf "$stage/archive.tar.gz" --no-same-owner --no-same-permissions -C "$stage/package"
(cd -- "$stage/package" && sha256sum --status -c files.sha256) || fail 'Package file checksum mismatch'
field() { sed -n "s/.*\"$1\":[[:space:]]*\"\([^\"]*\)\".*/\1/p" "$stage/package/package.json"; }
[ "$(field version)" = "$release_version" ] && [ "$(field product)" = "$product" ] && [ "$(field target)" = "$target" ] || fail 'Package metadata mismatch'
chmod 755 "$stage/package/corvid"
[ "$("$stage/package/corvid" --version)" = "corvid $release_version" ] || fail 'Executable version mismatch'
if [ "$product" = desktop ]; then
    chmod 755 "$stage/package/corvid-desktop"
    if [ -f "$stage/package/plugins/slot-board/corvid-slot-board" ]; then
        chmod 755 "$stage/package/plugins/slot-board/corvid-slot-board"
    fi
    chmod 755 "$stage/package/daemon/x86_64-unknown-linux-musl/corvid"
fi
mkdir -p -- "$install_root/versions"
destination="$install_root/versions/$release_version"
if [ -e "$destination" ]; then
    cmp "$destination/files.sha256" "$stage/package/files.sha256" >/dev/null || fail 'An installed version has different contents; refusing to replace it'
    (cd -- "$destination" && sha256sum --status -c files.sha256) || fail 'The installed version is damaged; move it aside before reinstalling'
else
    mv -- "$stage/package" "$destination"
fi
printf '%s\n' "$receipt" > "$stage/install.json"
printf '%s\n' "$bin_dir" > "$stage/bin-dir"
mv -f -- "$stage/install.json" "$install_root/install.json"
mv -f -- "$stage/bin-dir" "$install_root/bin-dir"
ln -s "versions/$release_version" "$stage/current"
mv -Tf -- "$stage/current" "$install_root/current"
for name in $commands; do
    if [ ! -L "$bin_dir/$name" ]; then ln -s "$install_root/current/$name" "$bin_dir/$name"; fi
done
printf 'Installed Corvid %s (%s).\n' "$release_version" "$product"
printf 'CORVID_EXECUTABLE=%s/corvid\n' "$destination"
case ":$PATH:" in *":$bin_dir:"*) ;; *) printf 'Add %s to PATH to run Corvid.\n' "$bin_dir" ;; esac
printf 'Running daemons were left unchanged. Use corvid upgrade for an automatically started daemon; use the service manager for a supervised daemon.\n'

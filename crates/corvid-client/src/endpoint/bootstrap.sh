export LC_ALL=C

root=${HOME:-}/.corvid

safe_home() {
    case ${HOME:-}/ in
        /|*//*|*/./*|*/../*) return 1 ;;
        /*) ;;
        *) return 1 ;;
    esac
    user=$(id -u) || return 1
    [ "$(stat -c %u -- "$HOME")" = "$user" ] || return 1
    parent=$HOME
    while [ -n "$parent" ]; do
        [ -d "$parent" ] && [ ! -L "$parent" ] || return 1
        owner=$(stat -c %u -- "$parent") || return 1
        mode=$(stat -c %a -- "$parent") || return 1
        { [ "$owner" = 0 ] || [ "$owner" = "$user" ]; } || return 1
        [ "$((0$mode & 022))" -eq 0 ] || return 1
        parent=${parent%/*}
    done
}

private_root() {
    [ -d "$root" ] && [ ! -L "$root" ] &&
        [ "$(stat -c %u:%a -- "$root")" = "$user:700" ]
}

managed_binary() {
    [ -f "$root/corvid" ] && [ ! -L "$root/corvid" ] &&
        [ "$(stat -c %u:%a -- "$root/corvid")" = "$user:700" ]
}

prepare_runtime() {
    if [ -n "${CORVID_RUNTIME_DIR:-}" ]; then return; fi
    unset CORVID_RUNTIME_DIR
    if [ -n "${XDG_RUNTIME_DIR:-}" ]; then return; fi
    safe_home || exit 71
    IFS= read -r boot < /proc/sys/kernel/random/boot_id || exit 71
    case "$boot" in ''|*[!a-f0-9-]*) exit 71 ;; esac
    [ "${#boot}" -eq 36 ] || exit 71
    # Keep the boot-scoped directory short enough for Unix socket paths.
    export CORVID_RUNTIME_DIR="$root/r-$(printf '%.23s' "$boot")"
}

verify_payload() {
    [ "$(wc -c < "$1")" -eq "$size" ] || return 1
    digest=$(sha256sum < "$1") || return 1
    [ "${digest%% *}" = "$sha" ]
}

case "$1" in
    probe)
        [ "$(uname -s)" = Linux ] || exit 70
        if command -v corvid >/dev/null 2>&1; then
            printf 'path\n'
        elif [ -e "$root/corvid" ] || [ -L "$root/corvid" ]; then
            safe_home && private_root && managed_binary || exit 71
            printf 'managed\n'
        elif [ "$(uname -m)" = x86_64 ]; then
            printf 'missing\n'
        else
            exit 70
        fi
        ;;
    install|upgrade)
        [ "$(uname -s)" = Linux ] && [ "$(uname -m)" = x86_64 ] || exit 70
        if command -v corvid >/dev/null 2>&1; then exit 74; fi
        safe_home || exit 71
        umask 077
        mkdir -m 700 -- "$root" 2>/dev/null || [ -d "$root" ] || exit 71
        private_root || exit 71
        read -r size sha || exit 72
        case "$size" in ''|*[!0-9]*) exit 72 ;; esac
        case "$sha" in ''|*[!a-f0-9]*) exit 72 ;; esac
        [ "${#size}" -le 8 ] && [ "$size" -gt 0 ] && [ "$size" -le 33554432 ] &&
            [ "${#sha}" -eq 64 ] || exit 72
        stage=$(mktemp "$root/.corvid.XXXXXXXX") || exit 73
        trap 'rm -f -- "$stage"' 0
        trap 'exit 73' HUP INT TERM
        head -c "$size" > "$stage" && verify_payload "$stage" || exit 72
        chmod 700 -- "$stage" || exit 73
        if [ "$1" = upgrade ]; then
            managed_binary || exit 74
            prepare_runtime
            version=$2
            host=${3:-}
            set -- upgrade --managed-ssh-from "$version"
            if [ -n "$host" ]; then set -- "$@" --expected-host "$host"; fi
            "$stage" "$@"
        else
            # Publish the verified inode without replacing another installation.
            if ! ln -T -- "$stage" "$root/corvid" 2>/dev/null; then
                managed_binary && verify_payload "$root/corvid" || exit 74
            fi
        fi
        ;;
    path)
        shift
        if ! command -v corvid >/dev/null 2>&1; then
            printf 'corvid: not found\n' >&2
            exit 127
        fi
        prepare_runtime
        exec corvid "$@"
        ;;
    managed)
        shift
        safe_home && private_root && managed_binary || exit 71
        prepare_runtime
        exec "$root/corvid" "$@"
        ;;
    *) exit 73 ;;
esac

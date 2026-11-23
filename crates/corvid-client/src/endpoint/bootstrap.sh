# Use predictable output for bootstrap validation without changing the locale
# inherited by the daemon and its interactive shells.
runtime_lc_all_set=${LC_ALL+x}
runtime_lc_all=${LC_ALL-}
export LC_ALL=C

restore_runtime_locale() {
    if [ "$runtime_lc_all_set" = x ]; then
        export LC_ALL="$runtime_lc_all"
    else
        unset LC_ALL
    fi
}

root=${HOME:-}/.corvid
wsl_bootstrap=${CORVID_WSL_BOOTSTRAP:-}
unset CORVID_WSL_BOOTSTRAP

safe_parents() (
    parent=$1
    uid=$2
    case $parent in /*) ;; *) return 1 ;; esac
    while [ -n "$parent" ]; do
        case ${parent##*/} in
            ''|.) ;;
            ..) return 1 ;;
            *)
                [ -d "$parent" ] && [ ! -L "$parent" ] || return 1
                metadata=$(stat -c %u:%a -- "$parent") || return 1
                owner=${metadata%:*}
                mode=${metadata#*:}
                { [ "$owner" = 0 ] || [ "$owner" = "$uid" ]; } || return 1
                [ "$((0$mode & 022))" -eq 0 ] || return 1
                ;;
        esac
        parent=${parent%/*}
    done
)

safe_home() {
    case ${HOME:-}/ in
        /|*//*|*/./*|*/../*) return 1 ;;
        /*) ;;
        *) return 1 ;;
    esac
    user=$(id -u) || return 1
    [ "$(stat -c %u -- "$HOME")" = "$user" ] || return 1
    safe_parents "$HOME" "$user"
}

safe_xdg_runtime() (
    uid=$(id -u) || return 1
    safe_parents "$XDG_RUNTIME_DIR" "$uid" || return 1
    runtime=$XDG_RUNTIME_DIR/corvid
    [ ! -L "$runtime" ] || return 1
    [ ! -e "$runtime" ] || {
        [ -d "$runtime" ] && [ "$(stat -c %u:%a -- "$runtime")" = "$uid:700" ]
    }
)

private_root() {
    [ -d "$root" ] && [ ! -L "$root" ] &&
        [ "$(stat -c %u:%a -- "$root")" = "$user:700" ]
}

path_daemon() {
    if command -v corvidd >/dev/null 2>&1; then
        printf 'corvidd\n'
    elif command -v corvid >/dev/null 2>&1; then
        printf 'corvid\n'
    else
        return 1
    fi
}

managed_binary() {
    [ -f "$root/corvid" ] && [ ! -L "$root/corvid" ] &&
        [ "$(stat -c %u:%a -- "$root/corvid")" = "$user:700" ]
}

prepare_runtime() {
    if [ -n "${CORVID_RUNTIME_DIR:-}" ]; then return; fi
    unset CORVID_RUNTIME_DIR
    # WSLg can set XDG_RUNTIME_DIR below a writable shared mount.
    if [ -n "${XDG_RUNTIME_DIR:-}" ] &&
        { [ "$wsl_bootstrap" != 1 ] || safe_xdg_runtime; }; then
        return
    fi
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

# Persisted SSH and WSL endpoints can resolve either installation after reconnecting.
if [ "$1" = auto ]; then
    shift
    if path_daemon >/dev/null; then
        set -- path "$@"
    elif [ -e "$root/corvid" ] || [ -L "$root/corvid" ]; then
        set -- managed "$@"
    else
        printf 'corvid: not found\n' >&2
        exit 127
    fi
fi

case "$1" in
    probe)
        [ "$(uname -s)" = Linux ] || exit 70
        if path_daemon >/dev/null; then
            printf 'path\n'
        elif [ -e "$root/corvid" ] || [ -L "$root/corvid" ]; then
            safe_home && private_root && managed_binary || exit 71
            printf 'managed\n'
        else
            case "$(uname -m)" in x86_64|aarch64|arm64) printf 'missing\n' ;; *) exit 70 ;; esac
        fi
        ;;
    install|upgrade)
        [ "$(uname -s)" = Linux ] || exit 70
        case "$(uname -m)" in x86_64|aarch64|arm64) ;; *) exit 70 ;; esac
        if path_daemon >/dev/null; then exit 74; fi
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
            restore_runtime_locale
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
        if ! binary=$(path_daemon); then
            printf 'corvid: not found\n' >&2
            exit 127
        fi
        prepare_runtime
        restore_runtime_locale
        exec "$binary" "$@"
        ;;
    managed)
        shift
        safe_home && private_root && managed_binary || exit 71
        prepare_runtime
        restore_runtime_locale
        exec "$root/corvid" "$@"
        ;;
    *) exit 73 ;;
esac

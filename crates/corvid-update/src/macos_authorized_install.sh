# Executed by macOS after the administrator authorizes this update.
set -eu
umask 022
app=$1
incoming=$2
expected=$3
cdhash=$4
fail() { printf '%s\n' "$*" >&2; exit 1; }

# Reject macOS extended ACL entries granting write/modification rights to
# non-administrator accounts on the bundle or its ancestor directories.
check_macos_acl() {
    acl_output=$(/bin/ls -led "$1") || fail 'Cannot inspect the installed app ACL.'
    printf '%s\n' "$acl_output" | while IFS= read -r line; do
        case "$line" in
            *[0-9]:\ *)
                rest=${line#*: }
                principal=${rest%% *}
                attrs=${rest#"$principal"}
                case " $attrs " in
                    *' deny '*)
                        continue
                        ;;
                    *' allow '*)
                        perms=${attrs#* allow }
                        case ",$perms," in
                            *,write,*|*,append,*|*,add_file,*|*,add_subdirectory,*|*,delete,*|*,delete_child,*|*,writeattr,*|*,writeextattr,*|*,writesecurity,*|*,chown,*)
                                case "$principal" in
                                    user:root|group:wheel|group:admin) ;;
                                    *) fail 'The app path has an extended ACL granting non-administrator write access.' ;;
                                esac
                                ;;
                        esac
                        ;;
                    *)
                        fail 'The app path has an unrecognized extended ACL.'
                        ;;
                esac
                ;;
        esac
    done || exit 1
}

# Elevated filesystem operations are limited to bundles under administrator-
# controlled paths. Recheck every component here before creating any files.
entry=$app
app_uid=
app_gid=
while [ "$entry" != / ]; do
    [ -d "$entry" ] && [ ! -L "$entry" ] || fail 'The installed app path changed.'
    set -- $(/usr/bin/stat -f '%u %g %p' "$entry")
    [ "$#" -eq 3 ] || fail 'Cannot inspect the installed app path.'
    if [ -z "$app_uid" ]; then
        app_uid=$1
        app_gid=$2
    fi
    case " $(/usr/bin/id -Gn "$1") " in
        *' admin '*|*' wheel '*) ;;
        *) fail 'The app path is not controlled by an administrator.' ;;
    esac
    [ "$((0$3 & 0002))" -eq 0 ] || fail 'The app path is writable by other users.'
    if [ "$((0$3 & 0020))" -ne 0 ]; then
        [ "$2" -eq 0 ] || [ "$2" -eq 80 ] || fail 'The app path is writable by other users.'
    fi
    check_macos_acl "$entry"
    entry=$(/usr/bin/dirname "$entry")
done

/usr/bin/codesign --verify --deep --strict -R='identifier "org.corvid.desktop"' "$app"

lock="$app/.corvid-authorized-update"
/bin/mkdir "$lock" || fail 'Another administrator is updating Corvid.'
# Keep rollback material in the existing app directory until publication.
restore() {
    if [ -d "$lock/previous" ] && [ ! -e "$app/Contents" ]; then
        /bin/mv "$lock/previous" "$app/Contents" || return
    fi
    /bin/rm -rf "$lock"
}
trap restore EXIT
trap 'exit 1' HUP INT TERM
[ -d "$app/Contents" ] && [ ! -L "$app/Contents" ] || fail 'The original app contents changed.'
digest=$(/usr/bin/shasum -a 256 "$app/Contents/Resources/package.json")
[ "${digest%% *}" = "$expected" ] || fail 'Corvid changed while the update was being prepared.'
/usr/bin/ditto --noacl --noqtn "$incoming" "$lock/Corvid.app"
# Preserve administrator ownership and read-only access for non-administrators.
/usr/sbin/chown -Rh "$app_uid:$app_gid" "$lock/Corvid.app"
/bin/chmod -R go-w,a+rX "$lock/Corvid.app"
/usr/bin/codesign --verify --deep --strict -R='identifier "org.corvid.desktop"' "$lock/Corvid.app"
actual=$(/usr/bin/codesign --display --verbose=4 "$lock/Corvid.app" 2>&1 | /usr/bin/sed -n 's/^CDHash=//p')
[ "$actual" = "$cdhash" ] || fail 'The staged Corvid code signature changed.'
/bin/mv "$app/Contents" "$lock/previous"
/bin/mv "$lock/Corvid.app/Contents" "$app/Contents"

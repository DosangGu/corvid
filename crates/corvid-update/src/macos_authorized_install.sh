# Executed by macOS after the administrator authorizes this update.
set -eu
umask 022
app=$1
incoming=$2
expected=$3
cdhash=$4
fail() { printf '%s\n' "$*" >&2; exit 1; }

# Elevated filesystem operations are limited to bundles under administrator-
# controlled paths. Recheck every component here before creating any files.
entry=$app
while [ "$entry" != / ]; do
    [ -d "$entry" ] && [ ! -L "$entry" ] || fail 'The installed app path changed.'
    set -- $(/usr/bin/stat -f '%u %g %p' "$entry")
    [ "$#" -eq 3 ] || fail 'Cannot inspect the installed app path.'
    case " $(/usr/bin/id -Gn "$1") " in
        *' admin '*|*' wheel '*) ;;
        *) fail 'The app path is not controlled by an administrator.' ;;
    esac
    [ "$((0$3 & 0002))" -eq 0 ] || fail 'The app path is writable by other users.'
    if [ "$((0$3 & 0020))" -ne 0 ]; then
        [ "$2" -eq 0 ] || [ "$2" -eq 80 ] || fail 'The app path is writable by other users.'
    fi
    entry=$(/usr/bin/dirname "$entry")
done

/usr/bin/codesign --verify --deep --strict -R='identifier "org.corvid.desktop"' "$app"

lock="$app/.corvid-authorized-update"
mkdir "$lock" || fail 'Another administrator is updating Corvid.'
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
# Allow users of the shared installation to read its new resources.
chmod -R a+rX "$lock/Corvid.app"
/usr/bin/codesign --verify --deep --strict -R='identifier "org.corvid.desktop"' "$lock/Corvid.app"
actual=$(/usr/bin/codesign --display --verbose=4 "$lock/Corvid.app" 2>&1 | /usr/bin/sed -n 's/^CDHash=//p')
[ "$actual" = "$cdhash" ] || fail 'The staged Corvid code signature changed.'
/bin/mv "$app/Contents" "$lock/previous"
/bin/mv "$lock/Corvid.app/Contents" "$app/Contents"

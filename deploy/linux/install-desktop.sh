#!/bin/sh
# Install per-user menu entries against the stable current-version link.
set -eu
install_root=$1
data_home=${XDG_DATA_HOME:-$HOME/.local/share}
case "$data_home" in /*) ;; *) echo 'XDG_DATA_HOME must be absolute' >&2; exit 1 ;; esac
case "$data_home" in *'
'*) echo 'XDG_DATA_HOME cannot contain newlines' >&2; exit 1 ;; esac

# Desktop-entry strings and quoted Exec arguments have separate escaping rules.
desktop_string() { printf '%s' "$1" | sed 's/\\/\\\\/g'; }
desktop_exec() {
    printf '%s' "$1" | sed 's/\\/\\\\\\\\/g; s/"/\\\\"/g; s/`/\\\\`/g; s/\$/\\\\$/g; s/%/%%/g'
}
owner="X-Corvid-InstallRoot=$(desktop_string "$install_root")"
applications="$data_home/applications"
mkdir -p -- "$applications"

install_icon() {
    destination="$data_home/icons/hicolor/$1/apps/corvid.$2"
    source="$install_root/current/icons/corvid.$2"
    if [ -e "$destination" ] || [ -L "$destination" ]; then
        if [ ! -L "$destination" ] || [ "$(readlink "$destination")" != "$source" ]; then
            printf 'Keeping existing icon: %s\n' "$destination" >&2
            return
        fi
    else
        mkdir -p -- "$(dirname "$destination")"
        ln -s -- "$source" "$destination"
    fi
}

install_entry() {
    id=$1
    name=$2
    executable=$3
    class=$4
    destination="$applications/$id.desktop"
    if [ -e "$destination" ] || [ -L "$destination" ]; then
        if [ -L "$destination" ] || ! grep -Fxq -- "$owner" "$destination"; then
            printf 'Keeping existing menu entry: %s\n' "$destination" >&2
            return
        fi
    fi
    temporary=$(mktemp "$applications/.corvid.XXXXXXXX")
    cat > "$temporary" <<EOF
[Desktop Entry]
Type=Application
Name=$name
Exec="$(desktop_exec "$executable")"
Icon=$(desktop_string "$install_root/current/icons/corvid.svg")
Terminal=false
Categories=Development;Utility;
StartupWMClass=$class
$owner
EOF
    chmod 644 "$temporary"
    mv -f -- "$temporary" "$destination"
}

install_icon scalable svg
install_icon 256x256 png
install_entry Corvid.Desktop 'Corvid Desktop' "$install_root/current/corvid-desktop" Corvid.Desktop
if [ -f "$install_root/current/plugins/agent-deck/corvid-agent-deck" ]; then
    install_entry corvid-agent-deck 'Corvid Agent Deck' "$install_root/current/plugins/agent-deck/corvid-agent-deck" corvid-agent-deck
else
    entry="$applications/corvid-agent-deck.desktop"
    if [ -f "$entry" ] && [ ! -L "$entry" ] && grep -Fxq -- "$owner" "$entry"; then
        rm -f -- "$entry"
    fi
fi
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$applications" || true
fi
if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$data_home/icons/hicolor" >/dev/null 2>&1 || true
fi

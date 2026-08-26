#!/bin/sh
# Removes a native (non-Flatpak) install of powerdisplay.
#
# The project now ships only as a Flatpak. This script exists so anyone who installed
# with the old install.sh can take that copy off the machine without hunting down files
# by hand. Settings in ~/.config/powerdisplay are kept unless --purge is passed; the
# Flatpak reads from a different directory, so those files are leftover, not shared.
set -eu

PURGE=0
PREFIXES=""

usage() {
    cat <<EOF
Usage: ./uninstall.sh [--prefix DIR] [--purge]

Removes powerdisplay and powerdisplayd installed by the old install.sh.

  --prefix DIR   Only look here. Without this, both \$HOME/.local and
                 /usr/local are cleaned, which covers the two places
                 install.sh used to write to.
  --purge        Also delete ~/.config/powerdisplay. Leave this off if you
                 still want a copy of the profiles.

A Flatpak install is not touched. Remove that with:
  flatpak uninstall --user io.github.Emanuel4100.PowerDisplay
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --purge) PURGE=1 ;;
        --prefix) PREFIXES="$PREFIXES $2"; shift ;;
        --prefix=*) PREFIXES="$PREFIXES ${1#--prefix=}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "uninstall.sh: unknown option $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

if [ -z "$PREFIXES" ]; then
    PREFIXES="$HOME/.local /usr/local"
fi

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user disable --now powerdisplayd.service 2>/dev/null || true
fi

removed=""

remove_if_present() {
    path="$1"
    if [ -e "$path" ] || [ -L "$path" ]; then
        rm -f "$path"
        removed="$removed
  $path"
    fi
}

for PREFIX in $PREFIXES; do
    BINDIR="$PREFIX/bin"
    DATADIR="$PREFIX/share"
    ICONDIR="$DATADIR/icons/hicolor/scalable/apps"
    APPDIR="$DATADIR/applications"

    case "$PREFIX" in
        "$HOME"*) UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user" ;;
        *) UNITDIR="$PREFIX/lib/systemd/user" ;;
    esac

    remove_if_present "$BINDIR/powerdisplay"
    remove_if_present "$BINDIR/powerdisplayd"
    remove_if_present "$APPDIR/io.github.Emanuel4100.PowerDisplay.desktop"
    remove_if_present "$APPDIR/io.github.powerdisplay.Powerdisplay.desktop"
    remove_if_present "$ICONDIR/io.github.Emanuel4100.PowerDisplay.svg"
    remove_if_present "$ICONDIR/io.github.powerdisplay.Powerdisplay.svg"
    remove_if_present "$UNITDIR/powerdisplayd.service"

    if command -v update-desktop-database >/dev/null 2>&1 && [ -d "$APPDIR" ]; then
        update-desktop-database "$APPDIR" 2>/dev/null || true
    fi
    if command -v gtk-update-icon-cache >/dev/null 2>&1 && [ -d "$DATADIR/icons/hicolor" ]; then
        gtk-update-icon-cache -qtf "$DATADIR/icons/hicolor" 2>/dev/null || true
    fi
done

if command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload 2>/dev/null || true
fi

# install.sh always created ~/.config/powerdisplay unless XDG_CONFIG_HOME was set at
# install time. Honour both, so a leftover copy is not missed either way.
HOME_CONFIG="$HOME/.config/powerdisplay"
XDG_CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/powerdisplay"

purge_dir() {
    dir="$1"
    [ -e "$dir" ] || return 0
    rm -rf "$dir"
    echo "Removed settings in $dir."
}

if [ "$PURGE" -eq 1 ]; then
    purge_dir "$HOME_CONFIG"
    if [ "$XDG_CONFIG" != "$HOME_CONFIG" ]; then
        purge_dir "$XDG_CONFIG"
    fi
fi

echo "Removed the native install of powerdisplay."
if [ -n "$removed" ]; then
    echo "Deleted:$removed"
else
    echo "Nothing was installed under the prefixes that were checked."
fi
if [ "$PURGE" -eq 0 ]; then
    echo "Native settings were kept, if they exist:"
    echo "  $HOME_CONFIG"
    if [ "$XDG_CONFIG" != "$HOME_CONFIG" ]; then
        echo "  $XDG_CONFIG"
    fi
    echo "The Flatpak reads a different file and does not use these."
fi
echo
echo "A Flatpak copy, if you have one, is still installed:"
echo "  flatpak uninstall --user io.github.Emanuel4100.PowerDisplay"

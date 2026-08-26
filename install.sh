#!/bin/sh
# Builds and installs powerdisplay. Works for a per-user install (the default) and for a
# system-wide one, which is why the systemd unit path is chosen from the prefix rather
# than hard-coded.
set -eu

PREFIX="${PREFIX:-$HOME/.local}"
DESTDIR="${DESTDIR:-}"
ACTION="install"

usage() {
    cat <<EOF
Usage: ./install.sh [--uninstall] [--prefix DIR]

Installs powerdisplay (the settings window) and powerdisplayd (the background
service).

  --prefix DIR   Where to install. Default: \$HOME/.local
                 Use --prefix /usr/local for a system-wide install.
  --uninstall    Remove a previous installation instead.

Environment: PREFIX, DESTDIR and CARGO_TARGET_DIR are honoured.
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --uninstall) ACTION="uninstall" ;;
        --prefix) PREFIX="$2"; shift ;;
        --prefix=*) PREFIX="${1#--prefix=}" ;;
        -h|--help) usage; exit 0 ;;
        *) echo "install.sh: unknown option $1" >&2; usage >&2; exit 1 ;;
    esac
    shift
done

BINDIR="$PREFIX/bin"
DATADIR="$PREFIX/share"
ICONDIR="$DATADIR/icons/hicolor/scalable/apps"
APPDIR="$DATADIR/applications"

# systemd looks for user units in ~/.config/systemd/user for a home install, and in
# <prefix>/lib/systemd/user for anything shared.
case "$PREFIX" in
    "$HOME"*) UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user" ;;
    *) UNITDIR="$PREFIX/lib/systemd/user" ;;
esac

APP_ID="io.github.powerdisplay.Powerdisplay"

if [ "$ACTION" = "uninstall" ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --user disable --now powerdisplayd.service 2>/dev/null || true
    fi
    rm -f "$DESTDIR$BINDIR/powerdisplay" \
          "$DESTDIR$BINDIR/powerdisplayd" \
          "$DESTDIR$APPDIR/$APP_ID.desktop" \
          "$DESTDIR$ICONDIR/$APP_ID.svg" \
          "$DESTDIR$UNITDIR/powerdisplayd.service"
    echo "Removed powerdisplay from $PREFIX."
    echo "Your settings in ${XDG_CONFIG_HOME:-$HOME/.config}/powerdisplay were kept."
    exit 0
fi

command -v cargo >/dev/null 2>&1 || {
    echo "install.sh: cargo is required to build powerdisplay" >&2
    exit 1
}

echo "Building (this takes a few minutes the first time)..."
cargo build --release --locked

TARGET_DIR="${CARGO_TARGET_DIR:-$(cargo metadata --format-version 1 --no-deps \
    | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')}"
TARGET_DIR="${TARGET_DIR:-target}"

install -Dm755 "$TARGET_DIR/release/powerdisplay"  "$DESTDIR$BINDIR/powerdisplay"
install -Dm755 "$TARGET_DIR/release/powerdisplayd" "$DESTDIR$BINDIR/powerdisplayd"
install -Dm644 "data/$APP_ID.desktop" "$DESTDIR$APPDIR/$APP_ID.desktop"
install -Dm644 "data/$APP_ID.svg"     "$DESTDIR$ICONDIR/$APP_ID.svg"

mkdir -p "$DESTDIR$UNITDIR"
sed "s|@BINDIR@|$BINDIR|g" data/powerdisplayd.service \
    > "$DESTDIR$UNITDIR/powerdisplayd.service"
chmod 644 "$DESTDIR$UNITDIR/powerdisplayd.service"

if [ -z "$DESTDIR" ] && command -v systemctl >/dev/null 2>&1; then
    systemctl --user daemon-reload 2>/dev/null || true
fi
if [ -z "$DESTDIR" ] && command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPDIR" 2>/dev/null || true
fi
if [ -z "$DESTDIR" ] && command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -qtf "$DATADIR/icons/hicolor" 2>/dev/null || true
fi

echo
echo "Installed to $PREFIX."
case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *) echo "Note: $BINDIR is not in your PATH." ;;
esac
echo
echo "Next: run 'powerdisplay' to pick your settings, then turn on"
echo "'Run automatically in the background' from the window's menu."

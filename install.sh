#!/bin/sh
# Install Power Display. Prefers a prebuilt x86_64 Flatpak (local dist/ or GitHub
# Releases) so users do not need the SDK. Falls back to building from this tree.
set -eu

APP_ID=io.github.Emanuel4100.PowerDisplay
REPO=Emanuel4100/powerdisplay
ASSET=powerdisplay-x86_64.flatpak
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if ! command -v flatpak >/dev/null; then
    echo "install.sh: flatpak is not installed." >&2
    exit 1
fi

flatpak remote-add --user --if-not-exists flathub https://dl.flathub.org/repo/flathub.flatpakrepo

install_bundle() {
    flatpak install --user -y flathub org.gnome.Platform//50
    flatpak install --user -y "$1"
}

build_from_source() {
    echo "Building from source (needs the GNOME 50 SDK and the Rust extension)."
    flatpak install --user -y flathub \
        org.gnome.Sdk//50 \
        org.freedesktop.Sdk.Extension.rust-stable//25.08
    flatpak-builder --user --install --force-clean --disable-rofiles-fuse \
        "$ROOT/build-aux/build" "$ROOT/build-aux/io.github.Emanuel4100.PowerDisplay.yml"
}

arch=$(uname -m)
bundle=""
cleanup=""

if [ -f "$ROOT/dist/$ASSET" ]; then
    bundle="$ROOT/dist/$ASSET"
elif [ -f "$ROOT/$ASSET" ]; then
    bundle="$ROOT/$ASSET"
elif [ "$arch" = x86_64 ] || [ "$arch" = amd64 ]; then
    if command -v curl >/dev/null; then
        tmp=$(mktemp)
        url="https://github.com/$REPO/releases/latest/download/$ASSET"
        echo "Downloading $url"
        if curl -fL --progress-bar -o "$tmp" "$url"; then
            bundle=$tmp
            cleanup=$tmp
        else
            rm -f "$tmp"
            echo "No prebuilt release yet."
        fi
    fi
else
    echo "No prebuilt bundle for $arch."
fi

trap ' [ -n "$cleanup" ] && rm -f "$cleanup" ' EXIT

if [ -n "$bundle" ]; then
    install_bundle "$bundle"
else
    build_from_source
fi

echo "Installed. Run: flatpak run $APP_ID"
echo "Uninstall from Software, or: flatpak uninstall --user --delete-data $APP_ID"

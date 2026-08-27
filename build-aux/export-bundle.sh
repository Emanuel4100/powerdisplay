#!/bin/sh
# Produce dist/powerdisplay-x86_64.flatpak from this tree. Needs the GNOME 50 SDK
# and the Rust extension, same as a from-source install, but users of the bundle do not.
set -eu

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

mkdir -p dist
flatpak-builder --user --repo=build-aux/repo --force-clean --disable-rofiles-fuse \
    build-aux/build build-aux/io.github.Emanuel4100.PowerDisplay.yml
flatpak build-bundle build-aux/repo dist/powerdisplay-x86_64.flatpak \
    io.github.Emanuel4100.PowerDisplay master
echo "wrote dist/powerdisplay-x86_64.flatpak"

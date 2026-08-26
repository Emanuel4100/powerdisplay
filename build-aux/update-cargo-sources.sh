#!/bin/sh
# Regenerates build-aux/cargo-sources.json from Cargo.lock.
#
# A Flatpak build has no network, so every crate has to be listed as a source up front.
# Run this after anything changes Cargo.lock, or the build will fail on a missing crate.
#
# The generator is fetched rather than vendored because it tracks flatpak-builder, and its
# dependencies go in a throwaway virtualenv so that nothing is installed system-wide.
set -eu

HERE=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
ROOT=$(dirname "$HERE")
GENERATOR_URL="https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py"

command -v python3 >/dev/null 2>&1 || {
    echo "update-cargo-sources.sh: python3 is required" >&2
    exit 1
}

WORKDIR=$(mktemp -d)
trap 'rm -rf "$WORKDIR"' EXIT

echo "Setting up a temporary environment..."
python3 -m venv "$WORKDIR/venv"
"$WORKDIR/venv/bin/pip" install --quiet aiohttp toml tomlkit

echo "Fetching the generator..."
curl -fsSL "$GENERATOR_URL" -o "$WORKDIR/flatpak-cargo-generator.py"

echo "Reading Cargo.lock..."
"$WORKDIR/venv/bin/python3" "$WORKDIR/flatpak-cargo-generator.py" \
    "$ROOT/Cargo.lock" -o "$HERE/cargo-sources.json"

echo "Wrote $HERE/cargo-sources.json"

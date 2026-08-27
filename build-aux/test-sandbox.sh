#!/usr/bin/env bash
# Host unit tests plus a live probe of the installed Flatpak sandbox.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

APP_ID=io.github.Emanuel4100.PowerDisplay
MANIFEST="$ROOT/build-aux/io.github.Emanuel4100.PowerDisplay.yml"

echo "==> cargo test"
cargo test --workspace

echo "==> Flatpak manifest permissions"
for arg in \
    --share=network \
    --filesystem=/run/udev:ro \
    --filesystem=xdg-config/autostart:create \
    --talk-name=org.gnome.Mutter.DisplayConfig \
    --system-talk-name=org.freedesktop.UPower.PowerProfiles \
    --system-talk-name=net.hadess.PowerProfiles \
    --system-talk-name=org.freedesktop.login1 \
    --socket=wayland
do
    if ! grep -F -q -- "$arg" "$MANIFEST"; then
        echo "missing finish-arg: $arg" >&2
        exit 1
    fi
done

if ! command -v flatpak >/dev/null; then
    echo "flatpak not installed; skipping sandbox probes"
    exit 0
fi

if ! flatpak info "$APP_ID" >/dev/null 2>&1; then
    echo "$APP_ID is not installed; skipping sandbox probes"
    echo "Install with the commands in the README, then re-run $0"
    exit 0
fi

echo "==> installed Flatpak permissions"
PERMS="$(flatpak info --show-permissions "$APP_ID")"
echo "$PERMS"
echo "$PERMS" | grep -q 'network' || { echo "installed app is missing share=network" >&2; exit 1; }
echo "$PERMS" | grep -q '/run/udev' || { echo "installed app is missing /run/udev" >&2; exit 1; }
echo "$PERMS" | grep -q 'org.gnome.Mutter.DisplayConfig' || {
    echo "installed app cannot talk to Mutter DisplayConfig" >&2
    exit 1
}

echo "==> powerdisplayd --self-test (inside the sandbox)"
flatpak run --command=powerdisplayd "$APP_ID" --self-test

echo "==> powerdisplayd --show --dry-run (inside the sandbox)"
SHOW="$(flatpak run --command=powerdisplayd "$APP_ID" --show --dry-run)"
echo "$SHOW" | grep -q 'Power source:' || { echo "--show did not report a power source" >&2; exit 1; }
echo "$SHOW" | grep -q 'Backend:' || { echo "--show did not find a display backend" >&2; exit 1; }
echo "$SHOW" | grep -q '.var/app/io.github.Emanuel4100.PowerDisplay' || {
    echo "--show is not using the Flatpak config overlay" >&2
    exit 1
}

echo "sandbox tests passed"

# powerdisplay

Drops refresh rate and switches to power-saver on battery, then puts them back when you plug in.

Works on GNOME, KDE, Sway/Hyprland and most X11 desktops. No root.

## Install

```bash
flatpak install --user org.gnome.Sdk//50 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean \
    build-aux/build build-aux/io.github.Emanuel4100.PowerDisplay.yml
```

## Use

```bash
flatpak run io.github.Emanuel4100.PowerDisplay
```

Set **On battery** and **Plugged in**, hit **Save**, then enable **Run automatically in the background** in the menu. Nothing switches until that is on.

**Apply now** tries the current tab without saving.

On GNOME, leave “remember this layout” off. Mutter would otherwise pop **Keep changes?** on every switch and revert if you don’t click it.

## Config

```toml
version = 1
enabled = true
apply_on_start = true

[on_battery]
power_profile = "power-saver"
persist_display_config = false

[[on_battery.outputs]]
mode = "1920x1080@60.000"

[on_battery.outputs.match]
connector = "eDP-1"

[on_ac]
power_profile = "performance"
persist_display_config = false

[[on_ac.outputs]]
mode = "2880x1800@120.000"

[on_ac.outputs.match]
connector = "eDP-1"
```

Path: `~/.var/app/io.github.Emanuel4100.PowerDisplay/config/powerdisplay/config.toml`

Match on `connector`, `make`, `model`, and/or `serial`. To see what’s connected:

```bash
flatpak run --command=powerdisplayd io.github.Emanuel4100.PowerDisplay --show
```

`--apply-now` applies once and exits. `--dry-run` only logs.

If the wrong display backend is picked, set `POWERDISPLAY_BACKEND` to `gnome`, `kde`, `wlroots`, or `x11`.

After reinstalling the Flatpak, open the settings window once. That restarts the background service so it is not left running from the previous install.

## Tests

```bash
cargo test --workspace
build-aux/test-sandbox.sh
```

`test-sandbox.sh` runs the unit tests, then `powerdisplayd --self-test` inside the installed Flatpak. That probe is what catches sandbox holes (no `/sys/class/power_supply`, no udev socket, no Mutter on the bus) that host tests cannot see.

## Notes

- GNOME, KDE (via `kscreen-doctor`), wlroots compositors, and X11 (RandR).
- Performance modes need `power-profiles-daemon` or `tuned-ppd`. Display switching still works without them.
- After changing `Cargo.lock`, run `build-aux/update-cargo-sources.sh`.

GPL-3.0-or-later.

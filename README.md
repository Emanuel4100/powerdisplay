# powerdisplay

Switches your screen resolution, refresh rate and performance mode automatically when you
plug in or unplug the charger.

Set up two profiles — one for **On battery**, one for **Plugged in** — and a small
background service applies the right one the moment the power source changes. Running at
60 Hz and `power-saver` on battery and 120 Hz and `performance` on the wall is the usual
reason to want this.

It is built to run on any Linux desktop, not just GNOME: the resolution change goes
through whichever display API your session actually speaks.

## Desktop support

| Session | How the mode is changed | Notes |
| --- | --- | --- |
| GNOME (Wayland or X11) | `org.gnome.Mutter.DisplayConfig` over D-Bus | Can apply temporarily or save into GNOME's own display settings |
| KDE Plasma | `kscreen-doctor`, which ships with Plasma | KScreen always remembers the layout |
| sway, Hyprland, river, Wayfire, labwc, COSMIC | `wlr-output-management-unstable-v1` | Spoken directly; `wlr-randr` is not needed |
| XFCE, MATE, Cinnamon, i3, and any other X session | RandR, with `xrandr` as a fallback | |

Detection is automatic. If it guesses wrong, set `POWERDISPLAY_BACKEND` to `gnome`, `kde`,
`wlroots` or `x11`.

Performance modes come from whatever power-profiles service the distro ships:
`power-profiles-daemon` under either its current or its old bus name, or `tuned-ppd` on
Fedora 41 and later. If none is running, the resolution half still works and the
performance dropdown explains itself.

## Installing

This is a Flatpak. A tool whose whole job is reconfiguring the session fits awkwardly
inside a sandbox, which is why the permissions below are as wide as they are; there is no
native install path any more.

```bash
flatpak install --user org.gnome.Sdk//50 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --install --force-clean \
    build-aux/build build-aux/io.github.Emanuel4100.PowerDisplay.yml
flatpak run io.github.Emanuel4100.PowerDisplay
```

If you previously installed with `install.sh`, `./uninstall.sh` takes that copy off the
machine. It leaves `~/.config/powerdisplay` alone unless you pass `--purge`; those settings
are not shared with the Flatpak.

A few things about the sandbox are load-bearing rather than decorative:

- **Settings live under the app.**
  `~/.var/app/io.github.Emanuel4100.PowerDisplay/config/powerdisplay/config.toml`, not
  `~/.config/powerdisplay`. A leftover native copy and the Flatpak do not share a file, and
  running both daemons at once gives you two processes fighting over the same display.
- **The daemon is started by the desktop portal.** Nothing inside a sandbox may write a
  systemd unit onto the host, so the checkbox in the window asks the portal for an autostart
  entry instead. The portal has no way to be asked what it currently has on file, so the
  window keeps its own note of the setting.
- **`--share=network` is required**, for an app that otherwise never touches the network.
  udev delivers power-supply events over a netlink socket that only exists in the host's
  network namespace. The daemon also re-reads `/sys/class/power_supply` several times a
  second, so a missed uevent still switches within a fraction of a second rather than
  waiting for a long resync.
- **Plasma needs `--talk-name=org.freedesktop.Flatpak`**, because `kscreen-doctor` belongs
  to the desktop and lives on the host. That permission allows running host commands in
  general, which is close to no sandbox at all. Delete that line from the manifest if you
  do not run Plasma; the other three backends are unaffected.

After anything changes `Cargo.lock`, run `build-aux/update-cargo-sources.sh` — the Flatpak
build has no network and installs every crate from `build-aux/cargo-sources.json`.

## Using it

Run `flatpak run io.github.Emanuel4100.PowerDisplay`, pick what each tab should do, and
press **Save**. Then turn on **Run automatically in the background** from the menu in the
header bar — that asks the desktop to start `powerdisplayd` at login, and nothing happens
automatically until you do.

**Apply now** tries the current tab's settings straight away without saving them, which is
the quick way to check a mode actually works before committing to it.

The **Remember this layout in the desktop's display settings** checkbox decides whether
the change is temporary or written into your desktop's saved configuration. Leaving it off
is the safer choice: your normal display settings stay exactly as you left them.

On GNOME there is a second reason to leave it off. Mutter shows its **Keep changes?**
countdown for every configuration it is asked to save, and it offers no way to say that the
request did not come from a person clicking something — so switching this on means that
dialog appears on every single switch, including the periodic resyncs. There is no way
around it short of a GNOME Shell extension, which would suppress the dialog for the real
Settings app too. Since the daemon reapplies your settings on every power change, on resume
and on hotplug regardless, the saved copy buys you very little.

## Configuration file

Everything lives in
`~/.var/app/io.github.Emanuel4100.PowerDisplay/config/powerdisplay/config.toml`. The
daemon re-reads it whenever it changes, so editing it by hand works as well as using the
window.

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

A `match` block may name `connector`, `make`, `model` and `serial`. The EDID fields are
weighted above the connector, so a monitor that moves from one port to another is still
recognised.
`flatpak run --command=powerdisplayd io.github.Emanuel4100.PowerDisplay --show` prints
every display and every mode id it can see, which is where the values above come from.

## Command line

```
flatpak run --command=powerdisplayd io.github.Emanuel4100.PowerDisplay --show
flatpak run --command=powerdisplayd io.github.Emanuel4100.PowerDisplay --apply-now
flatpak run --command=powerdisplayd io.github.Emanuel4100.PowerDisplay --dry-run
```

`--show` prints what session, power source and modes were detected. `--apply-now` applies
the matching profile once and exits. `--dry-run` logs what would change and changes
nothing.

## How it works

```
                power_supply udev events ─┐
  ~/.var/app/.../config/powerdisplay/config.toml ├─→ powerdisplayd ─→ display backend
                     DRM hotplug events ─┤                   └→ power profiles D-Bus
              logind PrepareForSleep(false) ┘
```

The window and the daemon never talk to each other directly. The window writes the config
file, the daemon watches it. Closing the window changes nothing about the automation, and
the resident process carries no GUI toolkit with it.

Power source detection reads `/sys/class/power_supply`. It is woken by kernel uevents when
those arrive, and it re-reads the same files several times a second so a Flatpak that never
sees udev still switches as soon as the kernel updates `online`. It accounts for USB-C
charging (which reports through a `USB` supply rather than `Mains`) and for batteries
parked at a charge limit (which report `Not charging` rather than `Charging`).

Changes are debounced: docks and chargers produce a burst of events over a second or two,
and one settled mode switch is much better than four in a row.

## Licence

GPL-3.0-or-later.

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

Build dependencies:

```bash
# Fedora / RHEL
sudo dnf install cargo gtk4-devel systemd-devel

# Debian / Ubuntu
sudo apt install cargo libgtk-4-dev libudev-dev

# Arch
sudo pacman -S rust gtk4 systemd-libs
```

Then:

```bash
./install.sh                    # into ~/.local
./install.sh --prefix /usr/local   # system-wide
```

`./install.sh --uninstall` removes it again and leaves your settings alone.

## Using it

Run `powerdisplay`, pick what each tab should do, and press **Save**. Then turn on
**Run automatically in the background** from the menu in the header bar — that is what
enables the `powerdisplayd` user service, and nothing happens automatically until you do.

**Apply now** tries the current tab's settings straight away without saving them, which is
the quick way to check a mode actually works before committing to it.

The **Remember this layout in the desktop's display settings** checkbox decides whether
the change is temporary or written into your desktop's saved configuration. Leaving it off
is the safer choice: your normal display settings stay exactly as you left them.

## Configuration file

Everything lives in `~/.config/powerdisplay/config.toml`. The daemon re-reads it whenever
it changes, so editing it by hand works as well as using the window.

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
recognised. `powerdisplayd --show` prints every display and every mode id it can see,
which is where the values above come from.

## Command line

```
powerdisplayd --show        # what session, power source and modes were detected
powerdisplayd --apply-now   # apply the matching profile once and exit
powerdisplayd --dry-run     # log what would change, change nothing
```

## How it works

```
                power_supply udev events ─┐
        ~/.config/powerdisplay/config.toml ├─→ powerdisplayd ─→ display backend
                     DRM hotplug events ─┤                   └→ power profiles D-Bus
              logind PrepareForSleep(false) ┘
```

The window and the daemon never talk to each other directly. The window writes the config
file, the daemon watches it. Closing the window changes nothing about the automation, and
the resident process carries no GUI toolkit with it.

Power source detection reads `/sys/class/power_supply` and is woken by udev rather than by
polling, so an idle laptop stays idle. It accounts for USB-C charging (which reports
through a `USB` supply rather than `Mains`) and for batteries parked at a charge limit
(which report `Not charging` rather than `Charging`).

Changes are debounced: docks and chargers produce a burst of events over a second or two,
and one settled mode switch is much better than four in a row.

## Licence

GPL-3.0-or-later.

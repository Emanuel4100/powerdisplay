//! Where the machine's power is coming from, straight from the kernel.
//!
//! UPower would answer the same question, but it is not installed everywhere and adding a
//! hard dependency on it would undercut the point of running on any desktop. `sysfs` plus
//! a `udev` netlink monitor is available on every Linux system and reacts instantly.

use std::fs;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::Result;

use crate::config::PowerState;
use crate::watch::spawn_udev;

const SUPPLY_ROOT: &str = "/sys/class/power_supply";
const RESYNC_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupplyReading {
    pub kind: String,
    pub online: Option<bool>,
    pub status: Option<String>,
}

pub fn read_state() -> PowerState {
    classify(&read_supplies(Path::new(SUPPLY_ROOT)))
}

fn read_supplies(root: &Path) -> Vec<SupplyReading> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };

    entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let read = |name: &str| {
                fs::read_to_string(path.join(name))
                    .ok()
                    .map(|value| value.trim().to_string())
            };
            Some(SupplyReading {
                kind: read("type")?,
                online: read("online").map(|value| value == "1"),
                status: read("status"),
            })
        })
        .collect()
}

/// Decides between battery and wall power from a set of `power_supply` readings.
///
/// A laptop charging over USB-C reports through a `USB` supply rather than `Mains`, and a
/// machine sitting at its charge limit reports `Not charging` rather than `Charging`, so
/// neither signal alone is enough.
pub fn classify(supplies: &[SupplyReading]) -> PowerState {
    let mut any_line_supply = false;
    let mut line_online = false;
    let mut battery_discharging = false;

    for supply in supplies {
        let kind = supply.kind.to_ascii_uppercase();
        if kind == "BATTERY" {
            if let Some(status) = &supply.status {
                battery_discharging |= status.eq_ignore_ascii_case("discharging");
            }
        } else if kind == "MAINS" || kind.starts_with("USB") {
            if let Some(online) = supply.online {
                any_line_supply = true;
                line_online |= online;
            }
        }
    }

    if line_online {
        PowerState::Ac
    } else if battery_discharging || any_line_supply {
        PowerState::Battery
    } else {
        // No battery and no adapter to speak of: a desktop, which is always on the wall.
        PowerState::Ac
    }
}

/// Sends the current state immediately, then again on every change.
pub fn spawn_watcher(tx: Sender<PowerState>) -> Result<JoinHandle<()>> {
    let mut last = read_state();
    let _ = tx.send(last);

    spawn_udev("power_supply", RESYNC_INTERVAL, move || {
        let current = read_state();
        if current == last {
            return true;
        }
        last = current;
        tx.send(current).is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn supply(kind: &str, online: Option<bool>, status: Option<&str>) -> SupplyReading {
        SupplyReading {
            kind: kind.into(),
            online,
            status: status.map(str::to_string),
        }
    }

    #[test]
    fn a_live_adapter_means_ac() {
        let supplies = [
            supply("Mains", Some(true), None),
            supply("Battery", None, Some("Charging")),
        ];
        assert_eq!(classify(&supplies), PowerState::Ac);
    }

    #[test]
    fn an_idle_adapter_means_battery() {
        let supplies = [
            supply("Mains", Some(false), None),
            supply("Battery", None, Some("Discharging")),
        ];
        assert_eq!(classify(&supplies), PowerState::Battery);
    }

    #[test]
    fn charging_over_usb_c_counts_as_ac() {
        let supplies = [
            supply("Mains", Some(false), None),
            supply("USB_PD", Some(true), None),
            supply("Battery", None, Some("Charging")),
        ];
        assert_eq!(classify(&supplies), PowerState::Ac);
    }

    #[test]
    fn a_battery_at_its_charge_limit_is_not_treated_as_unplugged() {
        let supplies = [
            supply("Mains", Some(true), None),
            supply("Battery", None, Some("Not charging")),
        ];
        assert_eq!(classify(&supplies), PowerState::Ac);
    }

    #[test]
    fn a_machine_without_a_battery_is_assumed_plugged_in() {
        assert_eq!(classify(&[]), PowerState::Ac);
        assert_eq!(classify(&[supply("UPS", None, None)]), PowerState::Ac);
    }

    #[test]
    fn a_discharging_battery_wins_over_a_silent_adapter() {
        let supplies = [supply("Battery", None, Some("Discharging"))];
        assert_eq!(classify(&supplies), PowerState::Battery);
    }
}

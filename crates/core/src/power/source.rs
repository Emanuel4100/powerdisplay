//! Where the machine's power is coming from, straight from the kernel.
//!
//! UPower would answer the same question, but it is not installed everywhere and adding a
//! hard dependency on it would undercut the point of running on any desktop. `sysfs` plus
//! a `udev` netlink monitor is available on every Linux system and reacts instantly.

use std::fs;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::Result;

use crate::config::PowerState;
use crate::watch;

const SUPPLY_ROOT: &str = "/sys/class/power_supply";
/// Upper bound when udev never wakes us. A few tiny sysfs reads a second is nothing next
/// to missing a charger event for a minute, which is what the Flatpak was doing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
const UDEV_RESYNC: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SupplyReading {
    pub kind: String,
    pub online: Option<bool>,
    pub status: Option<String>,
}

pub fn read_state() -> PowerState {
    classify(&read_supplies_at(Path::new(SUPPLY_ROOT)))
}

pub fn read_supplies_at(root: &Path) -> Vec<SupplyReading> {
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

/// Requires two identical readings in a row before announcing a change.
///
/// USB-C ports and charge-limit firmware glitch `online` for a single poll. Acting on
/// that would flip the panel to 60 Hz and back, which is what "autoswitch is broken"
/// looks like on this laptop.
#[derive(Debug)]
pub struct DebouncedState {
    published: PowerState,
    candidate: Option<PowerState>,
}

impl DebouncedState {
    pub fn new(initial: PowerState) -> Self {
        Self {
            published: initial,
            candidate: None,
        }
    }

    pub fn published(&self) -> PowerState {
        self.published
    }

    /// Returns `Some` when `current` should be announced as the new power source.
    pub fn observe(&mut self, current: PowerState) -> Option<PowerState> {
        if current == self.published {
            self.candidate = None;
            return None;
        }
        if self.candidate == Some(current) {
            self.published = current;
            self.candidate = None;
            return Some(current);
        }
        self.candidate = Some(current);
        None
    }
}

/// Sends the current state immediately, then again on every confirmed change.
///
/// Udev is the fast path. A short sysfs poll sits behind it because a Flatpak often never
/// sees `power_supply` netlink messages — the socket is in the host network namespace and
/// the udev database under `/run/udev` is not in the sandbox — and the previous 60-second
/// resync felt like the app had simply stopped working.
pub fn spawn_watcher(tx: Sender<PowerState>) -> Result<JoinHandle<()>> {
    let last = Arc::new(Mutex::new(DebouncedState::new(read_state())));
    let _ = tx.send(locked_published(&last));

    let udev_last = last.clone();
    let udev_tx = tx.clone();
    match watch::spawn_kernel_uevents("power_supply", UDEV_RESYNC, move || {
        publish(&udev_last, &udev_tx)
    }) {
        Ok(handle) => {
            // The thread is the process's; dropping the handle detaches it.
            drop(handle);
        }
        Err(err) => tracing::warn!(
            error = %err,
            "no kernel uevents for the power supply; polling sysfs instead"
        ),
    }

    let poll_last = last;
    thread::Builder::new()
        .name("power-poll".into())
        .spawn(move || loop {
            thread::sleep(POLL_INTERVAL);
            if !publish(&poll_last, &tx) {
                return;
            }
        })
        .map_err(Into::into)
}

fn locked_published(last: &Mutex<DebouncedState>) -> PowerState {
    last.lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .published()
}

fn publish(last: &Mutex<DebouncedState>, tx: &Sender<PowerState>) -> bool {
    let current = read_state();
    let mut last = last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match last.observe(current) {
        Some(state) => tx.send(state).is_ok(),
        None => true,
    }
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

    #[test]
    fn a_usb_c_port_that_is_offline_does_not_hide_a_live_adapter() {
        let supplies = [
            supply("Mains", Some(true), None),
            supply("USB", Some(false), None),
            supply("Battery", None, Some("Not charging")),
        ];
        assert_eq!(classify(&supplies), PowerState::Ac);
    }

    #[test]
    fn a_single_glitch_does_not_publish_a_power_change() {
        let mut state = DebouncedState::new(PowerState::Ac);
        assert_eq!(state.observe(PowerState::Battery), None);
        assert_eq!(state.published(), PowerState::Ac);
        assert_eq!(state.observe(PowerState::Ac), None);
        assert_eq!(state.published(), PowerState::Ac);
    }

    #[test]
    fn two_matching_readings_publish_the_new_source() {
        let mut state = DebouncedState::new(PowerState::Ac);
        assert_eq!(state.observe(PowerState::Battery), None);
        assert_eq!(state.observe(PowerState::Battery), Some(PowerState::Battery));
        assert_eq!(state.published(), PowerState::Battery);
    }
}

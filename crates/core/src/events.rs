//! Everything the daemon needs to wake up for, funnelled into one channel.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use anyhow::Result;

use crate::config::{ConfigWatcher, PowerState};
use crate::power::source;
use crate::watch::spawn_udev;

const DRM_RESYNC_INTERVAL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// The adapter was plugged in or unplugged.
    Power(PowerState),
    /// A monitor was connected, disconnected, or otherwise re-probed.
    DisplaysChanged,
    /// The config file on disk changed, most likely because the GUI saved it.
    ConfigChanged,
    /// The machine came back from suspend, where the desktop often restores its own
    /// display configuration behind our back.
    Resumed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charger_events_settle_faster_than_hotplug() {
        assert!(Event::Power(PowerState::Ac).settle_delay() < Event::DisplaysChanged.settle_delay());
        assert!(Event::ConfigChanged.settle_delay() < Event::Power(PowerState::Ac).settle_delay());
    }
}

impl Event {
    /// How long to wait for the dust to settle before acting.
    ///
    /// Chargers and docks bounce: plugging a laptop into a Thunderbolt dock produces a
    /// burst of power and DRM events over a second or two, and reacting to each one would
    /// mean several mode switches in a row.
    pub fn settle_delay(self) -> Duration {
        match self {
            Event::Power(_) => Duration::from_millis(1500),
            Event::DisplaysChanged => Duration::from_millis(2000),
            Event::ConfigChanged => Duration::from_millis(300),
            Event::Resumed => Duration::from_millis(2500),
        }
    }
}

/// Owns the watcher threads and the file watcher; dropping it stops delivery.
pub struct EventSources {
    pub rx: Receiver<Event>,
    _config_watcher: Option<ConfigWatcher>,
}

impl EventSources {
    pub fn spawn() -> Result<Self> {
        let (tx, rx) = channel();

        let (power_tx, power_rx) = channel();
        source::spawn_watcher(power_tx)?;
        forward_power(power_rx, tx.clone());

        // DRM events cover monitor hotplug on every driver and compositor, which is why
        // this is not done through a desktop-specific signal.
        let display_tx = tx.clone();
        if let Err(err) = spawn_udev("drm", DRM_RESYNC_INTERVAL, move |_| {
            display_tx.send(Event::DisplaysChanged).is_ok()
        }) {
            tracing::warn!(error = %err, "no monitor hotplug detection");
        }

        let (config_tx, config_rx) = channel();
        let config_watcher = match ConfigWatcher::new(config_tx) {
            Ok(watcher) => Some(watcher),
            Err(err) => {
                tracing::warn!(error = %err, "config changes will need a restart to take effect");
                None
            }
        };
        forward_config(config_rx, tx.clone());

        spawn_resume_watcher(tx);

        Ok(Self {
            rx,
            _config_watcher: config_watcher,
        })
    }
}

fn forward_power(rx: Receiver<PowerState>, tx: Sender<Event>) {
    thread::Builder::new()
        .name("power-events".into())
        .spawn(move || {
            for state in rx {
                if tx.send(Event::Power(state)).is_err() {
                    return;
                }
            }
        })
        .expect("spawning a thread");
}

fn forward_config(rx: Receiver<()>, tx: Sender<Event>) {
    thread::Builder::new()
        .name("config-events".into())
        .spawn(move || {
            for () in rx {
                if tx.send(Event::ConfigChanged).is_err() {
                    return;
                }
            }
        })
        .expect("spawning a thread");
}

/// Listens for logind's `PrepareForSleep(false)`, which is the portable "we just woke up"
/// signal. Absent logind we simply never fire, and the periodic resync covers us.
fn spawn_resume_watcher(tx: Sender<Event>) {
    thread::Builder::new()
        .name("resume-watcher".into())
        .spawn(move || {
            if let Err(err) = watch_resume(&tx) {
                tracing::info!(error = %format!("{err:#}"), "not watching for resume from suspend");
            }
        })
        .expect("spawning a thread");
}

fn watch_resume(tx: &Sender<Event>) -> Result<()> {
    let connection = zbus::blocking::Connection::system()?;
    let proxy = zbus::blocking::Proxy::new(
        &connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;

    for message in proxy.receive_signal("PrepareForSleep")? {
        let going_to_sleep: bool = match message.body().deserialize() {
            Ok(value) => value,
            Err(err) => {
                tracing::debug!(%err, "unreadable PrepareForSleep signal");
                continue;
            }
        };

        if !going_to_sleep && tx.send(Event::Resumed).is_err() {
            return Ok(());
        }
    }

    Ok(())
}

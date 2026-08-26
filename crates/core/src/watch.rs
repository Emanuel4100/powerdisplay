//! A small blocking udev listener shared by the power-supply and DRM watchers.

use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// Which netlink group to listen on.
///
/// Userspace (`udev`) is what systemd-udevd broadcasts after it has processed an event.
/// Kernel is the raw `kobject` uevent, which still arrives in a Flatpak that can see the
/// host network namespace even when the udev database under `/run/udev` is missing.
#[derive(Clone, Copy, Debug)]
pub enum UdevSource {
    Userspace,
    Kernel,
}

/// Runs `on_event` whenever the kernel reports a change in `subsystem`, and at least once
/// every `resync` even if nothing was reported. Returning `false` from the callback stops
/// the thread.
///
/// The periodic wake-up is deliberate: a suspend/resume cycle can swallow netlink
/// messages, and silently acting on a stale reading forever is worse than one extra check.
pub fn spawn_udev(
    subsystem: &'static str,
    resync: Duration,
    on_event: impl FnMut() -> bool + Send + 'static,
) -> Result<JoinHandle<()>> {
    spawn_uevent_monitor(UdevSource::Userspace, subsystem, resync, on_event)
}

pub fn spawn_kernel_uevents(
    subsystem: &'static str,
    resync: Duration,
    on_event: impl FnMut() -> bool + Send + 'static,
) -> Result<JoinHandle<()>> {
    spawn_uevent_monitor(UdevSource::Kernel, subsystem, resync, on_event)
}

fn spawn_uevent_monitor(
    source: UdevSource,
    subsystem: &'static str,
    resync: Duration,
    mut on_event: impl FnMut() -> bool + Send + 'static,
) -> Result<JoinHandle<()>> {
    let builder = match source {
        UdevSource::Userspace => udev::MonitorBuilder::new().context("creating a udev monitor")?,
        UdevSource::Kernel => {
            udev::MonitorBuilder::new_kernel().context("creating a kernel uevent monitor")?
        }
    };

    let monitor = builder
        .match_subsystem(subsystem)
        .with_context(|| format!("filtering udev events to {subsystem}"))?
        .listen()
        .context("listening for udev events")?;

    let timeout = Timespec {
        tv_sec: resync.as_secs() as _,
        tv_nsec: resync.subsec_nanos() as _,
    };

    thread::Builder::new()
        .name(format!("{subsystem}-watcher"))
        .spawn(move || {
            loop {
                let mut fds = [PollFd::new(&monitor, PollFlags::IN)];
                match poll(&mut fds, Some(&timeout)) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(err) => {
                        tracing::error!(%err, subsystem, "udev poll failed; backing off");
                        thread::sleep(resync);
                    }
                }

                let readable = fds[0].revents().contains(PollFlags::IN);
                let mut matched = 0usize;
                while monitor.iter().next().is_some() {
                    matched += 1;
                }

                // A wake with nothing matching is some other subsystem's uevent leaking
                // through; acting on it would make the DRM watcher re-apply on every
                // backlight tick.
                if matched == 0 && readable {
                    continue;
                }

                if !on_event() {
                    return;
                }
            }
        })
        .with_context(|| format!("starting the {subsystem} watcher thread"))
}

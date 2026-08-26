//! A small blocking udev listener shared by the power-supply and DRM watchers.

use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use rustix::event::{PollFd, PollFlags, Timespec, poll};

/// Runs `on_event` whenever the kernel reports a change in `subsystem`, and at least once
/// every `resync` even if nothing was reported. Returning `false` from the callback stops
/// the thread.
///
/// The periodic wake-up is deliberate: a suspend/resume cycle can swallow netlink
/// messages, and silently acting on a stale reading forever is worse than one wake-up a
/// minute.
pub fn spawn_udev(
    subsystem: &'static str,
    resync: Duration,
    mut on_event: impl FnMut() -> bool + Send + 'static,
) -> Result<JoinHandle<()>> {
    let monitor = udev::MonitorBuilder::new()
        .context("creating a udev monitor")?
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

                // The events themselves say no more than "something changed here", so
                // they are drained and the real state is read back by the callback.
                while monitor.iter().next().is_some() {}

                if !on_event() {
                    return;
                }
            }
        })
        .with_context(|| format!("starting the {subsystem} watcher thread"))
}

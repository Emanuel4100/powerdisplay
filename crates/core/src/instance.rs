//! Single-instance lock for `powerdisplayd`, plus host autostart cleanup.
//!
//! The lock lives under the Flatpak app runtime dir so every instance of this app sees
//! the same file. `flock` is released when the process dies, so a leftover file after a
//! crash does not wedge the next start.
//!
//! GNOME Software does not reliably kill a `flatpak run --command=powerdisplayd` instance
//! or delete the portal autostart entry. The daemon therefore watches whether this app
//! is still installed, and the autostart Exec is a wrapper that no-ops if it is not.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};

pub const FLATPAK_ID: &str = "io.github.Emanuel4100.PowerDisplay";

/// How long to keep trying for the lock before concluding another daemon really owns it.
///
/// [`is_held`] takes a shared lock for an instant to answer its question, and a daemon
/// that gave up the first time it lost that race would leave the machine with no daemon
/// at all.
const ACQUIRE_TIMEOUT: Duration = Duration::from_secs(2);
const ACQUIRE_INTERVAL: Duration = Duration::from_millis(50);

pub fn lock_path() -> PathBuf {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    if crate::sandboxed() {
        runtime
            .join("app")
            .join(FLATPAK_ID)
            .join("powerdisplayd.lock")
    } else {
        runtime.join("powerdisplayd.lock")
    }
}

fn open_lock_file(path: &Path) -> Result<File> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Returns `Some(file)` holding the exclusive lock, or `None` if another daemon is up.
pub fn try_acquire() -> Result<Option<File>> {
    try_acquire_at(&lock_path())
}

fn try_acquire_at(path: &Path) -> Result<Option<File>> {
    let mut file = open_lock_file(path)?;

    let deadline = Instant::now() + ACQUIRE_TIMEOUT;
    loop {
        match flock(&file, FlockOperation::NonBlockingLockExclusive) {
            Ok(()) => {
                // Informational only: inside a Flatpak this is the sandbox's own pid
                // namespace, so it means nothing to anyone outside this instance.
                file.set_len(0)
                    .and_then(|_| writeln!(file, "{}", std::process::id()))
                    .with_context(|| format!("writing {}", path.display()))?;
                return Ok(Some(file));
            }
            Err(rustix::io::Errno::WOULDBLOCK) if Instant::now() < deadline => {
                std::thread::sleep(ACQUIRE_INTERVAL);
            }
            Err(rustix::io::Errno::WOULDBLOCK) => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("locking {}", path.display())),
        }
    }
}

/// True when a daemon currently holds the lock.
///
/// The probe takes a *shared* lock: it still fails against the daemon's exclusive one, but
/// two windows asking at the same time do not block each other, and the moment it is held
/// for cannot make a starting daemon think it lost.
pub fn is_held() -> bool {
    is_held_at(&lock_path())
}

fn is_held_at(path: &Path) -> bool {
    let Ok(file) = File::open(path) else {
        return false;
    };
    match flock(&file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => {
            let _ = flock(&file, FlockOperation::Unlock);
            false
        }
        Err(rustix::io::Errno::WOULDBLOCK) => true,
        Err(_) => false,
    }
}

/// Waits until no daemon holds the lock, returning false if one still does.
///
/// `pkill` returns as soon as the signal is queued, so the process it killed may still be
/// holding the lock; starting a replacement before then makes the replacement exit.
pub fn wait_until_free(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if !is_held() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(ACQUIRE_INTERVAL);
    }
}

/// Held for the duration of an apply so the daemon and a manual `--apply-now` cannot drive
/// the compositor at the same time. Dropping the returned file releases it.
///
/// Failing to take the guard is not worth aborting over: applying unserialised is better
/// than not applying at all.
pub fn apply_guard() -> Option<File> {
    let path = lock_path().with_extension("apply");
    let file = open_lock_file(&path).ok()?;
    flock(&file, FlockOperation::LockExclusive).ok()?;
    Some(file)
}

/// Host `~/.config/autostart` entry. Inside the sandbox this is visible only with
/// `--filesystem=xdg-config/autostart`; `XDG_CONFIG_HOME` is the Flatpak overlay and
/// must not be used here.
pub fn host_autostart_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home)
        .join(".config/autostart")
        .join(format!("{FLATPAK_ID}.desktop")))
}

/// Autostart file that starts the daemon at login, but does nothing if Software has
/// already uninstalled the app (so the session does not show a failed-start dialog).
pub fn autostart_desktop_contents() -> String {
    format!(
        "\
[Desktop Entry]
Type=Application
Name=Power Display
Comment=Switch display mode when the charger is plugged in or out
X-GNOME-Autostart-enabled=true
X-Flatpak={FLATPAK_ID}
X-XDP-Autostart={FLATPAK_ID}
Exec=/bin/sh -c \"flatpak info {FLATPAK_ID} >/dev/null 2>&1 && exec flatpak run --command=powerdisplayd {FLATPAK_ID}\"
"
    )
}

pub fn write_host_autostart() -> Result<()> {
    let path = host_autostart_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    fs::write(&path, autostart_desktop_contents())
        .with_context(|| format!("writing {}", path.display()))
}

pub fn remove_host_autostart() {
    if let Ok(path) = host_autostart_path() {
        match fs::remove_file(&path) {
            Ok(()) => tracing::info!(path = %path.display(), "removed autostart entry"),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(
                path = %path.display(),
                error = %err,
                "could not remove autostart entry"
            ),
        }
    }
}

/// Whether this Flatpak is still installed on the host.
///
/// Outside a sandbox there is no app-store copy to disappear from under us.
pub fn app_still_installed() -> bool {
    if !crate::sandboxed() {
        return true;
    }

    Command::new("flatpak-spawn")
        .args(["--host", "flatpak", "info", FLATPAK_ID])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(true)
}

/// Drop host leftovers after Software uninstalls the app, then the caller should exit.
pub fn cleanup_after_uninstall() {
    tracing::info!("app is no longer installed; stopping the background service");
    remove_host_autostart();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_path_is_under_the_runtime_dir() {
        let path = lock_path();
        assert!(
            path.ends_with("powerdisplayd.lock"),
            "{}",
            path.display()
        );
    }

    #[test]
    fn autostart_is_a_no_op_when_the_flatpak_is_gone() {
        let desktop = autostart_desktop_contents();
        assert!(desktop.contains("flatpak info"));
        assert!(desktop.contains("powerdisplayd"));
        assert!(desktop.contains(FLATPAK_ID));
        assert!(
            desktop.contains(">/dev/null 2>&1 && exec"),
            "login must not show an error after Software uninstall:\n{desktop}"
        );
    }

    fn scratch_lock(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("powerdisplay-{}-{name}.lock", std::process::id()))
    }

    #[test]
    fn a_second_daemon_backs_off_while_the_first_holds_the_lock() {
        let path = scratch_lock("held");
        let _ = fs::remove_file(&path);

        let first = try_acquire_at(&path).unwrap().expect("first should win");
        assert!(is_held_at(&path));
        assert!(try_acquire_at(&path).unwrap().is_none());

        drop(first);
        assert!(!is_held_at(&path));
        assert!(try_acquire_at(&path).unwrap().is_some());
        let _ = fs::remove_file(&path);
    }

    /// The window probes the lock to word its status message. Doing that with an exclusive
    /// lock could make a daemon starting at the same moment believe it had a rival and
    /// exit, leaving nothing running.
    #[test]
    fn probing_for_a_daemon_does_not_lock_out_a_starting_one() {
        let path = scratch_lock("probed");
        let _ = fs::remove_file(&path);
        open_lock_file(&path).unwrap();

        let probing = path.clone();
        let probe = std::thread::spawn(move || {
            for _ in 0..2000 {
                let _ = is_held_at(&probing);
            }
        });

        let held = try_acquire_at(&path).expect("acquiring the lock");
        probe.join().unwrap();
        let _ = fs::remove_file(&path);
        assert!(held.is_some(), "the probe must not starve a starting daemon");
    }

    #[test]
    fn the_apply_guard_is_a_different_lock_from_the_instance_lock() {
        assert_ne!(lock_path(), lock_path().with_extension("apply"));
    }

    #[test]
    fn host_autostart_is_not_under_the_flatpak_overlay() {
        let path = host_autostart_path().unwrap();
        assert!(path.ends_with("autostart/io.github.Emanuel4100.PowerDisplay.desktop"));
        assert!(!path.to_string_lossy().contains(".var/app"));
    }
}

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
use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use rustix::fs::{FlockOperation, flock};

pub const FLATPAK_ID: &str = "io.github.Emanuel4100.PowerDisplay";

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

/// Returns `Some(file)` holding the exclusive lock, or `None` if another daemon is up.
pub fn try_acquire() -> Result<Option<File>> {
    let path = lock_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;

    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            file.set_len(0)
                .and_then(|_| writeln!(file, "{}", std::process::id()))
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(Some(file))
        }
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("locking {}", path.display())),
    }
}

/// True when another process currently holds the daemon lock.
pub fn is_held() -> bool {
    let path = lock_path();
    let Ok(file) = File::open(&path) else {
        return false;
    };
    match flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => {
            let _ = flock(&file, FlockOperation::Unlock);
            false
        }
        Err(rustix::io::Errno::WOULDBLOCK) => true,
        Err(_) => false,
    }
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

    #[test]
    fn host_autostart_is_not_under_the_flatpak_overlay() {
        let path = host_autostart_path().unwrap();
        assert!(path.ends_with("autostart/io.github.Emanuel4100.PowerDisplay.desktop"));
        assert!(!path.to_string_lossy().contains(".var/app"));
    }
}

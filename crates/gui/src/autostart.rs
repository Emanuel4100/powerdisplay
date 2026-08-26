//! Enabling and disabling the background service.
//!
//! systemd is assumed but not required: on a system without it the toggle is hidden
//! rather than broken, and the user starts `powerdisplayd` however their session does it.

use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

const UNIT: &str = "powerdisplayd.service";

pub fn available() -> bool {
    Command::new("systemctl")
        .args(["--user", "cat", UNIT])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn is_enabled() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-enabled", UNIT])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    let action = if enabled { "enable" } else { "disable" };
    let output = Command::new("systemctl")
        .args(["--user", action, "--now", UNIT])
        .output()
        .context("running systemctl")?;

    if !output.status.success() {
        bail!(
            "systemctl --user {action} --now {UNIT} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

/// Nudges an already-running daemon so a saved change takes effect without waiting for
/// the file watcher. Failure is not interesting: the watcher is the real mechanism.
pub fn is_running() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", UNIT])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

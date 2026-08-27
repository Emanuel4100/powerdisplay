//! Enabling and disabling the background service.
//!
//! powerdisplay ships as a Flatpak, and nothing inside a sandbox may write a systemd unit
//! onto the host. The autostart entry is therefore delegated to the desktop portal: the
//! checkbox asks it to start `powerdisplayd` at login, and to drop that entry again when
//! the box is unticked.
//!
//! Outside a sandbox the toggle is hidden. There is no unit to enable, and asking the
//! portal to autostart a host binary named `powerdisplayd` would be a lie after the native
//! install has been removed.

use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use powerdisplay_core::instance;
use zbus::blocking::Connection;
use zbus::zvariant::Value;

/// How long to wait for the portal to answer. The request is not expected to prompt, but
/// the portal is free to ask the user, and a hung UI is worse than a slow one.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(60);

pub fn available() -> bool {
    powerdisplay_core::sandboxed()
}

pub fn is_enabled() -> bool {
    marker_path().map(|p| p.exists()).unwrap_or(false)
}

pub fn set_enabled(enabled: bool) -> Result<()> {
    if !available() {
        bail!("no way to start the background service on this system");
    }
    portal_set_enabled(enabled)?;
    if enabled {
        restart()?;
    } else {
        stop()?;
    }
    Ok(())
}

/// Whether the daemon is up right now, used only to word the status message after a save.
pub fn is_running() -> bool {
    instance::is_held()
}

/// After a Flatpak reinstall the previous `powerdisplayd` often keeps running with the
/// old `/app` mount. Opening the settings window replaces it so autoswitch matches the
/// installed build.
pub fn ensure_fresh_daemon() {
    if !available() || !is_enabled() {
        return;
    }
    std::thread::spawn(|| {
        if let Err(err) = restart() {
            tracing::warn!("could not start the background service: {err:#}");
        }
    });
}

pub fn restart() -> Result<()> {
    stop()?;
    std::thread::sleep(Duration::from_millis(300));
    start()
}

pub fn stop() -> Result<()> {
    let mut cmd = host_command("pkill", &["-x", "powerdisplayd"]);
    let status = cmd.status().context("stopping powerdisplayd")?;
    if status.success() || status.code() == Some(1) {
        return Ok(());
    }
    bail!("pkill exited with {status}");
}

fn start() -> Result<()> {
    let log_path = log_path()?;
    if let Some(dir) = log_path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("opening {}", log_path.display()))?;

    let mut cmd = if powerdisplay_core::sandboxed() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.args([
            "--host",
            "flatpak",
            "run",
            "--command=powerdisplayd",
            instance::FLATPAK_ID,
        ]);
        cmd
    } else {
        Command::new("powerdisplayd")
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("starting powerdisplayd")?;
    Ok(())
}

fn host_command(program: &str, args: &[&str]) -> Command {
    if powerdisplay_core::sandboxed() {
        let mut cmd = Command::new("flatpak-spawn");
        cmd.arg("--host").arg(program).args(args);
        cmd
    } else {
        let mut cmd = Command::new(program);
        cmd.args(args);
        cmd
    }
}

fn log_path() -> Result<PathBuf> {
    let state = match std::env::var_os("XDG_STATE_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var_os("HOME").context("HOME is not set")?;
            PathBuf::from(home).join(".local/state")
        }
    };
    Ok(state.join("powerdisplayd.log"))
}

/// Records that autostart was requested.
///
/// The portal writes its autostart entry to the host's `~/.config/autostart`, which the
/// sandbox cannot read back, and it offers no way to query the current state. Without a
/// local note the checkbox would forget its own setting every time the window opens.
fn marker_path() -> Result<PathBuf> {
    Ok(powerdisplay_core::config::config_dir()?.join("autostart-requested"))
}

fn portal_set_enabled(enabled: bool) -> Result<()> {
    let granted = request_background(enabled)?;

    if enabled && !granted {
        bail!("the desktop refused permission to run in the background");
    }

    let marker = marker_path()?;
    if enabled {
        if let Some(dir) = marker.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        std::fs::write(&marker, "").with_context(|| format!("writing {}", marker.display()))?;
        instance::write_host_autostart()?;
    } else {
        if marker.exists() {
            std::fs::remove_file(&marker)
                .with_context(|| format!("removing {}", marker.display()))?;
        }
        instance::remove_host_autostart();
    }

    Ok(())
}

/// Asks the portal to start `powerdisplayd` at login, and returns whether autostart is on
/// afterwards.
///
/// Turning it off is the same call with `autostart: false`, which makes the portal drop the
/// entry it previously wrote.
///
/// The exchange happens on its own thread so that a portal which never answers costs a
/// stranded thread rather than a frozen window.
fn request_background(enabled: bool) -> Result<bool> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(request_background_blocking(enabled));
    });

    match rx.recv_timeout(PORTAL_TIMEOUT) {
        Ok(result) => result,
        Err(_) => bail!("the desktop portal did not answer"),
    }
}

fn request_background_blocking(enabled: bool) -> Result<bool> {
    let connection = Connection::session().context("connecting to the session bus")?;

    // The response is a signal on an object the portal derives from our bus name and the
    // token we pass, so the path is known in advance. Subscribing before the call rather
    // than after the reply is what stops a fast portal from answering into the void.
    let token = request_token();
    let handle = request_path(&connection, &token)?;
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.portal.Desktop")
        .context("naming the portal in the match rule")?
        .interface("org.freedesktop.portal.Request")
        .context("naming the request interface in the match rule")?
        .member("Response")
        .context("naming the response signal in the match rule")?
        .path(handle)
        .context("naming the request object in the match rule")?
        .build();

    let responses = zbus::blocking::MessageIterator::for_match_rule(rule, &connection, None)
        .context("subscribing to the portal response")?;

    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("handle_token", Value::from(token));
    options.insert(
        "reason",
        Value::from("Switch display mode when the charger is plugged in or out"),
    );
    options.insert("autostart", Value::from(enabled));
    options.insert("background", Value::from(enabled));
    options.insert("dbus-activatable", Value::from(false));
    options.insert(
        "commandline",
        Value::from(vec!["powerdisplayd".to_string()]),
    );

    connection
        .call_method(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            Some("org.freedesktop.portal.Background"),
            "RequestBackground",
            // An empty parent window: the toggle is not modal to anything.
            &("", options),
        )
        .context("calling the background portal")?;

    for message in responses {
        let message = message.context("receiving the portal response")?;
        let (response, results): (u32, HashMap<String, zbus::zvariant::OwnedValue>) = message
            .body()
            .deserialize()
            .context("reading the portal response")?;

        // 0 is success; 1 is the user cancelling and 2 is anything else going wrong.
        // Neither of the latter is an error worth raising: the setting simply stays off.
        if response != 0 {
            return Ok(false);
        }

        return Ok(results
            .get("autostart")
            .and_then(|value| bool::try_from(value.clone()).ok())
            .unwrap_or(false));
    }

    bail!("the session bus closed while waiting for the portal")
}

fn request_token() -> String {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("powerdisplay_{stamp}")
}

fn request_path(connection: &Connection, token: &str) -> Result<String> {
    let unique = connection
        .unique_name()
        .context("the session bus did not give us a name")?;
    let sender = unique.trim_start_matches(':').replace('.', "_");
    Ok(format!(
        "/org/freedesktop/portal/desktop/request/{sender}/{token}"
    ))
}

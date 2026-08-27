//! Probes that the environment the daemon is actually running in can drive autoswitch.
//!
//! Unit tests cover the decision logic. These checks cover the Flatpak sandbox: sysfs,
//! udev, D-Bus names, the display backend, and the config overlay. `powerdisplayd
//! --self-test` prints them; `build-aux/test-sandbox.sh` runs that inside the installed
//! app.

use std::path::Path;

use crate::config;
use crate::display;
use crate::power::{self, source};
use crate::sandboxed;
use crate::watch::{UdevSource, probe_monitor};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub required: bool,
    pub ok: bool,
    pub detail: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn passed(&self) -> bool {
        self.checks.iter().all(|c| c.ok || !c.required)
    }

    pub fn failed_required(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|c| c.required && !c.ok)
    }

    fn push(&mut self, name: &'static str, required: bool, ok: bool, detail: impl Into<String>) {
        self.checks.push(Check {
            name,
            required,
            ok,
            detail: detail.into(),
        });
    }
}

/// Runs every probe against the current process environment.
pub fn probe() -> Report {
    let mut report = Report::default();
    check_sandbox_marker(&mut report);
    check_config_location(&mut report);
    check_sysfs_power(&mut report);
    check_udev(&mut report, "udev userspace", UdevSource::Userspace, false);
    check_udev(&mut report, "udev kernel uevents", UdevSource::Kernel, false);
    check_display_backend(&mut report);
    check_power_profiles(&mut report);
    check_run_udev(&mut report);
    report
}

fn check_sandbox_marker(report: &mut Report) {
    let inside = sandboxed();
    let detail = if inside {
        "running inside a Flatpak (/.flatpak-info present)".to_string()
    } else {
        "not a Flatpak; host tools and paths apply".to_string()
    };
    // Informative: autoswitch has to work in both places.
    report.push("sandbox", false, true, detail);
}

fn check_config_location(report: &mut Report) {
    match config::config_path() {
        Ok(path) => {
            let inside = sandboxed();
            let overlaid = path_is_flatpak_overlay(&path);
            let ok = !inside || overlaid;
            let detail = if inside && !overlaid {
                format!(
                    "{} is not under the Flatpak XDG overlay; the GUI and daemon would not share settings",
                    path.display()
                )
            } else {
                path.display().to_string()
            };
            report.push("config path", inside, ok, detail);
        }
        Err(err) => report.push("config path", true, false, format!("{err:#}")),
    }
}

pub(crate) fn path_is_flatpak_overlay(path: &Path) -> bool {
    path.components().any(|c| c.as_os_str() == ".var")
        && path
            .to_string_lossy()
            .contains("io.github.Emanuel4100.PowerDisplay")
}

fn check_sysfs_power(report: &mut Report) {
    let root = Path::new("/sys/class/power_supply");
    if !root.is_dir() {
        report.push(
            "sysfs power_supply",
            true,
            false,
            "/sys/class/power_supply is missing; charger events cannot be read",
        );
        return;
    }

    let supplies = source::read_supplies_at(root);
    if supplies.is_empty() {
        report.push(
            "sysfs power_supply",
            true,
            false,
            "directory is there but no supply could be read (sandbox hiding attributes?)",
        );
        return;
    }

    let state = source::classify(&supplies);
    let listed: Vec<String> = supplies
        .iter()
        .map(|s| {
            format!(
                "{} online={:?} status={:?}",
                s.kind,
                s.online,
                s.status.as_deref().unwrap_or("")
            )
        })
        .collect();
    report.push(
        "sysfs power_supply",
        true,
        true,
        format!("{} → {}; {}", state.label(), listed.len(), listed.join(", ")),
    );
}

fn check_udev(report: &mut Report, name: &'static str, source: UdevSource, required: bool) {
    match probe_monitor(source) {
        Ok(()) => report.push(name, required, true, format!("{source:?} monitor accepted a socket")),
        Err(err) => report.push(name, required, false, format!("{err:#}")),
    }
}

fn check_display_backend(report: &mut Report) {
    match display::detect() {
        Ok(backend) => match backend.outputs() {
            Ok(outputs) => {
                let names: Vec<String> = outputs
                    .iter()
                    .map(|o| format!("{}{}", o.connector, if o.enabled { "" } else { " (off)" }))
                    .collect();
                report.push(
                    "display backend",
                    true,
                    !outputs.is_empty(),
                    format!("{}: {}", backend.name(), names.join(", ")),
                );
            }
            Err(err) => report.push(
                "display backend",
                true,
                false,
                format!("{} connected but listing failed: {err:#}", backend.name()),
            ),
        },
        Err(err) => report.push("display backend", true, false, format!("{err:#}")),
    }
}

fn check_power_profiles(report: &mut Report) {
    match power::PowerProfiles::connect() {
        Some(profiles) => {
            let active = profiles.active().unwrap_or_default();
            let available = profiles.available().unwrap_or_default().join(", ");
            report.push(
                "power profiles",
                false,
                true,
                format!("{} active={active} available=[{available}]", profiles.service_name()),
            );
        }
        None => report.push(
            "power profiles",
            false,
            true,
            "none on the system bus (display switching still works)",
        ),
    }
}

fn check_run_udev(report: &mut Report) {
    let ok = Path::new("/run/udev").is_dir();
    report.push(
        "/run/udev",
        false,
        ok,
        if ok {
            "present; userspace udev can look up devices"
        } else {
            "missing; kernel uevents plus sysfs polling have to cover charger detection"
        },
    );
}

/// Text report used by `powerdisplayd --self-test`.
pub fn format_report(report: &Report) -> String {
    let mut out = String::new();
    for check in &report.checks {
        let mark = if check.ok {
            "ok"
        } else if check.required {
            "FAIL"
        } else {
            "warn"
        };
        out.push_str(&format!("[{mark:>4}] {}: {}\n", check.name, check.detail));
    }
    if report.passed() {
        out.push_str("self-test passed\n");
    } else {
        out.push_str("self-test failed\n");
    }
    out
}

/// Permissions the Flatpak manifest must grant for autoswitch. Used by the sandbox
/// script, and by a unit test so the manifest cannot drop one of them unnoticed.
pub const REQUIRED_FLATPAK_FINISH_ARGS: &[&str] = &[
    "--share=network",
    "--filesystem=/run/udev:ro",
    "--filesystem=xdg-config/autostart:create",
    "--talk-name=org.gnome.Mutter.DisplayConfig",
    "--system-talk-name=org.freedesktop.UPower.PowerProfiles",
    "--system-talk-name=net.hadess.PowerProfiles",
    "--system-talk-name=org.freedesktop.login1",
    "--socket=wayland",
];

pub fn missing_finish_args(manifest: &str) -> Vec<&'static str> {
    REQUIRED_FLATPAK_FINISH_ARGS
        .iter()
        .copied()
        .filter(|arg| !manifest.contains(arg))
        .collect()
}

/// Whether `flatpak info --show-permissions` output still has the bits autoswitch needs.
pub fn missing_installed_permissions(text: &str) -> Vec<&'static str> {
    let mut missing = Vec::new();
    if !text.contains("shared=") || !text.contains("network") {
        missing.push("share=network (udev netlink lives in the host network namespace)");
    }
    if !text.contains("/run/udev") {
        missing.push("filesystem=/run/udev:ro");
    }
    if !text.contains("xdg-config/autostart") {
        missing.push("filesystem=xdg-config/autostart");
    }
    if !text.contains("org.gnome.Mutter.DisplayConfig") {
        missing.push("talk-name=org.gnome.Mutter.DisplayConfig");
    }
    if !text.contains("org.freedesktop.UPower.PowerProfiles")
        && !text.contains("net.hadess.PowerProfiles")
    {
        missing.push("system-talk-name for power-profiles-daemon");
    }
    missing
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_flatpak_manifest_keeps_the_permissions_autoswitch_needs() {
        let manifest = include_str!("../../../build-aux/io.github.Emanuel4100.PowerDisplay.yml");
        let missing = missing_finish_args(manifest);
        assert!(
            missing.is_empty(),
            "Flatpak manifest dropped permissions required for autoswitch: {missing:?}"
        );
    }

    #[test]
    fn installed_permission_parser_accepts_a_healthy_dump() {
        let text = "\
[Context]
shared=ipc;network;
sockets=fallback-x11;wayland;
devices=dri;
filesystems=/run/udev:ro;xdg-config/autostart;

[Session Bus Policy]
org.gnome.Mutter.DisplayConfig=talk

[System Bus Policy]
org.freedesktop.UPower.PowerProfiles=talk
org.freedesktop.login1=talk
";
        assert!(missing_installed_permissions(text).is_empty());
    }

    #[test]
    fn installed_permission_parser_catches_a_missing_network_share() {
        let text = "\
[Context]
shared=ipc;
filesystems=/run/udev:ro;
";
        let missing = missing_installed_permissions(text);
        assert!(missing.iter().any(|m| m.contains("network")), "{missing:?}");
    }

    #[test]
    fn flatpak_overlay_paths_are_recognised() {
        let path = PathBuf::from(
            "/home/u/.var/app/io.github.Emanuel4100.PowerDisplay/config/powerdisplay/config.toml",
        );
        assert!(path_is_flatpak_overlay(&path));
        assert!(!path_is_flatpak_overlay(Path::new(
            "/home/u/.config/powerdisplay/config.toml"
        )));
    }

    #[test]
    fn sysfs_probe_sees_this_machine() {
        // Runs on the host during `cargo test`. Inside the Flatpak, `--self-test` is the
        // same function against the sandboxed view of /sys.
        if !Path::new("/sys/class/power_supply").is_dir() {
            return;
        }
        let mut report = Report::default();
        check_sysfs_power(&mut report);
        assert!(report.checks[0].ok, "{:?}", report.checks[0]);
    }
}

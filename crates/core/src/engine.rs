//! Turns "we are now on battery" into the calls that make it so.

use anyhow::{Context, Result};

use crate::config::{Config, PowerState};
use crate::display::{self, DisplayBackend, Output, resolve_settings};
use crate::power::PowerProfiles;

/// What an apply actually did, kept separate from the logging so the GUI can show it too.
#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    pub actions: Vec<String>,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl ApplyReport {
    pub fn succeeded(&self) -> bool {
        self.errors.is_empty()
    }

    /// A single line suitable for a status bar or a log entry.
    pub fn summary(&self) -> String {
        if !self.errors.is_empty() {
            return self.errors.join("; ");
        }
        if self.actions.is_empty() {
            return match self.warnings.first() {
                Some(warning) => warning.clone(),
                None => "Nothing to change".to_string(),
            };
        }
        self.actions.join("; ")
    }
}

pub struct Engine {
    backend: Box<dyn DisplayBackend>,
    profiles: Option<PowerProfiles>,
    dry_run: bool,
}

impl Engine {
    pub fn new(dry_run: bool) -> Result<Self> {
        Ok(Self {
            backend: display::detect().context("finding a way to talk to this desktop")?,
            profiles: PowerProfiles::connect(),
            dry_run,
        })
    }

    /// Builds an engine around a given backend, without probing the session or the bus.
    #[cfg(test)]
    fn with_backend(backend: Box<dyn DisplayBackend>, dry_run: bool) -> Self {
        Self {
            backend,
            profiles: None,
            dry_run,
        }
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    pub fn supports_persist(&self) -> bool {
        self.backend.supports_persist()
    }

    pub fn outputs(&self) -> Result<Vec<Output>> {
        self.backend.outputs()
    }

    /// Profile names offered by the power-profiles service, empty when there is none.
    pub fn power_profiles(&self) -> Vec<String> {
        self.profiles
            .as_ref()
            .and_then(|profiles| profiles.available().ok())
            .unwrap_or_default()
    }

    pub fn active_power_profile(&self) -> Option<String> {
        self.profiles.as_ref()?.active().ok()
    }

    pub fn power_profile_service(&self) -> Option<&'static str> {
        self.profiles.as_ref().map(PowerProfiles::service_name)
    }

    /// Applies the profile for `state`.
    ///
    /// Failures are collected rather than propagated: an unplugged dock should not stop
    /// the laptop panel from being switched, and a missing power-profiles daemon should
    /// not stop the resolution change.
    pub fn apply(&self, config: &Config, state: PowerState) -> ApplyReport {
        let mut report = ApplyReport::default();

        if !config.enabled {
            report.warnings.push("powerdisplay is turned off".into());
            return report;
        }

        let profile = config.profile(state);
        self.apply_displays(profile, &mut report);
        self.apply_power_profile(profile, &mut report);
        report
    }

    fn apply_displays(&self, profile: &crate::config::Profile, report: &mut ApplyReport) {
        if profile.outputs.iter().all(|rule| rule.mode.is_none()) {
            return;
        }

        let outputs = match self.backend.outputs() {
            Ok(outputs) => outputs,
            Err(err) => {
                report.errors.push(format!("could not list displays: {err:#}"));
                return;
            }
        };

        let (settings, warnings) = resolve_settings(&profile.outputs, &outputs);
        report.warnings.extend(warnings);

        if settings.is_empty() {
            return;
        }

        let described: Vec<String> = settings
            .iter()
            .map(|setting| format!("{} to {}", setting.connector, setting.mode_id))
            .collect();

        if self.dry_run {
            report.actions.push(format!("would set {}", described.join(", ")));
            return;
        }

        let persist = profile.persist_display_config && self.backend.supports_persist();
        match self.backend.apply(&settings, persist) {
            Ok(()) => report.actions.push(format!("set {}", described.join(", "))),
            Err(err) => report.errors.push(format!("display change failed: {err:#}")),
        }
    }

    fn apply_power_profile(&self, profile: &crate::config::Profile, report: &mut ApplyReport) {
        let Some(wanted) = profile.power_profile.as_deref().filter(|p| !p.is_empty()) else {
            return;
        };

        let Some(profiles) = self.profiles.as_ref() else {
            report
                .warnings
                .push(format!("no power profiles service, cannot select {wanted}"));
            return;
        };

        if self.dry_run {
            report.actions.push(format!("would select the {wanted} power profile"));
            return;
        }

        match profiles.ensure_active(wanted) {
            Ok(true) => report.actions.push(format!("selected the {wanted} power profile")),
            Ok(false) => {}
            Err(err) => report.errors.push(format!("power profile change failed: {err:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::config::{OutputMatch, OutputRule, Profile};
    use crate::display::{OutputSetting, test_output};

    /// What the stub backend was asked to do, kept behind a handle so the test can still
    /// read it after the backend has been handed to the engine.
    type ApplyLog = Arc<Mutex<Vec<(Vec<OutputSetting>, bool)>>>;

    /// Stands in for a compositor, and can be told to refuse so the failure paths are
    /// exercised too.
    struct StubBackend {
        outputs: Vec<Output>,
        refuse: bool,
        applied: ApplyLog,
    }

    impl StubBackend {
        fn new(outputs: Vec<Output>) -> Self {
            Self {
                outputs,
                refuse: false,
                applied: ApplyLog::default(),
            }
        }
    }

    impl DisplayBackend for StubBackend {
        fn name(&self) -> &'static str {
            "stub"
        }

        fn outputs(&self) -> Result<Vec<Output>> {
            if self.refuse {
                anyhow::bail!("no");
            }
            Ok(self.outputs.clone())
        }

        fn apply(&self, wanted: &[OutputSetting], persist: bool) -> Result<()> {
            self.applied
                .lock()
                .unwrap()
                .push((wanted.to_vec(), persist));
            if self.refuse {
                anyhow::bail!("the compositor rejected the configuration");
            }
            Ok(())
        }

        fn supports_persist(&self) -> bool {
            true
        }
    }

    fn rule(connector: &str, mode: &str) -> OutputRule {
        OutputRule {
            matcher: OutputMatch {
                connector: Some(connector.into()),
                ..Default::default()
            },
            mode: Some(mode.into()),
        }
    }

    fn config_with(profile: Profile) -> Config {
        Config {
            on_battery: profile,
            ..Default::default()
        }
    }

    fn laptop() -> Vec<Output> {
        vec![test_output(
            "eDP-1",
            &[(1920, 1080, 60.0), (2880, 1800, 120.0)],
        )]
    }

    /// An engine over a stub panel, plus a handle on what the stub was told to do.
    fn engine_with_laptop(dry_run: bool) -> (Engine, ApplyLog) {
        let backend = StubBackend::new(laptop());
        let log = backend.applied.clone();
        (Engine::with_backend(Box::new(backend), dry_run), log)
    }

    #[test]
    fn a_matching_rule_reaches_the_backend() {
        let (engine, log) = engine_with_laptop(false);

        let config = config_with(Profile {
            outputs: vec![rule("eDP-1", "1920x1080@60.000")],
            ..Default::default()
        });
        let report = engine.apply(&config, PowerState::Battery);

        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.actions, vec!["set eDP-1 to 1920x1080@60.000"]);

        let calls = log.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0[0].mode_id, "1920x1080@60.000");
    }

    #[test]
    fn the_master_switch_stops_everything() {
        let engine = Engine::with_backend(Box::new(StubBackend::new(laptop())), false);
        let config = Config {
            enabled: false,
            on_battery: Profile {
                outputs: vec![rule("eDP-1", "1920x1080@60.000")],
                ..Default::default()
            },
            ..Default::default()
        };

        let report = engine.apply(&config, PowerState::Battery);
        assert!(report.actions.is_empty());
        assert_eq!(report.warnings, vec!["powerdisplay is turned off"]);
    }

    /// A dry run has to be genuinely inert, not merely quiet.
    #[test]
    fn a_dry_run_changes_nothing() {
        let (engine, log) = engine_with_laptop(true);

        let config = config_with(Profile {
            outputs: vec![rule("eDP-1", "2880x1800@120.000")],
            ..Default::default()
        });
        let report = engine.apply(&config, PowerState::Battery);

        assert_eq!(report.actions, vec!["would set eDP-1 to 2880x1800@120.000"]);
        assert!(log.lock().unwrap().is_empty());
    }

    #[test]
    fn an_unplugged_monitor_does_not_hold_back_the_laptop_panel() {
        let (engine, _) = engine_with_laptop(false);
        let config = config_with(Profile {
            outputs: vec![
                rule("DP-3", "3840x2160@60.000"),
                rule("eDP-1", "1920x1080@60.000"),
            ],
            ..Default::default()
        });

        let report = engine.apply(&config, PowerState::Battery);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.actions, vec!["set eDP-1 to 1920x1080@60.000"]);
        assert_eq!(report.warnings.len(), 1);
    }

    #[test]
    fn a_mode_the_panel_cannot_do_is_reported_not_applied() {
        let (engine, _) = engine_with_laptop(false);
        let config = config_with(Profile {
            outputs: vec![rule("eDP-1", "5120x2880@60.000")],
            ..Default::default()
        });

        let report = engine.apply(&config, PowerState::Battery);
        assert!(report.actions.is_empty());
        assert_eq!(
            report.warnings,
            vec!["eDP-1 cannot do 5120x2880@60.000; leaving it unchanged"]
        );
    }

    #[test]
    fn a_refusing_compositor_becomes_an_error() {
        let mut stub = StubBackend::new(laptop());
        stub.refuse = true;
        let engine = Engine::with_backend(Box::new(stub), false);

        let config = config_with(Profile {
            outputs: vec![rule("eDP-1", "1920x1080@60.000")],
            ..Default::default()
        });
        let report = engine.apply(&config, PowerState::Battery);

        assert!(!report.succeeded());
        assert!(report.errors[0].contains("could not list displays"), "{report:?}");
    }

    /// Without a profiles service the resolution half still has to go through.
    #[test]
    fn a_missing_power_profiles_service_only_warns() {
        let (engine, _) = engine_with_laptop(false);
        let config = config_with(Profile {
            power_profile: Some("power-saver".into()),
            outputs: vec![rule("eDP-1", "1920x1080@60.000")],
            ..Default::default()
        });

        let report = engine.apply(&config, PowerState::Battery);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.actions, vec!["set eDP-1 to 1920x1080@60.000"]);
        assert_eq!(
            report.warnings,
            vec!["no power profiles service, cannot select power-saver"]
        );
    }

    #[test]
    fn the_persist_flag_is_passed_through() {
        let (engine, log) = engine_with_laptop(false);

        let config = config_with(Profile {
            persist_display_config: true,
            outputs: vec![rule("eDP-1", "1920x1080@60.000")],
            ..Default::default()
        });
        engine.apply(&config, PowerState::Battery);

        assert!(log.lock().unwrap()[0].1);
    }

    #[test]
    fn each_power_state_uses_its_own_profile() {
        let (engine, _) = engine_with_laptop(true);
        let config = Config {
            on_battery: Profile {
                outputs: vec![rule("eDP-1", "1920x1080@60.000")],
                ..Default::default()
            },
            on_ac: Profile {
                outputs: vec![rule("eDP-1", "2880x1800@120.000")],
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            engine.apply(&config, PowerState::Battery).actions,
            vec!["would set eDP-1 to 1920x1080@60.000"]
        );
        assert_eq!(
            engine.apply(&config, PowerState::Ac).actions,
            vec!["would set eDP-1 to 2880x1800@120.000"]
        );
    }
}

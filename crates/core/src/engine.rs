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

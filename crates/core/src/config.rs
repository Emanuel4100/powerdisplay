use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::time::Duration;

use anyhow::{Context, Result};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};

pub const APP_ID: &str = "powerdisplay";
pub const CONFIG_VERSION: u32 = 1;

/// Whether the machine is currently running off the battery or the wall.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerState {
    Battery,
    Ac,
}

impl PowerState {
    pub fn label(self) -> &'static str {
        match self {
            PowerState::Battery => "On battery",
            PowerState::Ac => "Plugged in",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    /// Master switch. When false the daemon observes but never acts.
    pub enabled: bool,
    /// Apply the matching profile once at daemon start-up.
    pub apply_on_start: bool,
    pub on_battery: Profile,
    pub on_ac: Profile,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: CONFIG_VERSION,
            enabled: true,
            apply_on_start: true,
            on_battery: Profile::default(),
            on_ac: Profile::default(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    /// Name of a power-profiles-daemon profile, e.g. "power-saver". `None` leaves it alone.
    pub power_profile: Option<String>,
    /// Ask the desktop to remember this layout instead of applying it temporarily.
    pub persist_display_config: bool,
    pub outputs: Vec<OutputRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputRule {
    #[serde(rename = "match")]
    pub matcher: OutputMatch,
    /// Mode id in `WIDTHxHEIGHT@REFRESH` form. `None` leaves the output alone.
    pub mode: Option<String>,
}

/// Identifies a physical display. `make`/`model`/`serial` survive replugging into a
/// different port; `connector` is the fallback for panels that report no EDID strings.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputMatch {
    pub connector: Option<String>,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
}

impl OutputMatch {
    fn field_matches(expected: &Option<String>, actual: &str) -> Option<bool> {
        let expected = expected.as_deref()?.trim();
        if expected.is_empty() {
            return None;
        }
        Some(expected.eq_ignore_ascii_case(actual.trim()))
    }

    /// Higher is a better match; `None` means this rule does not describe the output at all.
    ///
    /// EDID fields are weighted far above the connector so that a monitor moved from
    /// `DP-1` to `HDMI-A-1` still wins over an unrelated panel that happens to sit on `DP-1`.
    pub fn score(&self, connector: &str, make: &str, model: &str, serial: &str) -> Option<u32> {
        let checks = [
            (Self::field_matches(&self.serial, serial), 8),
            (Self::field_matches(&self.model, model), 4),
            (Self::field_matches(&self.make, make), 2),
            (Self::field_matches(&self.connector, connector), 1),
        ];

        let mut score = 0;
        let mut compared = 0;
        for (result, weight) in checks {
            match result {
                Some(true) => {
                    score += weight;
                    compared += 1;
                }
                Some(false) => return None,
                None => {}
            }
        }

        (compared > 0).then_some(score)
    }
}

impl Config {
    pub fn profile(&self, state: PowerState) -> &Profile {
        match state {
            PowerState::Battery => &self.on_battery,
            PowerState::Ac => &self.on_ac,
        }
    }

    pub fn profile_mut(&mut self, state: PowerState) -> &mut Profile {
        match state {
            PowerState::Battery => &mut self.on_battery,
            PowerState::Ac => &mut self.on_ac,
        }
    }

    /// Reads the config, falling back to defaults when the file does not exist yet.
    pub fn load() -> Result<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&config_path()?)
    }

    /// Writes via a temporary file so a crash mid-write cannot leave a truncated config
    /// behind, and so the daemon's watcher sees one atomic change instead of two.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string_pretty(self).context("serialising config")?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
        Ok(())
    }
}

pub fn config_dir() -> Result<PathBuf> {
    let base = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let home = std::env::var_os("HOME").context("neither XDG_CONFIG_HOME nor HOME is set")?;
            PathBuf::from(home).join(".config")
        }
    };
    Ok(base.join(APP_ID))
}

pub fn config_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("config.toml"))
}

/// Watches the config directory and sends `()` whenever the config file changes.
///
/// The directory rather than the file is watched because saving replaces the file,
/// which would otherwise leave an inode watch pointing at the old copy.
pub struct ConfigWatcher {
    _watcher: RecommendedWatcher,
}

impl ConfigWatcher {
    pub fn new(tx: Sender<()>) -> Result<Self> {
        let path = config_path()?;
        let dir = config_dir()?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

        let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(event) = event else { return };

            // Access events must be ignored: inotify reports our own reads of the file,
            // and reacting to them means reloading forever.
            let changed = matches!(
                event.kind,
                EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
            );

            if changed && event.paths.iter().any(|p| p == &path) {
                let _ = tx.send(());
            }
        })
        .context("creating config watcher")?;

        watcher
            .watch(&dir, RecursiveMode::NonRecursive)
            .with_context(|| format!("watching {}", dir.display()))?;

        Ok(Self { _watcher: watcher })
    }
}

/// Editors and atomic renames produce bursts of events; callers use this to settle.
pub const CONFIG_DEBOUNCE: Duration = Duration::from_millis(300);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let text = toml::to_string_pretty(&Config::default()).unwrap();
        let parsed: Config = toml::from_str(&text).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.version, CONFIG_VERSION);
        assert!(parsed.on_ac.outputs.is_empty());
    }

    #[test]
    fn missing_fields_fall_back_to_defaults() {
        let parsed: Config = toml::from_str("version = 1\n[on_ac]\n").unwrap();
        assert!(parsed.enabled);
        assert!(parsed.on_ac.power_profile.is_none());
    }

    #[test]
    fn edid_match_beats_connector_match() {
        let by_serial = OutputMatch {
            serial: Some("ABC123".into()),
            ..Default::default()
        };
        let by_connector = OutputMatch {
            connector: Some("DP-1".into()),
            ..Default::default()
        };

        let serial_score = by_serial.score("HDMI-A-1", "Dell", "U2720Q", "ABC123").unwrap();
        let connector_score = by_connector.score("DP-1", "Other", "Panel", "XYZ").unwrap();
        assert!(serial_score > connector_score);
    }

    #[test]
    fn a_single_wrong_field_rejects_the_rule() {
        let matcher = OutputMatch {
            connector: Some("eDP-1".into()),
            serial: Some("ABC123".into()),
            ..Default::default()
        };
        assert!(matcher.score("eDP-1", "", "", "DIFFERENT").is_none());
        assert_eq!(matcher.score("eDP-1", "", "", "abc123"), Some(9));
    }

    #[test]
    fn an_empty_matcher_never_matches() {
        assert!(OutputMatch::default().score("eDP-1", "", "", "").is_none());
    }
}

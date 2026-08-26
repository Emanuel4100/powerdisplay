//! KDE Plasma backend, driving `kscreen-doctor`.
//!
//! Plasma keeps its display state in KScreen rather than in the compositor alone, so
//! going through its own tool is what makes a change stick instead of being reverted on
//! the next hotplug. `kscreen-doctor` ships with Plasma itself, so there is nothing extra
//! for the user to install.

use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::{DisplayBackend, Mode, Output, OutputSetting, format_mode_id};

const TOOL: &str = "kscreen-doctor";

/// Builds an invocation of `kscreen-doctor`.
///
/// The tool belongs to Plasma and is therefore on the host, not in our sandbox, so inside
/// a Flatpak the call has to be handed out through `flatpak-spawn`. Everywhere else this is
/// a plain exec.
fn tool() -> Command {
    if crate::sandboxed() {
        let mut command = Command::new("flatpak-spawn");
        command.arg("--host").arg(TOOL);
        command
    } else {
        Command::new(TOOL)
    }
}

pub struct KdeBackend {
    _private: (),
}

impl KdeBackend {
    pub fn new() -> Result<Self> {
        let backend = Self { _private: () };
        backend
            .query()
            .with_context(|| format!("{TOOL} is not usable in this session"))?;
        Ok(backend)
    }

    fn query(&self) -> Result<Vec<KdeOutput>> {
        let output = tool()
            .arg("-j")
            .output()
            .with_context(|| format!("running {TOOL}"))?;

        if !output.status.success() {
            bail!(
                "{TOOL} -j failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }

        parse_outputs(&String::from_utf8_lossy(&output.stdout))
    }
}

impl DisplayBackend for KdeBackend {
    fn name(&self) -> &'static str {
        "KDE Plasma (kscreen)"
    }

    fn supports_persist(&self) -> bool {
        // KScreen always remembers the layout it applied; there is no temporary mode.
        false
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        Ok(self
            .query()?
            .into_iter()
            .filter(|output| output.connected)
            .map(KdeOutput::into_output)
            .collect())
    }

    fn apply(&self, wanted: &[OutputSetting], _persist: bool) -> Result<()> {
        if wanted.is_empty() {
            return Ok(());
        }

        let outputs = self.query()?;
        let mut args = Vec::new();

        for setting in wanted {
            let Some(output) = outputs.iter().find(|o| o.name == setting.connector) else {
                tracing::warn!(connector = %setting.connector, "kscreen no longer lists this display");
                continue;
            };
            let Some(mode) = output
                .modes
                .iter()
                .find(|mode| mode.mode_id() == setting.mode_id)
            else {
                tracing::warn!(
                    connector = %setting.connector,
                    mode = %setting.mode_id,
                    "kscreen does not offer this mode"
                );
                continue;
            };

            args.push(format!("output.{}.mode.{}", output.name, mode.id));
        }

        if args.is_empty() {
            bail!("none of the requested modes are available");
        }

        // One invocation so KScreen validates and commits the whole layout at once.
        let result = tool()
            .args(&args)
            .output()
            .with_context(|| format!("running {TOOL}"))?;

        if !result.status.success() {
            bail!(
                "{TOOL} {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&result.stderr).trim()
            );
        }

        Ok(())
    }
}

fn parse_outputs(json: &str) -> Result<Vec<KdeOutput>> {
    let reply: KdeReply = serde_json::from_str(json).context("parsing kscreen-doctor JSON")?;
    Ok(reply.outputs)
}

#[derive(Debug, Deserialize)]
struct KdeReply {
    #[serde(default)]
    outputs: Vec<KdeOutput>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KdeOutput {
    name: String,
    #[serde(default = "default_true")]
    connected: bool,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    current_mode_id: Option<String>,
    #[serde(default)]
    modes: Vec<KdeMode>,
    #[serde(default, alias = "manufacturer")]
    vendor: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default, alias = "serialNumber")]
    serial: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct KdeMode {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    refresh_rate: Option<f64>,
    #[serde(default, rename = "size")]
    size: Option<KdeSize>,
}

#[derive(Debug, Deserialize)]
struct KdeSize {
    width: u32,
    height: u32,
}

impl KdeMode {
    /// kscreen reports the geometry either as a `size` object or inside the mode name
    /// (`2880x1800@90`), depending on the Plasma version.
    fn geometry(&self) -> Option<(u32, u32, f64)> {
        let (width, height) = match &self.size {
            Some(size) => (size.width, size.height),
            None => {
                let name = self.name.as_deref()?;
                let resolution = name.split('@').next()?;
                let (width, height) = resolution.split_once('x')?;
                (width.trim().parse().ok()?, height.trim().parse().ok()?)
            }
        };

        let refresh = match self.refresh_rate {
            Some(refresh) => refresh,
            None => self
                .name
                .as_deref()
                .and_then(|name| name.split_once('@'))
                .and_then(|(_, refresh)| refresh.trim().parse().ok())?,
        };

        Some((width, height, refresh))
    }

    fn mode_id(&self) -> String {
        match self.geometry() {
            Some((width, height, refresh)) => format_mode_id(width, height, refresh),
            None => self.id.clone(),
        }
    }
}

impl KdeOutput {
    fn into_output(self) -> Output {
        let current_mode = self.current_mode_id.as_deref().and_then(|current| {
            self.modes
                .iter()
                .find(|mode| mode.id == current)
                .map(KdeMode::mode_id)
        });

        let modes = self
            .modes
            .iter()
            .filter_map(|mode| {
                let (width, height, refresh) = mode.geometry()?;
                Some(Mode {
                    id: mode.mode_id(),
                    width,
                    height,
                    refresh,
                    preferred: false,
                    current: self.current_mode_id.as_deref() == Some(mode.id.as_str()),
                })
            })
            .collect();

        Output {
            connector: self.name,
            make: self.vendor.unwrap_or_default(),
            model: self.model.unwrap_or_default(),
            serial: self.serial.unwrap_or_default(),
            modes,
            current_mode,
            enabled: self.enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "outputs": [
        {
          "id": 79,
          "name": "eDP-1",
          "connected": true,
          "enabled": true,
          "currentModeId": "72",
          "vendor": "Sharp",
          "model": "LQ140M1",
          "serialNumber": "0x00000000",
          "modes": [
            { "id": "72", "name": "2880x1800@90", "refreshRate": 90.0009,
              "size": { "width": 2880, "height": 1800 } },
            { "id": "73", "name": "1920x1080@60", "refreshRate": 59.9997,
              "size": { "width": 1920, "height": 1080 } }
          ]
        },
        {
          "id": 80,
          "name": "DP-2",
          "connected": false,
          "enabled": false,
          "modes": []
        }
      ]
    }"#;

    #[test]
    fn disconnected_outputs_are_dropped() {
        let backend_outputs: Vec<Output> = parse_outputs(SAMPLE)
            .unwrap()
            .into_iter()
            .filter(|o| o.connected)
            .map(KdeOutput::into_output)
            .collect();
        assert_eq!(backend_outputs.len(), 1);
        assert_eq!(backend_outputs[0].connector, "eDP-1");
    }

    #[test]
    fn modes_and_edid_details_are_carried_over() {
        let output = parse_outputs(SAMPLE)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .into_output();

        assert_eq!(output.make, "Sharp");
        assert_eq!(output.model, "LQ140M1");
        assert_eq!(output.modes.len(), 2);
        assert_eq!(output.current_mode.as_deref(), Some("2880x1800@90.001"));
        assert_eq!(output.modes[1].id, "1920x1080@60.000");
    }

    #[test]
    fn geometry_falls_back_to_the_mode_name() {
        let mode = KdeMode {
            id: "3".into(),
            name: Some("1280x720@59.94".into()),
            refresh_rate: None,
            size: None,
        };
        assert_eq!(mode.geometry(), Some((1280, 720, 59.94)));
    }

    #[test]
    fn a_config_written_elsewhere_still_resolves() {
        let output = parse_outputs(SAMPLE)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .into_output();

        // Mutter would have written this id for the same physical mode.
        let mode = output.resolve_mode("1920x1080@59.999996185302734").unwrap();
        assert_eq!(mode.id, "1920x1080@60.000");
    }
}

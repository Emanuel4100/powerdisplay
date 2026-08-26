pub mod gnome;
pub mod kde;
pub mod wlroots;
pub mod x11;

use anyhow::{Result, anyhow, bail};

use crate::config::OutputMatch;

/// A video mode. `id` is the stable key stored in the config file and is always
/// `WIDTHxHEIGHT@REFRESH`; for Mutter it is additionally the opaque token that must be
/// handed straight back to `ApplyMonitorsConfig`, so it is never reconstructed by hand.
#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub id: String,
    pub width: u32,
    pub height: u32,
    pub refresh: f64,
    pub preferred: bool,
    pub current: bool,
}

impl Mode {
    pub fn resolution(&self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    pub fn refresh_label(&self) -> String {
        format!("{:.2} Hz", self.refresh)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Output {
    pub connector: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    pub modes: Vec<Mode>,
    pub current_mode: Option<String>,
    pub enabled: bool,
}

impl Output {
    /// What to show in the UI: the panel's name when the EDID gives us one.
    pub fn display_name(&self) -> String {
        let model = self.model.trim();
        let make = self.make.trim();
        match (make.is_empty(), model.is_empty()) {
            (_, true) => self.connector.clone(),
            (true, false) => format!("{} ({})", model, self.connector),
            (false, false) => format!("{make} {model} ({})", self.connector),
        }
    }

    pub fn matcher(&self) -> OutputMatch {
        let non_empty = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        OutputMatch {
            connector: Some(self.connector.clone()),
            make: non_empty(&self.make),
            model: non_empty(&self.model),
            serial: non_empty(&self.serial),
        }
    }

    /// Finds the mode a config entry refers to.
    ///
    /// Exact id first, then the same resolution at the closest refresh rate. The fallback
    /// matters because backends report refresh rates with differing precision (Mutter says
    /// `59.810825347900391`, kscreen says `59.81`) and because a config written against one
    /// desktop should still work when the user logs into another.
    pub fn resolve_mode(&self, wanted: &str) -> Option<&Mode> {
        if let Some(exact) = self.modes.iter().find(|m| m.id == wanted) {
            return Some(exact);
        }

        let (width, height, refresh) = parse_mode_id(wanted)?;
        let mut candidates: Vec<&Mode> = self
            .modes
            .iter()
            .filter(|m| m.width == width && m.height == height)
            .collect();
        candidates.sort_by(|a, b| {
            (a.refresh - refresh)
                .abs()
                .total_cmp(&(b.refresh - refresh).abs())
        });
        candidates.into_iter().next()
    }

    /// Modes with the variant duplicates collapsed away, for populating menus.
    ///
    /// Mutter lists a `+vrr` twin of every mode, and offering the user two entries that
    /// read identically is worse than quietly picking the plain one.
    pub fn distinct_modes(&self) -> Vec<&Mode> {
        let mut seen: Vec<(u32, u32, i64)> = Vec::new();
        let mut result = Vec::new();

        for mode in &self.modes {
            let key = (mode.width, mode.height, (mode.refresh * 100.0).round() as i64);
            if seen.contains(&key) {
                continue;
            }
            seen.push(key);
            result.push(mode);
        }

        result
    }
}

/// Splits `WIDTHxHEIGHT@REFRESH` apart, ignoring any trailing variant marker.
///
/// Mutter appends flags to mode ids, so the same mode shows up as both
/// `2880x1800@120.000` and `2880x1800@120.000+vrr`.
pub fn parse_mode_id(id: &str) -> Option<(u32, u32, f64)> {
    let (resolution, rest) = id.split_once('@')?;
    let (width, height) = resolution.split_once('x')?;
    let refresh_len = rest
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(rest.len());

    Some((
        width.trim().parse().ok()?,
        height.trim().parse().ok()?,
        rest[..refresh_len].parse().ok()?,
    ))
}

pub fn format_mode_id(width: u32, height: u32, refresh: f64) -> String {
    format!("{width}x{height}@{refresh:.3}")
}

/// One resolved instruction: put `connector` into the mode named `mode_id`.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputSetting {
    pub connector: String,
    pub mode_id: String,
}

pub trait DisplayBackend: Send {
    fn name(&self) -> &'static str;

    /// Everything currently connected, whether enabled or not.
    fn outputs(&self) -> Result<Vec<Output>>;

    /// Applies the given modes. Outputs not mentioned keep their current mode.
    ///
    /// `persist` asks the desktop to remember the layout; backends that have no such
    /// concept ignore it.
    fn apply(&self, wanted: &[OutputSetting], persist: bool) -> Result<()>;

    /// Whether `persist` means anything for this backend, so the UI can say so.
    fn supports_persist(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Gnome,
    Kde,
    Wlroots,
    X11,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Gnome => "gnome",
            BackendKind::Kde => "kde",
            BackendKind::Wlroots => "wlroots",
            BackendKind::X11 => "x11",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "gnome" | "mutter" => Some(BackendKind::Gnome),
            "kde" | "plasma" | "kwin" => Some(BackendKind::Kde),
            "wlroots" | "wlr" | "sway" | "hyprland" => Some(BackendKind::Wlroots),
            "x11" | "xrandr" | "randr" => Some(BackendKind::X11),
            _ => None,
        }
    }

    pub fn build(self) -> Result<Box<dyn DisplayBackend>> {
        match self {
            BackendKind::Gnome => Ok(Box::new(gnome::GnomeBackend::new()?)),
            BackendKind::Kde => Ok(Box::new(kde::KdeBackend::new()?)),
            BackendKind::Wlroots => Ok(Box::new(wlroots::WlrootsBackend::new()?)),
            BackendKind::X11 => Ok(Box::new(x11::X11Backend::new()?)),
        }
    }
}

/// Picks the backend that can talk to the running session.
///
/// `POWERDISPLAY_BACKEND` overrides the probing, which is the escape hatch for
/// compositors we guess wrong about.
pub fn detect() -> Result<Box<dyn DisplayBackend>> {
    if let Some(forced) = std::env::var("POWERDISPLAY_BACKEND").ok().filter(|v| !v.is_empty()) {
        let kind = BackendKind::parse(&forced)
            .ok_or_else(|| anyhow!("unknown POWERDISPLAY_BACKEND value {forced:?}"))?;
        return kind.build();
    }

    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_ascii_lowercase();
    let mut order = Vec::new();

    // Prefer the desktop's own API: it owns the saved configuration, so going around it
    // means the desktop reverts us on the next hotplug.
    if desktop.contains("kde") || desktop.contains("plasma") {
        order.push(BackendKind::Kde);
    }
    if desktop.contains("gnome") || desktop.contains("unity") {
        order.push(BackendKind::Gnome);
    }
    order.extend([
        BackendKind::Gnome,
        BackendKind::Kde,
        BackendKind::Wlroots,
        BackendKind::X11,
    ]);

    let mut errors = Vec::new();
    let mut tried = Vec::new();
    for kind in order {
        if tried.contains(&kind) {
            continue;
        }
        tried.push(kind);
        match kind.build() {
            Ok(backend) => {
                tracing::info!(backend = backend.name(), "selected display backend");
                return Ok(backend);
            }
            Err(err) => errors.push(format!("{}: {err:#}", kind.as_str())),
        }
    }

    bail!(
        "no usable display backend for this session; tried {}",
        errors.join("; ")
    )
}

/// Turns config rules into concrete instructions for the backend.
///
/// Rules that match nothing, or ask for a mode the panel cannot do, are reported instead
/// of aborting: a docking station being unplugged should not stop the laptop panel from
/// being switched.
pub fn resolve_settings(
    rules: &[crate::config::OutputRule],
    outputs: &[Output],
) -> (Vec<OutputSetting>, Vec<String>) {
    let mut settings = Vec::new();
    let mut warnings = Vec::new();

    for rule in rules {
        let Some(wanted) = rule.mode.as_deref().filter(|m| !m.is_empty()) else {
            continue;
        };

        let best = outputs
            .iter()
            .filter_map(|o| {
                rule.matcher
                    .score(&o.connector, &o.make, &o.model, &o.serial)
                    .map(|score| (score, o))
            })
            .max_by_key(|(score, _)| *score)
            .map(|(_, output)| output);

        let Some(output) = best else {
            warnings.push(format!("no connected display matches {:?}", rule.matcher));
            continue;
        };

        if settings.iter().any(|s: &OutputSetting| s.connector == output.connector) {
            warnings.push(format!(
                "{} is targeted by more than one rule; keeping the first",
                output.connector
            ));
            continue;
        }

        match output.resolve_mode(wanted) {
            Some(mode) => settings.push(OutputSetting {
                connector: output.connector.clone(),
                mode_id: mode.id.clone(),
            }),
            None => warnings.push(format!(
                "{} cannot do {wanted}; leaving it unchanged",
                output.connector
            )),
        }
    }

    (settings, warnings)
}

#[cfg(test)]
pub(crate) fn test_output(connector: &str, modes: &[(u32, u32, f64)]) -> Output {
    Output {
        connector: connector.to_string(),
        modes: modes
            .iter()
            .map(|&(width, height, refresh)| Mode {
                id: format_mode_id(width, height, refresh),
                width,
                height,
                refresh,
                preferred: false,
                current: false,
            })
            .collect(),
        enabled: true,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::OutputRule;

    #[test]
    fn mode_ids_round_trip() {
        let id = format_mode_id(2880, 1800, 120.0);
        assert_eq!(id, "2880x1800@120.000");
        assert_eq!(parse_mode_id(&id), Some((2880, 1800, 120.0)));
        assert_eq!(parse_mode_id("nonsense"), None);
    }

    #[test]
    fn resolve_mode_prefers_an_exact_id() {
        let output = test_output("eDP-1", &[(1920, 1080, 60.0), (1920, 1080, 144.0)]);
        let mode = output.resolve_mode("1920x1080@144.000").unwrap();
        assert_eq!(mode.refresh, 144.0);
    }

    #[test]
    fn resolve_mode_tolerates_refresh_rate_precision() {
        let output = test_output("eDP-1", &[(1920, 1080, 59.81), (1920, 1080, 120.0)]);
        let mode = output.resolve_mode("1920x1080@59.810825347900391").unwrap();
        assert_eq!(mode.refresh, 59.81);
    }

    #[test]
    fn mode_ids_may_carry_a_variant_suffix() {
        assert_eq!(
            parse_mode_id("2880x1800@120.000+vrr"),
            Some((2880, 1800, 120.0))
        );
    }

    #[test]
    fn variant_duplicates_are_hidden_from_menus() {
        let mut output = test_output("eDP-1", &[(2880, 1800, 120.0), (2880, 1800, 60.001)]);
        let vrr = Mode {
            id: "2880x1800@120.000+vrr".into(),
            ..output.modes[0].clone()
        };
        output.modes.insert(1, vrr);

        let distinct = output.distinct_modes();
        assert_eq!(distinct.len(), 2);
        assert_eq!(distinct[0].id, "2880x1800@120.000");
    }

    #[test]
    fn resolve_mode_will_not_invent_a_resolution() {
        let output = test_output("eDP-1", &[(1920, 1080, 60.0)]);
        assert!(output.resolve_mode("3840x2160@60.000").is_none());
    }

    #[test]
    fn unmatched_rules_warn_without_dropping_the_others() {
        let outputs = vec![
            test_output("eDP-1", &[(1920, 1080, 60.0)]),
            test_output("DP-1", &[(3840, 2160, 60.0)]),
        ];
        let rules = vec![
            OutputRule {
                matcher: OutputMatch {
                    connector: Some("eDP-1".into()),
                    ..Default::default()
                },
                mode: Some("1920x1080@60.000".into()),
            },
            OutputRule {
                matcher: OutputMatch {
                    connector: Some("HDMI-A-1".into()),
                    ..Default::default()
                },
                mode: Some("1920x1080@60.000".into()),
            },
            OutputRule {
                matcher: OutputMatch {
                    connector: Some("DP-1".into()),
                    ..Default::default()
                },
                mode: Some("1280x720@60.000".into()),
            },
        ];

        let (settings, warnings) = resolve_settings(&rules, &outputs);
        assert_eq!(settings.len(), 1);
        assert_eq!(settings[0].connector, "eDP-1");
        assert_eq!(warnings.len(), 2);
    }

    #[test]
    fn rules_without_a_mode_are_skipped() {
        let outputs = vec![test_output("eDP-1", &[(1920, 1080, 60.0)])];
        let rules = vec![OutputRule {
            matcher: OutputMatch {
                connector: Some("eDP-1".into()),
                ..Default::default()
            },
            mode: None,
        }];
        let (settings, warnings) = resolve_settings(&rules, &outputs);
        assert!(settings.is_empty());
        assert!(warnings.is_empty());
    }
}

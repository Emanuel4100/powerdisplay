//! GNOME / Mutter backend, speaking `org.gnome.Mutter.DisplayConfig` over the session bus.
//!
//! Two things about this API drive the shape of the code:
//!
//! * `ApplyMonitorsConfig` is not a patch. It replaces the whole layout, so every applied
//!   change starts by reading the current state back and rebuilding it with only the mode
//!   ids swapped out.
//! * Mode ids are opaque strings that must round-trip byte for byte
//!   (`1920x1080@59.810825347900391`), and the scale in a logical monitor is only legal if
//!   the chosen mode lists it in `supported_scales`.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

use super::{DisplayBackend, Mode, Output, OutputSetting};

const DESTINATION: &str = "org.gnome.Mutter.DisplayConfig";
const OBJECT_PATH: &str = "/org/gnome/Mutter/DisplayConfig";
const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";

const METHOD_TEMPORARY: u32 = 1;
const METHOD_PERSISTENT: u32 = 2;

const SCALE_EPSILON: f64 = 0.001;

/// `(connector, vendor, product, serial)`
type MonitorSpec = (String, String, String, String);
/// `(id, width, height, refresh, preferred_scale, supported_scales, properties)`
type ModeInfo = (String, i32, i32, f64, f64, Vec<f64>, HashMap<String, OwnedValue>);
type MonitorInfo = (MonitorSpec, Vec<ModeInfo>, HashMap<String, OwnedValue>);
/// `(x, y, scale, transform, primary, monitors, properties)`
type LogicalMonitorInfo = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (
    u32,
    Vec<MonitorInfo>,
    Vec<LogicalMonitorInfo>,
    HashMap<String, OwnedValue>,
);

/// `(connector, mode_id, properties)`
type MonitorConfig = (String, String, HashMap<String, OwnedValue>);
/// `(x, y, scale, transform, primary, monitors)`
type LogicalMonitorConfig = (i32, i32, f64, u32, bool, Vec<MonitorConfig>);

pub struct GnomeBackend {
    connection: Connection,
}

impl GnomeBackend {
    pub fn new() -> Result<Self> {
        let connection = Connection::session().context("connecting to the session bus")?;
        let backend = Self { connection };
        backend
            .current_state()
            .context("org.gnome.Mutter.DisplayConfig is not answering")?;
        Ok(backend)
    }

    fn proxy(&self) -> Result<Proxy<'_>> {
        Proxy::new(&self.connection, DESTINATION, OBJECT_PATH, INTERFACE)
            .context("creating the DisplayConfig proxy")
    }

    fn current_state(&self) -> Result<CurrentState> {
        self.proxy()?
            .call::<_, (), CurrentState>("GetCurrentState", &())
            .context("calling GetCurrentState")
    }

    fn apply_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<LogicalMonitorConfig>,
        properties: HashMap<String, OwnedValue>,
    ) -> Result<()> {
        self.proxy()?
            .call::<_, _, ()>(
                "ApplyMonitorsConfig",
                &(serial, method, logical_monitors, properties),
            )
            .context("calling ApplyMonitorsConfig")
    }
}

impl DisplayBackend for GnomeBackend {
    fn name(&self) -> &'static str {
        "GNOME (Mutter)"
    }

    fn supports_persist(&self) -> bool {
        true
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        let (_, monitors, logical_monitors, _) = self.current_state()?;

        let active: Vec<&str> = logical_monitors
            .iter()
            .flat_map(|lm| lm.5.iter().map(|spec| spec.0.as_str()))
            .collect();

        Ok(monitors
            .into_iter()
            .map(|(spec, modes, _)| {
                let (connector, vendor, product, serial) = spec;
                let mut current_mode = None;
                let modes = modes
                    .into_iter()
                    .map(|(id, width, height, refresh, _, _, props)| {
                        let mode = Mode {
                            width: width.max(0) as u32,
                            height: height.max(0) as u32,
                            refresh,
                            preferred: prop_bool(&props, "is-preferred").unwrap_or(false),
                            current: prop_bool(&props, "is-current").unwrap_or(false),
                            id,
                        };
                        if mode.current {
                            current_mode = Some(mode.id.clone());
                        }
                        mode
                    })
                    .collect();

                Output {
                    enabled: active.contains(&connector.as_str()),
                    connector,
                    make: vendor,
                    model: product,
                    serial,
                    modes,
                    current_mode,
                }
            })
            .collect())
    }

    fn apply(&self, wanted: &[OutputSetting], persist: bool) -> Result<()> {
        let (serial, monitors, logical_monitors, state_properties) = self.current_state()?;

        if prop_bool(&state_properties, "apply-monitors-config-allowed") == Some(false) {
            bail!("the compositor is currently refusing display configuration changes");
        }

        if logical_monitors.is_empty() {
            bail!("Mutter reports no active monitors");
        }

        for setting in wanted {
            if !logical_monitors
                .iter()
                .any(|lm| lm.5.iter().any(|spec| spec.0 == setting.connector))
            {
                tracing::warn!(
                    connector = %setting.connector,
                    "display is not part of the active layout; skipping it"
                );
            }
        }

        let modes_by_connector: HashMap<&str, &Vec<ModeInfo>> = monitors
            .iter()
            .map(|(spec, modes, _)| (spec.0.as_str(), modes))
            .collect();

        // Resolve every logical monitor to the mode each of its outputs should end up in.
        let mut plan: Vec<PlannedLogicalMonitor> = Vec::new();
        for logical in &logical_monitors {
            let (x, y, scale, transform, primary, specs, _) = logical;
            let mut chosen = Vec::new();

            for spec in specs {
                let connector = spec.0.as_str();
                let modes = modes_by_connector
                    .get(connector)
                    .with_context(|| format!("Mutter listed {connector} without any modes"))?;

                let mode = match wanted.iter().find(|s| s.connector == connector) {
                    Some(setting) => modes
                        .iter()
                        .find(|m| m.0 == setting.mode_id)
                        .with_context(|| {
                            format!("{connector} does not offer mode {}", setting.mode_id)
                        })?,
                    None => modes
                        .iter()
                        .find(|m| prop_bool(&m.6, "is-current").unwrap_or(false))
                        .with_context(|| format!("{connector} has no current mode"))?,
                };

                chosen.push(ChosenMode {
                    connector: connector.to_string(),
                    id: mode.0.clone(),
                    width: mode.1.max(0) as u32,
                    height: mode.2.max(0) as u32,
                    supported_scales: mode.5.clone(),
                });
            }

            plan.push(PlannedLogicalMonitor {
                x: *x,
                y: *y,
                scale: *scale,
                transform: *transform,
                primary: *primary,
                monitors: chosen,
            });
        }

        // A mode change can invalidate the current scale; snap to the nearest legal one.
        if prop_bool(&state_properties, "global-scale-required").unwrap_or(false) {
            let all_scales: Vec<&[f64]> = plan
                .iter()
                .flat_map(|lm| lm.monitors.iter().map(|m| m.supported_scales.as_slice()))
                .collect();
            let target = plan
                .iter()
                .find(|lm| lm.primary)
                .unwrap_or(&plan[0])
                .scale;
            let scale = pick_scale(target, &all_scales);
            for logical in &mut plan {
                logical.scale = scale;
            }
        } else {
            for logical in &mut plan {
                let scales: Vec<&[f64]> = logical
                    .monitors
                    .iter()
                    .map(|m| m.supported_scales.as_slice())
                    .collect();
                logical.scale = pick_scale(logical.scale, &scales);
            }
        }

        let method = if persist {
            METHOD_PERSISTENT
        } else {
            METHOD_TEMPORARY
        };

        let mut properties = HashMap::new();
        if prop_bool(&state_properties, "supports-changing-layout-mode").unwrap_or(false)
            && let Some(layout_mode) = prop_u32(&state_properties, "layout-mode")
        {
            properties.insert("layout-mode".to_string(), owned(Value::U32(layout_mode)));
        }

        let first_attempt = self.apply_config(serial, method, build_config(&plan), properties.clone());
        let Err(err) = first_attempt else {
            return Ok(());
        };

        // Changing one monitor's resolution shifts everything to its right, which Mutter
        // rejects as a gap or an overlap. Repack the row and try once more.
        if plan.len() < 2 {
            return Err(err);
        }

        tracing::warn!(error = %err, "retrying with a repacked monitor layout");
        let logical_layout = prop_u32(&state_properties, "layout-mode") != Some(2);
        repack(&mut plan, logical_layout);

        // The failed call did not change anything, but re-read the serial anyway: Mutter
        // bumps it on unrelated events such as a hotplug racing with us.
        let serial = self.current_state()?.0;
        self.apply_config(serial, method, build_config(&plan), properties)
            .with_context(|| format!("after the original layout was rejected: {err:#}"))
    }
}

struct ChosenMode {
    connector: String,
    id: String,
    width: u32,
    height: u32,
    supported_scales: Vec<f64>,
}

struct PlannedLogicalMonitor {
    x: i32,
    y: i32,
    scale: f64,
    transform: u32,
    primary: bool,
    monitors: Vec<ChosenMode>,
}

impl PlannedLogicalMonitor {
    /// Size this logical monitor occupies in the layout, honouring rotation and,
    /// in logical layout mode, the scale.
    fn size(&self, logical_layout: bool) -> (i32, i32) {
        let (width, height) = self
            .monitors
            .first()
            .map(|m| (m.width, m.height))
            .unwrap_or((0, 0));

        let (width, height) = if self.transform % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        };

        if logical_layout && self.scale > 0.0 {
            (
                (width as f64 / self.scale).round() as i32,
                (height as f64 / self.scale).round() as i32,
            )
        } else {
            (width as i32, height as i32)
        }
    }
}

fn build_config(plan: &[PlannedLogicalMonitor]) -> Vec<LogicalMonitorConfig> {
    plan.iter()
        .map(|logical| {
            (
                logical.x,
                logical.y,
                logical.scale,
                logical.transform,
                logical.primary,
                logical
                    .monitors
                    .iter()
                    .map(|m| (m.connector.clone(), m.id.clone(), HashMap::new()))
                    .collect(),
            )
        })
        .collect()
}

/// Lays the monitors out left to right in their existing order, removing the gaps a
/// resolution change opens up.
fn repack(plan: &mut [PlannedLogicalMonitor], logical_layout: bool) {
    let mut order: Vec<usize> = (0..plan.len()).collect();
    order.sort_by_key(|&i| (plan[i].x, plan[i].y));

    let mut cursor = 0;
    for index in order {
        let (width, _) = plan[index].size(logical_layout);
        plan[index].x = cursor;
        plan[index].y = 0;
        cursor += width;
    }
}

/// Nearest scale to `target` that every one of `options` supports, or 1.0 if the lists
/// have nothing in common.
fn pick_scale(target: f64, options: &[&[f64]]) -> f64 {
    let Some((first, rest)) = options.split_first() else {
        return target;
    };

    let supported_everywhere = |candidate: f64| {
        rest.iter()
            .all(|list| list.iter().any(|s| (s - candidate).abs() < SCALE_EPSILON))
    };

    if first.iter().any(|s| (s - target).abs() < SCALE_EPSILON) && supported_everywhere(target) {
        return target;
    }

    first
        .iter()
        .copied()
        .filter(|&candidate| supported_everywhere(candidate))
        .min_by(|a, b| (a - target).abs().total_cmp(&(b - target).abs()))
        .unwrap_or(1.0)
}

fn prop_bool(props: &HashMap<String, OwnedValue>, key: &str) -> Option<bool> {
    match &**props.get(key)? {
        Value::Bool(value) => Some(*value),
        _ => None,
    }
}

fn prop_u32(props: &HashMap<String, OwnedValue>, key: &str) -> Option<u32> {
    match &**props.get(key)? {
        Value::U32(value) => Some(*value),
        _ => None,
    }
}

fn owned(value: Value<'static>) -> OwnedValue {
    OwnedValue::try_from(value).expect("basic values are always convertible")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(x: i32, width: u32, height: u32, scale: f64) -> PlannedLogicalMonitor {
        PlannedLogicalMonitor {
            x,
            y: 0,
            scale,
            transform: 0,
            primary: false,
            monitors: vec![ChosenMode {
                connector: "test".into(),
                id: "test".into(),
                width,
                height,
                supported_scales: vec![1.0, 2.0],
            }],
        }
    }

    #[test]
    fn pick_scale_keeps_a_supported_value() {
        assert_eq!(pick_scale(2.0, &[&[1.0, 1.5, 2.0]]), 2.0);
    }

    #[test]
    fn pick_scale_snaps_to_the_nearest_supported_value() {
        assert_eq!(pick_scale(1.75, &[&[1.0, 1.5, 3.0]]), 1.5);
    }

    #[test]
    fn pick_scale_requires_agreement_between_monitors() {
        assert_eq!(pick_scale(2.0, &[&[1.0, 2.0], &[1.0]]), 1.0);
    }

    #[test]
    fn pick_scale_falls_back_when_nothing_is_shared() {
        assert_eq!(pick_scale(2.0, &[&[2.0], &[3.0]]), 1.0);
    }

    #[test]
    fn repack_closes_the_gap_left_by_a_smaller_mode() {
        let mut plan = vec![planned(0, 1920, 1080, 1.0), planned(3840, 2560, 1440, 1.0)];
        repack(&mut plan, true);
        assert_eq!(plan[0].x, 0);
        assert_eq!(plan[1].x, 1920);
    }

    #[test]
    fn repack_uses_logical_sizes_when_scaling_is_logical() {
        let mut plan = vec![planned(0, 3840, 2160, 2.0), planned(9999, 1920, 1080, 1.0)];
        repack(&mut plan, true);
        assert_eq!(plan[1].x, 1920);
    }

    #[test]
    fn rotated_monitors_occupy_swapped_dimensions() {
        let mut monitor = planned(0, 1920, 1080, 1.0);
        monitor.transform = 1;
        assert_eq!(monitor.size(false), (1080, 1920));
    }
}

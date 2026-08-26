//! wlroots backend, speaking `wlr-output-management-unstable-v1` directly.
//!
//! The protocol is implemented by sway, Hyprland, river, Wayfire, labwc and cosmic-comp,
//! so one client covers most of the non-GNOME, non-KDE Wayland world. Talking the
//! protocol rather than shelling out to `wlr-randr` means there is nothing extra to
//! install and no output format to parse.
//!
//! Note that a configuration must describe *every* head, not just the ones being changed:
//! heads left out are rejected by the compositor.

use anyhow::{Context, Result, anyhow, bail};
use wayland_client::backend::ObjectId;
use wayland_client::protocol::{wl_output, wl_registry};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::output_management::v1::client::{
    zwlr_output_configuration_head_v1::ZwlrOutputConfigurationHeadV1,
    zwlr_output_configuration_v1::{self, ZwlrOutputConfigurationV1},
    zwlr_output_head_v1::{self, ZwlrOutputHeadV1},
    zwlr_output_manager_v1::{self, ZwlrOutputManagerV1},
    zwlr_output_mode_v1::{self, ZwlrOutputModeV1},
};

use super::{DisplayBackend, Mode, Output, OutputSetting, format_mode_id};

/// Highest version this client understands. `make`/`model`/`serial_number` arrived in 2.
const MANAGER_VERSION: u32 = 4;

/// A misbehaving compositor should make us give up rather than hang the daemon.
const MAX_DISPATCHES: usize = 200;

pub struct WlrootsBackend {
    _private: (),
}

impl WlrootsBackend {
    pub fn new() -> Result<Self> {
        Session::open().context("wlr-output-management is not available")?;
        Ok(Self { _private: () })
    }
}

impl DisplayBackend for WlrootsBackend {
    fn name(&self) -> &'static str {
        "wlroots (wlr-output-management)"
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        Ok(Session::open()?.state.heads.iter().map(Head::to_output).collect())
    }

    fn apply(&self, wanted: &[OutputSetting], _persist: bool) -> Result<()> {
        let mut session = Session::open()?;
        let queue_handle = session.queue.handle();
        let manager = session
            .state
            .manager
            .clone()
            .ok_or_else(|| anyhow!("the output manager went away"))?;

        // Snapshot what to send before touching the connection, so the borrow of the
        // gathered state ends before events start arriving again.
        let mut plan = Vec::new();
        for head in &session.state.heads {
            let target = match wanted.iter().find(|s| s.connector == head.name) {
                Some(setting) => {
                    if !head.enabled {
                        tracing::warn!(connector = %head.name, "display is switched off; skipping it");
                        head.current_mode()
                    } else {
                        Some(
                            head.modes
                                .iter()
                                .find(|mode| mode.id() == setting.mode_id)
                                .with_context(|| {
                                    format!("{} does not offer mode {}", head.name, setting.mode_id)
                                })?,
                        )
                    }
                }
                None => head.current_mode(),
            };

            plan.push(PlannedHead {
                proxy: head.proxy.clone(),
                enabled: head.enabled,
                mode: target.map(|mode| mode.proxy.clone()),
                position: head.position,
                transform: head.transform,
                scale: head.scale,
            });
        }

        let configuration = manager.create_configuration(session.state.serial, &queue_handle, ());

        for head in plan {
            if !head.enabled {
                configuration.disable_head(&head.proxy);
                continue;
            }

            let configured = configuration.enable_head(&head.proxy, &queue_handle, ());
            if let Some(mode) = &head.mode {
                configured.set_mode(mode);
            }
            // Re-stating the rest keeps a compositor from resetting position or scale to
            // its own defaults when we only meant to change the mode.
            configured.set_position(head.position.0, head.position.1);
            if let Some(transform) = head.transform {
                configured.set_transform(transform);
            }
            if head.scale > 0.0 {
                configured.set_scale(head.scale);
            }
        }

        configuration.apply();

        session.pump(|state| state.outcome.is_some())?;
        configuration.destroy();

        match session.state.outcome {
            Some(Outcome::Succeeded) => Ok(()),
            Some(Outcome::Failed) => bail!("the compositor rejected the configuration"),
            Some(Outcome::Cancelled) => {
                bail!("the display setup changed while applying; try again")
            }
            None => bail!("the compositor never answered"),
        }
    }
}

struct PlannedHead {
    proxy: ZwlrOutputHeadV1,
    enabled: bool,
    mode: Option<ZwlrOutputModeV1>,
    position: (i32, i32),
    transform: Option<wl_output::Transform>,
    scale: f64,
}

struct Session {
    queue: EventQueue<State>,
    state: State,
}

impl Session {
    /// Connects and waits for the compositor to finish describing every head.
    fn open() -> Result<Self> {
        let connection =
            Connection::connect_to_env().context("connecting to the Wayland compositor")?;
        let mut queue = connection.new_event_queue();
        let queue_handle = queue.handle();
        connection.display().get_registry(&queue_handle, ());

        let mut state = State::default();
        queue
            .roundtrip(&mut state)
            .context("reading the Wayland globals")?;

        if state.manager.is_none() {
            bail!("this compositor does not implement wlr-output-management-unstable-v1");
        }

        let mut session = Self { queue, state };
        // The manager sends every head and mode, then a `done` carrying the serial that
        // a configuration must quote.
        session.pump(|state| state.done)?;
        Ok(session)
    }

    fn pump(&mut self, ready: impl Fn(&State) -> bool) -> Result<()> {
        let mut dispatches = 0;
        while !ready(&self.state) {
            self.queue
                .blocking_dispatch(&mut self.state)
                .context("waiting for the compositor")?;

            dispatches += 1;
            if dispatches > MAX_DISPATCHES {
                bail!("the compositor stopped responding");
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Succeeded,
    Failed,
    Cancelled,
}

#[derive(Default)]
struct State {
    manager: Option<ZwlrOutputManagerV1>,
    serial: u32,
    done: bool,
    heads: Vec<Head>,
    outcome: Option<Outcome>,
}

impl State {
    fn head_mut(&mut self, id: &ObjectId) -> Option<&mut Head> {
        self.heads.iter_mut().find(|head| head.proxy.id() == *id)
    }

    fn mode_mut(&mut self, id: &ObjectId) -> Option<&mut ModeEntry> {
        self.heads
            .iter_mut()
            .flat_map(|head| head.modes.iter_mut())
            .find(|mode| mode.proxy.id() == *id)
    }
}

struct Head {
    proxy: ZwlrOutputHeadV1,
    name: String,
    make: String,
    model: String,
    serial_number: String,
    enabled: bool,
    current_mode_id: Option<ObjectId>,
    modes: Vec<ModeEntry>,
    position: (i32, i32),
    transform: Option<wl_output::Transform>,
    scale: f64,
}

impl Head {
    fn new(proxy: ZwlrOutputHeadV1) -> Self {
        Self {
            proxy,
            name: String::new(),
            make: String::new(),
            model: String::new(),
            serial_number: String::new(),
            enabled: false,
            current_mode_id: None,
            modes: Vec::new(),
            position: (0, 0),
            transform: None,
            scale: 1.0,
        }
    }

    fn current_mode(&self) -> Option<&ModeEntry> {
        let id = self.current_mode_id.as_ref()?;
        self.modes.iter().find(|mode| mode.proxy.id() == *id)
    }

    fn to_output(&self) -> Output {
        let current = self.current_mode().map(ModeEntry::id);
        Output {
            connector: self.name.clone(),
            make: self.make.clone(),
            model: self.model.clone(),
            serial: self.serial_number.clone(),
            modes: self
                .modes
                .iter()
                .map(|mode| Mode {
                    id: mode.id(),
                    width: mode.width.max(0) as u32,
                    height: mode.height.max(0) as u32,
                    refresh: mode.refresh_hz(),
                    preferred: mode.preferred,
                    current: Some(mode.proxy.id()) == self.current_mode_id,
                })
                .collect(),
            current_mode: current,
            enabled: self.enabled,
        }
    }
}

struct ModeEntry {
    proxy: ZwlrOutputModeV1,
    width: i32,
    height: i32,
    refresh_mhz: i32,
    preferred: bool,
}

impl ModeEntry {
    fn refresh_hz(&self) -> f64 {
        refresh_hz(self.refresh_mhz)
    }

    fn id(&self) -> String {
        mode_id(self.width, self.height, self.refresh_mhz)
    }
}

fn refresh_hz(refresh_mhz: i32) -> f64 {
    refresh_mhz as f64 / 1000.0
}

fn mode_id(width: i32, height: i32, refresh_mhz: i32) -> String {
    format_mode_id(
        width.max(0) as u32,
        height.max(0) as u32,
        refresh_hz(refresh_mhz),
    )
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        queue_handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };

        if interface == ZwlrOutputManagerV1::interface().name {
            state.manager = Some(registry.bind::<ZwlrOutputManagerV1, _, _>(
                name,
                version.min(MANAGER_VERSION),
                queue_handle,
                (),
            ));
        }
    }
}

impl Dispatch<ZwlrOutputManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputManagerV1,
        event: zwlr_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_output_manager_v1::Event::Head { head } => state.heads.push(Head::new(head)),
            zwlr_output_manager_v1::Event::Done { serial } => {
                state.serial = serial;
                state.done = true;
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputManagerV1, [
        zwlr_output_manager_v1::EVT_HEAD_OPCODE => (ZwlrOutputHeadV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputHeadV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputHeadV1,
        event: zwlr_output_head_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id();

        // Removal is handled before borrowing the head, since it mutates the whole list.
        if matches!(event, zwlr_output_head_v1::Event::Finished) {
            state.heads.retain(|head| head.proxy.id() != id);
            return;
        }

        let Some(head) = state.head_mut(&id) else {
            return;
        };

        match event {
            zwlr_output_head_v1::Event::Name { name } => head.name = name,
            zwlr_output_head_v1::Event::Make { make } => head.make = make,
            zwlr_output_head_v1::Event::Model { model } => head.model = model,
            zwlr_output_head_v1::Event::SerialNumber { serial_number } => {
                head.serial_number = serial_number
            }
            zwlr_output_head_v1::Event::Enabled { enabled } => head.enabled = enabled != 0,
            zwlr_output_head_v1::Event::CurrentMode { mode } => {
                head.current_mode_id = Some(mode.id())
            }
            zwlr_output_head_v1::Event::Position { x, y } => head.position = (x, y),
            zwlr_output_head_v1::Event::Transform { transform } => {
                head.transform = transform.into_result().ok()
            }
            zwlr_output_head_v1::Event::Scale { scale } => head.scale = scale,
            zwlr_output_head_v1::Event::Mode { mode } => head.modes.push(ModeEntry {
                proxy: mode,
                width: 0,
                height: 0,
                refresh_mhz: 0,
                preferred: false,
            }),
            _ => {}
        }
    }

    event_created_child!(State, ZwlrOutputHeadV1, [
        zwlr_output_head_v1::EVT_MODE_OPCODE => (ZwlrOutputModeV1, ()),
    ]);
}

impl Dispatch<ZwlrOutputModeV1, ()> for State {
    fn event(
        state: &mut Self,
        proxy: &ZwlrOutputModeV1,
        event: zwlr_output_mode_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = proxy.id();

        if matches!(event, zwlr_output_mode_v1::Event::Finished) {
            for head in &mut state.heads {
                head.modes.retain(|mode| mode.proxy.id() != id);
            }
            return;
        }

        let Some(mode) = state.mode_mut(&id) else {
            return;
        };

        match event {
            zwlr_output_mode_v1::Event::Size { width, height } => {
                mode.width = width;
                mode.height = height;
            }
            zwlr_output_mode_v1::Event::Refresh { refresh } => mode.refresh_mhz = refresh,
            zwlr_output_mode_v1::Event::Preferred => mode.preferred = true,
            _ => {}
        }
    }
}

impl Dispatch<ZwlrOutputConfigurationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrOutputConfigurationV1,
        event: zwlr_output_configuration_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        state.outcome = match event {
            zwlr_output_configuration_v1::Event::Succeeded => Some(Outcome::Succeeded),
            zwlr_output_configuration_v1::Event::Failed => Some(Outcome::Failed),
            zwlr_output_configuration_v1::Event::Cancelled => Some(Outcome::Cancelled),
            _ => return,
        };
    }
}

impl Dispatch<ZwlrOutputConfigurationHeadV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrOutputConfigurationHeadV1,
        _: <ZwlrOutputConfigurationHeadV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The protocol reports millihertz, and the ids written into the config file have to
    /// come out the same as every other backend's.
    #[test]
    fn millihertz_becomes_a_matching_mode_id() {
        assert_eq!(refresh_hz(143_912), 143.912);
        assert_eq!(mode_id(2560, 1440, 143_912), "2560x1440@143.912");
        assert_eq!(mode_id(1920, 1080, 60_000), "1920x1080@60.000");
    }

    /// Some compositors report no refresh rate for virtual outputs.
    #[test]
    fn a_missing_refresh_rate_does_not_produce_a_broken_id() {
        assert_eq!(mode_id(1920, 1080, 0), "1920x1080@0.000");
        assert_eq!(super::super::parse_mode_id("1920x1080@0.000"), Some((1920, 1080, 0.0)));
    }
}

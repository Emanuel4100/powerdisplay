//! X11 backend, driving RandR directly.
//!
//! This is what covers XFCE, MATE, Cinnamon, i3 and every other X session, including
//! GNOME and Plasma when they are running on X rather than Wayland. RandR is spoken
//! natively instead of shelling out to `xrandr`, but the tool is kept as a fallback for
//! the awkward cases (mainly screen resizes that need a layout `xrandr` already knows how
//! to compute).

use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use x11rb::connection::Connection as _;
use x11rb::protocol::randr::{self, ConnectionExt as _, ModeFlag};
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, Window};
use x11rb::rust_connection::RustConnection;

use super::{DisplayBackend, Mode, Output, OutputSetting, format_mode_id, parse_mode_id};

/// X11 has no notion of DPI beyond the screen size in millimetres, and every toolkit
/// assumes this value when converting.
const ASSUMED_DPI: f64 = 96.0;

pub struct X11Backend {
    _private: (),
}

impl X11Backend {
    pub fn new() -> Result<Self> {
        let backend = Self { _private: () };
        backend.session().context("no usable X11 display")?;
        Ok(backend)
    }

    fn session(&self) -> Result<Session> {
        Session::open()
    }
}

impl DisplayBackend for X11Backend {
    fn name(&self) -> &'static str {
        "X11 (RandR)"
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        self.session()?.outputs()
    }

    fn apply(&self, wanted: &[OutputSetting], _persist: bool) -> Result<()> {
        match self.session()?.apply(wanted) {
            Ok(()) => Ok(()),
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "falling back to the xrandr command");
                apply_with_xrandr(wanted)
                    .with_context(|| format!("after RandR itself failed: {err:#}"))
            }
        }
    }
}

struct Session {
    connection: RustConnection,
    root: Window,
    resources: randr::GetScreenResourcesCurrentReply,
    screen_width: u16,
    screen_height: u16,
}

impl Session {
    fn open() -> Result<Self> {
        let (connection, screen_num) = x11rb::connect(None).context("connecting to the X server")?;
        let screen = connection
            .setup()
            .roots
            .get(screen_num)
            .ok_or_else(|| anyhow!("the X server reported no screens"))?;
        let root = screen.root;
        let screen_width = screen.width_in_pixels;
        let screen_height = screen.height_in_pixels;

        let resources = connection
            .randr_get_screen_resources_current(root)
            .context("querying RandR")?
            .reply()
            .context("RandR is not available on this display")?;

        Ok(Self {
            connection,
            root,
            resources,
            screen_width,
            screen_height,
        })
    }

    fn mode_table(&self) -> HashMap<u32, &randr::ModeInfo> {
        self.resources
            .modes
            .iter()
            .map(|mode| (mode.id, mode))
            .collect()
    }

    fn outputs(&self) -> Result<Vec<Output>> {
        let modes = self.mode_table();
        let mut result = Vec::new();

        for &output in &self.resources.outputs {
            let info = self
                .connection
                .randr_get_output_info(output, self.resources.config_timestamp)?
                .reply()?;

            if info.connection != randr::Connection::CONNECTED {
                continue;
            }

            let current_mode_id = (info.crtc != 0)
                .then(|| {
                    self.connection
                        .randr_get_crtc_info(info.crtc, self.resources.config_timestamp)
                        .ok()?
                        .reply()
                        .ok()
                        .map(|crtc| crtc.mode)
                })
                .flatten()
                .filter(|&mode| mode != 0);

            let preferred: Vec<u32> = info
                .modes
                .iter()
                .take(info.num_preferred as usize)
                .copied()
                .collect();

            let (make, model, serial) = self.identity(output).unwrap_or_default();

            result.push(Output {
                connector: String::from_utf8_lossy(&info.name).into_owned(),
                make,
                model,
                serial,
                modes: info
                    .modes
                    .iter()
                    .filter_map(|id| {
                        let info = modes.get(id)?;
                        Some(Mode {
                            id: mode_id(info),
                            width: info.width as u32,
                            height: info.height as u32,
                            refresh: refresh_of(info),
                            preferred: preferred.contains(id),
                            current: Some(*id) == current_mode_id,
                        })
                    })
                    .collect(),
                current_mode: current_mode_id
                    .and_then(|id| modes.get(&id).map(|info| mode_id(info))),
                enabled: info.crtc != 0,
            });
        }

        Ok(result)
    }

    /// Reads the EDID property so displays can be matched by make and model, in the same
    /// shape Mutter reports them, rather than only by connector name.
    fn identity(&self, output: randr::Output) -> Option<(String, String, String)> {
        let atom = self
            .connection
            .intern_atom(true, b"EDID")
            .ok()?
            .reply()
            .ok()?
            .atom;

        let property = self
            .connection
            .randr_get_output_property(output, atom, AtomEnum::ANY, 0, 128, false, false)
            .ok()?
            .reply()
            .ok()?;

        parse_edid(&property.data)
    }

    fn apply(&self, wanted: &[OutputSetting]) -> Result<()> {
        let modes = self.mode_table();
        let mut changes = Vec::new();

        for setting in wanted {
            let (_, info) = self
                .find_output(&setting.connector)?
                .with_context(|| format!("{} is not connected", setting.connector))?;

            if info.crtc == 0 {
                bail!("{} is switched off", setting.connector);
            }

            let mode = info
                .modes
                .iter()
                .filter_map(|id| modes.get(id))
                .find(|mode| mode_id(mode) == setting.mode_id)
                .with_context(|| {
                    format!("{} does not offer mode {}", setting.connector, setting.mode_id)
                })?;

            let crtc = self
                .connection
                .randr_get_crtc_info(info.crtc, self.resources.config_timestamp)?
                .reply()?;

            changes.push(Change {
                crtc: info.crtc,
                mode: mode.id,
                width: mode.width,
                height: mode.height,
                x: crtc.x,
                y: crtc.y,
                rotation: crtc.rotation,
                outputs: crtc.outputs.clone(),
            });
        }

        if changes.is_empty() {
            return Ok(());
        }

        let (width, height) = self.screen_size_after(&changes)?;
        let grow = width > self.screen_width || height > self.screen_height;

        // The framebuffer has to be big enough before a CRTC can be placed inside it, and
        // it may only shrink once nothing sticks out any more.
        if grow {
            self.set_screen_size(width.max(self.screen_width), height.max(self.screen_height))?;
        }

        for change in &changes {
            let reply = self
                .connection
                .randr_set_crtc_config(
                    change.crtc,
                    x11rb::CURRENT_TIME,
                    self.resources.config_timestamp,
                    change.x,
                    change.y,
                    change.mode,
                    change.rotation,
                    &change.outputs,
                )?
                .reply()?;

            if reply.status != randr::SetConfig::SUCCESS {
                bail!(
                    "the X server refused the mode change for CRTC {} ({:?})",
                    change.crtc,
                    reply.status
                );
            }
        }

        if !grow && (width < self.screen_width || height < self.screen_height) {
            self.set_screen_size(width, height)?;
        }

        self.connection.flush()?;
        Ok(())
    }

    fn find_output(
        &self,
        connector: &str,
    ) -> Result<Option<(randr::Output, randr::GetOutputInfoReply)>> {
        for &output in &self.resources.outputs {
            let info = self
                .connection
                .randr_get_output_info(output, self.resources.config_timestamp)?
                .reply()?;

            if info.connection == randr::Connection::CONNECTED
                && String::from_utf8_lossy(&info.name) == connector
            {
                return Ok(Some((output, info)));
            }
        }
        Ok(None)
    }

    /// The framebuffer must cover every CRTC, including the ones we are not touching.
    fn screen_size_after(&self, changes: &[Change]) -> Result<(u16, u16)> {
        let mut width = 0i32;
        let mut height = 0i32;

        for &crtc in &self.resources.crtcs {
            let (x, y, w, h) = match changes.iter().find(|change| change.crtc == crtc) {
                Some(change) => (
                    change.x as i32,
                    change.y as i32,
                    rotated(change.width, change.height, change.rotation).0 as i32,
                    rotated(change.width, change.height, change.rotation).1 as i32,
                ),
                None => {
                    let info = self
                        .connection
                        .randr_get_crtc_info(crtc, self.resources.config_timestamp)?
                        .reply()?;
                    if info.mode == 0 {
                        continue;
                    }
                    (
                        info.x as i32,
                        info.y as i32,
                        info.width as i32,
                        info.height as i32,
                    )
                }
            };

            width = width.max(x + w);
            height = height.max(y + h);
        }

        Ok((width.max(1) as u16, height.max(1) as u16))
    }

    fn set_screen_size(&self, width: u16, height: u16) -> Result<()> {
        let millimetres = |pixels: u16| ((pixels as f64) * 25.4 / ASSUMED_DPI).round() as u32;
        self.connection
            .randr_set_screen_size(
                self.root,
                width,
                height,
                millimetres(width),
                millimetres(height),
            )
            .context("resizing the X screen")?;
        self.connection.flush()?;
        Ok(())
    }
}

struct Change {
    crtc: randr::Crtc,
    mode: u32,
    width: u16,
    height: u16,
    x: i16,
    y: i16,
    rotation: randr::Rotation,
    outputs: Vec<randr::Output>,
}

fn rotated(width: u16, height: u16, rotation: randr::Rotation) -> (u16, u16) {
    if rotation.contains(randr::Rotation::ROTATE90) || rotation.contains(randr::Rotation::ROTATE270)
    {
        (height, width)
    } else {
        (width, height)
    }
}

fn refresh_of(mode: &randr::ModeInfo) -> f64 {
    let mut vertical = mode.vtotal as f64;
    if mode.mode_flags.contains(ModeFlag::DOUBLE_SCAN) {
        vertical *= 2.0;
    }
    if mode.mode_flags.contains(ModeFlag::INTERLACE) {
        vertical /= 2.0;
    }

    let divisor = mode.htotal as f64 * vertical;
    if divisor <= 0.0 {
        return 0.0;
    }
    mode.dot_clock as f64 / divisor
}

fn mode_id(mode: &randr::ModeInfo) -> String {
    format_mode_id(mode.width as u32, mode.height as u32, refresh_of(mode))
}

/// Pulls the manufacturer id, product code and serial out of an EDID block.
///
/// The formatting deliberately mirrors what Mutter reports (`SDC`, `0x419d`), so a config
/// written under GNOME still matches the same panel under i3.
fn parse_edid(data: &[u8]) -> Option<(String, String, String)> {
    if data.len() < 16 || data[..8] != [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00] {
        return None;
    }

    // Three five-bit letters packed big-endian into bytes 8 and 9.
    let packed = u16::from_be_bytes([data[8], data[9]]);
    let letter = |shift: u16| (b'A' - 1 + ((packed >> shift) & 0x1f) as u8) as char;
    let make: String = [letter(10), letter(5), letter(0)].into_iter().collect();

    if !make.chars().all(|c| c.is_ascii_uppercase()) {
        return None;
    }

    let product = u16::from_le_bytes([data[10], data[11]]);
    let serial = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);

    Some((make, format!("0x{product:04x}"), format!("0x{serial:08x}")))
}

/// The last resort: `xrandr` knows how to reflow a layout when our own arithmetic is
/// rejected, and it is present on effectively every X installation.
fn apply_with_xrandr(wanted: &[OutputSetting]) -> Result<()> {
    let mut args = Vec::new();

    for setting in wanted {
        let (width, height, refresh) = parse_mode_id(&setting.mode_id)
            .with_context(|| format!("unreadable mode id {}", setting.mode_id))?;
        args.push("--output".to_string());
        args.push(setting.connector.clone());
        args.push("--mode".to_string());
        args.push(format!("{width}x{height}"));
        args.push("--rate".to_string());
        args.push(format!("{refresh:.3}"));
    }

    let output = Command::new("xrandr")
        .args(&args)
        .output()
        .context("running xrandr")?;

    if !output.status.success() {
        bail!(
            "xrandr {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real EDID header followed by the Samsung panel in the machine this was written
    /// on: manufacturer `SDC`, product `0x419d`.
    fn sample_edid() -> Vec<u8> {
        let mut data = vec![0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];
        data.extend_from_slice(&[0x4c, 0x83]); // "SDC"
        data.extend_from_slice(&0x419du16.to_le_bytes());
        data.extend_from_slice(&0u32.to_le_bytes());
        data.resize(128, 0);
        data
    }

    #[test]
    fn edid_identity_matches_the_shape_mutter_reports() {
        let (make, model, serial) = parse_edid(&sample_edid()).unwrap();
        assert_eq!(make, "SDC");
        assert_eq!(model, "0x419d");
        assert_eq!(serial, "0x00000000");
    }

    #[test]
    fn a_missing_or_corrupt_edid_is_not_fatal() {
        assert!(parse_edid(&[]).is_none());
        assert!(parse_edid(&[0u8; 128]).is_none());
    }

    #[test]
    fn refresh_rate_comes_from_the_mode_timings() {
        let mode = randr::ModeInfo {
            id: 1,
            width: 1920,
            height: 1080,
            dot_clock: 148_500_000,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 2200,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 1125,
            name_len: 0,
            mode_flags: ModeFlag::default(),
        };

        assert!((refresh_of(&mode) - 60.0).abs() < 0.001);
        assert_eq!(mode_id(&mode), "1920x1080@60.000");
    }

    #[test]
    fn interlaced_modes_report_their_field_rate() {
        let mode = randr::ModeInfo {
            id: 2,
            width: 1920,
            height: 1080,
            dot_clock: 74_250_000,
            hsync_start: 0,
            hsync_end: 0,
            htotal: 2200,
            hskew: 0,
            vsync_start: 0,
            vsync_end: 0,
            vtotal: 1125,
            name_len: 0,
            mode_flags: ModeFlag::INTERLACE,
        };

        assert!((refresh_of(&mode) - 60.0).abs() < 0.001);
    }

    #[test]
    fn rotation_swaps_the_occupied_area() {
        assert_eq!(rotated(1920, 1080, randr::Rotation::ROTATE0), (1920, 1080));
        assert_eq!(rotated(1920, 1080, randr::Rotation::ROTATE90), (1080, 1920));
    }
}

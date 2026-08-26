//! One tab: everything that should happen while on battery, or while plugged in.

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{Align, Box as GtkBox, CheckButton, DropDown, Label, Orientation, StringList};

use powerdisplay_core::config::{OutputRule, Profile};
use powerdisplay_core::display::{Mode, Output};
use powerdisplay_core::power::profiles::describe;

const LEAVE_UNCHANGED: &str = "Leave unchanged";

/// The modes of one output, bucketed by resolution so the two dropdowns stay in step.
struct ResolutionGroup {
    width: u32,
    height: u32,
    preferred: bool,
    modes: Vec<Mode>,
}

fn group_modes(output: &Output) -> Vec<ResolutionGroup> {
    let mut groups: Vec<ResolutionGroup> = Vec::new();

    for mode in output.distinct_modes() {
        match groups
            .iter_mut()
            .find(|group| group.width == mode.width && group.height == mode.height)
        {
            Some(group) => {
                group.preferred |= mode.preferred;
                group.modes.push(mode.clone());
            }
            None => groups.push(ResolutionGroup {
                width: mode.width,
                height: mode.height,
                preferred: mode.preferred,
                modes: vec![mode.clone()],
            }),
        }
    }

    groups.sort_by_key(|group| std::cmp::Reverse(group.width as u64 * group.height as u64));
    for group in &mut groups {
        group.modes.sort_by(|a, b| b.refresh.total_cmp(&a.refresh));
    }
    groups
}

struct OutputRow {
    output: Output,
    groups: Rc<Vec<ResolutionGroup>>,
    enable: CheckButton,
    resolution: DropDown,
    refresh: DropDown,
}

impl OutputRow {
    fn build(output: &Output, rule: Option<&OutputRule>, on_change: Rc<dyn Fn()>) -> (Self, GtkBox) {
        let groups = Rc::new(group_modes(output));

        let enable = CheckButton::with_label(&output.display_name());
        enable.add_css_class("pd-output-name");

        let resolution = DropDown::from_strings(
            &groups
                .iter()
                .map(|group| resolution_label(group))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );
        let refresh = DropDown::from_strings(&[]);

        // Start from whatever the display is doing right now, so enabling the checkbox
        // without touching anything else is a no-op rather than a surprise.
        let selected_mode = rule
            .and_then(|rule| rule.mode.as_deref())
            .and_then(|wanted| output.resolve_mode(wanted))
            .or_else(|| {
                output
                    .current_mode
                    .as_deref()
                    .and_then(|current| output.resolve_mode(current))
            })
            .cloned();

        let group_index = selected_mode
            .as_ref()
            .and_then(|mode| {
                groups
                    .iter()
                    .position(|group| group.width == mode.width && group.height == mode.height)
            })
            .unwrap_or(0);
        resolution.set_selected(group_index as u32);

        fill_refresh(&refresh, &groups, group_index, selected_mode.as_ref());

        let controls = GtkBox::new(Orientation::Horizontal, 12);
        controls.set_margin_start(28);
        controls.append(&field("Resolution", &resolution));
        controls.append(&field("Refresh rate", &refresh));

        let row = GtkBox::new(Orientation::Vertical, 8);
        row.append(&enable);
        row.append(&controls);

        if groups.is_empty() {
            enable.set_sensitive(false);
            enable.set_tooltip_text(Some("This display reports no modes"));
        }

        enable.set_active(rule.and_then(|rule| rule.mode.as_ref()).is_some());
        enable
            .bind_property("active", &controls, "sensitive")
            .sync_create()
            .build();

        {
            let groups = groups.clone();
            let refresh = refresh.clone();
            resolution.connect_selected_notify(move |dropdown| {
                fill_refresh(&refresh, &groups, dropdown.selected() as usize, None);
            });
        }

        enable.connect_toggled({
            let on_change = on_change.clone();
            move |_| on_change()
        });
        resolution.connect_selected_notify({
            let on_change = on_change.clone();
            move |_| on_change()
        });
        refresh.connect_selected_notify({
            let on_change = on_change.clone();
            move |_| on_change()
        });

        (
            Self {
                output: output.clone(),
                groups,
                enable,
                resolution,
                refresh,
            },
            row,
        )
    }

    /// `None` when this display is set to be left alone.
    fn collect(&self) -> Option<OutputRule> {
        if !self.enable.is_active() {
            return None;
        }

        let group = self.groups.get(self.resolution.selected() as usize)?;
        let mode = group.modes.get(self.refresh.selected() as usize)?;

        Some(OutputRule {
            matcher: self.output.matcher(),
            mode: Some(mode.id.clone()),
        })
    }
}

fn resolution_label(group: &ResolutionGroup) -> String {
    let base = format!("{} × {}", group.width, group.height);
    if group.preferred {
        format!("{base}  (native)")
    } else {
        base
    }
}

fn fill_refresh(
    refresh: &DropDown,
    groups: &[ResolutionGroup],
    group_index: usize,
    wanted: Option<&Mode>,
) {
    let Some(group) = groups.get(group_index) else {
        refresh.set_model(None::<&StringList>);
        return;
    };

    let labels: Vec<String> = group
        .modes
        .iter()
        .map(|mode| {
            if mode.current {
                format!("{}  (current)", mode.refresh_label())
            } else {
                mode.refresh_label()
            }
        })
        .collect();

    let model = StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>());
    refresh.set_model(Some(&model));

    let selected = wanted
        .and_then(|wanted| group.modes.iter().position(|mode| mode.id == wanted.id))
        // Otherwise the fastest mode, which is what someone opening this app is usually after.
        .unwrap_or(0);
    refresh.set_selected(selected as u32);
}

fn field(label: &str, widget: &impl IsA<gtk::Widget>) -> GtkBox {
    let container = GtkBox::new(Orientation::Vertical, 4);
    // Otherwise a lone dropdown stretches across the whole card.
    container.set_halign(Align::Start);
    let caption = Label::new(Some(label));
    caption.add_css_class("pd-dim");
    caption.set_xalign(0.0);
    container.append(&caption);
    container.append(widget);
    container
}

pub struct ProfilePage {
    pub root: gtk::ScrolledWindow,
    power_profile_names: Vec<String>,
    power_profile: DropDown,
    persist: CheckButton,
    rows: Vec<OutputRow>,
}

impl ProfilePage {
    pub fn build(
        profile: &Profile,
        outputs: &[Output],
        power_profiles: &[String],
        supports_persist: bool,
        on_change: Rc<dyn Fn()>,
    ) -> Self {
        let content = GtkBox::new(Orientation::Vertical, 18);
        content.add_css_class("pd-page");

        // Performance card
        let performance = card("Performance");
        let mut names: Vec<String> = vec![LEAVE_UNCHANGED.to_string()];
        names.extend(power_profiles.iter().cloned());
        let power_profile = DropDown::from_strings(
            &names
                .iter()
                .map(|name| pretty_profile(name))
                .collect::<Vec<_>>()
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
        );

        let selected = profile
            .power_profile
            .as_deref()
            .and_then(|wanted| names.iter().position(|name| name == wanted))
            .unwrap_or(0);
        power_profile.set_selected(selected as u32);
        power_profile.set_sensitive(!power_profiles.is_empty());
        power_profile.connect_selected_notify({
            let on_change = on_change.clone();
            move |_| on_change()
        });

        if power_profiles.is_empty() {
            power_profile.set_tooltip_text(Some(
                "No power-profiles-daemon or tuned-ppd is running on this system",
            ));
        } else if let Some(name) = profile.power_profile.as_deref() {
            power_profile.set_tooltip_text(Some(describe(name)));
        }

        performance.append(&field("Power profile", &power_profile));

        if power_profiles.is_empty() {
            let note = Label::new(Some(
                "Install power-profiles-daemon or tuned-ppd to switch performance modes.",
            ));
            note.add_css_class("pd-dim");
            note.set_xalign(0.0);
            note.set_wrap(true);
            performance.append(&note);
        }

        content.append(&performance);

        // Displays card
        let displays = card("Displays");
        let mut rows = Vec::new();

        if outputs.is_empty() {
            let empty = Label::new(Some("No displays were reported by this session."));
            empty.add_css_class("pd-empty");
            displays.append(&empty);
        }

        for output in outputs {
            let rule = profile.outputs.iter().find(|rule| {
                rule.matcher
                    .score(&output.connector, &output.make, &output.model, &output.serial)
                    .is_some()
            });

            let (row, widget) = OutputRow::build(output, rule, on_change.clone());
            rows.push(row);
            displays.append(&widget);
        }

        let persist = CheckButton::with_label("Remember this layout in the desktop's display settings");
        persist.set_active(profile.persist_display_config);
        persist.set_sensitive(supports_persist);
        persist.set_tooltip_text(Some(if supports_persist {
            "Off: the change is temporary and your saved display settings are left alone"
        } else {
            "This desktop always remembers display changes"
        }));
        persist.connect_toggled({
            let on_change = on_change.clone();
            move |_| on_change()
        });
        displays.append(&persist);

        content.append(&displays);

        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&content)
            .build();

        Self {
            root,
            power_profile_names: names,
            power_profile,
            persist,
            rows,
        }
    }

    /// Reads the widgets back into a config profile.
    pub fn collect(&self) -> Profile {
        let selected = self.power_profile.selected() as usize;
        Profile {
            power_profile: (selected > 0)
                .then(|| self.power_profile_names.get(selected).cloned())
                .flatten(),
            persist_display_config: self.persist.is_active(),
            outputs: self.rows.iter().filter_map(OutputRow::collect).collect(),
        }
    }
}

fn card(title: &str) -> GtkBox {
    let container = GtkBox::new(Orientation::Vertical, 12);
    container.add_css_class("pd-card");

    let label = Label::new(Some(title));
    label.add_css_class("pd-section-title");
    label.set_xalign(0.0);
    label.set_halign(Align::Start);
    container.append(&label);

    container
}

fn pretty_profile(name: &str) -> String {
    match name {
        LEAVE_UNCHANGED => name.to_string(),
        "power-saver" => "Power saver".to_string(),
        "balanced" => "Balanced".to_string(),
        "balanced-performance" => "Balanced performance".to_string(),
        "performance" => "Performance".to_string(),
        other => {
            let mut chars = other.replace(['-', '_'], " ");
            if let Some(first) = chars.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            chars
        }
    }
}

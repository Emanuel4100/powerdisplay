use std::cell::RefCell;
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{
    ActionBar, Align, ApplicationWindow, Box as GtkBox, Button, CheckButton, HeaderBar, Label,
    MenuButton, Orientation, Popover, Separator, Stack, StackSwitcher, Switch,
};

use powerdisplay_core::config::{Config, PowerState};
use powerdisplay_core::engine::Engine;
use powerdisplay_core::power;

use crate::autostart;
use crate::page::ProfilePage;

/// Builds, or rebuilds, the whole window contents.
///
/// Everything is thrown away and recreated when the display set changes, which is far
/// easier to reason about than patching a tree of dropdowns in place, and happens rarely
/// enough that the cost does not matter.
pub fn populate(window: &ApplicationWindow) {
    let header = HeaderBar::new();
    window.set_titlebar(Some(&header));

    match Engine::new(false) {
        Ok(engine) => build_main(window, &header, engine),
        Err(err) => build_error(window, &header, &format!("{err:#}")),
    }
}

fn build_error(window: &ApplicationWindow, header: &HeaderBar, message: &str) {
    header.set_title_widget(Some(&Label::new(Some("powerdisplay"))));

    let content = GtkBox::new(Orientation::Vertical, 12);
    content.set_valign(Align::Center);
    content.set_halign(Align::Center);
    content.set_margin_top(48);
    content.set_margin_bottom(48);
    content.set_margin_start(48);
    content.set_margin_end(48);

    let icon = gtk::Image::from_icon_name("dialog-error-symbolic");
    icon.set_pixel_size(48);
    content.append(&icon);

    let title = Label::new(Some("This session cannot be controlled"));
    title.add_css_class("pd-section-title");
    content.append(&title);

    let detail = Label::new(Some(message));
    detail.add_css_class("pd-dim");
    detail.set_wrap(true);
    detail.set_justify(gtk::Justification::Center);
    detail.set_max_width_chars(60);
    content.append(&detail);

    let hint = Label::new(Some(
        "Set POWERDISPLAY_BACKEND to gnome, kde, wlroots or x11 to force a backend.",
    ));
    hint.add_css_class("pd-dim");
    hint.set_wrap(true);
    content.append(&hint);

    let retry = Button::with_label("Try again");
    retry.set_halign(Align::Center);
    retry.connect_clicked({
        let window = window.clone();
        move |_| populate(&window)
    });
    content.append(&retry);

    window.set_child(Some(&content));
}

fn build_main(window: &ApplicationWindow, header: &HeaderBar, engine: Engine) {
    let engine = Rc::new(engine);
    let config = Rc::new(RefCell::new(Config::load().unwrap_or_default()));
    let power_profiles = engine.power_profiles();

    let (outputs, enumeration_error) = match engine.outputs() {
        Ok(outputs) => (outputs, None),
        Err(err) => (Vec::new(), Some(format!("{err:#}"))),
    };

    let stack = Stack::builder()
        .transition_type(gtk::StackTransitionType::SlideLeftRight)
        .build();
    header.set_title_widget(Some(&StackSwitcher::builder().stack(&stack).build()));

    let status = Label::new(None);
    status.add_css_class("pd-status");
    status.set_ellipsize(gtk::pango::EllipsizeMode::End);

    let save = Button::with_label("Save");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);

    let on_change: Rc<dyn Fn()> = Rc::new({
        let save = save.clone();
        let status = status.clone();
        move || {
            save.set_sensitive(true);
            set_status(&status, "Unsaved changes", false);
        }
    });

    let battery = Rc::new(ProfilePage::build(
        &config.borrow().on_battery,
        &outputs,
        &power_profiles,
        engine.supports_persist(),
        on_change.clone(),
    ));
    let ac = Rc::new(ProfilePage::build(
        &config.borrow().on_ac,
        &outputs,
        &power_profiles,
        engine.supports_persist(),
        on_change.clone(),
    ));

    stack.add_titled(&battery.root, Some("battery"), "On battery");
    stack.add_titled(&ac.root, Some("ac"), "Plugged in");

    // Open on the tab describing the situation the user is in right now.
    let live_state = power::read_state();
    stack.set_visible_child_name(match live_state {
        PowerState::Battery => "battery",
        PowerState::Ac => "ac",
    });

    let apply = Button::with_label("Apply now");
    apply.set_tooltip_text(Some(
        "Apply the settings on this tab immediately, without saving them",
    ));

    let enabled = Switch::new();
    enabled.set_active(config.borrow().enabled);
    enabled.set_valign(Align::Center);
    enabled.set_tooltip_text(Some(
        "When off, the background service watches but never changes anything",
    ));
    enabled.connect_active_notify({
        let on_change = on_change.clone();
        move |_| on_change()
    });

    header.pack_end(&menu_button(window, &engine, live_state));

    let collect = {
        let config = config.clone();
        let battery = battery.clone();
        let ac = ac.clone();
        let enabled = enabled.clone();
        move || {
            let mut config = config.borrow_mut();
            config.version = powerdisplay_core::config::CONFIG_VERSION;
            config.enabled = enabled.is_active();
            config.on_battery = battery.collect();
            config.on_ac = ac.collect();
            config.clone()
        }
    };

    save.connect_clicked({
        let collect = collect.clone();
        let status = status.clone();
        let save = save.clone();
        move |_| {
            let config = collect();
            match config.save() {
                Ok(()) => {
                    save.set_sensitive(false);
                    let message = if autostart::is_running() {
                        "Saved. The background service has picked it up."
                    } else if autostart::available() {
                        "Saved. Turn on automatic switching in the menu to apply it in the background."
                    } else {
                        "Saved. Start powerdisplayd to apply it automatically."
                    };
                    set_status(&status, message, false);
                }
                Err(err) => set_status(&status, &format!("Could not save: {err:#}"), true),
            }
        }
    });

    apply.connect_clicked({
        let collect = collect.clone();
        let stack = stack.clone();
        let engine = engine.clone();
        let status = status.clone();
        move |_| {
            let mut config = collect();
            config.enabled = true;

            let state = match stack.visible_child_name().as_deref() {
                Some("battery") => PowerState::Battery,
                _ => PowerState::Ac,
            };

            let report = engine.apply(&config, state);
            set_status(&status, &report.summary(), !report.succeeded());
        }
    });

    let footer = ActionBar::new();
    let switch_box = GtkBox::new(Orientation::Horizontal, 8);
    switch_box.append(&enabled);
    switch_box.append(&Label::new(Some("Automatic switching")));
    footer.pack_start(&switch_box);
    footer.set_center_widget(Some(&status));
    footer.pack_end(&save);
    footer.pack_end(&apply);

    let content = GtkBox::new(Orientation::Vertical, 0);
    content.append(&stack);
    content.append(&Separator::new(Orientation::Horizontal));
    content.append(&footer);

    window.set_child(Some(&content));

    if let Some(err) = enumeration_error {
        set_status(&status, &format!("Could not list displays: {err}"), true);
    }
}

fn menu_button(window: &ApplicationWindow, engine: &Rc<Engine>, state: PowerState) -> MenuButton {
    let content = GtkBox::new(Orientation::Vertical, 10);
    content.set_margin_top(10);
    content.set_margin_bottom(10);
    content.set_margin_start(10);
    content.set_margin_end(10);

    if autostart::available() {
        let run = CheckButton::with_label("Run automatically in the background");
        run.set_active(autostart::is_enabled());
        run.connect_toggled(|button| {
            if let Err(err) = autostart::set_enabled(button.is_active()) {
                tracing::error!("{err:#}");
                // Put the toggle back where it was rather than lying about the state.
                button.set_active(!button.is_active());
            }
        });
        content.append(&run);
        content.append(&Separator::new(Orientation::Horizontal));
    }

    let refresh = Button::with_label("Refresh displays");
    refresh.connect_clicked({
        let window = window.clone();
        move |_| populate(&window)
    });
    content.append(&refresh);
    content.append(&Separator::new(Orientation::Horizontal));

    for line in [
        format!("Session: {}", engine.backend_name()),
        format!("Power source: {}", state.label()),
        match engine.power_profile_service() {
            Some(service) => format!("Power profiles: {service}"),
            None => "Power profiles: unavailable".to_string(),
        },
    ] {
        let label = Label::new(Some(&line));
        label.add_css_class("pd-dim");
        label.set_xalign(0.0);
        content.append(&label);
    }

    let popover = Popover::new();
    popover.set_child(Some(&content));

    MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .popover(&popover)
        .tooltip_text("Options")
        .build()
}

fn set_status(status: &Label, message: &str, is_error: bool) {
    status.set_text(message);
    status.set_tooltip_text(Some(message));
    if is_error {
        status.add_css_class("pd-error");
    } else {
        status.remove_css_class("pd-error");
    }
}

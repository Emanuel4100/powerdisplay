//! `powerdisplay` - the settings window.
//!
//! The window only ever reads and writes the config file; `powerdisplayd` is what watches
//! the power source. That keeps the resident process free of GTK and means closing the
//! window changes nothing about the automation.

mod autostart;
mod page;
mod window;

use gtk::prelude::*;
use gtk::{Application, ApplicationWindow, gdk};

const APP_ID: &str = "io.github.Emanuel4100.PowerDisplay";
const STYLE: &str = include_str!("style.css");

fn main() -> gtk::glib::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .without_time()
        .init();

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build_ui);
    app.run()
}

fn load_css() {
    let Some(display) = gdk::Display::default() else {
        return;
    };

    let provider = gtk::CssProvider::new();
    // `load_from_string` would need GTK 4.12; this keeps the app buildable against the
    // GTK 4.6/4.8 that long-term-support distros still ship.
    #[allow(deprecated)]
    provider.load_from_data(STYLE);
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
}

fn build_ui(app: &Application) {
    let window = ApplicationWindow::builder()
        .application(app)
        .title("powerdisplay")
        .default_width(680)
        .default_height(-1)
        .build();

    window.set_icon_name(Some(APP_ID));
    window::populate(&window);
    window.present();
}

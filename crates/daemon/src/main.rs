//! `powerdisplayd` - watches the power source and applies the matching profile.
//!
//! Deliberately single-threaded at the decision level: watcher threads only push events
//! into a channel, and one loop debounces them and calls the engine. That keeps "what is
//! the current state" in exactly one place.

use std::process::ExitCode;
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use powerdisplay_core::config::{Config, PowerState};
use powerdisplay_core::engine::Engine;
use powerdisplay_core::events::{Event, EventSources};
use powerdisplay_core::power;
use powerdisplay_core::{Controller, instance, sandbox};

/// How often to check that GNOME Software (or `flatpak uninstall`) has not removed us.
const UNINSTALL_POLL: Duration = Duration::from_secs(5);

const USAGE: &str = "\
powerdisplayd - apply a display mode and power profile based on the power source

Usage: powerdisplayd [OPTIONS]

Options:
      --apply-now   Apply the profile for the current power source, then exit
      --show        Print the detected session, power source and available modes
      --dry-run     Log what would change without changing anything
      --self-test   Probe sysfs, udev, D-Bus and the display backend, then exit
  -v, --verbose     Log at debug level
  -V, --version     Print the version
  -h, --help        Print this message

Configuration lives in $XDG_CONFIG_HOME/powerdisplay/config.toml and is re-read
automatically when it changes. Set POWERDISPLAY_BACKEND to one of gnome, kde,
wlroots or x11 to override display backend detection.
";

#[derive(Default)]
struct Options {
    apply_now: bool,
    show: bool,
    dry_run: bool,
    self_test: bool,
    verbose: bool,
}

fn main() -> ExitCode {
    let options = match parse_args() {
        Ok(Some(options)) => options,
        Ok(None) => return ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("powerdisplayd: {message}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };

    let level = if options.verbose { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_writer(std::io::stderr)
        .with_target(false)
        .without_time()
        .init();

    match run(&options) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!("{err:#}");
            ExitCode::FAILURE
        }
    }
}

fn parse_args() -> Result<Option<Options>, String> {
    let mut options = Options::default();
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--apply-now" => options.apply_now = true,
            "--show" => options.show = true,
            "--dry-run" => options.dry_run = true,
            "--self-test" => options.self_test = true,
            "-v" | "--verbose" => options.verbose = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("powerdisplayd {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(Some(options))
}

fn run(options: &Options) -> Result<()> {
    if options.self_test {
        return run_self_test();
    }

    if options.show {
        return show(options.dry_run);
    }

    if options.apply_now {
        let engine = Engine::new(options.dry_run)?;
        let config = Config::load().context("loading the configuration")?;
        let state = power::read_state();
        let report = engine.apply(&config, state);
        log_report(state, &report);
        return Ok(());
    }

    let _lock = match instance::try_acquire()? {
        Some(file) => file,
        None => {
            tracing::info!("another powerdisplayd is already running");
            return Ok(());
        }
    };

    let config = Config::load().context("loading the configuration")?;
    watch(wait_for_session(options.dry_run), config)
}

fn run_self_test() -> Result<()> {
    let report = sandbox::probe();
    print!("{}", sandbox::format_report(&report));
    if report.passed() {
        Ok(())
    } else {
        anyhow::bail!("self-test failed")
    }
}

/// Started at login by the desktop portal, the daemon can easily win the race against the
/// compositor. Waiting is friendlier than exiting and being restarted in a loop, and it
/// also covers the compositor being restarted underneath us.
fn wait_for_session(dry_run: bool) -> Engine {
    let mut delay = Duration::from_secs(2);
    let mut reported = false;

    loop {
        match Engine::new(dry_run) {
            Ok(engine) => return engine,
            Err(err) => {
                if !reported {
                    tracing::warn!("waiting for a usable session: {err:#}");
                    reported = true;
                }
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

/// Whatever we can see, printed in the order that survives failure: the power source and
/// the profiles service do not depend on the compositor, and a session we cannot drive is
/// precisely when someone runs this.
fn show(dry_run: bool) -> Result<()> {
    println!("Power source: {}", power::read_state().label());
    println!("Config:       {}", powerdisplay_core::config::config_path()?.display());

    match power::PowerProfiles::connect() {
        Some(profiles) => {
            let active = profiles.active().unwrap_or_default();
            println!("Power profiles: {}", profiles.service_name());
            for profile in profiles.available().unwrap_or_default() {
                let marker = if profile == active { "*" } else { " " };
                println!("  {marker} {profile}");
            }
        }
        None => println!("Power profiles: none available"),
    }

    let engine = match Engine::new(dry_run) {
        Ok(engine) => engine,
        Err(err) => {
            println!("Displays:     unavailable");
            println!();
            println!("{err:#}");
            println!();
            println!("Set POWERDISPLAY_BACKEND to gnome, kde, wlroots or x11 to force one.");
            return Ok(());
        }
    };

    println!("Backend:      {}", engine.backend_name());
    println!();

    for output in engine.outputs()? {
        println!(
            "{} [{}]",
            output.display_name(),
            if output.enabled { "active" } else { "inactive" }
        );
        for mode in &output.modes {
            let marker = if Some(&mode.id) == output.current_mode.as_ref() {
                "*"
            } else {
                " "
            };
            println!("  {marker} {:<28} {}", mode.id, mode.refresh_label());
        }
        println!();
    }

    Ok(())
}

fn watch(engine: Engine, config: Config) -> Result<()> {
    let sources = EventSources::spawn().context("starting the system watchers")?;

    tracing::info!(
        backend = engine.backend_name(),
        "watching for power source changes"
    );

    let mut controller = Controller::new(config, power::read_state(), Instant::now());

    loop {
        if !instance::app_still_installed() {
            instance::cleanup_after_uninstall();
            return Ok(());
        }

        // Apply *before* reading the next event. After our own mode set, DRM udev events
        // keep arriving; a `recv_timeout(0)` on a non-empty channel would starve the
        // timeout branch and never apply again — which is how GNOME's saved 60 Hz layout
        // won over the AC profile after a reinstall.
        if controller.take_due(Instant::now()) {
            let state = power::read_state();
            controller.applied(state);
            let report = engine.apply(controller.config(), state);
            log_report(state, &report);
            continue;
        }

        let wait = match controller.wait(Instant::now()) {
            Some(timeout) => timeout.min(UNINSTALL_POLL),
            None => UNINSTALL_POLL,
        };
        let received = sources.rx.recv_timeout(wait);

        match received {
            Ok(event) => {
                tracing::debug!(?event, "event");
                match event {
                    Event::Power(new_state) => {
                        if new_state != controller.state() {
                            tracing::info!(state = new_state.label(), "power source changed");
                        }
                        controller.on_event(event, Instant::now());
                    }
                    Event::ConfigChanged => match Config::load() {
                        Ok(reloaded) => {
                            if controller.reload_config(reloaded, Instant::now()) {
                                tracing::info!("configuration reloaded");
                            }
                        }
                        Err(err) => {
                            tracing::error!("keeping the previous configuration: {err:#}");
                        }
                    },
                    Event::DisplaysChanged | Event::Resumed => {
                        controller.on_event(event, Instant::now());
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                // The next loop iteration's take_due handles the apply. Falling through
                // keeps one code path for "it is time".
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("all system watchers stopped");
            }
        }
    }
}

fn log_report(state: PowerState, report: &powerdisplay_core::ApplyReport) {
    for warning in &report.warnings {
        tracing::warn!("{warning}");
    }
    for error in &report.errors {
        tracing::error!("{error}");
    }
    if !report.actions.is_empty() {
        tracing::info!(state = state.label(), "{}", report.actions.join("; "));
    } else if report.errors.is_empty() && report.warnings.is_empty() {
        tracing::debug!(state = state.label(), "already in the requested state");
    }
}

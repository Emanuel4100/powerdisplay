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

const USAGE: &str = "\
powerdisplayd - apply a display mode and power profile based on the power source

Usage: powerdisplayd [OPTIONS]

Options:
      --apply-now   Apply the profile for the current power source, then exit
      --show        Print the detected session, power source and available modes
      --dry-run     Log what would change without changing anything
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
    if options.show {
        return show(&Engine::new(options.dry_run)?);
    }

    if options.apply_now {
        let engine = Engine::new(options.dry_run)?;
        let config = Config::load().context("loading the configuration")?;
        let state = power::read_state();
        let report = engine.apply(&config, state);
        log_report(state, &report);
        return Ok(());
    }

    let config = Config::load().context("loading the configuration")?;
    watch(wait_for_session(options.dry_run), config)
}

/// Started from a systemd user unit, the daemon can easily win the race against the
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

fn show(engine: &Engine) -> Result<()> {
    println!("Backend:      {}", engine.backend_name());
    println!("Power source: {}", power::read_state().label());

    match engine.power_profile_service() {
        Some(service) => {
            let active = engine.active_power_profile().unwrap_or_default();
            println!("Power profiles: {service}");
            for profile in engine.power_profiles() {
                let marker = if profile == active { "*" } else { " " };
                println!("  {marker} {profile}");
            }
        }
        None => println!("Power profiles: none available"),
    }

    println!("Config:       {}", powerdisplay_core::config::config_path()?.display());
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

fn watch(engine: Engine, mut config: Config) -> Result<()> {
    let sources = EventSources::spawn().context("starting the system watchers")?;

    tracing::info!(
        backend = engine.backend_name(),
        "watching for power source changes"
    );

    let mut state = power::read_state();
    let mut pending = config
        .apply_on_start
        .then(|| Pending::new(Event::Power(state)));

    loop {
        let deadline = pending.as_ref().map(Pending::deadline);
        let received = match deadline {
            Some(at) => sources
                .rx
                .recv_timeout(at.saturating_duration_since(Instant::now())),
            None => sources.rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };

        match received {
            Ok(event) => {
                tracing::debug!(?event, "event");
                match event {
                    Event::Power(new_state) => {
                        if new_state != state {
                            tracing::info!(state = new_state.label(), "power source changed");
                        }
                        state = new_state;
                    }
                    Event::ConfigChanged => match Config::load() {
                        Ok(reloaded) => {
                            // Editors touch a file several times per save, and a
                            // re-read that changed nothing is not worth acting on.
                            if reloaded == config {
                                continue;
                            }
                            tracing::info!("configuration reloaded");
                            config = reloaded;
                        }
                        Err(err) => {
                            tracing::error!("keeping the previous configuration: {err:#}");
                            continue;
                        }
                    },
                    Event::DisplaysChanged | Event::Resumed => {}
                }

                // Coalesce: every new event pushes the deadline out, so a burst of dock
                // events produces exactly one apply once things go quiet.
                pending = Some(match pending {
                    Some(pending) => pending.extend(event),
                    None => Pending::new(event),
                });
            }
            Err(RecvTimeoutError::Timeout) => {
                pending = None;
                // Re-read rather than trusting the last event: the state may have flipped
                // back and forth while we were waiting for it to settle.
                state = power::read_state();
                let report = engine.apply(&config, state);
                log_report(state, &report);
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("all system watchers stopped");
            }
        }
    }
}

/// An apply that is waiting for events to stop arriving.
struct Pending {
    settles_at: Instant,
    /// Hard limit on the coalescing, so a misbehaving event source cannot postpone the
    /// apply indefinitely by trickling events in.
    latest: Instant,
}

impl Pending {
    const MAX_WAIT: Duration = Duration::from_secs(10);

    fn new(event: Event) -> Self {
        let now = Instant::now();
        Self {
            settles_at: now + event.settle_delay(),
            latest: now + Self::MAX_WAIT,
        }
    }

    fn extend(self, event: Event) -> Self {
        Self {
            settles_at: (Instant::now() + event.settle_delay()).max(self.settles_at),
            latest: self.latest,
        }
    }

    fn deadline(&self) -> Instant {
        self.settles_at.min(self.latest)
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

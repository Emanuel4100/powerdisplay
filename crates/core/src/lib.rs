pub mod config;
pub mod display;
pub mod engine;
pub mod events;
pub mod power;
pub mod watch;

/// Whether we are running inside a Flatpak sandbox.
///
/// Two things change in there and both are load-bearing: desktop tools such as
/// `kscreen-doctor` live on the host rather than on our `PATH`, and autostart has to go
/// through the desktop portal because nothing inside may write a unit onto the host.
pub fn sandboxed() -> bool {
    std::path::Path::new("/.flatpak-info").exists()
}

pub use config::{Config, PowerState, Profile};
pub use display::{DisplayBackend, Mode, Output, OutputSetting};
pub use engine::{ApplyReport, Engine};
pub use events::{Event, EventSources};
pub use power::PowerProfiles;

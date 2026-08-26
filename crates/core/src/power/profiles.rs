//! Performance mode, via whichever power-profiles service the distro ships.
//!
//! `power-profiles-daemon` renamed its bus name from `net.hadess.PowerProfiles` to
//! `org.freedesktop.UPower.PowerProfiles` in 0.20, and Fedora 41+ serves the newer name
//! from `tuned-ppd` instead. All three are the same interface, so we probe for whichever
//! one answers and treat "nothing answers" as a supported configuration.

use std::collections::HashMap;

use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

struct Endpoint {
    service: &'static str,
    path: &'static str,
    interface: &'static str,
}

const ENDPOINTS: [Endpoint; 2] = [
    Endpoint {
        service: "org.freedesktop.UPower.PowerProfiles",
        path: "/org/freedesktop/UPower/PowerProfiles",
        interface: "org.freedesktop.UPower.PowerProfiles",
    },
    Endpoint {
        service: "net.hadess.PowerProfiles",
        path: "/net/hadess/PowerProfiles",
        interface: "net.hadess.PowerProfiles",
    },
];

pub struct PowerProfiles {
    connection: Connection,
    endpoint: &'static Endpoint,
}

impl PowerProfiles {
    /// Returns `None` when no power-profiles service is running, which is a normal
    /// situation on minimal installs rather than an error.
    pub fn connect() -> Option<Self> {
        let connection = match Connection::system() {
            Ok(connection) => connection,
            Err(err) => {
                tracing::warn!(%err, "no system bus; performance modes are unavailable");
                return None;
            }
        };

        for endpoint in &ENDPOINTS {
            let candidate = Self {
                connection: connection.clone(),
                endpoint,
            };
            if candidate.active().is_ok() {
                tracing::info!(service = endpoint.service, "found a power profiles service");
                return Some(candidate);
            }
        }

        tracing::info!("no power-profiles-daemon or tuned-ppd on the bus");
        None
    }

    pub fn service_name(&self) -> &'static str {
        self.endpoint.service
    }

    fn proxy(&self) -> Result<Proxy<'_>> {
        Proxy::new(
            &self.connection,
            self.endpoint.service,
            self.endpoint.path,
            self.endpoint.interface,
        )
        .context("creating the power profiles proxy")
    }

    pub fn active(&self) -> Result<String> {
        self.proxy()?
            .get_property::<String>("ActiveProfile")
            .context("reading ActiveProfile")
    }

    /// Profile names in the order the daemon advertises them, coldest first.
    pub fn available(&self) -> Result<Vec<String>> {
        let raw = self
            .proxy()?
            .get_property::<Vec<HashMap<String, OwnedValue>>>("Profiles")
            .context("reading Profiles")?;

        Ok(raw
            .iter()
            .filter_map(|entry| match &**entry.get("Profile")? {
                Value::Str(name) => Some(name.to_string()),
                _ => None,
            })
            .collect())
    }

    pub fn set_active(&self, profile: &str) -> Result<()> {
        self.proxy()?
            .set_property("ActiveProfile", Value::new(profile.to_string()))
            .with_context(|| format!("switching to the {profile} power profile"))?;
        Ok(())
    }

    /// Switches only when needed, so repeated applies stay quiet in the logs and do not
    /// stomp on a hold placed by another application.
    pub fn ensure_active(&self, profile: &str) -> Result<bool> {
        if self.active().ok().as_deref() == Some(profile) {
            return Ok(false);
        }
        self.set_active(profile)?;
        Ok(true)
    }
}

/// A sensible ordering hint for the UI when the daemon reports unfamiliar names.
pub fn describe(profile: &str) -> &'static str {
    match profile {
        "power-saver" => "Longest battery life, lowest clocks",
        "balanced" => "The system default",
        "performance" => "Highest clocks, more heat and fan noise",
        _ => "Provided by the power profiles daemon",
    }
}

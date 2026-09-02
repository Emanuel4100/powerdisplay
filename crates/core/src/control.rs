//! The daemon's decision loop, pulled out of `main` so it can be tested without threads.
//!
//! Watcher threads only enqueue [`Event`]s. This type is the single place that decides
//! when those events have settled enough to apply a profile.

use std::time::{Duration, Instant};

use crate::config::{Config, PowerState};
use crate::events::Event;

/// An apply that is waiting for events to stop arriving.
#[derive(Clone, Debug)]
struct Pending {
    settles_at: Instant,
    /// Hard limit on the coalescing, so a misbehaving event source cannot postpone the
    /// apply indefinitely by trickling events in.
    latest: Instant,
}

impl Pending {
    const MAX_WAIT: Duration = Duration::from_secs(10);

    fn new(event: Event, now: Instant) -> Self {
        Self {
            settles_at: now + event.settle_delay(),
            latest: now + Self::MAX_WAIT,
        }
    }

    /// A retry is not coalescing anything, so its own delay is also its hard limit.
    fn after(delay: Duration, now: Instant) -> Self {
        Self {
            settles_at: now + delay,
            latest: now + delay.max(Self::MAX_WAIT),
        }
    }

    fn extend(&mut self, event: Event, now: Instant) {
        self.settles_at = (now + event.settle_delay()).max(self.settles_at);
    }

    fn deadline(&self) -> Instant {
        self.settles_at.min(self.latest)
    }
}

/// Debounces power, display, config and resume events into a single apply.
pub struct Controller {
    state: PowerState,
    config: Config,
    pending: Option<Pending>,
    retries: usize,
}

impl Controller {
    /// Delays between attempts after an apply fails. A compositor that refuses right now
    /// usually stops refusing shortly afterwards — GNOME rejects configuration changes
    /// while the screen is locked or a settings dialog is open — so the first retries are
    /// quick and the last one covers a longer interruption.
    const RETRY_BACKOFF: [Duration; 4] = [
        Duration::from_secs(1),
        Duration::from_secs(3),
        Duration::from_secs(10),
        Duration::from_secs(30),
    ];

    pub fn new(config: Config, state: PowerState, now: Instant) -> Self {
        let pending = config.apply_on_start.then(|| Pending::new(Event::Power(state), now));
        Self {
            state,
            config,
            pending,
            retries: 0,
        }
    }

    pub fn state(&self) -> PowerState {
        self.state
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// How long the caller should wait for the next event before treating the pending
    /// apply as due. `None` means block indefinitely.
    pub fn wait(&self, now: Instant) -> Option<Duration> {
        Some(self.pending.as_ref()?.deadline().saturating_duration_since(now))
    }

    /// True when an apply should run *now*, even if more events are already queued.
    ///
    /// Checking this before `recv` is load-bearing: a DRM flood after our own mode set
    /// would otherwise keep `recv_timeout(0)` returning `Ok(event)` forever and the
    /// apply would never run.
    pub fn due(&self, now: Instant) -> bool {
        self.pending
            .as_ref()
            .is_some_and(|pending| now >= pending.deadline())
    }

    /// Consume a due apply. The caller should re-read the power source, then call
    /// [`Self::applied`].
    pub fn take_due(&mut self, now: Instant) -> bool {
        if !self.due(now) {
            return false;
        }
        self.pending = None;
        true
    }

    /// Record the power source used for a completed apply, so a duplicate reading does
    /// not start another debounce.
    pub fn applied(&mut self, state: PowerState) {
        self.state = state;
        self.retries = 0;
    }

    /// Schedule another attempt after an apply failed, returning how long the caller will
    /// wait. `None` means the retries for this change are exhausted.
    ///
    /// Without this a transient refusal was permanent: `take_due` had already cleared the
    /// pending apply, so nothing would try again until the charger was next touched.
    pub fn schedule_retry(&mut self, now: Instant) -> Option<Duration> {
        let delay = *Self::RETRY_BACKOFF.get(self.retries)?;
        self.retries += 1;
        self.pending = Some(Pending::after(delay, now));
        Some(delay)
    }

    /// Install a new config. No-ops if nothing actually changed.
    pub fn reload_config(&mut self, config: Config, now: Instant) -> bool {
        if config == self.config {
            return false;
        }
        self.config = config;
        self.queue(Event::ConfigChanged, now);
        true
    }

    /// Fold an event into the debounce. Duplicate power readings are ignored so a 250ms
    /// sysfs poll cannot postpone the apply forever.
    pub fn on_event(&mut self, event: Event, now: Instant) {
        if let Event::Power(new_state) = event {
            if new_state == self.state {
                return;
            }
            self.state = new_state;
        }
        self.queue(event, now);
    }

    fn queue(&mut self, event: Event, now: Instant) {
        // A fresh trigger deserves the full retry budget, whatever the last one spent.
        self.retries = 0;
        match &mut self.pending {
            Some(pending) => pending.extend(event, now),
            None => self.pending = Some(Pending::new(event, now)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    fn controller(now: Instant) -> Controller {
        Controller::new(Config::default(), PowerState::Ac, now)
    }

    #[test]
    fn apply_on_start_schedules_an_apply() {
        let now = t0();
        let c = controller(now);
        assert!(c.due(now + Duration::from_millis(1500)));
        assert!(!c.due(now + Duration::from_millis(500)));
    }

    #[test]
    fn a_disabled_apply_on_start_waits_for_a_real_event() {
        let now = t0();
        let config = Config {
            apply_on_start: false,
            ..Default::default()
        };
        let c = Controller::new(config, PowerState::Ac, now);
        assert!(!c.due(now + Duration::from_secs(60)));
        assert!(c.wait(now).is_none());
    }

    #[test]
    fn duplicate_power_events_do_not_restart_the_debounce() {
        let now = t0();
        let mut c = Controller::new(
            Config {
                apply_on_start: false,
                ..Default::default()
            },
            PowerState::Ac,
            now,
        );

        c.on_event(Event::Power(PowerState::Battery), now);
        let first_deadline = now + Event::Power(PowerState::Battery).settle_delay();

        c.on_event(Event::Power(PowerState::Battery), now + Duration::from_millis(200));
        assert!(c.due(first_deadline));
        assert!(!c.due(first_deadline - Duration::from_millis(1)));
    }

    #[test]
    fn a_real_power_change_does_restart_the_debounce() {
        let now = t0();
        let mut c = Controller::new(
            Config {
                apply_on_start: false,
                ..Default::default()
            },
            PowerState::Ac,
            now,
        );

        c.on_event(Event::Power(PowerState::Battery), now);
        c.on_event(Event::Power(PowerState::Ac), now + Duration::from_millis(1000));

        assert!(!c.due(now + Duration::from_millis(1500)));
        assert!(c.due(now + Duration::from_millis(1000) + Duration::from_millis(1500)));
        assert_eq!(c.state(), PowerState::Ac);
    }

    #[test]
    fn an_overdue_apply_is_not_starved_by_more_events() {
        let now = t0();
        let mut c = controller(now);

        let later = now + Duration::from_secs(11);
        c.on_event(Event::DisplaysChanged, later);
        c.on_event(Event::DisplaysChanged, later + Duration::from_millis(1));
        c.on_event(Event::Resumed, later + Duration::from_millis(2));

        assert!(c.due(later), "MAX_WAIT must cap coalescing even under a flood");
        assert!(c.take_due(later));
        assert!(!c.due(later));
    }

    #[test]
    fn wait_is_zero_once_the_deadline_has_passed() {
        let now = t0();
        let c = controller(now);
        assert_eq!(c.wait(now + Duration::from_secs(30)), Some(Duration::ZERO));
    }

    #[test]
    fn a_failed_apply_is_tried_again() {
        let now = t0();
        let mut c = controller(now);
        assert!(c.take_due(now + Duration::from_secs(2)));
        assert!(!c.due(now + Duration::from_secs(2)));

        let delay = c.schedule_retry(now + Duration::from_secs(2)).unwrap();
        assert!(!c.due(now + Duration::from_secs(2) + delay - Duration::from_millis(1)));
        assert!(c.due(now + Duration::from_secs(2) + delay));
    }

    #[test]
    fn retries_back_off_and_then_give_up() {
        let now = t0();
        let mut c = controller(now);

        let mut delays = Vec::new();
        while let Some(delay) = c.schedule_retry(now) {
            delays.push(delay);
            assert!(delays.len() <= 8, "schedule_retry must not loop forever");
        }

        assert!(delays.len() > 1);
        assert!(
            delays.windows(2).all(|pair| pair[0] < pair[1]),
            "each retry should wait longer: {delays:?}"
        );
    }

    #[test]
    fn a_new_event_restores_the_retry_budget() {
        let now = t0();
        let mut c = controller(now);
        while c.schedule_retry(now).is_some() {}

        c.on_event(Event::Power(PowerState::Battery), now);
        assert!(c.schedule_retry(now).is_some());
    }

    #[test]
    fn a_long_retry_is_not_cut_short_by_the_coalescing_cap() {
        let now = t0();
        let mut c = controller(now);
        let mut last = Duration::ZERO;
        while let Some(delay) = c.schedule_retry(now) {
            last = delay;
        }
        assert!(last > Pending::MAX_WAIT);
        assert!(!c.due(now + Pending::MAX_WAIT));
        assert!(c.due(now + last));
    }

    #[test]
    fn an_identical_config_reload_is_ignored() {
        let now = t0();
        let mut c = Controller::new(
            Config {
                apply_on_start: false,
                ..Default::default()
            },
            PowerState::Ac,
            now,
        );
        assert!(!c.reload_config(Config {
            apply_on_start: false,
            ..Default::default()
        }, now));
        assert!(!c.due(now + Duration::from_secs(1)));
    }

    #[test]
    fn a_changed_config_schedules_an_apply() {
        let now = t0();
        let mut c = Controller::new(
            Config {
                apply_on_start: false,
                ..Default::default()
            },
            PowerState::Ac,
            now,
        );
        let mut next = c.config().clone();
        next.enabled = false;
        assert!(c.reload_config(next, now));
        assert!(c.due(now + Duration::from_millis(300)));
    }
}

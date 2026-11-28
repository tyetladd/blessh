//! The watchdog's decision logic: from probe results and the clock, when to
//! reset the controller, when to stop, and when to say it is back.
//!
//! There is no I/O and no clock here: the caller passes `Instant`s in, so
//! tests drive time.

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// One probe's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Probe {
    /// The controller answered.
    Answered,
    /// The controller exists but did not answer in time.
    NoAnswer,
    /// The adapter is down, so probing was skipped.
    Down,
    /// The adapter does not exist, so probing was skipped.
    Absent,
}

/// What the watchdog should do after a probe.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Keep probing.
    None,
    /// Reset the controller.
    Reset,
    /// The reset limit is spent: log once, then keep probing.
    GaveUp,
    /// The controller answers again after a reset or after giving up.
    Recovered,
}

/// The thresholds of the policy.
pub struct Limits {
    /// Consecutive missed probes that mean the controller is dead.
    misses: u32,
    /// How long probing stays off after a reset.
    cooldown: Duration,
    /// How many resets are allowed within one window.
    max_resets: usize,
    /// The sliding window the reset count applies to.
    window: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            misses: 2,
            cooldown: Duration::from_secs(30),
            max_resets: 3,
            window: Duration::from_secs(600),
        }
    }
}

impl Limits {
    /// How many resets are allowed within one window.
    pub fn max_resets(&self) -> usize {
        self.max_resets
    }

    /// The sliding window the reset count applies to.
    pub fn window(&self) -> Duration {
        self.window
    }
}

/// The watchdog's state: consecutive misses, recent resets, the cooldown
/// and whether it gave up or is waiting for the controller to answer after
/// a reset.
pub struct Policy {
    limits: Limits,
    misses: u32,
    resets: VecDeque<Instant>,
    cooldown_until: Option<Instant>,
    gave_up: bool,
    after_reset: bool,
}

impl Policy {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            misses: 0,
            resets: VecDeque::new(),
            cooldown_until: None,
            gave_up: false,
            after_reset: false,
        }
    }

    /// Feeds one probe result in and says what to do about it.
    pub fn on_probe(&mut self, now: Instant, probe: Probe) -> Action {
        match probe {
            Probe::Answered => {
                self.misses = 0;
                if self.gave_up || self.after_reset {
                    self.gave_up = false;
                    self.after_reset = false;
                    Action::Recovered
                } else {
                    Action::None
                }
            }
            // A controller that is down or absent was not asked, so these
            // are not missed probes and never lead to a reset. They keep
            // the "answers again" notice pending, though.
            Probe::Down | Probe::Absent => {
                self.misses = 0;
                Action::None
            }
            Probe::NoAnswer => {
                self.misses += 1;
                if self.misses < self.limits.misses || self.gave_up {
                    return Action::None;
                }
                self.prune(now);
                if self.resets.len() >= self.limits.max_resets {
                    // Logged once; further misses stay quiet until the
                    // controller answers.
                    self.gave_up = true;
                    Action::GaveUp
                } else {
                    self.misses = 0;
                    Action::Reset
                }
            }
        }
    }

    /// Whether a probe may be sent now: false during the cooldown after a
    /// reset.
    pub fn may_probe(&self, now: Instant) -> bool {
        match self.cooldown_until {
            Some(until) => now >= until,
            None => true,
        }
    }

    /// Records that a reset finished: starts the cooldown and counts the
    /// reset towards the limit.
    pub fn reset_done(&mut self, now: Instant) {
        self.resets.push_back(now);
        self.after_reset = true;
        self.cooldown_until = Some(now + self.limits.cooldown);
    }

    /// The thresholds this policy was built with, for the log lines.
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The consecutive probes the controller did not answer.
    pub fn misses(&self) -> u32 {
        self.misses
    }

    /// Drops reset times that are older than the window.
    fn prune(&mut self, now: Instant) {
        while let Some(&reset) = self.resets.front() {
            if now.duration_since(reset) > self.limits.window {
                self.resets.pop_front();
            } else {
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, secs: u64) -> Instant {
        t0 + Duration::from_secs(secs)
    }

    fn new_policy() -> Policy {
        Policy::new(Limits::default())
    }

    #[test]
    fn one_miss_does_not_reset() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        assert_eq!(policy.on_probe(at(t0, 0), Probe::NoAnswer), Action::None);
    }

    #[test]
    fn two_misses_reset() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        assert_eq!(policy.on_probe(at(t0, 0), Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(at(t0, 5), Probe::NoAnswer), Action::Reset);
    }

    #[test]
    fn answer_clears_miss() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        assert_eq!(policy.on_probe(at(t0, 0), Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(at(t0, 5), Probe::Answered), Action::None);
        assert_eq!(policy.on_probe(at(t0, 10), Probe::NoAnswer), Action::None);
    }

    #[test]
    fn down_is_not_dead() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        assert_eq!(policy.on_probe(at(t0, 0), Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(at(t0, 5), Probe::Down), Action::None);
        assert_eq!(policy.on_probe(at(t0, 10), Probe::NoAnswer), Action::None);
    }

    #[test]
    fn absent_is_not_dead() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        for secs in [0, 5, 10, 15, 20] {
            assert_eq!(policy.on_probe(at(t0, secs), Probe::Absent), Action::None);
        }
    }

    #[test]
    fn cooldown_after_reset() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        policy.reset_done(t0);
        assert!(!policy.may_probe(at(t0, 29)));
        assert!(policy.may_probe(at(t0, 30)));
    }

    #[test]
    fn fourth_reset_in_window_gives_up() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        for secs in [0, 60, 120] {
            let t = at(t0, secs);
            assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::None);
            assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::Reset);
            policy.reset_done(t);
        }
        let t = at(t0, 200);
        assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::GaveUp);
        assert_eq!(policy.on_probe(at(t0, 205), Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(at(t0, 210), Probe::NoAnswer), Action::None);
    }

    #[test]
    fn window_slides() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        for secs in [0, 60, 120] {
            let t = at(t0, secs);
            assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::None);
            assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::Reset);
            policy.reset_done(t);
        }
        let t = at(t0, 601);
        assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::Reset);
    }

    #[test]
    fn answer_after_giving_up_recovers() {
        let t0 = Instant::now();
        let mut policy = new_policy();
        for secs in [0, 60, 120] {
            let t = at(t0, secs);
            assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::None);
            assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::Reset);
            policy.reset_done(t);
        }
        let t = at(t0, 200);
        assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::None);
        assert_eq!(policy.on_probe(t, Probe::NoAnswer), Action::GaveUp);
        assert_eq!(
            policy.on_probe(at(t0, 205), Probe::Answered),
            Action::Recovered
        );
        // The misses count again from zero; once the window has slid, two
        // misses reset again.
        assert_eq!(policy.on_probe(at(t0, 1300), Probe::NoAnswer), Action::None);
        assert_eq!(
            policy.on_probe(at(t0, 1301), Probe::NoAnswer),
            Action::Reset
        );
    }
}

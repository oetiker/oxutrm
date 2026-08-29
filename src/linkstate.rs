//! Whether the host is still answering, and what the user is told about it.
//!
//! Pure: no I/O, no terminal, no runtime. Every method takes the current
//! `Instant` as a parameter rather than reading the clock, which is what lets
//! the whole state machine be tested without sleeping.

use std::time::{Duration, Instant};

/// How long a reply may be owed before the user is told. Below this a blip
/// resolves without ever painting: an indicator that fires on every hiccup is
/// the noise it was built to remove.
pub const SILENT_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Live,
    Silent { since: Instant },
    Confirming,
}

/// What the client believes about the link, and why.
pub struct LinkState {
    phase: Phase,
    /// The last time anything at all arrived from the host.
    last_heard: Instant,
}

impl LinkState {
    pub fn new(now: Instant) -> LinkState {
        LinkState {
            phase: Phase::Live,
            last_heard: now,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// A frame arrived. Whatever we believed, the host is answering.
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
        self.phase = Phase::Live;
    }

    /// One lap's worth of judgement.
    ///
    /// `reply_owed` is the caller's answer to "have we said something the host
    /// has not acknowledged". It is the whole signal: the sync engine sends an
    /// empty-diff frame purely to move an owed ack, so an unanswered input is a
    /// real round-trip failure rather than an inference. With nothing owed,
    /// silence is indistinguishable from calm and this reports `Live` --
    /// closing that gap is what the heartbeat is for.
    pub fn evaluate(&mut self, now: Instant, reply_owed: bool) -> Phase {
        if let Phase::Silent { .. } = self.phase {
            // Already told the user. The clock keeps running from `last_heard`;
            // recomputing it here would restart the counter every lap.
            return self.phase;
        }

        if reply_owed && now.duration_since(self.last_heard) >= SILENT_AFTER {
            // `last_heard` and not `now`: the counter must report how long the
            // host has been quiet, not how long since we worked it out.
            self.phase = Phase::Silent {
                since: self.last_heard,
            };
        }
        self.phase
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_fresh_link_is_live() {
        assert_eq!(LinkState::new(t0()).phase(), Phase::Live);
    }

    #[test]
    fn silence_with_a_reply_owed_becomes_silent_after_the_grace_period() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(
            s.evaluate(t + Duration::from_millis(1900), true),
            Phase::Live
        );
        assert!(matches!(
            s.evaluate(t + Duration::from_millis(2100), true),
            Phase::Silent { .. }
        ));
    }

    /// Nothing owed means nothing is knowable. Without the heartbeat of Task 6
    /// this is also why an idle session would never notice an outage.
    #[test]
    fn silence_with_nothing_owed_stays_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(s.evaluate(t + Duration::from_secs(60), false), Phase::Live);
    }

    #[test]
    fn hearing_from_the_host_returns_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.evaluate(t + Duration::from_secs(3), true);

        s.heard(t + Duration::from_secs(4));
        assert_eq!(s.phase(), Phase::Live);
    }

    /// The `since` is the moment the host went quiet, not the moment we
    /// noticed. A counter that started at the grace period would under-report
    /// every outage by two seconds.
    #[test]
    fn the_silence_started_when_the_host_went_quiet_not_when_we_noticed() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.heard(t);

        let Phase::Silent { since } = s.evaluate(t + Duration::from_secs(5), true) else {
            panic!("expected Silent");
        };
        assert_eq!(
            since, t,
            "the counter must run from the last thing we heard"
        );
    }

    #[test]
    fn silence_persists_across_laps_without_restarting_the_clock() {
        let t = t0();
        let mut s = LinkState::new(t);

        let first = s.evaluate(t + Duration::from_secs(3), true);
        let later = s.evaluate(t + Duration::from_secs(9), true);

        assert_eq!(first, later, "the clock restarted mid-outage");
    }
}

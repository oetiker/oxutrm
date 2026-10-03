//! The client's standby: whether it has one, when to look for one, and where
//! this outage's probe of it stands (spec §3).
//!
//! State only. The work (searching, probing) runs in spawned tasks that
//! report back through a channel the session loop holds as a local (C1), and
//! the loop calls `step` on the laps it already takes. So nothing here holds a
//! timer: an idle session with a healthy standby costs no wakeups (19cc001).

use std::net::SocketAddr;
use std::time::Instant;

use oxutrm_net::{NetConfig, RemoteFilter};
use oxutrm_proto::PathDescription;

use crate::connect::Established;
use crate::link::Link;
use crate::linkstate::{
    PROBE_RETRY, Phase, ProbeState, STANDBY_DELAY, failover_due, standby_backoff,
};

/// What a spawned search or probe reports back to the loop.
pub(crate) enum StandbyEvent {
    Found(Box<Established>),
    /// The search failed. Why is not kept: the user is told only that there
    /// is no standby (spec §3.7), and the next search asks again anyway.
    NotFound,
    Probed {
        answered: bool,
    },
}

/// What `step` asks the loop to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StandbyAction {
    Nothing,
    /// Spawn `control::request_standby` against the primary, reporting
    /// `Found`/`NotFound`.
    Search,
    /// Spawn `control::probe` against the standby, reporting `Probed`.
    Probe {
        nonce: u64,
    },
    /// Swap the standby in now.
    FailOver,
}

pub(crate) struct Standby {
    pub(crate) cfg: NetConfig,
    /// The filter a search nominates through, built from the primary's
    /// remote: [`crate::egress::avoiding_source`]. A field so a test on
    /// loopback, where every candidate is the primary's own path, can admit
    /// them.
    pub(crate) admit_for: fn(SocketAddr) -> Option<RemoteFilter>,
    link: Option<(Link, PathDescription)>,
    searching: bool,
    failures: u32,
    next_search: Instant,
    probe: ProbeState,
    probing: bool,
    nonce: u64,
    /// Whether "no standby path" has been said for the current dry spell, so
    /// it is said once, not after every failed search.
    told_none: bool,
}

impl Standby {
    pub(crate) fn new(cfg: NetConfig, now: Instant) -> Standby {
        Standby {
            cfg,
            admit_for: crate::egress::avoiding_source,
            link: None,
            searching: false,
            failures: 0,
            next_search: now + STANDBY_DELAY,
            probe: ProbeState::Idle,
            probing: false,
            nonce: 0,
            told_none: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_link(&self) -> bool {
        self.link.is_some()
    }

    #[cfg(test)]
    pub(crate) fn next_search(&self) -> Instant {
        self.next_search
    }

    /// The standby's connection, for the loop's `closed()` arm and for
    /// probing.
    pub(crate) fn connection(&self) -> Option<quinn::Connection> {
        self.link.as_ref().map(|(l, _)| l.sink.connection().clone())
    }

    /// One decision per lap. `phase` is the lap's own phase;
    /// `rebuild_running` is whether an ssh rebuild attempt is in flight.
    ///
    /// Nothing is failed over onto while a rebuild runs (ruling B1): the host
    /// would adopt the attempt as a primary when it lands and close the
    /// standby this had just promoted as taken over. An answer from before
    /// the attempt is forgotten too, so the standby is asked again once the
    /// attempt is over rather than trusted on old news.
    pub(crate) fn step(
        &mut self,
        phase: Phase,
        now: Instant,
        rebuild_running: bool,
    ) -> StandbyAction {
        if !phase.is_outage() {
            // The outage (if there was one) is over, and so is its probe.
            self.probe = ProbeState::Idle;
            if self.link.is_none() && !self.searching && now >= self.next_search {
                self.searching = true;
                return StandbyAction::Search;
            }
            return StandbyAction::Nothing;
        }
        if self.link.is_none() {
            return StandbyAction::Nothing;
        }
        if rebuild_running {
            self.probe = ProbeState::Idle;
            return StandbyAction::Nothing;
        }
        if failover_due(phase, self.probe, now) {
            return StandbyAction::FailOver;
        }
        let may_probe = match self.probe {
            ProbeState::Idle => true,
            ProbeState::Failed { at } => now.duration_since(at) >= PROBE_RETRY,
            _ => false,
        };
        if may_probe && !self.probing {
            self.probing = true;
            self.nonce = self.nonce.wrapping_add(1);
            self.probe = ProbeState::Pending { sent: now };
            return StandbyAction::Probe { nonce: self.nonce };
        }
        StandbyAction::Nothing
    }

    /// A search finished with a link.
    pub(crate) fn found(&mut self, e: Established) {
        self.searching = false;
        self.failures = 0;
        self.told_none = false;
        self.link = Some((e.link, e.path));
    }

    /// A search finished without one. Returns whether to tell the user.
    pub(crate) fn not_found(&mut self, now: Instant) -> bool {
        self.searching = false;
        self.backoff(now);
        self.dry_spell_news()
    }

    /// Whatever a probe found. A result for a probe whose outage already
    /// ended is dropped: `step` reset `probe` to `Idle`, and `Idle` is not
    /// `Pending`.
    pub(crate) fn probed(&mut self, answered: bool, now: Instant) {
        self.probing = false;
        if let ProbeState::Pending { sent } = self.probe {
            self.probe = if answered {
                ProbeState::Answered { sent }
            } else {
                ProbeState::Failed { at: now }
            };
        }
    }

    /// The standby's connection closed on its own. Returns whether to tell
    /// the user (spec §3.7: the standby is announced when it is lost).
    pub(crate) fn lost(&mut self, now: Instant) -> bool {
        self.link = None;
        self.probe = ProbeState::Idle;
        self.backoff(now);
        self.dry_spell_news()
    }

    /// The route to the host moved: an interface that was absent may now be
    /// present (spec §3.1), so a search is due at once rather than at the end
    /// of a backoff that was measured against the old one.
    pub(crate) fn route_moved(&mut self, now: Instant) {
        self.next_search = now;
    }

    /// Hand the standby over for a failover. After it, a fresh search is due
    /// once the new primary has settled.
    pub(crate) fn take_for_failover(&mut self, now: Instant) -> Option<(Link, PathDescription)> {
        self.probe = ProbeState::Idle;
        self.failures = 0;
        self.next_search = now + STANDBY_DELAY;
        self.link.take()
    }

    /// A rebuild over ssh landed. The host dropped our standby when it adopted
    /// the rebuild, so ours is a corpse.
    pub(crate) fn forget(&mut self, now: Instant) {
        self.link = None;
        self.probe = ProbeState::Idle;
        self.next_search = now + STANDBY_DELAY;
    }

    fn backoff(&mut self, now: Instant) {
        self.next_search = now + standby_backoff(self.failures);
        self.failures = self.failures.saturating_add(1);
    }

    /// True the first time in a dry spell, false after.
    fn dry_spell_news(&mut self) -> bool {
        !std::mem::replace(&mut self.told_none, true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linkstate::{FAILOVER_GRACE, PROBE_RETRY, STANDBY_DELAY, standby_backoff};
    use oxutrm_proto::{NatType, Rung};
    use std::time::Duration;

    /// A real standby link, as a search would hand it over, and the host end
    /// that keeps its connection alive for as long as the test holds it.
    async fn established() -> (Established, Link) {
        let (host, client) = crate::link::fixtures::link_pair().await;
        let e = Established {
            link: client,
            path: PathDescription {
                rung: Rung::StunPunch,
                local: "127.0.0.1:1".parse().unwrap(),
                remote: "203.0.113.7:443".parse().unwrap(),
                probes_sent: 0,
                nat_type: NatType::Unknown,
                rtt_ms: 38,
                mtu: 1400,
            },
            session_id: String::new(),
            attach_id: 0,
            host_features: vec![],
        };
        (e, host)
    }

    fn silent(since: Instant) -> Phase {
        Phase::Silent { since }
    }

    /// A standby, found at `t0`, whose outage has just begun.
    async fn with_a_standby(t0: Instant) -> (Standby, Link) {
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let (e, host) = established().await;
        s.found(e);
        assert!(s.has_link(), "the fixture found nothing");
        (s, host)
    }

    #[tokio::test]
    async fn a_standby_that_closes_is_forgotten_and_searched_for_again() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert_eq!(
            s.step(Phase::Live, t0 + STANDBY_DELAY, false),
            StandbyAction::Nothing,
            "a session that has a standby searched for another"
        );

        s.lost(t0);

        assert!(!s.has_link());
        assert_eq!(s.next_search(), t0 + standby_backoff(0));
        assert_eq!(
            s.step(Phase::Live, t0 + standby_backoff(0), false),
            StandbyAction::Search
        );
    }

    #[test]
    fn the_first_search_waits_for_the_link_to_settle_and_runs_one_at_a_time() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        assert_eq!(
            s.step(
                Phase::Live,
                t0 + STANDBY_DELAY - Duration::from_millis(1),
                false
            ),
            StandbyAction::Nothing,
            "searched before the first paint had its moment"
        );
        assert_eq!(
            s.step(Phase::Live, t0 + STANDBY_DELAY, false),
            StandbyAction::Search
        );
        assert_eq!(
            s.step(Phase::Live, t0 + STANDBY_DELAY * 2, false),
            StandbyAction::Nothing,
            "a second search started while the first was still running"
        );
    }

    #[test]
    fn a_failed_search_is_reported_once_and_backs_off() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        assert_eq!(
            s.step(Phase::Live, t0 + STANDBY_DELAY, false),
            StandbyAction::Search
        );

        let t1 = t0 + STANDBY_DELAY;
        assert!(s.not_found(t1), "the first failure was not reported");
        assert_eq!(s.next_search(), t1 + standby_backoff(0));
        assert_eq!(
            s.step(Phase::Live, t1 + standby_backoff(0), false),
            StandbyAction::Search
        );

        let t2 = t1 + standby_backoff(0);
        assert!(!s.not_found(t2), "the same dry spell was reported twice");
        assert_eq!(s.next_search(), t2 + standby_backoff(1));
    }

    #[test]
    fn a_moved_route_makes_a_search_due_at_once() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let t1 = t0 + STANDBY_DELAY;
        assert_eq!(s.step(Phase::Live, t1, false), StandbyAction::Search);
        let _ = s.not_found(t1);
        assert_eq!(
            s.step(Phase::Live, t1 + Duration::from_secs(1), false),
            StandbyAction::Nothing,
            "the fixture has no backoff to cut short"
        );

        s.route_moved(t1 + Duration::from_secs(1));

        assert_eq!(
            s.step(Phase::Live, t1 + Duration::from_secs(1), false),
            StandbyAction::Search
        );
    }

    /// The positive case the guards below are measured against.
    #[tokio::test]
    async fn an_answered_probe_fails_over_once_the_grace_has_passed() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;

        assert_eq!(
            s.step(silent(t0), t0, false),
            StandbyAction::Probe { nonce: 1 }
        );
        s.probed(true, t0);
        assert_eq!(
            s.step(
                silent(t0),
                t0 + FAILOVER_GRACE - Duration::from_millis(1),
                false
            ),
            StandbyAction::Nothing,
            "failed over inside the grace a blipping primary gets"
        );
        assert_eq!(
            s.step(silent(t0), t0 + FAILOVER_GRACE, false),
            StandbyAction::FailOver
        );
    }

    #[tokio::test]
    async fn step_never_fails_over_on_an_unanswered_probe() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;

        assert_eq!(
            s.step(silent(t0), t0, false),
            StandbyAction::Probe { nonce: 1 }
        );
        s.probed(false, t0);

        let later = t0 + Duration::from_secs(10);
        let action = s.step(silent(t0), later, false);
        assert_ne!(action, StandbyAction::FailOver);
        // Not merely quiet: the failed probe is retried.
        assert_eq!(action, StandbyAction::Probe { nonce: 2 });
    }

    #[tokio::test]
    async fn a_failed_probe_is_retried_only_after_the_pause() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert_eq!(
            s.step(silent(t0), t0, false),
            StandbyAction::Probe { nonce: 1 }
        );
        s.probed(false, t0);
        assert_eq!(
            s.step(
                silent(t0),
                t0 + PROBE_RETRY - Duration::from_millis(1),
                false
            ),
            StandbyAction::Nothing
        );
        assert_eq!(
            s.step(silent(t0), t0 + PROBE_RETRY, false),
            StandbyAction::Probe { nonce: 2 }
        );
    }

    /// An answer belongs to the outage it was sent in.
    #[tokio::test]
    async fn an_answer_from_an_earlier_outage_does_not_fail_over_the_next() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;

        assert_eq!(
            s.step(silent(t0), t0, false),
            StandbyAction::Probe { nonce: 1 }
        );
        s.probed(true, t0);
        let t1 = t0 + Duration::from_millis(100);
        assert_eq!(s.step(Phase::Live, t1, false), StandbyAction::Nothing);

        let t2 = t1 + Duration::from_secs(60);
        assert_eq!(
            s.step(silent(t2), t2, false),
            StandbyAction::Probe { nonce: 2 },
            "the next outage failed over on the last one's answer"
        );
    }

    /// Ruling B1: an ssh rebuild in flight would be adopted by the host as a
    /// primary and close the standby this promoted as taken over. So nothing
    /// is promoted while one runs, and an answer from before it is not
    /// trusted after it.
    #[tokio::test]
    async fn no_failover_while_a_rebuild_attempt_is_running() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        let recovering = Phase::Recovering {
            attempt: 0,
            next_try: t0,
        };

        assert_eq!(
            s.step(recovering, t0, false),
            StandbyAction::Probe { nonce: 1 }
        );
        s.probed(true, t0);
        let due = t0 + FAILOVER_GRACE;
        assert_eq!(
            s.step(recovering, due, true),
            StandbyAction::Nothing,
            "failed over while a rebuild attempt was in flight"
        );

        // The attempt ends without landing. The standby is asked again rather
        // than promoted on an answer that predates the attempt.
        assert_eq!(
            s.step(recovering, due + Duration::from_secs(30), false),
            StandbyAction::Probe { nonce: 2 }
        );
    }

    #[tokio::test]
    async fn losing_the_standby_is_reported_once() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert!(s.lost(t0), "losing the standby went unsaid");
        assert!(
            !s.not_found(t0 + standby_backoff(0)),
            "the dry spell that losing it began was reported twice"
        );
    }

    #[tokio::test]
    async fn a_failover_takes_the_link_and_schedules_a_fresh_search() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        let t1 = t0 + Duration::from_secs(3);

        let taken = s.take_for_failover(t1);

        assert!(taken.is_some());
        assert!(!s.has_link());
        assert_eq!(s.next_search(), t1 + STANDBY_DELAY);
    }
}

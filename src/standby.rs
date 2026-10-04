//! The client's standby: whether it has one, when to look for one, and where
//! this outage's probe of it stands (spec §3).
//!
//! State only. The work (searching, probing) runs in spawned tasks that
//! report back through a channel the session loop holds as a local (C1), and
//! the loop calls `step` on the laps it already takes. So nothing here holds a
//! timer and the loop gains no wakeups (19cc001). The standby link itself
//! does: like every link, its QUIC keep-alive fires every ten seconds on both
//! ends (spec §3.4), which is the price of keeping its path warm.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal. What happens here is
// recorded by the session in its activity log, and a search's failure reason
// is kept on `last_failure` for the popup.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::net::SocketAddr;
use std::time::Instant;

use oxutrm_net::{NetConfig, RemoteFilter};
use oxutrm_proto::PathDescription;

use crate::connect::Established;
use crate::link::Link;
use crate::linkstate::{
    PROBE_RETRY, Phase, ProbeState, STANDBY_DELAY, failover_due, standby_backoff,
};
use crate::session::{REBUILT, SHELL_EXITED, TAKEN_OVER};

/// Why the client closed a standby that a search found for a primary which
/// has since been replaced. Local, like [`REBUILT`]: the host just drops it.
const STALE: &[u8] = b"found for a link that has since been replaced";

/// Close a link the standby lets go of. Dropping a `Link` does not close its
/// connection: its source's tasks hold clones of it.
fn close(link: &Link, reason: &'static [u8]) {
    link.sink
        .connection()
        .close(quinn::VarInt::from_u32(0), reason);
}

/// What a spawned search or probe reports back to the loop.
///
/// A search's result carries the `search` number `step` handed out with
/// [`StandbyAction::Search`], so one that outlived the primary it ran over
/// is recognised as stale (see [`Standby::forget`]).
pub(crate) enum StandbyEvent {
    Found {
        search: u64,
        e: Box<Established>,
    },
    /// The search failed, and the next search asks again anyway. The reason
    /// is recorded in the activity log by the session, and kept on
    /// [`Standby::last_failure`] for the popup's standby block.
    NotFound {
        search: u64,
        reason: String,
    },
    Probed {
        answered: bool,
    },
}

/// What `step` asks the loop to do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StandbyAction {
    Nothing,
    /// Spawn `control::request_standby` against the primary, reporting
    /// `Found`/`NotFound` with this `search` number.
    Search {
        search: u64,
    },
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
    /// The standby, as its search established it.
    link: Option<Established>,
    searching: bool,
    /// The number of the search whose result is still wanted. Moved on by
    /// every search started and by every change of primary, so a result
    /// from a search that ran over an earlier primary matches nothing.
    search: u64,
    failures: u32,
    next_search: Instant,
    probe: ProbeState,
    probing: bool,
    nonce: u64,
    /// Why the last search failed, for the popup's standby block. Cleared the
    /// moment a search succeeds; never printed.
    last_failure: Option<String>,
}

impl Standby {
    pub(crate) fn new(cfg: NetConfig, now: Instant) -> Standby {
        Standby {
            cfg,
            admit_for: crate::egress::avoiding_source,
            link: None,
            searching: false,
            search: 0,
            failures: 0,
            next_search: now + STANDBY_DELAY,
            probe: ProbeState::Idle,
            probing: false,
            nonce: 0,
            last_failure: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn has_link(&self) -> bool {
        self.link.is_some()
    }

    pub(crate) fn next_search(&self) -> Instant {
        self.next_search
    }

    /// Why the last search failed, for the popup's standby block.
    pub(crate) fn last_failure(&self) -> Option<&str> {
        self.last_failure.as_deref()
    }

    /// The standby's path, while there is one.
    pub(crate) fn path(&self) -> Option<&PathDescription> {
        self.link.as_ref().map(|e| &e.path)
    }

    /// quinn's round-trip estimate on the standby's own connection.
    pub(crate) fn rtt(&self) -> Option<std::time::Duration> {
        self.link.as_ref().map(|e| e.link.sink.connection().rtt())
    }

    pub(crate) fn probe(&self) -> ProbeState {
        self.probe
    }

    /// Whether a search is running now.
    pub(crate) fn searching(&self) -> bool {
        self.searching
    }

    /// The standby's connection, for the loop's `closed()` arm and for
    /// probing.
    pub(crate) fn connection(&self) -> Option<quinn::Connection> {
        self.link.as_ref().map(|e| e.link.sink.connection().clone())
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
                self.search = self.search.wrapping_add(1);
                return StandbyAction::Search {
                    search: self.search,
                };
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

    /// A search finished with a link. Returns whether it was kept: a link
    /// found for a primary that has since been replaced stood by for nothing,
    /// and is closed rather than dropped (see [`close`]).
    pub(crate) fn found(&mut self, search: u64, e: Established) -> bool {
        if search != self.search {
            close(&e.link, STALE);
            return false;
        }
        self.searching = false;
        self.failures = 0;
        self.last_failure = None;
        self.link = Some(e);
        true
    }

    /// A search finished without one. Returns whether the result was for the
    /// search still wanted -- the one worth recording.
    ///
    /// A stale search changes nothing, `reason` included: it most likely
    /// failed because the primary it ran over was closed under it, which says
    /// nothing about the path the session has now.
    pub(crate) fn not_found(&mut self, search: u64, now: Instant, reason: String) -> bool {
        if search != self.search {
            return false;
        }
        self.searching = false;
        self.last_failure = Some(reason);
        self.backoff(now);
        true
    }

    /// Whatever a probe found. Returns whether it was for the probe in
    /// flight: a result whose outage already ended is dropped, because
    /// `step` reset `probe` to `Idle`, and `Idle` is not `Pending`.
    pub(crate) fn probed(&mut self, answered: bool, now: Instant) -> bool {
        self.probing = false;
        let ProbeState::Pending { sent } = self.probe else {
            return false;
        };
        self.probe = if answered {
            ProbeState::Answered { sent }
        } else {
            ProbeState::Failed { at: now }
        };
        true
    }

    /// The standby's connection closed on its own. Returns whether that is
    /// worth recording: not when the shell exited or another client took the
    /// session over -- the session is ending, and the primary's close is
    /// about to say why.
    pub(crate) fn lost(&mut self, now: Instant, reason: &quinn::ConnectionError) -> bool {
        self.link = None;
        self.probe = ProbeState::Idle;
        self.backoff(now);
        let ending = matches!(
            reason,
            quinn::ConnectionError::ApplicationClosed(c)
                if c.reason.as_ref() == SHELL_EXITED || c.reason.as_ref() == TAKEN_OVER
        );
        !ending
    }

    /// The route to the host moved: an interface that was absent may now be
    /// present (spec §3.1), so a search is due at once rather than at the end
    /// of a backoff that was measured against the old one.
    pub(crate) fn route_moved(&mut self, now: Instant) {
        self.next_search = now;
    }

    /// Hand the standby over for a failover. After it, a fresh search is due
    /// once the new primary has settled, and a search still running over the
    /// old primary is disowned.
    pub(crate) fn take_for_failover(&mut self, now: Instant) -> Option<Established> {
        self.probe = ProbeState::Idle;
        self.failures = 0;
        self.next_search = now + STANDBY_DELAY;
        self.new_primary();
        self.link.take()
    }

    /// A rebuild over ssh landed. The host dropped our standby when it adopted
    /// the rebuild, so ours is a corpse -- and is closed here, because nothing
    /// else would: with no idle timeout, a standby whose path is dead never
    /// hears the host's close, and its socket, tasks and keep-alive timer
    /// would outlive the session's interest in it for good.
    pub(crate) fn forget(&mut self, now: Instant) {
        if let Some(e) = self.link.take() {
            close(&e.link, REBUILT);
        }
        self.probe = ProbeState::Idle;
        self.next_search = now + STANDBY_DELAY;
        self.new_primary();
    }

    /// The primary changed under any search in flight. Its result is no
    /// longer wanted, and the next search may start without waiting for it.
    fn new_primary(&mut self) {
        self.searching = false;
        self.search = self.search.wrapping_add(1);
    }

    fn backoff(&mut self, now: Instant) {
        self.next_search = now + standby_backoff(self.failures);
        self.failures = self.failures.saturating_add(1);
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

    /// The search `step` starts at `now`, by its number.
    fn search_at(s: &mut Standby, now: Instant) -> u64 {
        match s.step(Phase::Live, now, false) {
            StandbyAction::Search { search } => search,
            other => panic!("no search started: {other:?}"),
        }
    }

    /// A standby, found by a search that settled before `t0`, whose outage
    /// has just begun.
    async fn with_a_standby(t0: Instant) -> (Standby, Link) {
        let mut s = Standby::new(
            crate::attach_exchange::fixtures::stun_free(),
            t0.checked_sub(STANDBY_DELAY).expect("a clock this young"),
        );
        let search = search_at(&mut s, t0);
        let (e, host) = established().await;
        assert!(s.found(search, e), "the fixture's search was stale");
        assert!(s.has_link(), "the fixture found nothing");
        (s, host)
    }

    /// How a connection ends when its far end closes it with `phrase`.
    async fn closed_by_the_host(phrase: &'static [u8]) -> quinn::ConnectionError {
        let (host, client) = crate::link::fixtures::link_pair().await;
        host.sink
            .connection()
            .close(quinn::VarInt::from_u32(0), phrase);
        tokio::time::timeout(Duration::from_secs(5), client.sink.connection().closed())
            .await
            .expect("the close never arrived")
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

        s.lost(t0, &quinn::ConnectionError::TimedOut);

        assert!(!s.has_link());
        assert_eq!(s.next_search(), t0 + standby_backoff(0));
        assert!(matches!(
            s.step(Phase::Live, t0 + standby_backoff(0), false),
            StandbyAction::Search { .. }
        ));
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
        search_at(&mut s, t0 + STANDBY_DELAY);
        assert_eq!(
            s.step(Phase::Live, t0 + STANDBY_DELAY * 2, false),
            StandbyAction::Nothing,
            "a second search started while the first was still running"
        );
    }

    #[test]
    fn every_current_failed_search_counts_and_backs_off() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let first = search_at(&mut s, t0 + STANDBY_DELAY);

        let t1 = t0 + STANDBY_DELAY;
        assert!(
            s.not_found(first, t1, "no path".to_string()),
            "the first failure did not count"
        );
        assert_eq!(s.next_search(), t1 + standby_backoff(0));
        let second = search_at(&mut s, t1 + standby_backoff(0));

        let t2 = t1 + standby_backoff(0);
        assert!(
            s.not_found(second, t2, "no path".to_string()),
            "a second current failure did not count"
        );
        assert_eq!(s.next_search(), t2 + standby_backoff(1));
    }

    #[tokio::test]
    async fn the_popup_can_read_what_the_standby_is_doing() {
        let t0 = Instant::now();
        let mut fresh = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        assert!(fresh.path().is_none() && fresh.rtt().is_none() && !fresh.searching());
        search_at(&mut fresh, t0 + STANDBY_DELAY);
        assert!(fresh.searching());

        let (s, _host) = with_a_standby(t0).await;
        assert_eq!(s.path().map(|p| p.rtt_ms), Some(38));
        assert!(s.rtt().is_some());
        assert_eq!(s.probe(), ProbeState::Idle);
        assert!(!s.searching());
    }

    /// A `NotFound` with a reason is kept for the popup's standby block.
    #[test]
    fn a_not_found_with_a_reason_sets_last_failure() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let search = search_at(&mut s, t0 + STANDBY_DELAY);
        assert_eq!(
            s.last_failure(),
            None,
            "a reason appeared before any search failed"
        );

        s.not_found(
            search,
            t0 + STANDBY_DELAY,
            "the standby search ran out of time".to_string(),
        );

        assert_eq!(
            s.last_failure(),
            Some("the standby search ran out of time"),
            "the failure's reason was dropped rather than kept"
        );
    }

    /// A later `Found` is good news, and a stale reason from the dry spell
    /// before it must not linger in the popup.
    #[tokio::test]
    async fn a_later_found_clears_last_failure() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let search = search_at(&mut s, t0 + STANDBY_DELAY);
        s.not_found(search, t0 + STANDBY_DELAY, "no path".to_string());
        assert!(
            s.last_failure().is_some(),
            "the fixture's failure was not set"
        );

        let next_at = s.next_search();
        let next = search_at(&mut s, next_at);
        let (e, _host) = established().await;
        assert!(s.found(next, e), "the fixture's search was stale");

        assert_eq!(
            s.last_failure(),
            None,
            "a standby was found, but the old dry spell's reason was still shown"
        );
    }

    /// A stale `NotFound` -- one whose search has already been disowned,
    /// because the primary it ran over is gone -- says nothing about the
    /// current primary and must not overwrite its box.
    #[test]
    fn a_stale_not_found_does_not_set_last_failure() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let stale = search_at(&mut s, t0 + STANDBY_DELAY);
        let t1 = t0 + STANDBY_DELAY + Duration::from_secs(1);
        s.forget(t1);
        assert_eq!(s.last_failure(), None, "the fixture started with a reason");

        s.not_found(stale, t1, "this must not be kept".to_string());

        assert_eq!(
            s.last_failure(),
            None,
            "a stale search's reason was kept as if it were current"
        );
    }

    #[test]
    fn a_moved_route_makes_a_search_due_at_once() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let t1 = t0 + STANDBY_DELAY;
        let search = search_at(&mut s, t1);
        let _ = s.not_found(search, t1, "no path".to_string());
        assert_eq!(
            s.step(Phase::Live, t1 + Duration::from_secs(1), false),
            StandbyAction::Nothing,
            "the fixture has no backoff to cut short"
        );

        s.route_moved(t1 + Duration::from_secs(1));

        search_at(&mut s, t1 + Duration::from_secs(1));
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
    async fn losing_the_standby_counts_and_so_does_the_next_failure() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert!(
            s.lost(t0, &quinn::ConnectionError::TimedOut),
            "losing the standby went unrecorded"
        );
        let search = search_at(&mut s, t0 + standby_backoff(0));
        assert!(
            s.not_found(search, t0 + standby_backoff(0), "no path".to_string()),
            "the failure after the loss did not count"
        );
    }

    /// `probed` says whether the result was for the probe in flight, which
    /// is what the session records.
    #[tokio::test]
    async fn a_probe_result_after_its_outage_ended_does_not_count() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert!(matches!(
            s.step(silent(t0), t0, false),
            StandbyAction::Probe { .. }
        ));
        s.step(Phase::Live, t0 + Duration::from_millis(100), false);
        assert!(
            !s.probed(true, t0 + Duration::from_millis(200)),
            "a stale answer counted"
        );

        assert!(matches!(
            s.step(silent(t0), t0 + Duration::from_secs(1), false),
            StandbyAction::Probe { .. }
        ));
        assert!(
            s.probed(true, t0 + Duration::from_secs(1)),
            "a current answer did not count"
        );
    }

    /// The host closed the standby the moment it adopted the rebuild, but a
    /// standby whose path is dead never hears that, and with no idle timeout
    /// it would never end. So forgetting it closes it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn forgetting_the_standby_closes_it() {
        let t0 = Instant::now();
        let (mut s, host) = with_a_standby(t0).await;
        assert!(
            host.sink.connection().close_reason().is_none(),
            "the fixture's standby was closed before anything forgot it"
        );

        s.forget(t0);

        assert!(!s.has_link());
        let reason = tokio::time::timeout(Duration::from_secs(5), host.sink.connection().closed())
            .await
            .expect("the forgotten standby was never closed");
        assert!(
            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c) if c.reason.as_ref() == REBUILT),
            "closed as {reason:?}"
        );
    }

    /// A search still running when a rebuild lands fails as soon as the old
    /// primary is closed under it. That failure is about a link the session
    /// no longer has: it must neither be recorded nor push the next
    /// search out by a backoff.
    #[test]
    fn a_search_over_a_replaced_primary_changes_nothing() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let stale = search_at(&mut s, t0 + STANDBY_DELAY);
        let t1 = t0 + STANDBY_DELAY + Duration::from_secs(1);

        s.forget(t1);

        assert!(
            !s.not_found(stale, t1, "this must not be kept".to_string()),
            "a search over the replaced primary counted as current"
        );
        assert_eq!(
            s.next_search(),
            t1 + STANDBY_DELAY,
            "a search over the replaced primary backed the next one off"
        );
        // And the next search is not held up waiting for the stale one.
        let next = search_at(&mut s, t1 + STANDBY_DELAY);
        assert_ne!(next, stale);
        // A current search's failure still counts, so the check above is
        // not a `not_found` that never reports anything.
        assert!(s.not_found(next, t1 + STANDBY_DELAY, "no path".to_string()));
    }

    /// The same after a failover, and a search that lands late is closed
    /// rather than kept: it stood by for the primary that just died.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_found_for_a_replaced_primary_is_closed_not_kept() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let stale = search_at(&mut s, t0 + STANDBY_DELAY);

        assert!(s.take_for_failover(t0 + STANDBY_DELAY).is_none());
        let (e, host) = established().await;

        assert!(!s.found(stale, e), "a stale search's link was kept");
        assert!(!s.has_link());
        let reason = tokio::time::timeout(Duration::from_secs(5), host.sink.connection().closed())
            .await
            .expect("the stale standby was never closed");
        assert!(
            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c) if c.reason.as_ref() == STALE),
            "closed as {reason:?}"
        );
    }

    /// A standby closed because the session is ending -- its shell exited,
    /// or another client took it over -- is not worth recording: the
    /// primary's close is about to say why.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_closed_by_the_session_ending_goes_unrecorded() {
        for phrase in [TAKEN_OVER, SHELL_EXITED] {
            let t0 = Instant::now();
            let (mut s, _host) = with_a_standby(t0).await;
            let reason = closed_by_the_host(phrase).await;

            assert!(
                !s.lost(t0, &reason),
                "recorded a standby closed as {:?}",
                String::from_utf8_lossy(phrase)
            );
            let search = search_at(&mut s, t0 + standby_backoff(0));
            assert!(
                s.not_found(search, t0 + standby_backoff(0), "no path".to_string()),
                "a current failure after the close went unrecorded (closed as {:?})",
                String::from_utf8_lossy(phrase)
            );
        }
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

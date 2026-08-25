//! The ladder: which rungs get tried, which get skipped, and what happens when
//! none of them works.
//!
//! Every rung here is supplied by a fake runner, so the decision logic is under
//! test without a network. The real implementations drop in behind the same
//! seam.

use std::sync::Arc;

use oxutrm_host::ladder::{LadderError, LadderPlan, RungResult, RungRunner, nominate, status_line};
use oxutrm_host::{DetachPermit, SessionMeta, daemonize_session, settle_detachability};
use oxutrm_proto::{NatType, PathDescription, Rung, TermSize};

fn nominated(rtt_ms: u32, probes_sent: u32) -> RungResult {
    RungResult::Nominated {
        local: "198.51.100.4:51234".parse().unwrap(),
        remote: "192.0.2.7:443".parse().unwrap(),
        probes_sent,
        rtt_ms,
        mtu: 1392,
    }
}

/// A runner whose answer for each rung is decided up front, and which records
/// what it was actually asked to do.
struct Scripted {
    answers: Vec<(Rung, RungResult)>,
    asked: Arc<std::sync::Mutex<Vec<Rung>>>,
    /// Milliseconds each attempt takes, so a race has something to race.
    delay_ms: u64,
}

impl Scripted {
    fn new(answers: Vec<(Rung, RungResult)>) -> (Arc<Self>, Arc<std::sync::Mutex<Vec<Rung>>>) {
        let asked = Arc::new(std::sync::Mutex::new(Vec::new()));
        (
            Arc::new(Scripted {
                answers,
                asked: Arc::clone(&asked),
                delay_ms: 0,
            }),
            asked,
        )
    }
}

impl RungRunner for Scripted {
    fn attempt(
        self: Arc<Self>,
        rung: Rung,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = RungResult> + Send>> {
        Box::pin(async move {
            if self.delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)).await;
            }
            if let Ok(mut g) = self.asked.lock() {
                g.push(rung);
            }
            self.answers
                .iter()
                .find(|(r, _)| *r == rung)
                .map(|(_, res)| res.clone())
                .unwrap_or_else(|| RungResult::Failed("no script entry".to_string()))
        })
    }
}

fn meta() -> SessionMeta {
    SessionMeta {
        session_id: "1111222233334444aaaabbbbccccdddd".to_string(),
        attach_id: 1,
        pid: std::process::id(),
        created_unix: oxutrm_host::now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    }
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

#[test]
fn the_cheap_rungs_are_raced_and_the_noisy_one_is_not() {
    let plan = LadderPlan::for_nat(NatType::EndpointIndependent);
    assert_eq!(
        plan.raced,
        vec![Rung::Ipv6Direct, Rung::PortMapped, Rung::StunPunch],
        "rungs 0 to 2 are cheap and race together"
    );
    assert_eq!(
        plan.sequential,
        vec![Rung::Birthday, Rung::SshTunnel],
        "the blast is deliberately noisy, so it never joins the race"
    );
    assert!(plan.skipped.is_empty());
}

#[test]
fn a_symmetric_nat_skips_punching_rather_than_failing_at_it() {
    // The three-probe classification already established that rung 2 cannot
    // work. Attempting it anyway burns several seconds to discover something
    // known in advance.
    let plan = LadderPlan::for_nat(NatType::Symmetric);
    assert!(
        !plan.raced.contains(&Rung::StunPunch),
        "punching a symmetric NAT is hopeless, not merely unlikely"
    );
    assert_eq!(plan.raced, vec![Rung::Ipv6Direct, Rung::PortMapped]);
    assert!(
        plan.sequential.first() == Some(&Rung::Birthday),
        "and the blast comes next, which is what it exists for"
    );

    let (rung, why) = plan.skipped.first().expect("the skip must be recorded");
    assert_eq!(*rung, Rung::StunPunch);
    assert!(
        why.contains("symmetric"),
        "a skipped rung must say why, for the bug report: {why}"
    );
}

#[test]
fn an_unknown_nat_type_skips_nothing() {
    // Guessing wrong towards skipping costs a connection; guessing wrong
    // towards attempting costs a few seconds.
    let plan = LadderPlan::for_nat(NatType::Unknown);
    assert!(plan.skipped.is_empty());
    assert!(plan.raced.contains(&Rung::StunPunch));
}

#[test]
fn the_tunnel_is_always_last() {
    for nat in [
        NatType::None,
        NatType::EndpointIndependent,
        NatType::AddressDependent,
        NatType::Symmetric,
        NatType::Unknown,
    ] {
        let plan = LadderPlan::for_nat(nat);
        assert_eq!(
            plan.attempted().last(),
            Some(&Rung::SshTunnel),
            "the tunnel is the fallback of last resort for {nat:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Nomination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_validated_path_wins_the_race() {
    let plan = LadderPlan::for_nat(NatType::EndpointIndependent);
    let (runner, _asked) = Scripted::new(vec![
        (Rung::Ipv6Direct, nominated(11, 0)),
        (Rung::PortMapped, nominated(38, 0)),
        (Rung::StunPunch, nominated(61, 4)),
    ]);

    let path = nominate(&plan, NatType::EndpointIndependent, runner)
        .await
        .expect("something must win");
    assert!(
        plan.raced.contains(&path.rung),
        "the winner came from the raced group, not from later: {:?}",
        path.rung
    );
}

#[tokio::test]
async fn the_ladder_falls_through_to_the_tunnel_when_nothing_else_forms() {
    let plan = LadderPlan::for_nat(NatType::Symmetric);
    let (runner, asked) = Scripted::new(vec![
        (
            Rung::Ipv6Direct,
            RungResult::Failed("no global v6".to_string()),
        ),
        (
            Rung::PortMapped,
            RungResult::Failed("no NAT-PMP".to_string()),
        ),
        (
            Rung::Birthday,
            RungResult::Failed("budget exhausted".to_string()),
        ),
        (Rung::SshTunnel, nominated(45, 0)),
    ]);

    let path = nominate(&plan, NatType::Symmetric, runner)
        .await
        .expect("the tunnel always works if ssh is up");
    assert_eq!(path.rung, Rung::SshTunnel);
    assert_eq!(path.nat_type, NatType::Symmetric, "carried into the result");

    let asked = asked.lock().unwrap().clone();
    assert!(
        !asked.contains(&Rung::StunPunch),
        "the skipped rung must never be attempted: {asked:?}"
    );
    assert_eq!(
        asked.last(),
        Some(&Rung::SshTunnel),
        "and the tunnel is reached last: {asked:?}"
    );
}

#[tokio::test]
async fn a_path_description_is_filled_in_from_what_happened() {
    // This is what the status line renders and what a user pastes into a bug
    // report, so no field may be left at a default.
    let plan = LadderPlan::for_nat(NatType::AddressDependent);
    let (runner, _) = Scripted::new(vec![(Rung::Ipv6Direct, nominated(11, 7))]);

    let path = nominate(&plan, NatType::AddressDependent, runner)
        .await
        .expect("nominated");
    assert_eq!(path.rung, Rung::Ipv6Direct);
    assert_eq!(path.rtt_ms, 11);
    assert_eq!(path.probes_sent, 7);
    assert_eq!(path.mtu, 1392);
    assert_eq!(path.nat_type, NatType::AddressDependent);
    assert_eq!(path.local.port(), 51234);
    assert_eq!(path.remote.port(), 443);
}

#[tokio::test]
async fn every_rung_failing_reports_each_reason_rather_than_connection_failed() {
    let plan = LadderPlan::for_nat(NatType::Symmetric);
    let (runner, _) = Scripted::new(vec![
        (
            Rung::Ipv6Direct,
            RungResult::Failed("no global v6 address".to_string()),
        ),
        (
            Rung::PortMapped,
            RungResult::Failed("gateway refused PCP".to_string()),
        ),
        (
            Rung::Birthday,
            RungResult::Failed("65k probes, no hit".to_string()),
        ),
        (
            Rung::SshTunnel,
            RungResult::Failed("ssh channel closed".to_string()),
        ),
    ]);

    let err = nominate(&plan, NatType::Symmetric, runner)
        .await
        .expect_err("nothing worked");
    let LadderError::NoPath { .. } = &err;

    let text = err.to_string();
    for expected in [
        "no global v6 address",
        "gateway refused PCP",
        "65k probes, no hit",
        "ssh channel closed",
    ] {
        assert!(text.contains(expected), "must keep {expected:?}: {text}");
    }
    assert!(
        text.contains("skipped"),
        "and must distinguish a skipped rung from a failed one: {text}"
    );
}

// ---------------------------------------------------------------------------
// Rung 4 is announced, not silent
// ---------------------------------------------------------------------------

fn described(rung: Rung, nat: NatType, probes: u32) -> PathDescription {
    PathDescription {
        rung,
        local: "198.51.100.4:51234".parse().unwrap(),
        remote: "192.0.2.7:443".parse().unwrap(),
        probes_sent: probes,
        nat_type: nat,
        rtt_ms: 45,
        mtu: 1392,
    }
}

#[test]
fn the_tunnel_status_line_is_a_warning_that_names_what_was_lost() {
    let line = status_line(&described(Rung::SshTunnel, NatType::Symmetric, 0));
    assert!(line.contains("[warning]"), "{line}");
    assert!(line.contains("SSH tunnel"), "{line}");
    assert!(
        line.contains("cannot roam") && line.contains("cannot be reattached"),
        "silent degradation is worse than slow; the user finds out by closing \
         their laptop otherwise: {line}"
    );
}

#[test]
fn an_ordinary_path_is_one_quiet_line() {
    let line = status_line(&described(Rung::Ipv6Direct, NatType::None, 0));
    assert!(!line.contains("warning"), "{line}");
    assert!(line.contains("IPv6 direct"), "{line}");
    assert!(line.contains("mtu 1392"), "{line}");
}

#[test]
fn a_birthday_path_reports_what_it_cost() {
    let line = status_line(&described(Rung::Birthday, NatType::Symmetric, 312));
    assert!(line.contains("312 probes"), "the cost was real: {line}");
    assert!(line.contains("symmetric NAT"), "{line}");
}

// ---------------------------------------------------------------------------
// A rung-4 session cannot daemonize. Enforced by the type, not by convention.
// ---------------------------------------------------------------------------

#[test]
fn a_tunnelled_session_gets_no_permit_and_is_recorded_as_undetachable() {
    let mut m = meta();
    assert!(m.detachable, "the handshake was optimistic");

    let permit = settle_detachability(&mut m, Rung::SshTunnel);

    assert!(
        permit.is_none(),
        "there must be no way to obtain the token daemonize_session demands"
    );
    assert!(
        !m.detachable,
        "and --list must not offer this session for reattach"
    );
}

#[test]
fn every_other_rung_yields_a_permit() {
    for rung in [
        Rung::Ipv6Direct,
        Rung::PortMapped,
        Rung::StunPunch,
        Rung::Birthday,
    ] {
        let mut m = meta();
        m.detachable = false;
        let permit: Option<DetachPermit> = settle_detachability(&mut m, rung);
        assert!(permit.is_some(), "{rung:?} carries its own socket");
        assert!(m.detachable, "{rung:?}");
    }
}

/// The ordering made structural. `daemonize_session` takes a `DetachPermit`,
/// and the only source of one is `settle_detachability`, which needs the
/// nominated rung — so there is no way to write a call that detaches before the
/// rung is known, because there is nothing to pass.
#[test]
fn daemonize_session_cannot_be_called_without_settling_the_rung_first() {
    let mut m = meta();
    // The only way to get here is through a nominated rung.
    let permit = settle_detachability(&mut m, Rung::Ipv6Direct).expect("permit");

    // Deliberately NOT calling daemonize_session: it would fork this test
    // process. What matters is that the signature demands the token, which the
    // line below proves at compile time.
    let _takes_a_permit: fn(DetachPermit) -> anyhow::Result<()> = daemonize_session;
    let _ = permit;

    // And the rung-4 path simply has no token to offer.
    let mut tunnelled = meta();
    assert!(settle_detachability(&mut tunnelled, Rung::SshTunnel).is_none());
}

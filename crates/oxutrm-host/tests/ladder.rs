//! The ladder's policy: which rungs get tried, which get skipped, and the
//! ordering rule that stops a rung-4 session from detaching.
//!
//! There is no fake runner here any more. The nomination tests that used one
//! went with `RungRunner` itself: the trait's only implementor in the entire
//! tree was the double in this file, and the mechanism it stood in for cannot
//! be expressed rung-by-rung anyway — rungs 0 to 2 are candidate classes on one
//! shared socket, and rung 3 hands back a socket no `RungResult` could carry.
//! See the module docs on `oxutrm_host::ladder`. What remains under test is the
//! part that was always worth testing without a network: the plan.
//!
//! The status-line assertions went to their surviving implementation,
//! `oxutrm_client::status_line`, which is exercised in
//! `crates/oxutrm-client/src/status.rs`.

use oxutrm_host::ladder::LadderPlan;
use oxutrm_host::{DetachPermit, SessionMeta, daemonize_session, settle_detachability};
use oxutrm_proto::{NatType, Rung, TermSize};

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

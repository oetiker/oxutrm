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

use oxutrm_host::ladder::{EndFacts, LadderPlan};
use oxutrm_host::{DetachPermit, SessionMeta, daemonize_session, settle_detachability};
use oxutrm_proto::{Family, FamilyNat, NatType, Rung, TermSize};

fn meta() -> SessionMeta {
    SessionMeta {
        session_id: "1111222233334444aaaabbbbccccdddd".to_string(),
        attach_id: 1,
        pid: std::process::id(),
        created_unix: oxutrm_host::now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
        boot: None,
        name: None,
    }
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// An end that offers a server-reflexive candidate in every family it has an
/// address in, which is what a successful STUN round gives.
fn end(v4: NatType, v6: NatType, has_v4: bool, has_v6: bool) -> EndFacts {
    EndFacts {
        nat: FamilyNat { v4, v6 },
        v4: has_v4,
        v6: has_v6,
        srflx_v4: has_v4,
        srflx_v6: has_v6,
    }
}

/// The Birthday rungs a plan will run, in order.
fn births(p: &LadderPlan) -> Vec<Rung> {
    p.sequential
        .iter()
        .filter(|r| matches!(r, Rung::Birthday(_)))
        .copied()
        .collect()
}

/// The Birthday rungs a plan skipped, in order.
fn birth_skips(p: &LadderPlan) -> Vec<Rung> {
    p.skipped
        .iter()
        .map(|(r, _)| *r)
        .filter(|r| matches!(r, Rung::Birthday(_)))
        .collect()
}

#[test]
fn the_cheap_rungs_are_raced_and_the_noisy_one_is_not() {
    // Endpoint-independent on both ends in both families: ordinary punching
    // covers everything, so the blast is not planned at all.
    let e = end(
        NatType::EndpointIndependent,
        NatType::EndpointIndependent,
        true,
        true,
    );
    let plan = LadderPlan::for_ends(&e, &e);
    assert_eq!(
        plan.raced,
        vec![
            Rung::Direct(Family::V6),
            Rung::Direct(Family::V4),
            Rung::PortMapped(Family::V6),
            Rung::PortMapped(Family::V4),
            Rung::Punched(Family::V6),
            Rung::Punched(Family::V4),
        ],
        "methods 0 to 2 are cheap and race together, per family"
    );
    assert_eq!(
        plan.sequential,
        vec![Rung::SshTunnel],
        "neither NAT is symmetric, so the noisy blast is not planned"
    );
    assert_eq!(plan.skipped.len(), 2, "{:?}", plan.skipped);
    for (r, why) in &plan.skipped {
        assert!(matches!(r, Rung::Birthday(_)), "{r}");
        assert!(why.contains("ordinary punching covers it"), "{why}");
    }
}

#[test]
fn a_symmetric_nat_skips_punching_rather_than_failing_at_it() {
    // The three-probe classification already established that rung 2 cannot
    // work. Attempting it anyway burns several seconds to discover something
    // known in advance.
    let ours = end(NatType::Symmetric, NatType::Symmetric, true, true);
    let theirs = end(
        NatType::EndpointIndependent,
        NatType::EndpointIndependent,
        true,
        true,
    );
    let plan = LadderPlan::for_ends(&ours, &theirs);
    assert!(
        !plan.raced.contains(&Rung::Punched(Family::V6))
            && !plan.raced.contains(&Rung::Punched(Family::V4)),
        "punching a symmetric NAT is hopeless, not merely unlikely"
    );
    assert_eq!(
        plan.sequential,
        vec![
            Rung::Birthday(Family::V6),
            Rung::Birthday(Family::V4),
            Rung::SshTunnel
        ],
        "and the blast comes next, which is what it exists for"
    );
    for f in Family::BOTH {
        let why = &plan
            .skipped
            .iter()
            .find(|(r, _)| *r == Rung::Punched(f))
            .expect("punch skipped")
            .1;
        assert!(
            why.contains("symmetric") && why.contains(f.label()),
            "a skipped rung must say why, for the bug report: {why}"
        );
    }
}

#[test]
fn a_symmetric_ipv4_nat_does_not_stop_ipv6_punching() {
    let ours = end(NatType::Symmetric, NatType::EndpointIndependent, true, true);
    let theirs = end(
        NatType::EndpointIndependent,
        NatType::EndpointIndependent,
        true,
        true,
    );
    let plan = LadderPlan::for_ends(&ours, &theirs);
    assert!(plan.raced.contains(&Rung::Punched(Family::V6)));
    assert!(!plan.raced.contains(&Rung::Punched(Family::V4)));
    assert!(plan.sequential.contains(&Rung::Birthday(Family::V4)));
    assert!(!plan.sequential.contains(&Rung::Birthday(Family::V6)));
}

#[test]
fn a_family_one_end_lacks_is_skipped_with_whose_it_is() {
    let ours = end(NatType::EndpointIndependent, NatType::Unknown, true, false);
    let theirs = end(NatType::EndpointIndependent, NatType::None, true, true);
    let plan = LadderPlan::for_ends(&ours, &theirs);
    for r in [
        Rung::Direct(Family::V6),
        Rung::PortMapped(Family::V6),
        Rung::Punched(Family::V6),
        Rung::Birthday(Family::V6),
    ] {
        let why = &plan
            .skipped
            .iter()
            .find(|(s, _)| *s == r)
            .expect("skipped")
            .1;
        assert!(why.contains("this end has no IPv6 address"), "{r}: {why}");
    }
    let plan = LadderPlan::for_ends(&theirs, &ours);
    let why = &plan
        .skipped
        .iter()
        .find(|(s, _)| *s == Rung::Direct(Family::V6))
        .unwrap()
        .1;
    assert!(why.contains("the peer has no IPv6 address"), "{why}");
}

#[test]
fn an_unknown_family_nat_skips_nothing_in_that_family() {
    // Guessing wrong towards skipping costs a connection; guessing wrong
    // towards attempting costs a few seconds.
    let ours = end(NatType::Unknown, NatType::Unknown, true, true);
    let plan = LadderPlan::for_ends(&ours, &ours);
    assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
}

#[test]
fn both_ends_agree_on_which_family_to_blast() {
    let a = end(NatType::Symmetric, NatType::None, true, true);
    let b = end(NatType::EndpointIndependent, NatType::None, true, true);
    assert_eq!(
        births(&LadderPlan::for_ends(&a, &b)),
        births(&LadderPlan::for_ends(&b, &a))
    );
    assert_eq!(
        births(&LadderPlan::for_ends(&a, &b)),
        vec![Rung::Birthday(Family::V4)]
    );
}

/// A blast needs both ends spraying at once. An end without a
/// server-reflexive candidate in a family gives the other end no port to guess
/// around, so only one end could blast; the plan must say so on both ends,
/// or the one that does blast spends its budget alone and every later family
/// falls out of step.
#[test]
fn a_blast_needs_both_ends_server_reflexive_candidate_in_its_family() {
    let mut ours = end(NatType::Symmetric, NatType::Symmetric, true, true);
    ours.srflx_v4 = false;
    let theirs = end(NatType::Symmetric, NatType::Symmetric, true, true);

    let plan = LadderPlan::for_ends(&ours, &theirs);
    assert_eq!(births(&plan), vec![Rung::Birthday(Family::V6)]);
    let why = &plan
        .skipped
        .iter()
        .find(|(r, _)| *r == Rung::Birthday(Family::V4))
        .expect("the IPv4 blast is skipped")
        .1;
    assert!(
        why.contains("this end offered no IPv4 server-reflexive candidate"),
        "{why}"
    );

    let plan = LadderPlan::for_ends(&theirs, &ours);
    assert_eq!(births(&plan), vec![Rung::Birthday(Family::V6)]);
    let why = &plan
        .skipped
        .iter()
        .find(|(r, _)| *r == Rung::Birthday(Family::V4))
        .expect("the IPv4 blast is skipped")
        .1;
    assert!(
        why.contains("the peer offered no IPv4 server-reflexive candidate"),
        "{why}"
    );
}

/// An address-dependent mapping changes per destination IP, so the
/// server-reflexive address the peer was given is not the one it will see:
/// that NAT needs the blast as much as a symmetric one does, and ordinary
/// punching is as hopeless behind it.
#[test]
fn an_address_dependent_nat_on_either_end_admits_the_blast() {
    let ad = end(NatType::AddressDependent, NatType::None, true, true);
    let ei = end(NatType::EndpointIndependent, NatType::None, true, true);
    for (ours, theirs) in [(&ad, &ei), (&ei, &ad)] {
        let plan = LadderPlan::for_ends(ours, theirs);
        assert_eq!(births(&plan), vec![Rung::Birthday(Family::V4)], "{plan:?}");
    }
    let plan = LadderPlan::for_ends(&ad, &ei);
    let why = &plan
        .skipped
        .iter()
        .find(|(r, _)| *r == Rung::Punched(Family::V4))
        .expect("punching behind our address-dependent NAT is skipped")
        .1;
    assert!(why.contains("IPv4"), "{why}");
    assert!(
        !LadderPlan::for_ends(&ei, &ad)
            .skipped
            .iter()
            .any(|(r, _)| *r == Rung::Punched(Family::V4)),
        "only our own NAT decides our punch rung"
    );
}

/// Client and host each plan from their own side. For the blast to be aimed at
/// somebody, the two must admit, and skip, exactly the same Birthday rungs.
#[test]
fn every_pair_of_ends_agrees_on_the_blast_from_both_sides() {
    for (a, b) in fact_pairs() {
        let ab = LadderPlan::for_ends(&a, &b);
        let ba = LadderPlan::for_ends(&b, &a);
        assert_eq!(births(&ab), births(&ba), "{a:?} / {b:?}");
        assert_eq!(birth_skips(&ab), birth_skips(&ba), "{a:?} / {b:?}");
    }
}

#[test]
fn facts_come_from_the_candidates_by_family() {
    use oxutrm_proto::{Candidate, CandidateKind};
    let cand = |addr: &str| Candidate {
        kind: CandidateKind::Host,
        addr: addr.parse().unwrap(),
        priority: 1,
    };
    let facts = EndFacts::from_candidates(FamilyNat::UNKNOWN, &[cand("[2001:db8::1]:4000")]);
    assert!(facts.has(Family::V6) && !facts.has(Family::V4));
    let facts = EndFacts::from_candidates(FamilyNat::UNKNOWN, &[cand("192.0.2.1:4000")]);
    assert!(facts.has(Family::V4) && !facts.has(Family::V6));
    assert!(
        !facts.has_srflx(Family::V4),
        "a host candidate is not server-reflexive"
    );

    let srflx = Candidate {
        kind: CandidateKind::ServerReflexive,
        addr: "203.0.113.7:4000".parse().unwrap(),
        priority: 1,
    };
    let facts = EndFacts::from_candidates(
        FamilyNat::UNKNOWN,
        &[cand("[2001:db8::1]:4000"), cand("192.0.2.1:4000"), srflx],
    );
    assert!(facts.has_srflx(Family::V4) && !facts.has_srflx(Family::V6));
}

#[test]
fn every_rung_is_accounted_for_exactly_once() {
    for plan in plans() {
        for rung in Rung::ALL {
            let n = plan.raced.iter().filter(|r| **r == rung).count()
                + plan.sequential.iter().filter(|r| **r == rung).count()
                + plan.skipped.iter().filter(|(r, _)| *r == rung).count();
            assert_eq!(n, 1, "{rung} in {plan:?}");
        }
    }
}

/// A spread of fact pairs: every NAT type per family, every combination of
/// families present, and with a server-reflexive candidate missing in either.
fn fact_pairs() -> Vec<(EndFacts, EndFacts)> {
    let nats = [
        NatType::None,
        NatType::EndpointIndependent,
        NatType::AddressDependent,
        NatType::Symmetric,
        NatType::Unknown,
    ];
    let flags = [(true, true), (true, false), (false, true), (false, false)];
    let mut out = Vec::new();
    for n4 in nats {
        for n6 in nats {
            for (h4, h6) in flags {
                for (s4, s6) in flags {
                    let mut ours = end(n4, n6, h4, h6);
                    ours.srflx_v4 = h4 && s4;
                    ours.srflx_v6 = h6 && s6;
                    let theirs = end(n6, n4, true, true);
                    out.push((ours, theirs));
                }
            }
        }
    }
    out
}

/// The plans for those pairs, planned from each side.
fn plans() -> Vec<LadderPlan> {
    fact_pairs()
        .into_iter()
        .flat_map(|(a, b)| [LadderPlan::for_ends(&a, &b), LadderPlan::for_ends(&b, &a)])
        .collect()
}

#[test]
fn the_tunnel_is_always_last() {
    for plan in plans() {
        assert_eq!(
            plan.attempted().last(),
            Some(&Rung::SshTunnel),
            "the tunnel is the fallback of last resort: {plan:?}"
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
    for rung in Rung::ALL.into_iter().filter(|r| *r != Rung::SshTunnel) {
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
    let permit = settle_detachability(&mut m, Rung::Direct(Family::V6)).expect("permit");

    // Deliberately NOT calling daemonize_session: it would fork this test
    // process. What matters is that the signature demands the token, which the
    // line below proves at compile time.
    let _takes_a_permit: fn(DetachPermit) -> anyhow::Result<()> = daemonize_session;
    let _ = permit;

    // And the rung-4 path simply has no token to offer.
    let mut tunnelled = meta();
    assert!(settle_detachability(&mut tunnelled, Rung::SshTunnel).is_none());
}

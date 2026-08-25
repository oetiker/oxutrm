//! Detachability is decided by the rung ICE nominated, never at handshake.
//!
//! `daemonize` closes every inherited descriptor. For an ordinary session that
//! is exactly right. For a rung-4 session it is fatal: its QUIC traffic runs
//! inside a stream on the ssh connection those descriptors belong to, so
//! closing them destroys the link the session depends on.
//!
//! `HostHello.detachable` is therefore the host's *intent*, and
//! `SessionMeta.detachable` is the *outcome*. The ordering — settle, then
//! decide — is the whole point, and these tests pin it.

use oxutrm_host::{Registry, RegistryGuard, SessionMeta, detachable_for_rung, now_unix};
use oxutrm_proto::{Rung, TermSize};

fn meta(id: &str) -> SessionMeta {
    SessionMeta {
        session_id: id.to_string(),
        attach_id: 1,
        pid: std::process::id(),
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        // The host's intent, before ICE has nominated anything. Deliberately
        // optimistic, which is why it must not be trusted.
        detachable: true,
    }
}

#[test]
fn every_rung_but_the_ssh_tunnel_can_detach() {
    for rung in [
        Rung::Ipv6Direct,
        Rung::PortMapped,
        Rung::StunPunch,
        Rung::Birthday,
    ] {
        assert!(
            detachable_for_rung(rung),
            "{rung:?} carries QUIC on its own UDP socket, which outlives ssh"
        );
    }
    assert!(
        !detachable_for_rung(Rung::SshTunnel),
        "rung 4 tunnels QUIC through ssh and cannot outlive it"
    );
}

#[test]
fn settling_on_the_ssh_tunnel_overrides_an_optimistic_intent() {
    let mut m = meta("1111111111111111aaaaaaaaaaaaaaaa");
    assert!(m.detachable, "the handshake said it hoped to detach");

    let may_daemonize = m.set_detachable(Rung::SshTunnel);

    assert!(!may_daemonize, "set_detachable must report the outcome");
    assert!(
        !m.detachable,
        "intent must not survive contact with the nominated rung; a session \
         that daemonized here would have closed the ssh descriptors its own \
         QUIC traffic runs inside"
    );
}

#[test]
fn settling_on_a_direct_rung_confirms_detachability() {
    let mut m = meta("2222222222222222bbbbbbbbbbbbbbbb");
    m.detachable = false;
    assert!(m.set_detachable(Rung::Ipv6Direct));
    assert!(m.detachable);
}

#[test]
fn set_detachable_is_idempotent_and_last_write_wins() {
    // A reattach renegotiates, so the same session may settle differently on a
    // later attach. The recorded outcome must follow the current rung.
    let mut m = meta("3333333333333333cccccccccccccccc");
    assert!(m.set_detachable(Rung::StunPunch));
    assert!(m.set_detachable(Rung::StunPunch), "idempotent");
    assert!(
        !m.set_detachable(Rung::SshTunnel),
        "a later attach may differ"
    );
    assert!(m.set_detachable(Rung::PortMapped), "and differ again");
}

/// `--list` must show the difference, because "reattach later" is a promise
/// oxutrm must not make falsely.
#[test]
fn the_outcome_reaches_the_registry_and_survives_a_reread() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());

    let mut m = meta("4444444444444444dddddddddddddddd");
    let guard = RegistryGuard::register_in(&root, &m).expect("register");

    // ICE nominates the tunnel. Settle, then rewrite what --list reads.
    m.set_detachable(Rung::SshTunnel);
    guard.update(&m).expect("update");

    let listed = Registry::list_in(&root).expect("list");
    assert_eq!(listed.len(), 1);
    assert!(
        !listed[0].detachable,
        "--list must report this session as one that cannot be reattached"
    );
}

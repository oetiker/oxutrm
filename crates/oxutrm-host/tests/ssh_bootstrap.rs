//! Driving `ssh`, and failing usefully when it does not work.
//!
//! Every test here runs a **real subprocess over real pipes** — the fixture in
//! `src/bin/oxutrm-fake-ssh.rs` — rather than a mock behind a trait. That is
//! deliberate: the pipe handling is a large part of what can go wrong (a
//! deadlock on an undrained stderr, a `HostHello` stuck in a buffer), and a
//! trait-shaped mock would bypass exactly the code that has those bugs.
//!
//! The failure modes are the substance. A user whose remote binary is missing
//! must not be told the connection failed, because they will spend the next
//! hour on their network.

use oxutrm_host::ssh::{BootstrapError, SshChannel, SshLauncher};
use oxutrm_proto::{NatType, PROTO_VERSION, Signal, TermSize, TerminalCaps};

fn fake(mode: &str) -> SshLauncher {
    SshLauncher::command(env!("CARGO_BIN_EXE_oxutrm-fake-ssh")).env("OXUTRM_FAKE_SSH_MODE", mode)
}

fn client_hello() -> Signal {
    Signal::ClientHello {
        proto: PROTO_VERSION,
        candidates: vec![],
        nat_type: NatType::Unknown,
        caps: TerminalCaps {
            truecolor: true,
            colors: 16_777_216,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_string(),
        },
        size: TermSize { cols: 80, rows: 24 },
    }
}

// ---------------------------------------------------------------------------
// The happy path, over a deliberately noisy login
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_full_handshake_survives_a_banner_and_a_motd() {
    let mut ch = SshChannel::open(&fake("serve"), "bastion.example.net")
        .await
        .expect("open");

    let hello = ch.recv().await.expect("HostHello");
    let (session_id, attach_id, psk) = match hello {
        Signal::HostHello {
            session_id,
            attach_id,
            psk,
            detachable,
            ..
        } => {
            assert!(
                detachable,
                "HostHello.detachable is the host's INTENT; the fixture is optimistic"
            );
            (session_id, attach_id, psk)
        }
        other => panic!("expected HostHello, got {other:?}"),
    };
    assert_eq!(session_id.len(), 32, "128 bits as lowercase hex");
    assert_eq!(attach_id, 1);
    // The PSK is a 32-byte type now, so "not empty" is no longer sayable and
    // no longer the useful claim. What is useful is that the fixture actually
    // put entropy in it: an all-zero PSK would have decoded perfectly.
    assert!(
        psk.as_bytes().iter().any(|b| *b != 0),
        "the fixture sent an all-zero PSK"
    );

    ch.send(&client_hello()).await.expect("send ClientHello");

    match ch.recv().await.expect("Established") {
        Signal::Established { path } => assert!(path.rtt_ms > 0),
        other => panic!("expected Established, got {other:?}"),
    }
}

#[tokio::test]
async fn the_wrapper_asks_the_remote_for_host_serve() {
    // The fixture exits 2 if the argv it received is not
    // `<target> oxutrm host --serve`, so a wrapper that built the wrong
    // command line fails here rather than silently connecting to nothing.
    let mut ch = SshChannel::open(&fake("serve"), "bastion.example.net")
        .await
        .expect("open");
    assert!(
        matches!(ch.recv().await, Ok(Signal::HostHello { .. })),
        "the fixture rejected the command line the wrapper built"
    );
}

// ---------------------------------------------------------------------------
// Failure mode 1: ssh itself is not installed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_ssh_binary_is_not_reported_as_a_connection_failure() {
    let launcher = SshLauncher::command("/nonexistent/definitely-not-ssh");
    let err = SshChannel::open(&launcher, "bastion.example.net")
        .await
        .expect_err("cannot spawn");

    match &err {
        BootstrapError::SshNotFound { program } => {
            assert!(program.contains("definitely-not-ssh"));
        }
        other => panic!("expected SshNotFound, got {other:?}"),
    }
    let text = err.to_string();
    assert!(
        text.contains("could not be run") || text.contains("not found"),
        "must name the real problem: {text}"
    );
    assert!(
        !text.to_lowercase().contains("connection"),
        "a missing ssh is not a connection failure: {text}"
    );
}

// ---------------------------------------------------------------------------
// Failure mode 2: ssh ran and failed. The reason is in stderr.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_authentication_failure_surfaces_ssh_stderr() {
    let mut ch = SshChannel::open(&fake("auth-fail"), "bastion.example.net")
        .await
        .expect("spawning succeeds; the failure comes later");
    let err = ch.recv().await.expect_err("no signal will arrive");

    match &err {
        BootstrapError::SshFailed { status, stderr } => {
            assert_eq!(*status, Some(255));
            assert!(
                stderr.contains("Permission denied"),
                "the real reason must survive: {stderr}"
            );
        }
        other => panic!("expected SshFailed, got {other:?}"),
    }
    assert!(
        err.to_string().contains("Permission denied"),
        "and must reach the user: {err}"
    );
}

#[tokio::test]
async fn a_host_key_change_surfaces_the_warning_verbatim() {
    let mut ch = SshChannel::open(&fake("host-key"), "bastion.example.net")
        .await
        .expect("spawn");
    let err = ch.recv().await.expect_err("no signal");

    let text = err.to_string();
    assert!(
        text.contains("REMOTE HOST IDENTIFICATION HAS CHANGED"),
        "a host key change is not something to paraphrase: {text}"
    );
}

// ---------------------------------------------------------------------------
// Failure mode 3: the remote binary is missing. THE first-run problem.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_remote_binary_is_its_own_error_with_advice() {
    let mut ch = SshChannel::open(&fake("missing-binary"), "bastion.example.net")
        .await
        .expect("spawn");
    let err = ch.recv().await.expect_err("no signal");

    match &err {
        BootstrapError::RemoteBinaryMissing { target, .. } => {
            assert_eq!(target, "bastion.example.net");
        }
        other => panic!(
            "a missing remote binary must be its own variant, not {other:?}; \
             it is the most likely first-run problem and 'connection failed' \
             sends the user hunting the wrong thing entirely"
        ),
    }

    let text = err.to_string();
    assert!(
        text.contains("bastion.example.net"),
        "must name the host to install it on: {text}"
    );
    assert!(
        text.to_lowercase().contains("install oxutrm"),
        "must say what to do about it: {text}"
    );
    assert!(
        !text.to_lowercase().contains("connection failed"),
        "must not be dressed up as a network problem: {text}"
    );
}

// ---------------------------------------------------------------------------
// Failure mode 4: connected, authenticated, and said nothing useful
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_login_that_produces_no_signal_says_so() {
    let mut ch = SshChannel::open(&fake("silent"), "bastion.example.net")
        .await
        .expect("spawn");
    let err = ch.recv().await.expect_err("no signal");

    match &err {
        BootstrapError::NoSignal { .. } => {}
        other => panic!("expected NoSignal, got {other:?}"),
    }
    assert!(
        err.to_string().contains("no oxutrm handshake"),
        "must distinguish 'said nothing' from 'said something wrong': {err}"
    );
}

#[tokio::test]
async fn a_remote_speaking_a_newer_protocol_fails_loudly() {
    let mut ch = SshChannel::open(&fake("version-skew"), "bastion.example.net")
        .await
        .expect("spawn");
    let err = ch.recv().await.expect_err("version skew");

    let text = err.to_string();
    assert!(
        text.contains("version"),
        "a version mismatch is a hard failure, never a downgrade: {text}"
    );
    assert!(text.contains(&(PROTO_VERSION + 41).to_string()));
}

// ---------------------------------------------------------------------------
// Not a deadlock
// ---------------------------------------------------------------------------

/// A chatty `ssh` fills the stderr pipe buffer. If nothing drains it, the child
/// blocks on write while the wrapper blocks on read, and the connection hangs
/// forever with no error at all.
#[tokio::test]
async fn a_chatty_stderr_does_not_deadlock_the_handshake() {
    let launcher = fake("serve").env("OXUTRM_FAKE_SSH_NOISE_KIB", "256");
    let handshake = async {
        let mut ch = SshChannel::open(&launcher, "bastion.example.net")
            .await
            .expect("open");
        let hello = ch.recv().await.expect("HostHello despite the noise");
        ch.send(&client_hello()).await.expect("send");
        hello
    };

    let hello = tokio::time::timeout(std::time::Duration::from_secs(20), handshake)
        .await
        .expect("the handshake deadlocked on an undrained stderr");
    assert!(matches!(hello, Signal::HostHello { .. }));
}

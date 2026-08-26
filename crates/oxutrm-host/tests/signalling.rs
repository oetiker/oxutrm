//! Signalling over a pipe, with whatever the remote login printed first.
//!
//! Real SSH is chatty: a banner before authentication, a motd after it, and
//! `stty: standard input: Inappropriate ioctl for device` when the command runs
//! without a tty. A wrapper that cannot skip all of that works perfectly on a
//! quiet developer machine and fails on every real host — which is exactly why
//! these tests supply the noise deliberately.

use std::io::Cursor;

use oxutrm_host::signalling::{read_signal_async, write_signal_async};
use oxutrm_proto::{
    Candidate, CandidateKind, ClientSpki, HostSpki, NatType, PROTO_VERSION, ProtoError, Psk,
    Signal, TermSize, TerminalCaps,
};

fn host_hello(attach_id: u64) -> Signal {
    host_hello_with_proto(attach_id, PROTO_VERSION)
}

fn host_hello_with_proto(attach_id: u64, proto: u32) -> Signal {
    Signal::HostHello {
        proto,
        session_id: "00112233445566778899aabbccddeeff".to_string(),
        attach_id,
        // The same 32 bytes the base64 literals here used to spell out. The
        // field is a 32-byte type now, so the fixture says so directly.
        cert_spki_sha256: HostSpki::new(*b"base64certfingerprint32byteslong"),
        psk: Psk::new(*b"pskbase64thirtytwobytesofentropy"),
        candidates: vec![Candidate {
            addr: "192.0.2.7:443".parse().unwrap(),
            kind: CandidateKind::ServerReflexive,
            priority: 1_000,
        }],
        nat_type: NatType::EndpointIndependent,
        bound_port: 443,
        detachable: true,
    }
}

fn client_hello() -> Signal {
    Signal::ClientHello {
        proto: PROTO_VERSION,
        // The fingerprint of the client's throwaway certificate. The host
        // pins this in its QUIC `ClientCertVerifier`, so a `ClientHello`
        // without it is a client the host has nothing to authenticate.
        cert_spki_sha256: ClientSpki::new([0x11; 32]),
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

/// The noise a real login prints before the command's own output.
const REAL_SSH_PREAMBLE: &str = "\
#################################################################\n\
#                  Authorised users only.                       #\n\
#################################################################\n\
Linux bastion 6.1.0-18-amd64 #1 SMP PREEMPT_DYNAMIC Debian\n\
\n\
The programs included with the Debian GNU/Linux system are free software.\n\
Last login: Mon Aug 25 09:14:02 2026 from 192.0.2.11\n\
stty: standard input: Inappropriate ioctl for device\n";

#[tokio::test]
async fn a_signal_round_trips_over_a_pipe() {
    let mut out: Vec<u8> = Vec::new();
    write_signal_async(&mut out, &host_hello(1))
        .await
        .expect("write");
    write_signal_async(&mut out, &client_hello())
        .await
        .expect("write");

    let mut r = Cursor::new(out);
    let first = read_signal_async(&mut r).await.expect("read first");
    assert!(matches!(first, Signal::HostHello { attach_id: 1, .. }));
    let second = read_signal_async(&mut r).await.expect("read second");
    assert!(matches!(second, Signal::ClientHello { .. }));
}

#[tokio::test]
async fn a_banner_and_a_motd_before_the_first_signal_are_skipped() {
    let mut wire = REAL_SSH_PREAMBLE.as_bytes().to_vec();
    write_signal_async(&mut wire, &host_hello(3))
        .await
        .expect("write");

    let mut r = Cursor::new(wire);
    let s = read_signal_async(&mut r)
        .await
        .expect("the preamble must be skipped");
    match s {
        Signal::HostHello { attach_id, .. } => assert_eq!(attach_id, 3),
        other => panic!("expected HostHello, got {other:?}"),
    }
}

#[tokio::test]
async fn noise_between_two_signals_is_skipped_too() {
    // A host that logs to stderr with 2>&1, or a late motd, can interleave.
    let mut wire = Vec::new();
    write_signal_async(&mut wire, &host_hello(1))
        .await
        .expect("write");
    wire.extend_from_slice(b"Broadcast message from root: rebooting soon\n");
    write_signal_async(&mut wire, &client_hello())
        .await
        .expect("write");

    let mut r = Cursor::new(wire);
    assert!(matches!(
        read_signal_async(&mut r).await.expect("first"),
        Signal::HostHello { .. }
    ));
    assert!(matches!(
        read_signal_async(&mut r).await.expect("second"),
        Signal::ClientHello { .. }
    ));
}

#[tokio::test]
async fn a_stream_that_ends_before_any_signal_is_unexpected_eof() {
    // Told apart from rubbish, so the wrapper can say "the remote said
    // nothing" rather than "the remote said something wrong".
    let mut r = Cursor::new(REAL_SSH_PREAMBLE.as_bytes().to_vec());
    let err = read_signal_async(&mut r)
        .await
        .expect_err("nothing but noise");
    match err {
        ProtoError::Io(e) => assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof),
        other => panic!("expected UnexpectedEof, got {other:?}"),
    }
}

/// The corollary of skipping noise: anything that *looks* like a signal is
/// parsed strictly. Silently discarding a malformed `HostHello` would hide the
/// one failure that has to be loudest.
#[tokio::test]
async fn a_line_that_looks_like_a_signal_but_is_malformed_is_loud() {
    let mut wire = REAL_SSH_PREAMBLE.as_bytes().to_vec();
    wire.extend_from_slice(b"{\"t\":\"HostHello\",\"proto\":\n");
    let mut r = Cursor::new(wire);
    assert!(matches!(
        read_signal_async(&mut r)
            .await
            .expect_err("must not be skipped"),
        ProtoError::Malformed(_)
    ));
}

#[tokio::test]
async fn a_version_mismatch_is_a_hard_failure_not_a_downgrade() {
    let mut wire = Vec::new();
    let skewed = host_hello_with_proto(1, PROTO_VERSION + 41);
    write_signal_async(&mut wire, &skewed).await.expect("write");

    let mut r = Cursor::new(wire);
    match read_signal_async(&mut r).await.expect_err("must refuse") {
        ProtoError::VersionMismatch { peer, ours } => {
            assert_eq!(peer, PROTO_VERSION + 41);
            assert_eq!(ours, PROTO_VERSION);
        }
        other => panic!("expected VersionMismatch, got {other:?}"),
    }
}

/// A very long motd line must not be mistaken for a truncated stream, and must
/// not blow up memory either.
#[tokio::test]
async fn an_enormous_preamble_line_is_still_just_preamble() {
    let mut wire = vec![b'x'; 200_000];
    wire.push(b'\n');
    write_signal_async(&mut wire, &host_hello(9))
        .await
        .expect("write");

    let mut r = Cursor::new(wire);
    match read_signal_async(&mut r).await.expect("skip and continue") {
        Signal::HostHello { attach_id, .. } => assert_eq!(attach_id, 9),
        other => panic!("expected HostHello, got {other:?}"),
    }
}

#[tokio::test]
async fn a_signal_never_straddles_two_lines_however_hostile_its_contents() {
    // serde_json escapes newlines inside strings, so a session id full of them
    // still occupies exactly one line on the wire.
    let nasty = Signal::Failed {
        reason: "line one\nline two\n{\"t\":\"HostHello\"}\n".to_string(),
    };
    let mut wire = Vec::new();
    write_signal_async(&mut wire, &nasty).await.expect("write");
    assert_eq!(
        wire.iter().filter(|b| **b == b'\n').count(),
        1,
        "a message must occupy exactly one line"
    );

    let mut r = Cursor::new(wire);
    match read_signal_async(&mut r).await.expect("read") {
        Signal::Failed { reason } => assert!(reason.contains("line two")),
        other => panic!("expected Failed, got {other:?}"),
    }
}

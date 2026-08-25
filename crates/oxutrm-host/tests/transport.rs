//! The path a session sends over, and the size limit that must come from it.
//!
//! The bug these tests exist to prevent is `max_datagram_size().unwrap_or(1200)`:
//! a constant fallback that silently exceeds what the tunnel accepts, and that
//! turns "datagrams are disabled" into a mystery instead of an error.

use oxutrm_host::transport::{
    Path, PathError, TUNNEL_MAX_PAYLOAD, frame_tunnel_message, read_tunnel_message,
};
use oxutrm_proto::Rung;

// ---------------------------------------------------------------------------
// The limit belongs to the path
// ---------------------------------------------------------------------------

#[test]
fn the_tunnel_accepts_less_than_a_datagram_does() {
    // The whole reason a constant fallback is wrong. The tunnel adds its own
    // length prefix and rides inside ssh's framing, so it has less room than
    // the wire it replaces.
    let datagram = Path::datagram(Rung::Ipv6Direct, Some(1200)).expect("datagrams enabled");
    let tunnel = Path::tunnel();

    assert!(
        tunnel.max_payload() < datagram.max_payload(),
        "tunnel {} must be under datagram {}; a 1200-byte fallback would \
         exceed what the tunnel accepts",
        tunnel.max_payload(),
        datagram.max_payload()
    );
    assert_eq!(tunnel.max_payload(), TUNNEL_MAX_PAYLOAD);
}

#[test]
fn a_datagram_path_reports_the_connections_own_number_not_a_guess() {
    for reported in [1200usize, 1392, 1452, 900] {
        let path = Path::datagram(Rung::StunPunch, Some(reported)).expect("enabled");
        assert_eq!(
            path.max_payload(),
            reported,
            "the path reports what the connection said, never a constant"
        );
    }
}

/// `None` does not mean "unknown, pick something safe". It means the peer
/// turned datagrams off, so no size works and substituting one converts a
/// clear error into a stall.
#[test]
fn datagrams_disabled_is_an_error_rather_than_a_fallback() {
    let err = Path::datagram(Rung::Ipv6Direct, None).expect_err("must refuse");
    assert!(matches!(err, PathError::DatagramsDisabled));

    let text = err.to_string();
    assert!(
        text.contains("datagrams disabled"),
        "must name the real condition: {text}"
    );
    assert!(
        text.contains("datagram_receive_buffer_size"),
        "and point at the usual cause: {text}"
    );
}

/// There is no `Option` at the send site, so there is no `unwrap_or` for
/// anyone to write. The bug is unavailable rather than discouraged.
#[test]
fn max_payload_is_always_a_number_on_any_path_that_exists() {
    let paths = [
        Path::datagram(Rung::Ipv6Direct, Some(1452)).expect("enabled"),
        Path::datagram(Rung::Birthday, Some(1200)).expect("enabled"),
        Path::tunnel(),
    ];
    for path in &paths {
        let max: usize = path.max_payload();
        assert!(max > 0, "{path:?}");
    }
}

// ---------------------------------------------------------------------------
// Oversize payloads are refused, not truncated
// ---------------------------------------------------------------------------

#[test]
fn a_payload_that_fits_the_wire_but_not_the_tunnel_is_refused() {
    // Exactly the frame a session would build if it had assumed 1200.
    let oversize = vec![0u8; 1200];
    let tunnel = Path::tunnel();

    let err = tunnel
        .check_payload(oversize.len())
        .expect_err("1200 does not fit the tunnel");
    match err {
        PathError::TooLarge { len, max, kind } => {
            assert_eq!(len, 1200);
            assert_eq!(max, TUNNEL_MAX_PAYLOAD);
            assert_eq!(kind, "tunnel");
        }
        other => panic!("expected TooLarge, got {other:?}"),
    }

    // And the same bytes are fine on the path they were sized for.
    Path::datagram(Rung::Ipv6Direct, Some(1200))
        .expect("enabled")
        .check_payload(oversize.len())
        .expect("1200 fits a 1200-byte datagram");
}

#[test]
fn the_error_says_to_ask_the_path_rather_than_assume() {
    let err = Path::tunnel().check_payload(9000).expect_err("too large");
    let text = err.to_string();
    assert!(
        text.contains("ask the path"),
        "the fix belongs in the message: {text}"
    );
}

#[test]
fn a_payload_exactly_at_the_limit_is_accepted() {
    Path::tunnel()
        .check_payload(TUNNEL_MAX_PAYLOAD)
        .expect("the limit itself must fit, or it is off by one");
    assert!(
        Path::tunnel()
            .check_payload(TUNNEL_MAX_PAYLOAD + 1)
            .is_err()
    );
}

// ---------------------------------------------------------------------------
// Tunnel framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_tunnelled_message_round_trips() {
    let payload = b"a screen diff, postcard-encoded".to_vec();
    let framed = frame_tunnel_message(&payload).expect("frame");

    assert_eq!(
        framed.len(),
        4 + payload.len(),
        "a four-byte length prefix and the payload"
    );

    let mut r = std::io::Cursor::new(framed);
    let back = read_tunnel_message(&mut r).await.expect("read");
    assert_eq!(back, payload);
}

#[tokio::test]
async fn two_messages_on_one_stream_keep_their_boundaries() {
    // The ssh channel is a byte stream with no message boundaries of its own,
    // which is the entire reason for the length prefix.
    let mut wire = Vec::new();
    wire.extend(frame_tunnel_message(b"first").expect("frame"));
    wire.extend(frame_tunnel_message(b"second").expect("frame"));

    let mut r = std::io::Cursor::new(wire);
    assert_eq!(read_tunnel_message(&mut r).await.expect("first"), b"first");
    assert_eq!(
        read_tunnel_message(&mut r).await.expect("second"),
        b"second"
    );
}

#[test]
fn framing_refuses_an_oversize_payload_before_it_reaches_the_wire() {
    let err = frame_tunnel_message(&vec![0u8; TUNNEL_MAX_PAYLOAD + 1]).expect_err("too large");
    assert!(matches!(err, PathError::TooLarge { .. }));
}

/// A length prefix arrives from the far side of an ssh connection, so it is
/// input. Trusting it would let a corrupt or hostile four bytes ask for four
/// gigabytes.
#[tokio::test]
async fn an_absurd_length_prefix_is_refused_before_anything_is_allocated() {
    let mut wire = u32::MAX.to_be_bytes().to_vec();
    wire.extend_from_slice(b"only a few real bytes follow");

    let mut r = std::io::Cursor::new(wire);
    let err = read_tunnel_message(&mut r).await.expect_err("must refuse");
    match err {
        PathError::TooLarge { len, .. } => assert_eq!(len, u32::MAX as usize),
        other => panic!("expected TooLarge, got {other:?}"),
    }
}

#[tokio::test]
async fn a_truncated_message_is_an_io_error_not_a_short_read() {
    let mut framed = frame_tunnel_message(b"a complete message").expect("frame");
    framed.truncate(framed.len() - 4);

    let mut r = std::io::Cursor::new(framed);
    let err = read_tunnel_message(&mut r).await.expect_err("truncated");
    assert!(
        matches!(err, PathError::Io(_)),
        "a short read must not decode as a shorter message: {err:?}"
    );
}

#[tokio::test]
async fn an_empty_message_is_legal() {
    let framed = frame_tunnel_message(b"").expect("frame");
    let mut r = std::io::Cursor::new(framed);
    assert!(read_tunnel_message(&mut r).await.expect("read").is_empty());
}

// ---------------------------------------------------------------------------
// The path knows whether its session may detach
// ---------------------------------------------------------------------------

#[test]
fn only_the_tunnel_is_undetachable() {
    assert!(!Path::tunnel().is_detachable());
    assert_eq!(Path::tunnel().rung(), Rung::SshTunnel);

    for rung in [
        Rung::Ipv6Direct,
        Rung::PortMapped,
        Rung::StunPunch,
        Rung::Birthday,
    ] {
        let path = Path::datagram(rung, Some(1200)).expect("enabled");
        assert!(path.is_detachable(), "{rung:?} carries its own socket");
        assert_eq!(path.rung(), rung);
    }
}

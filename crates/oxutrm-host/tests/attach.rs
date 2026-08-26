//! Reattaching to a running session, and refusing to when it makes no sense.
//!
//! Connect and reattach are one code path by design, so the interesting tests
//! here are the refusals: an id that does not exist, a session that can never
//! be reattached, and an entry whose process is gone.

use std::path::Path;

use oxutrm_host::attach::{AttachError, connect_to_session, format_session_list, relay_signals};
use oxutrm_host::{Registry, RegistryGuard, SessionMeta, begin_attach, now_unix};
use oxutrm_proto::{NatType, PROTO_VERSION, Psk, Rung, Signal, SpkiSha256, TermSize, TerminalCaps};
use tokio::io::BufReader;

fn meta(id: &str, pid: u32) -> SessionMeta {
    SessionMeta {
        session_id: id.to_string(),
        attach_id: 1,
        pid,
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    }
}

/// A `Psk` as it appears on the wire, without the JSON quotes.
///
/// There is no `AttachKeys::psk_base64()` any more: encoding lives in `Psk`'s
/// `Serialize` and nowhere else, so a test that wants the encoded form asks
/// the one encoder for it rather than being handed a second one.
fn wire_form(psk: &Psk) -> String {
    serde_json::to_string(psk)
        .expect("a Psk always encodes")
        .trim_matches('"')
        .to_string()
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

/// A session that is listening, as a real one would be after `daemonize`.
async fn serving(root: &Path, id: &str) -> (RegistryGuard, tokio::net::UnixListener) {
    let guard = RegistryGuard::register_in(root, &meta(id, std::process::id())).expect("register");
    let listener = tokio::net::UnixListener::bind(guard.socket_path()).expect("bind");
    (guard, listener)
}

// ---------------------------------------------------------------------------
// Attaching
// ---------------------------------------------------------------------------

#[tokio::test]
async fn attaching_reaches_the_running_session() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let id = "1111222233334444aaaabbbbccccdddd";
    let (_guard, listener) = serving(&root, id).await;

    // The session's side: accept, and read one Signal off the socket.
    let session = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        let mut reader = BufReader::new(stream);
        oxutrm_host::signalling::read_signal_async(&mut reader)
            .await
            .expect("the relayed ClientHello")
    });

    let mut stream = connect_to_session(&root, id).await.expect("attach");
    oxutrm_host::signalling::write_signal_async(&mut stream, &client_hello())
        .await
        .expect("send");

    let got = session.await.expect("join");
    assert!(matches!(got, Signal::ClientHello { .. }));
}

#[tokio::test]
async fn an_unknown_id_lists_what_does_exist() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let live = "aaaa0000aaaa0000aaaa0000aaaa0000";
    let (_guard, _listener) = serving(&root, live).await;

    let err = connect_to_session(&root, "ffff9999ffff9999ffff9999ffff9999")
        .await
        .expect_err("no such session");

    match &err {
        AttachError::UnknownSession { available, .. } => {
            assert_eq!(available, &[live.to_string()]);
        }
        other => panic!("expected UnknownSession, got {other:?}"),
    }
    // The usual cause is a truncated or mistyped id, so saying what IS here is
    // more useful than saying no.
    assert!(
        err.to_string().contains(live),
        "must list the live sessions: {err}"
    );
}

#[tokio::test]
async fn an_empty_registry_says_so_rather_than_listing_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let err = connect_to_session(&root, "0000000000000000aaaaaaaaaaaaaaaa")
        .await
        .expect_err("nothing here");
    assert!(
        err.to_string().contains("no live sessions"),
        "must not print an empty list: {err}"
    );
}

/// The rung-4 case. Its entry may still be on disk, and attaching to it can
/// never work, so the refusal has to explain rather than just fail.
#[tokio::test]
async fn a_session_that_can_never_detach_is_refused_with_the_reason() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let id = "5555666655556666555566665555dddd";

    let mut m = meta(id, std::process::id());
    // ICE nominated the tunnel, so the outcome overrides the intent.
    m.set_detachable(Rung::SshTunnel);
    let guard = RegistryGuard::register_in(&root, &m).expect("register");
    let _listener = tokio::net::UnixListener::bind(guard.socket_path()).expect("bind");

    let err = connect_to_session(&root, id)
        .await
        .expect_err("must refuse even though the socket is right there");

    match &err {
        AttachError::NotDetachable { .. } => {}
        other => panic!("expected NotDetachable, got {other:?}"),
    }
    let text = err.to_string();
    assert!(text.contains("rung 4"), "must name the reason: {text}");
    assert!(
        text.contains("Start a new session"),
        "must say what to do instead: {text}"
    );
}

#[tokio::test]
async fn an_entry_with_no_listener_reports_an_unreachable_socket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let id = "7777888877778888777788887777dddd";
    // Registered, but nothing ever bound the socket.
    let _guard =
        RegistryGuard::register_in(&root, &meta(id, std::process::id())).expect("register");

    let err = connect_to_session(&root, id)
        .await
        .expect_err("nothing listening");
    match &err {
        AttachError::SocketUnreachable { .. } => {}
        other => panic!("expected SocketUnreachable, got {other:?}"),
    }
    assert!(
        err.to_string().contains("--list"),
        "must point at the thing that cleans up: {err}"
    );
}

// ---------------------------------------------------------------------------
// Fresh key material on every attach
// ---------------------------------------------------------------------------

#[test]
fn every_attach_mints_a_new_psk_and_bumps_the_generation() {
    let mut m = meta("9999aaaa9999aaaa9999aaaa9999aaaa", 1);
    assert_eq!(m.attach_id, 1);

    let first = begin_attach(&mut m, SpkiSha256::new([7u8; 32])).expect("first attach");
    assert_eq!(first.attach_id, 2, "the generation moves on every attach");

    let second = begin_attach(&mut m, SpkiSha256::new([9u8; 32])).expect("second attach");
    assert_eq!(second.attach_id, 3);

    assert_ne!(
        first.keys.psk(),
        second.keys.psk(),
        "a PSK captured from an earlier attach must not be able to reattach"
    );
    assert_ne!(
        wire_form(first.keys.psk()),
        wire_form(second.keys.psk()),
        "and they differ on the wire as well as in memory"
    );
    assert_eq!(m.attach_id, 3, "and the session records the generation");
}

#[test]
fn a_psk_is_thirty_two_bytes_and_is_not_all_zeroes() {
    let mut m = meta("bbbbccccbbbbccccbbbbccccbbbbcccc", 1);
    let attach = begin_attach(&mut m, SpkiSha256::new([0u8; 32])).expect("attach");
    assert_eq!(attach.keys.psk().as_bytes().len(), 32);
    assert!(
        attach.keys.psk().as_bytes().iter().any(|b| *b != 0),
        "the CSPRNG produced nothing"
    );
}

#[test]
fn debug_never_prints_key_material() {
    // A derived Debug would put the PSK into the first error that formatted a
    // struct containing one.
    let mut m = meta("ddddeeeeddddeeeeddddeeeeddddeeee", 1);
    let attach = begin_attach(&mut m, SpkiSha256::new([0xab; 32])).expect("attach");
    let text = format!("{:?}", attach);
    assert!(text.contains("redacted"), "{text}");
    let leaked = wire_form(attach.keys.psk());
    assert!(!text.contains(&leaked), "the PSK reached a Debug string");
    // And through the wire type too, which now carries its own redaction and
    // is what a formatted `Signal` would reach for.
    let shown = format!("{:?}", attach.keys.psk());
    assert!(!shown.contains(&leaked), "the PSK reached Psk's Debug");
}

/// The seam: what the host MINTS must be what the client RECOVERS.
///
/// Both halves are minted here as raw bytes, travel as base64 text, and are
/// consumed at the far end as raw bytes again — and until this test existed
/// nothing in the tree ever closed that loop. Every other test checks one side
/// of it: `AttachKeys` proves the bytes are fresh, `signal.rs` proves the JSON
/// round-trips. Neither notices that the two ends never agreed on what the
/// field means.
#[test]
fn the_minted_key_material_survives_the_wire_unchanged() {
    let mut m = meta("aaaa1111aaaa1111aaaa1111aaaa1111", 1);
    let fingerprint = [0x5au8; 32];
    let attach = begin_attach(&mut m, SpkiSha256::new(fingerprint)).expect("attach");
    let minted_psk = *attach.keys.psk().as_bytes();

    let hello = Signal::HostHello {
        proto: PROTO_VERSION,
        session_id: m.session_id.clone(),
        attach_id: attach.attach_id,
        cert_spki_sha256: attach.keys.cert_spki_sha256(),
        psk: attach.keys.psk().clone(),
        candidates: vec![],
        nat_type: NatType::Unknown,
        bound_port: 443,
        detachable: true,
    };

    let mut wire: Vec<u8> = Vec::new();
    oxutrm_proto::write_signal(&mut wire, &hello).expect("write");

    let mut r = std::io::BufReader::new(wire.as_slice());
    match oxutrm_proto::read_signal(&mut r).expect("read") {
        Signal::HostHello {
            psk,
            cert_spki_sha256,
            ..
        } => {
            assert_eq!(
                psk.as_bytes(),
                &minted_psk,
                "the PSK that came off the wire is not the PSK that was minted"
            );
            assert_eq!(
                cert_spki_sha256.as_bytes(),
                &fingerprint,
                "the fingerprint that came off the wire is not the one the \
                 certificate has"
            );
        }
        other => panic!("expected HostHello, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Relaying
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_relay_carries_every_signal_and_ends_cleanly() {
    let mut source: Vec<u8> = Vec::new();
    oxutrm_host::signalling::write_signal_async(&mut source, &client_hello())
        .await
        .expect("write");
    oxutrm_host::signalling::write_signal_async(
        &mut source,
        &Signal::CandidateUpdate { candidates: vec![] },
    )
    .await
    .expect("write");

    let mut from = std::io::Cursor::new(source);
    let mut to: Vec<u8> = Vec::new();
    let relayed = relay_signals(&mut from, &mut to).await.expect("relay");

    assert_eq!(relayed, 2);
    let mut back = std::io::Cursor::new(to);
    assert!(matches!(
        oxutrm_host::signalling::read_signal_async(&mut back).await,
        Ok(Signal::ClientHello { .. })
    ));
    assert!(matches!(
        oxutrm_host::signalling::read_signal_async(&mut back).await,
        Ok(Signal::CandidateUpdate { .. })
    ));
}

/// The relay decodes rather than copying bytes, so rubbish cannot be pushed
/// into a running session.
#[tokio::test]
async fn the_relay_refuses_to_carry_garbage_into_a_session() {
    let mut from = std::io::Cursor::new(b"{\"t\":\"NotASignal\"}\n".to_vec());
    let mut to: Vec<u8> = Vec::new();
    assert!(
        relay_signals(&mut from, &mut to).await.is_err(),
        "a relay that copied bytes would have passed this straight through"
    );
    assert!(to.is_empty(), "and nothing must reach the session");
}

// ---------------------------------------------------------------------------
// Listing
// ---------------------------------------------------------------------------

#[test]
fn the_listing_shows_detachability_rather_than_implying_it() {
    let mut ok = meta("1111111111111111aaaaaaaaaaaaaaaa", 4242);
    ok.set_detachable(Rung::Ipv6Direct);
    let mut tunnelled = meta("2222222222222222bbbbbbbbbbbbbbbb", 4243);
    tunnelled.set_detachable(Rung::SshTunnel);

    let text = format_session_list(&[ok, tunnelled]);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[0].contains("detachable"));
    assert!(
        lines[1].contains("NOT detachable"),
        "a session that cannot be reattached must say so before you try: {}",
        lines[1]
    );
    assert!(lines[0].contains("4242"), "the pid is useful: {}", lines[0]);
}

#[test]
fn an_empty_listing_is_a_sentence_not_a_blank() {
    assert!(format_session_list(&[]).contains("no live oxutrm sessions"));
}

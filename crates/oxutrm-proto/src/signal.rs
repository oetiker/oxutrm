//! The SSH signalling channel: newline-delimited JSON on the SSH child's
//! stdin and stdout.
//!
//! JSON rather than a binary format because this channel is low-volume,
//! human-debuggable, and version skew here must fail loudly (spec §4.2).

use serde::{Deserialize, Serialize};

use crate::{
    Candidate, ClientSpki, HostSpki, NatType, PROTO_VERSION, PathDescription, ProtoError, Psk,
    TermSize, TerminalCaps,
};

/// The most bytes one signalling line may occupy, newline included.
///
/// The reader must have a wall somewhere. Without one, `read_line` grows its
/// `String` until a newline arrives, so a peer that sends four gigabytes and no
/// newline makes us allocate four gigabytes — over a channel that is reachable
/// before anything is authenticated.
///
/// **1 MiB**, and the number is chosen from what a real line contains rather
/// than from what feels safe. The largest legitimate signal is a hello: a
/// candidate list at roughly fifty bytes per candidate, two base64 32-byte
/// keys, and a terminal capability record. That is kilobytes. A megabyte
/// leaves three orders of magnitude of headroom — enough that no plausible
/// growth of the message reaches it — while removing unbounded growth
/// entirely.
///
/// It also has to clear the *noise*, not just the messages: the reader skips
/// whatever the remote login printed first, and a motd line is bounded by
/// nobody. `oxutrm-host`'s `an_enormous_preamble_line_is_still_just_preamble`
/// pins a 200 KB preamble line as legitimate, which is deliberate intent about
/// how long a real line gets; 1 MiB keeps that intent true with room to spare.
///
/// The limit covers the terminating newline, so a line of exactly this many
/// bytes is the largest one that is still accepted.
pub const MAX_SIGNAL_LINE: usize = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Signal {
    /// host -> client, first line.
    HostHello {
        proto: u32,
        /// 32 lowercase hex characters.
        session_id: String,
        /// Which attach generation this is. Both `seq` counters reset to 1 at
        /// every attach, so the two ends must agree on the generation;
        /// otherwise a host already serving a session cannot tell a second
        /// `--attach` from the current one. Signalling and `meta.json` only —
        /// never per-frame, since each attach is a distinct QUIC connection.
        attach_id: u64,
        /// The SHA-256 of the **host** certificate's SPKI. base64 on the wire,
        /// 32 bytes everywhere else — see [`crate::SpkiSha256`], which is what
        /// makes those two the same value rather than two values that happen
        /// to share a field name.
        ///
        /// [`HostSpki`] rather than the bare encoding type because
        /// `ClientHello` now carries a field of exactly the same name, shape
        /// and encoding pointing the other way. The client pins this one; the
        /// host pins the other. Both fingerprints are in scope on both sides,
        /// and a swap is a compile error.
        cert_spki_sha256: HostSpki,
        /// 32 CSPRNG bytes, base64 on the wire. Never written to disk, on
        /// either side. A [`Psk`] and not a [`SpkiSha256`] deliberately: they
        /// are the same size and were both `String` here, so passing one where
        /// the other belonged used to type-check.
        psk: Psk,
        candidates: Vec<Candidate>,
        nat_type: NatType,
        bound_port: u16,
        /// The host's INTENT, not the outcome. `HostHello` is written BEFORE
        /// the ladder runs — the candidates travel in it — so at this point
        /// nobody yet knows which rung will be nominated. False here only when
        /// the host already knows it cannot detach.
        ///
        /// Actual detachability is settled LATER, by the nominated rung, and
        /// that is the only place it becomes `SessionMeta.detachable`. A
        /// session that daemonized on intent and then landed on rung 4 would
        /// have closed the very SSH descriptors it needs to carry its data.
        detachable: bool,
    },
    /// client -> host, first line.
    ClientHello {
        proto: u32,
        /// The SHA-256 of the **client** certificate's SPKI, for the host to
        /// pin in its QUIC `ClientCertVerifier`.
        ///
        /// The ordering this relies on is already the ordering the ladder has:
        /// `HostHello` goes out first, this reply comes back, and the QUIC
        /// endpoint is not built until after nomination — so the host holds
        /// the fingerprint well before it needs it. That is why the host's
        /// endpoint can require it by value with no `Option` and no setter,
        /// and why "pin it afterwards" is not a thing anyone can write.
        ///
        /// Without this the PSK is the only thing gating the punched socket,
        /// and the PSK never reaches TLS — it authenticates path discovery.
        /// Anything that completed the handshake reached `Command::new(shell)`.
        cert_spki_sha256: ClientSpki,
        candidates: Vec<Candidate>,
        nat_type: NatType,
        caps: TerminalCaps,
        size: TermSize,
    },
    /// Either direction, repeatable until the link is up.
    CandidateUpdate {
        candidates: Vec<Candidate>,
    },
    /// Either direction, terminates signalling.
    Established {
        path: PathDescription,
    },
    Failed {
        reason: String,
    },
}

impl Signal {
    /// The protocol version a message carries, where it carries one.
    fn proto(&self) -> Option<u32> {
        match self {
            Signal::HostHello { proto, .. } | Signal::ClientHello { proto, .. } => Some(*proto),
            _ => None,
        }
    }
}

/// True when a line could be a `Signal` rather than SSH noise.
fn looks_like_signal(line: &str) -> bool {
    line.trim_start().starts_with('{')
}

/// Hard version check (spec §4.2): a mismatch is a loud failure, never a
/// downgrade and never a warning. Messages that carry no version pass.
fn check_version(s: &Signal) -> Result<(), ProtoError> {
    match s.proto() {
        Some(peer) if peer != PROTO_VERSION => Err(ProtoError::VersionMismatch {
            peer,
            ours: PROTO_VERSION,
        }),
        _ => Ok(()),
    }
}

/// Serialise one `Signal` and terminate it with a newline.
///
/// `serde_json` escapes newlines inside strings, so a message can never
/// straddle two lines however hostile its contents.
pub fn write_signal<W: std::io::Write>(w: &mut W, s: &Signal) -> Result<(), ProtoError> {
    let line = serde_json::to_string(s)
        .map_err(|e| ProtoError::Malformed(format!("encoding signal: {e}")))?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

/// Read one `Signal`, discarding whatever the remote login printed first:
/// banners, motd, `stty` complaints on a non-tty. Real SSH emits these, and a
/// single un-skipped banner line breaks every connection.
///
/// A line that *looks* like a signal — it starts with `{` — is parsed
/// strictly: malformed JSON and version skew are reported, never skipped.
/// Silently discarding a bad `HostHello` would hide the one failure that must
/// be loudest. The corollary is that a motd line starting with `{` breaks the
/// bootstrap, which is the safe direction to fail in.
///
/// End of stream is `ProtoError::Io` with `ErrorKind::UnexpectedEof`, so a
/// peer that hung up cleanly can be told from one that sent rubbish.
pub fn read_signal<R: std::io::BufRead>(r: &mut R) -> Result<Signal, ProtoError> {
    use std::io::{BufRead as _, Read};

    loop {
        let mut raw = Vec::new();
        // `take` IS the wall, rather than a length check applied afterwards. It
        // caps what `read_until` may pull out of `r`, so a peer that sends no
        // newline costs us `MAX_SIGNAL_LINE` bytes and not one more. A limit
        // enforced after the read reports the identical error having already
        // allocated whatever the peer chose to send, which is the whole defect.
        let taken = Read::take(&mut *r, MAX_SIGNAL_LINE as u64).read_until(b'\n', &mut raw)?;
        if taken == 0 {
            return Err(ProtoError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "signalling stream closed before a message arrived",
            )));
        }
        // A spent budget with no terminator means the line needs at least one
        // byte more than the limit allows. A *short* read without one is the
        // ordinary last line of a stream that simply ended, and stays legal.
        if taken == MAX_SIGNAL_LINE && raw.last() != Some(&b'\n') {
            return Err(ProtoError::SignalLineTooLong {
                limit: MAX_SIGNAL_LINE,
            });
        }
        // `read_line` used to do this for us; the bounded read hands back bytes.
        let line = String::from_utf8(raw).map_err(|_| {
            ProtoError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "signalling line is not valid UTF-8",
            ))
        })?;
        if !looks_like_signal(&line) {
            continue;
        }
        let s: Signal = serde_json::from_str(line.trim())
            .map_err(|e| ProtoError::Malformed(format!("signal json: {e}")))?;
        check_version(&s)?;
        return Ok(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Candidate, CandidateKind, NatType, PROTO_VERSION, PathDescription, ProtoError, Rung,
        TermSize, TerminalCaps,
    };
    use std::io::BufReader;

    fn caps() -> TerminalCaps {
        TerminalCaps {
            truecolor: true,
            colors: 16_777_216,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_string(),
        }
    }

    fn every_variant() -> Vec<Signal> {
        let cand = Candidate {
            addr: "192.0.2.7:443".parse().unwrap(),
            kind: CandidateKind::ServerReflexive,
            priority: 1_000,
        };
        vec![
            Signal::HostHello {
                proto: PROTO_VERSION,
                session_id: "00112233445566778899aabbccddeeff".to_string(),
                attach_id: 3,
                // The same 32 bytes the base64 literals here used to spell
                // out, written as the bytes they are.
                cert_spki_sha256: HostSpki::new(*b"abcdefghijklmnopqrstuvwxyz123456"),
                psk: Psk::new(*b"0123456789abcdef0123456789abcdef"),
                candidates: vec![cand.clone()],
                nat_type: NatType::EndpointIndependent,
                bound_port: 443,
                detachable: true,
            },
            Signal::ClientHello {
                proto: PROTO_VERSION,
                // Deliberately NOT the host's 32 bytes above: a fixture that
                // reused them would round-trip identically whether or not the
                // two fields are distinct values.
                cert_spki_sha256: ClientSpki::new(*b"654321zyxwvutsrqponmlkjihgfedcba"),
                candidates: vec![cand.clone()],
                nat_type: NatType::Symmetric,
                caps: caps(),
                size: TermSize {
                    cols: 120,
                    rows: 40,
                },
            },
            Signal::CandidateUpdate {
                candidates: vec![cand],
            },
            Signal::Established {
                path: PathDescription {
                    rung: Rung::Ipv6Direct,
                    local: "[2001:db8::1]:443".parse().unwrap(),
                    remote: "[2001:db8::2]:443".parse().unwrap(),
                    probes_sent: 3,
                    nat_type: NatType::None,
                    rtt_ms: 11,
                    mtu: 1452,
                },
            },
            Signal::Failed {
                reason: "no UDP path and no SSH tunnel".to_string(),
            },
        ]
    }

    fn encoded(signals: &[Signal]) -> Vec<u8> {
        let mut buf = Vec::new();
        for s in signals {
            write_signal(&mut buf, s).expect("write");
        }
        buf
    }

    #[test]
    fn every_variant_round_trips_through_one_stream() {
        let buf = encoded(&every_variant());
        let mut r = BufReader::new(buf.as_slice());
        let got: Vec<Signal> = (0..5).map(|_| read_signal(&mut r).expect("read")).collect();
        for (a, b) in got.iter().zip(every_variant().iter()) {
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }

        // `Psk`'s Debug is redacted, so the loop above compares two copies of
        // "<redacted>" and would pass on a PSK that did not survive at all.
        // Check the key material directly, or the one field whose round trip
        // matters most is the one field asserted vacuously.
        match (&got[0], &every_variant()[0]) {
            (
                Signal::HostHello {
                    psk: a,
                    cert_spki_sha256: ca,
                    ..
                },
                Signal::HostHello {
                    psk: b,
                    cert_spki_sha256: cb,
                    ..
                },
            ) => {
                assert_eq!(a, b, "the PSK did not survive the round trip");
                assert_eq!(ca, cb, "the fingerprint did not survive the round trip");
            }
            _ => panic!("the first variant must be the HostHello"),
        }
    }

    #[test]
    fn each_message_is_exactly_one_terminated_line() {
        let text = String::from_utf8(encoded(&every_variant())).expect("utf8");
        assert_eq!(text.lines().count(), 5, "one line per Signal");
        assert!(text.ends_with('\n'), "every line is terminated");
    }

    #[test]
    fn variants_are_tagged_with_t() {
        let text = String::from_utf8(encoded(&[Signal::Failed { reason: "x".into() }])).unwrap();
        assert!(text.contains(r#""t":"Failed""#), "tag missing in {text}");
    }

    #[test]
    fn host_hello_carries_the_attach_generation_and_the_intent_to_detach() {
        let text = String::from_utf8(encoded(&every_variant()[..1])).unwrap();
        assert!(
            text.contains(r#""attach_id":3"#),
            "attach_id missing: {text}"
        );
        assert!(
            text.contains(r#""detachable":true"#),
            "detachable missing: {text}"
        );

        let mut r = BufReader::new(text.as_bytes());
        match read_signal(&mut r).expect("read") {
            Signal::HostHello {
                attach_id,
                detachable,
                session_id,
                ..
            } => {
                assert_eq!(attach_id, 3);
                assert!(detachable);
                assert_eq!(session_id, "00112233445566778899aabbccddeeff");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn a_reason_containing_a_newline_still_frames_as_one_line() {
        let buf = encoded(&[Signal::Failed {
            reason: "line one\nline two".into(),
        }]);
        assert_eq!(
            String::from_utf8(buf.clone()).unwrap().lines().count(),
            1,
            "an embedded newline must be escaped, or it splits the message"
        );
        let mut r = BufReader::new(buf.as_slice());
        match read_signal(&mut r).expect("read") {
            Signal::Failed { reason } => assert_eq!(reason, "line one\nline two"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ---- the hard version check ----

    /// A byte-exact `HostHello` line with a caller-chosen protocol version.
    ///
    /// The key material used to be `"YWJj"` and `"ZGVm"` — `b"abc"` and
    /// `b"def"`, three bytes each. Nothing rejected them, which is precisely
    /// the hole these fixtures now cannot express: the fields are 32-byte
    /// types, so a fixture with the wrong length fails to parse rather than
    /// sailing through and taking the version check with it.
    fn skewed_host_hello(proto: u32) -> String {
        format!(
            concat!(
                r#"{{"t":"HostHello","proto":{},"session_id":"00112233445566778899aabbccddeeff","#,
                r#""attach_id":1,"cert_spki_sha256":"{}","psk":"{}","candidates":[],"#,
                r#""nat_type":"Unknown","bound_port":443,"detachable":true}}"#,
                "\n"
            ),
            proto, GOOD_32, GOOD_32
        )
    }

    #[test]
    fn a_newer_peer_fails_loudly_and_names_both_versions() {
        let line = skewed_host_hello(PROTO_VERSION + 1);
        let mut r = BufReader::new(line.as_bytes());
        match read_signal(&mut r) {
            Err(ProtoError::VersionMismatch { peer, ours }) => {
                assert_eq!(peer, PROTO_VERSION + 1);
                assert_eq!(ours, PROTO_VERSION);
                let shown = ProtoError::VersionMismatch { peer, ours }.to_string();
                assert!(
                    shown.contains(&peer.to_string()),
                    "hides the peer version: {shown}"
                );
                assert!(
                    shown.contains(&ours.to_string()),
                    "hides our version: {shown}"
                );
            }
            other => panic!("version skew must be VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn an_older_peer_fails_too() {
        let line = skewed_host_hello(0);
        let mut r = BufReader::new(line.as_bytes());
        assert!(matches!(
            read_signal(&mut r),
            Err(ProtoError::VersionMismatch { peer: 0, .. })
        ));
    }

    #[test]
    fn a_skewed_client_hello_fails_as_well() {
        let line = format!(
            concat!(
                r#"{{"t":"ClientHello","proto":99,"cert_spki_sha256":"{}","#,
                r#""candidates":[],"nat_type":"Unknown","#,
                r#""caps":{{"truecolor":false,"colors":16,"bracketed_paste":false,"#,
                r#""mouse_sgr":false,"osc52":false,"term_name":"xterm"}},"#,
                r#""size":{{"cols":80,"rows":24}}}}"#,
                "\n"
            ),
            GOOD_32
        );
        let mut r = BufReader::new(line.as_bytes());
        assert!(matches!(
            read_signal(&mut r),
            Err(ProtoError::VersionMismatch { peer: 99, .. })
        ));
    }

    #[test]
    fn messages_without_a_version_are_not_version_checked() {
        let line = "{\"t\":\"CandidateUpdate\",\"candidates\":[]}\n";
        let mut r = BufReader::new(line.as_bytes());
        assert!(matches!(
            read_signal(&mut r),
            Ok(Signal::CandidateUpdate { .. })
        ));
    }

    // ---- key material is 32 bytes, or the message is not a message ----

    /// 32 bytes, correctly encoded. The shape every case below deviates from.
    const GOOD_32: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=";
    /// 31 bytes — and note it is *also* 44 characters. base64 pads to a
    /// multiple of four, so 31, 32 and 33 bytes all encode to 44 characters,
    /// and a character count on its own cannot tell them apart.
    const SHORT_31: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHw==";
    /// 33 bytes, and 44 characters again.
    const LONG_33: &str = "AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAh";

    /// One `HostHello` line whose `psk` is whatever the caller wants to try.
    /// Everything else in it is valid, so a rejection can only be about the
    /// PSK.
    fn host_hello_with_psk(psk: &str) -> String {
        let mut s = String::from(r#"{"t":"HostHello","proto":"#);
        s.push_str(&PROTO_VERSION.to_string());
        s.push_str(
            r#","session_id":"00112233445566778899aabbccddeeff","attach_id":1,"cert_spki_sha256":""#,
        );
        s.push_str(GOOD_32);
        s.push_str(r#"","psk":""#);
        s.push_str(psk);
        s.push_str(r#"","candidates":[],"nat_type":"Unknown","bound_port":443,"detachable":true}"#);
        s.push('\n');
        s
    }

    fn read_one(line: &str) -> Result<Signal, ProtoError> {
        let mut r = BufReader::new(line.as_bytes());
        read_signal(&mut r)
    }

    /// The control. Without it, every rejection below could be a `HostHello`
    /// that never parsed for some unrelated reason.
    #[test]
    fn a_correctly_sized_psk_is_accepted() {
        assert!(matches!(
            read_one(&host_hello_with_psk(GOOD_32)),
            Ok(Signal::HostHello { .. })
        ));
    }

    #[test]
    fn a_psk_that_decodes_to_thirty_one_bytes_is_rejected() {
        let got = read_one(&host_hello_with_psk(SHORT_31));
        assert!(
            matches!(got, Err(ProtoError::Malformed(_))),
            "a 31-byte PSK reached the session: {got:?}"
        );
    }

    #[test]
    fn a_psk_that_decodes_to_thirty_three_bytes_is_rejected() {
        let got = read_one(&host_hello_with_psk(LONG_33));
        assert!(
            matches!(got, Err(ProtoError::Malformed(_))),
            "a 33-byte PSK reached the session: {got:?}"
        );
    }

    #[test]
    fn a_psk_outside_the_base64_alphabet_is_rejected() {
        // 44 characters, so a length check alone lets it through. The fake-ssh
        // fixture used to emit exactly this shape, with a `}` in the middle.
        let bad = "AQIDBAUGBwgJCgsMDQ4PEBES}xQVFhcYGRobHB0eHyA=";
        assert_eq!(
            bad.len(),
            44,
            "the case is only interesting at the right length"
        );
        let got = read_one(&host_hello_with_psk(bad));
        assert!(
            matches!(got, Err(ProtoError::Malformed(_))),
            "a PSK that is not base64 at all reached the session: {got:?}"
        );
    }

    /// The payload is a quarter of [`MAX_SIGNAL_LINE`] deliberately. It used to
    /// be a full megabyte, which the line cap now refuses on sight — and a
    /// refusal by the cap would prove nothing about the PSK, which is what this
    /// test is about. Staying well under the wall keeps the only thing that can
    /// reject this line the field's own length check.
    #[test]
    fn a_vast_base64_psk_is_rejected_and_the_length_is_what_rejects_it() {
        let huge = "A".repeat(MAX_SIGNAL_LINE / 4);
        let line = host_hello_with_psk(&huge);
        assert!(
            line.len() < MAX_SIGNAL_LINE,
            "the fixture must not trip the line cap, or it proves nothing about the PSK"
        );
        let got = read_one(&line);
        let shown = format!("{got:?}");
        assert!(
            matches!(got, Err(ProtoError::Malformed(_))),
            "a megabyte of PSK reached the session: {shown}"
        );
        // Evidence of ORDER, which the error variant alone cannot give: the
        // rejection names the length it was handed. A decode-first
        // implementation would report what the DECODER made of a megabyte,
        // not how long the field was.
        assert!(
            shown.contains(&huge.len().to_string()),
            "the rejection does not name the offending length, so nothing here \
             shows the length was checked before the decode: {shown}"
        );
    }

    // ---- the client's fingerprint is not optional ----

    /// One `ClientHello` line whose `cert_spki_sha256` is whatever the caller
    /// wants to try — including nothing at all, via `None`.
    fn client_hello_with_fingerprint(fp: Option<&str>) -> String {
        let mut s = String::from(r#"{"t":"ClientHello","proto":"#);
        s.push_str(&PROTO_VERSION.to_string());
        if let Some(fp) = fp {
            s.push_str(r#","cert_spki_sha256":""#);
            s.push_str(fp);
            s.push('"');
        }
        s.push_str(concat!(
            r#","candidates":[],"nat_type":"Unknown","#,
            r#""caps":{"truecolor":false,"colors":16,"bracketed_paste":false,"#,
            r#""mouse_sgr":false,"osc52":false,"term_name":"xterm"},"#,
            r#""size":{"cols":80,"rows":24}}"#,
        ));
        s.push('\n');
        s
    }

    /// The control, without which every rejection below could be a
    /// `ClientHello` that failed to parse for an unrelated reason.
    #[test]
    fn a_client_hello_with_a_well_formed_fingerprint_is_accepted() {
        assert!(matches!(
            read_one(&client_hello_with_fingerprint(Some(GOOD_32))),
            Ok(Signal::ClientHello { .. })
        ));
    }

    /// The regression this whole change exists to make impossible: a client
    /// that sends no fingerprint is not a client with a defaulted field, it is
    /// a client the host has nothing to pin. `#[serde(default)]` here — or an
    /// `Option` — would silently restore the un-authenticated handshake while
    /// every positive test kept passing.
    #[test]
    fn a_client_hello_with_no_fingerprint_at_all_is_rejected() {
        let got = read_one(&client_hello_with_fingerprint(None));
        assert!(
            matches!(got, Err(ProtoError::Malformed(_))),
            "a ClientHello with no certificate fingerprint was accepted: {got:?}"
        );
    }

    /// The role types delegate their decoding rather than re-implementing it,
    /// and this is the assertion that would notice if one grew its own: 31 and
    /// 33 bytes are both 44 characters, so only a decoded-length check rejects
    /// them.
    #[test]
    fn a_client_fingerprint_that_is_not_thirty_two_bytes_is_rejected() {
        for bad in [SHORT_31, LONG_33] {
            let got = read_one(&client_hello_with_fingerprint(Some(bad)));
            assert!(
                matches!(got, Err(ProtoError::Malformed(_))),
                "a wrongly sized client fingerprint reached the session: {got:?}"
            );
        }
    }

    /// The two fingerprints are distinct values on the wire even though they
    /// share a field name — proof that nothing collapses them into one.
    #[test]
    fn the_two_hellos_carry_two_different_fingerprints() {
        let host = match &every_variant()[0] {
            Signal::HostHello {
                cert_spki_sha256, ..
            } => *cert_spki_sha256.as_bytes(),
            other => panic!("wrong variant: {other:?}"),
        };
        let client = match &every_variant()[1] {
            Signal::ClientHello {
                cert_spki_sha256, ..
            } => *cert_spki_sha256.as_bytes(),
            other => panic!("wrong variant: {other:?}"),
        };
        assert_ne!(host, client);
    }

    // ---- SSH noise before the first message ----

    const REAL_WORLD_PREAMBLE: &str = concat!(
        "Welcome to Ubuntu 24.04.1 LTS (GNU/Linux 6.8.0-40-generic x86_64)\n",
        "\n",
        " * Documentation:  https://help.ubuntu.com\n",
        "Last login: Tue Aug 25 17:33:01 2026 from 192.0.2.1\n",
        "stty: 'standard input': Inappropriate ioctl for device\n",
    );

    #[test]
    fn a_banner_and_motd_before_the_first_message_are_skipped() {
        let mut input = String::from(REAL_WORLD_PREAMBLE);
        input.push_str(
            &String::from_utf8(encoded(&[Signal::Failed {
                reason: "after the noise".into(),
            }]))
            .unwrap(),
        );
        let mut r = BufReader::new(input.as_bytes());
        match read_signal(&mut r).expect("must find the signal past the noise") {
            Signal::Failed { reason } => assert_eq!(reason, "after the noise"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn skipping_never_swallows_version_skew() {
        let input = format!(
            "Welcome to Ubuntu\n{}",
            skewed_host_hello(PROTO_VERSION + 1)
        );
        let mut r = BufReader::new(input.as_bytes());
        assert!(
            matches!(read_signal(&mut r), Err(ProtoError::VersionMismatch { .. })),
            "a skewed hello must stop the reader, not be treated as noise"
        );
    }

    #[test]
    fn skipping_never_swallows_malformed_json() {
        let input = "motd line\n{\"t\":\"HostHello\"}\n";
        let mut r = BufReader::new(input.as_bytes());
        assert!(
            matches!(read_signal(&mut r), Err(ProtoError::Malformed(_))),
            "a truncated signal must be reported, not skipped"
        );
    }

    #[test]
    fn a_stream_of_pure_noise_ends_at_end_of_file() {
        let mut r = BufReader::new(REAL_WORLD_PREAMBLE.as_bytes());
        match read_signal(&mut r) {
            Err(ProtoError::Io(e)) => {
                assert_eq!(e.kind(), std::io::ErrorKind::UnexpectedEof);
            }
            other => panic!("expected an end-of-file error, got {other:?}"),
        }
    }

    #[test]
    fn an_empty_stream_is_end_of_file_not_malformed() {
        let empty: &[u8] = b"";
        let mut r = BufReader::new(empty);
        assert!(matches!(
            read_signal(&mut r),
            Err(ProtoError::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn reading_continues_past_the_preamble_for_later_messages() {
        let mut input = String::from("Last login: whenever\n");
        input.push_str(
            &String::from_utf8(encoded(&[
                Signal::Failed {
                    reason: "first".into(),
                },
                Signal::Failed {
                    reason: "second".into(),
                },
            ]))
            .unwrap(),
        );
        let mut r = BufReader::new(input.as_bytes());
        assert!(matches!(read_signal(&mut r), Ok(Signal::Failed { .. })));
        match read_signal(&mut r).expect("second") {
            Signal::Failed { reason } => assert_eq!(reason, "second"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    // ---- the line is bounded ----
    //
    // `read_line` grows a `String` until a newline arrives. On a channel that
    // is reachable before anything is authenticated, that is an allocation the
    // peer chooses the size of.

    /// A `Failed` line whose encoded length, newline included, is exactly
    /// `total` bytes.
    ///
    /// Built by measuring and padding rather than by guessing: the boundary
    /// cases below are only worth anything if they land ON the boundary, and a
    /// fixture that was a few bytes out would quietly turn "exactly the limit"
    /// into "near the limit".
    fn line_of_exactly(total: usize) -> Vec<u8> {
        let empty = encoded(&[Signal::Failed {
            reason: String::new(),
        }]);
        let pad = total
            .checked_sub(empty.len())
            .expect("asked for a line shorter than the empty encoding");
        // ASCII 'a' needs no JSON escaping, so one character is one byte.
        let line = encoded(&[Signal::Failed {
            reason: "a".repeat(pad),
        }]);
        assert_eq!(line.len(), total, "the padding arithmetic is wrong");
        line
    }

    #[test]
    fn a_line_of_exactly_the_limit_is_accepted() {
        let line = line_of_exactly(MAX_SIGNAL_LINE);
        let mut r = BufReader::new(line.as_slice());
        match read_signal(&mut r).expect("a line at the limit is legitimate") {
            Signal::Failed { reason } => assert!(reason.starts_with("aaa")),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn one_byte_past_the_limit_is_refused() {
        let line = line_of_exactly(MAX_SIGNAL_LINE + 1);
        let mut r = BufReader::new(line.as_slice());
        match read_signal(&mut r) {
            Err(ProtoError::SignalLineTooLong { limit }) => {
                assert_eq!(limit, MAX_SIGNAL_LINE);
            }
            other => panic!("one byte past the limit must be refused, got {other:?}"),
        }
    }

    /// The test that a `Cursor`-shaped fixture cannot fake.
    ///
    /// The over-long line here **does** end in a newline, and a perfectly good
    /// signal follows it. An unbounded reader succeeds on this input: the long
    /// line does not start with `{`, so it is skipped as preamble and the
    /// signal after it is returned. Only a reader that stops at the limit can
    /// fail here — so this cannot pass by hitting end of file and reporting
    /// "no newline found", which is the wrong reason a naive fixture would
    /// have made it pass for.
    #[test]
    fn an_over_long_line_is_refused_even_though_the_stream_would_recover() {
        let mut input = vec![b'x'; MAX_SIGNAL_LINE + 1];
        input.push(b'\n');
        input.extend_from_slice(&encoded(&[Signal::Failed {
            reason: "a perfectly good signal, one line later".into(),
        }]));

        // The rescue this guards against, stated as an assertion rather than a
        // hope: everything needed for a successful read IS in the stream.
        assert!(input.ends_with(b"\n"), "the stream is well framed");

        let mut r = BufReader::new(input.as_slice());
        match read_signal(&mut r) {
            Err(ProtoError::SignalLineTooLong { limit }) => assert_eq!(limit, MAX_SIGNAL_LINE),
            other => panic!(
                "an over-long preamble line must stop the reader even when the \
                 stream recovers afterwards, got {other:?}"
            ),
        }
    }

    /// A reader that serves `b'x'` until the test's patience runs out, and
    /// counts what it handed out.
    ///
    /// This is the only way to tell a reader that STOPS at the limit from one
    /// that reads everything and complains afterwards. The second kind passes
    /// every error-variant assertion above while still allocating whatever the
    /// peer sends, which is the bug.
    ///
    /// # Why it ends at all
    ///
    /// A fixture that truly never ends proves the same property, and it does so
    /// by making the failing case allocate without limit — which is the
    /// diagnosis, not a test result. A red run then ends as an OOM kill of the
    /// test binary, taking down every sibling process that shares the memory
    /// cgroup and reporting nothing about which assertion failed.
    ///
    /// [`Endless::PATIENCE`] is far enough past `MAX_SIGNAL_LINE` that a
    /// draining reader overruns the bound below by a factor of sixty-four, so
    /// the test keeps every bit of its discriminating power and states its
    /// verdict as an assertion instead of as a signal 9.
    #[derive(Default)]
    struct Endless {
        served: usize,
    }

    impl Endless {
        /// Well past any wall the reader could legitimately draw, and still
        /// small enough to hold comfortably in memory.
        const PATIENCE: usize = 64 * MAX_SIGNAL_LINE;
    }

    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            // Zero means end of stream, so running out of patience reads as a
            // peer that hung up - a reader that got this far has already failed
            // the bound below.
            let n = buf.len().min(Self::PATIENCE - self.served);
            buf[..n].fill(b'x');
            self.served += n;
            Ok(n)
        }
    }

    #[test]
    fn the_reader_stops_at_the_limit_rather_than_draining_the_peer() {
        const CHUNK: usize = 8 * 1024;
        let mut r = BufReader::with_capacity(CHUNK, Endless::default());

        match read_signal(&mut r) {
            Err(ProtoError::SignalLineTooLong { limit }) => assert_eq!(limit, MAX_SIGNAL_LINE),
            other => panic!("an endless line must be refused, got {other:?}"),
        }

        let served = r.get_ref().served;
        assert!(
            served <= MAX_SIGNAL_LINE + CHUNK,
            "the reader took {served} bytes from a peer that would have sent any \
             number - it is reporting the limit, not enforcing it"
        );
        // And the other side of it: a reader that gave up immediately would
        // satisfy the bound above while rejecting lines it should accept.
        assert!(
            served >= MAX_SIGNAL_LINE,
            "the reader stopped after only {served} bytes, well short of the limit"
        );
    }

    /// A signal-shaped line gets the same wall as a noise-shaped one. Without
    /// this, a cap applied only on the skip path leaves the actual message path
    /// unbounded — which is the path a hostile peer would use.
    #[test]
    fn an_over_long_line_that_looks_like_a_signal_is_refused_too() {
        let mut line = Vec::from(r#"{"t":"Failed","reason":""#);
        line.extend(std::iter::repeat_n(b'a', MAX_SIGNAL_LINE));
        line.extend_from_slice(b"\"}\n");
        let mut r = BufReader::new(line.as_slice());
        assert!(
            matches!(
                read_signal(&mut r),
                Err(ProtoError::SignalLineTooLong { .. })
            ),
            "an over-long line must be refused before it is parsed"
        );
    }

    /// The refusal must be its own fault, not a parse failure wearing a
    /// different message. A caller has to be able to tell "the peer sent
    /// garbage" from "the peer sent too much" — they are different incidents.
    #[test]
    fn the_refusal_is_distinguishable_and_names_the_limit() {
        let line = line_of_exactly(MAX_SIGNAL_LINE + 1);
        let mut r = BufReader::new(line.as_slice());
        let err = read_signal(&mut r).expect_err("must refuse");
        assert!(
            !matches!(err, ProtoError::Malformed(_)),
            "an over-long line is not a parse failure: {err:?}"
        );
        assert!(
            !matches!(err, ProtoError::Io(_)),
            "an over-long line is not an I/O failure: {err:?}"
        );
        let shown = err.to_string();
        assert!(
            shown.contains(&MAX_SIGNAL_LINE.to_string()),
            "the refusal does not say where the wall is: {shown}"
        );
    }

    /// Nothing about the cap disturbs the ordinary case: short lines, a
    /// preamble, several messages in one stream. The control for everything
    /// above.
    #[test]
    fn ordinary_short_lines_are_unaffected_by_the_cap() {
        let mut input = String::from(REAL_WORLD_PREAMBLE);
        input.push_str(&String::from_utf8(encoded(&every_variant())).unwrap());
        let mut r = BufReader::new(input.as_bytes());
        for _ in 0..5 {
            read_signal(&mut r).expect("every variant still reads");
        }
    }
}

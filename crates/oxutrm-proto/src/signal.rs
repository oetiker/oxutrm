//! The SSH signalling channel: newline-delimited JSON on the SSH child's
//! stdin and stdout.
//!
//! JSON rather than a binary format because this channel is low-volume,
//! human-debuggable, and version skew here must fail loudly (spec §4.2).

use serde::{Deserialize, Serialize};

use crate::{
    Candidate, NatType, PROTO_VERSION, PathDescription, ProtoError, TermSize, TerminalCaps,
};

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
        /// base64 of the SHA-256 of the host certificate's SPKI.
        cert_spki_sha256: String,
        /// base64 of 32 CSPRNG bytes. Never written to disk, on either side.
        psk: String,
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
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(ProtoError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "signalling stream closed before a message arrived",
            )));
        }
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
                cert_spki_sha256: "YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoxMjM0NTY=".to_string(),
                psk: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=".to_string(),
                candidates: vec![cand.clone()],
                nat_type: NatType::EndpointIndependent,
                bound_port: 443,
                detachable: true,
            },
            Signal::ClientHello {
                proto: PROTO_VERSION,
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

    fn skewed_host_hello(proto: u32) -> String {
        format!(
            concat!(
                r#"{{"t":"HostHello","proto":{},"session_id":"00112233445566778899aabbccddeeff","#,
                r#""attach_id":1,"cert_spki_sha256":"YWJj","psk":"ZGVm","candidates":[],"#,
                r#""nat_type":"Unknown","bound_port":443,"detachable":true}}"#,
                "\n"
            ),
            proto
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
        let line = concat!(
            r#"{"t":"ClientHello","proto":99,"candidates":[],"nat_type":"Unknown","#,
            r#""caps":{"truecolor":false,"colors":16,"bracketed_paste":false,"#,
            r#""mouse_sgr":false,"osc52":false,"term_name":"xterm"},"#,
            r#""size":{"cols":80,"rows":24}}"#,
            "\n"
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
}

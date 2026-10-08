//! The first line of every door into a session process (switcher spec §4.1).
//!
//! A session process has two doors: the QUIC control stream from its own
//! client, and its Unix socket, from a sibling session or from
//! `oxutrm host --attach` over ssh. Both read one [`Open`] first and dispatch
//! it in one place. **Every request has exactly one reply before anything else
//! on the stream**: a [`Reply`] -- or, for a request that runs the attach
//! exchange (`Attach`, `Switch`, a sibling `New`), the exchange's own first
//! line, a `Signal::HostHello`.
//!
//! JSON lines, like the signalling channel, and told apart from it by their
//! tags: a [`Signal`] carries `"t"`, a [`Reply`] carries `"r"`, so a reader
//! that may get either ([`Answer`]) needs no guess.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Name, PROTO_VERSION, ProtoError, SessionId, Signal, TermSize};

/// What an attach is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Role {
    /// Adopt now; newest attach wins (recovery spec §6).
    Primary,
    /// Park until the client's first sync frame on it (standby spec §3.5).
    Standby,
}

/// The first line on a door.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Open {
    /// [`PROTO_VERSION`] of the asker. A door refuses another version with a
    /// reason rather than misreading what follows.
    pub proto: u32,
    pub req: Request,
}

impl Open {
    /// `req`, at this binary's protocol version.
    #[must_use]
    pub fn new(req: Request) -> Open {
        Open {
            proto: PROTO_VERSION,
            req,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "q")]
pub enum Request {
    /// Run the attach exchange over this stream; its `HostHello` is the reply.
    Attach { role: Role },
    /// Does this link answer? Replied to with [`Reply::ProbeAck`].
    Probe { nonce: u64 },
    /// The host's sessions, as [`Reply::Sessions`].
    Sessions,
    /// This session, as [`Reply::Entry`]. Asked of a sibling over its socket.
    Myself,
    /// Relayed to `to`'s socket as `Attach { Primary }`; the target's attach
    /// exchange follows.
    Switch { to: SessionId },
    /// From a lobby: it starts its shell and becomes the session, replying
    /// [`Reply::Entry`]. From a session: a sibling is started and its attach
    /// exchange follows.
    New { name: Option<Name> },
    /// Answered [`Reply::Done`] once the shell is reaped and the entry gone.
    Kill { id: SessionId },
    /// Answered [`Reply::Entry`]; `None` clears the name.
    Rename { id: SessionId, name: Option<Name> },
}

/// The one answer to a request that does not run an attach exchange.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "r", content = "v")]
pub enum Reply {
    Sessions(Vec<SessionEntry>),
    Entry(SessionEntry),
    Done,
    /// Any request; the reason is for the user.
    Refused(String),
    /// The answer to [`Request::Probe`].
    ProbeAck {
        nonce: u64,
    },
}

/// Whether a session has a client, as seen by whoever asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Attached {
    /// Detached.
    No,
    /// Attached to the asker's own client: the asker's own session.
    Here,
    /// Attached to some other client.
    Elsewhere,
    /// The sibling did not answer in time; listed from its `meta.json`.
    Unknown,
    /// The sibling runs a binary with another protocol version.
    OtherVersion,
}

/// One session, as the selector lists it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: SessionId,
    pub name: Option<Name>,
    pub shell: String,
    pub created_unix: u64,
    pub size: TermSize,
    pub detachable: bool,
    pub attached: Attached,
    /// The session the asker is in.
    pub this: bool,
}

/// One session, as the ssh offer lists it: what `meta.json` says, and
/// nothing a sibling would have to be asked (spec §4.2). Built with blocking
/// I/O before `run_host_connect`'s fork, where no runtime may exist.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OfferEntry {
    pub id: SessionId,
    pub name: Option<Name>,
    pub shell: String,
    pub created_unix: u64,
    pub size: TermSize,
    pub detachable: bool,
}

impl OfferEntry {
    /// The offer's facts, plus what only a live answer can say.
    #[must_use]
    pub fn entry(self, attached: Attached, this: bool) -> SessionEntry {
        SessionEntry {
            id: self.id,
            name: self.name,
            shell: self.shell,
            created_unix: self.created_unix,
            size: self.size,
            detachable: self.detachable,
            attached,
            this,
        }
    }
}

/// What may come back on a stream after an `Open` that can run an attach
/// exchange: the exchange's first line, or a refusal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Answer {
    Reply(Reply),
    Signal(Signal),
}

/// One value as one JSON line, newline included.
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    let mut line =
        serde_json::to_vec(value).map_err(|e| ProtoError::Malformed(format!("encoding: {e}")))?;
    line.push(b'\n');
    Ok(line)
}

/// One JSON line as a `T`, strictly: a door's first line is from a peer
/// that speaks the protocol from its first byte, so there is no preamble to
/// skip and anything else is malformed.
pub fn parse_line<T: DeserializeOwned>(raw: &[u8]) -> Result<T, ProtoError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| ProtoError::Malformed("a line that is not UTF-8".to_string()))?;
    serde_json::from_str(text.trim()).map_err(|e| ProtoError::Malformed(format!("json: {e}")))
}

/// One line as an [`Answer`]. A `Signal` in it is version-checked exactly as
/// `read_signal` checks one.
pub fn parse_answer(raw: &[u8]) -> Result<Answer, ProtoError> {
    let answer: Answer = parse_line(raw)?;
    if let Answer::Signal(s) = &answer {
        crate::signal::check_version(s)?;
    }
    Ok(answer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostSpki, NatType, Psk};

    fn id() -> SessionId {
        "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().unwrap()
    }

    fn entry() -> SessionEntry {
        SessionEntry {
            id: id(),
            name: Some(Name::parse("build").unwrap()),
            shell: "/bin/bash".to_string(),
            created_unix: 1_791_450_840,
            size: TermSize {
                cols: 120,
                rows: 40,
            },
            detachable: true,
            attached: Attached::Elsewhere,
            this: false,
        }
    }

    fn round<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) {
        let line = encode_line(v).unwrap();
        assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
        let back: T = parse_line(&line).unwrap();
        assert_eq!(&back, v);
    }

    #[test]
    fn every_request_round_trips() {
        for req in [
            Request::Attach {
                role: Role::Primary,
            },
            Request::Attach {
                role: Role::Standby,
            },
            Request::Probe { nonce: 7 },
            Request::Sessions,
            Request::Myself,
            Request::Switch { to: id() },
            Request::New { name: None },
            Request::New {
                name: Some(Name::parse("logs").unwrap()),
            },
            Request::Kill { id: id() },
            Request::Rename {
                id: id(),
                name: Some(Name::parse("build").unwrap()),
            },
            Request::Rename {
                id: id(),
                name: None,
            },
        ] {
            round(&Open::new(req));
        }
    }

    #[test]
    fn every_reply_round_trips() {
        for reply in [
            Reply::Sessions(vec![entry()]),
            Reply::Sessions(vec![]),
            Reply::Entry(entry()),
            Reply::Done,
            Reply::Refused("no session a3f9c01e here".to_string()),
            Reply::ProbeAck { nonce: 9 },
        ] {
            round(&reply);
        }
    }

    #[test]
    fn an_open_carries_this_binarys_version() {
        let line = encode_line(&Open::new(Request::Myself)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["proto"], PROTO_VERSION);
    }

    #[test]
    fn a_reply_and_a_hello_are_told_apart() {
        let refused = encode_line(&Reply::Refused("gone".to_string())).unwrap();
        assert!(matches!(
            parse_answer(&refused).unwrap(),
            Answer::Reply(Reply::Refused(r)) if r == "gone"
        ));
        let hello = Signal::HostHello {
            proto: PROTO_VERSION,
            session_id: id().to_string(),
            attach_id: 1,
            cert_spki_sha256: HostSpki::new([7; 32]),
            psk: Psk::new([9; 32]),
            candidates: vec![],
            nat_type: NatType::Unknown,
            bound_port: 443,
            detachable: true,
            features: vec![],
        };
        let line = crate::encode_line(&hello).unwrap();
        assert!(matches!(
            parse_answer(&line).unwrap(),
            Answer::Signal(Signal::HostHello { .. })
        ));
    }

    #[test]
    fn a_hello_of_another_version_in_an_answer_is_a_version_mismatch() {
        let hello = Signal::HostHello {
            proto: PROTO_VERSION + 1,
            session_id: id().to_string(),
            attach_id: 1,
            cert_spki_sha256: HostSpki::new([7; 32]),
            psk: Psk::new([9; 32]),
            candidates: vec![],
            nat_type: NatType::Unknown,
            bound_port: 443,
            detachable: true,
            features: vec![],
        };
        let line = encode_line(&hello).unwrap();
        assert!(matches!(
            parse_answer(&line),
            Err(ProtoError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn an_entry_with_a_bad_id_or_name_fails_to_parse() {
        let mut v = serde_json::to_value(entry()).unwrap();
        v["name"] = serde_json::json!("cafe");
        assert!(serde_json::from_value::<SessionEntry>(v).is_err());
        let mut v = serde_json::to_value(entry()).unwrap();
        v["id"] = serde_json::json!("../../etc");
        assert!(serde_json::from_value::<SessionEntry>(v).is_err());
    }

    #[test]
    fn an_offer_entry_becomes_a_session_entry() {
        let e = entry();
        let offer = OfferEntry {
            id: e.id,
            name: e.name.clone(),
            shell: e.shell.clone(),
            created_unix: e.created_unix,
            size: e.size,
            detachable: e.detachable,
        };
        assert_eq!(offer.entry(Attached::Elsewhere, false), e);
    }
}

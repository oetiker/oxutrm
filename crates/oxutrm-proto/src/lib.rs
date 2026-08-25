#![forbid(unsafe_code)]

//! The wire. Every type that crosses between the two ends of a session is
//! defined here and nowhere else: the SSH signalling messages, the datagram
//! `Frame`, the stream messages, and the protocol version that is checked at
//! handshake time.
//!
//! This crate is the single normative source for the wire format. When the
//! design spec and this crate appear to disagree, this crate is right.

pub mod frame;
pub mod ids;
pub mod signal;
pub mod stream;
pub mod types;

pub use frame::{FLAG_ZSTD, Frame};
pub use ids::SessionId;
pub use signal::{Signal, read_signal, write_signal};
pub use stream::{ControlMsg, ScrollbackReq};
pub use types::{Candidate, CandidateKind, NatType, PathDescription, Rung, TermSize, TerminalCaps};

/// The wire protocol version. Checked at handshake; a mismatch is a hard,
/// loud failure rather than a negotiation (spec §4.2).
pub const PROTO_VERSION: u32 = 1;

#[derive(thiserror::Error, Debug)]
pub enum ProtoError {
    #[error("protocol version mismatch: peer {peer}, ours {ours}")]
    VersionMismatch { peer: u32, ours: u32 },
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

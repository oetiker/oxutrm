#![forbid(unsafe_code)]

//! The wire. Every type that crosses between the two ends of a session is
//! defined here and nowhere else: the SSH signalling messages, the datagram
//! `Frame`, the stream messages, and the protocol version that is checked at
//! handshake time.
//!
//! This crate is the single normative source for the wire format. When the
//! design spec and this crate appear to disagree, this crate is right.
//!
//! # The screen model lives here, and that is a boundary decision
//!
//! [`ScreenState`] and everything it is made of — [`Cell`], [`Color`],
//! [`Attrs`], [`Cursor`], [`Modes`] — are wire types: the replicated state,
//! serialised and sent. They sit beside [`Frame`] and [`Signal`] rather than in
//! `oxutrm-term` for a reason that outlives taste: `oxutrm-term` owns
//! `alacritty_terminal`, which drags in a PTY, `polling` and `signal-hook` with
//! no feature flag to exclude them. If the screen model lived there, then
//! `oxutrm-sync` — whose entire value is that it performs no I/O and is
//! therefore exhaustively testable without a socket — would have a PTY in its
//! dependency tree the moment `HostTerm` landed.
//!
//! With the model here, `oxutrm-sync` depends on this crate alone and the
//! boundary holds by construction. `oxutrm-term`'s job is narrower and
//! clearer: run the emulator, and convert what it produces into a
//! [`ScreenState`].
//!
//! # The screen invariants
//!
//! [`ScreenState`] carries six rules that are **enforced rather than
//! documented**:
//!
//! | | Rule | Enforced by |
//! |---|---|---|
//! | I1 | `cells.len() == rows * cols`, exactly | [`ScreenState::validate`] |
//! | I2 | the cursor sits on a cell that exists | [`ScreenState::validate`] |
//! | I3 | `seq >= 1`; zero is the full-state sentinel | [`ScreenState::validate`] |
//! | I4 | there is no `icon` field; `vte` drops OSC 1 | the type itself |
//! | I5 | `bell` is a monotonic counter | [`ScreenState::validate_transition`] |
//! | I6 | `scrollback_len` never shrinks | [`ScreenState::validate_transition`] |
//!
//! I1 to I3 are properties of one state, so [`ScreenState::validate`] can see
//! them and every constructor calls it. I5 and I6 are properties of a
//! *transition* — one state in isolation carries no history — so they live in
//! [`ScreenState::validate_transition`], which the sync layer runs after
//! applying a diff. I4 is enforced by the strongest mechanism available: the
//! field does not exist, and an exhaustive struct literal in the test suite
//! stops anyone adding it back without noticing.
//!
//! Nothing here clamps. A cursor outside the screen is rejected, because a
//! clamped cursor is a state that validates while the two ends quietly
//! disagree about where the caret is.

pub mod cell;
pub mod error;
pub mod frame;
pub mod ids;
pub mod screen;
pub mod signal;
pub mod stream;
pub mod types;

pub use cell::{Attrs, Cell, CellText, Color};
pub use error::ApplyError;
pub use frame::{FLAG_ZSTD, Frame};
pub use ids::SessionId;
pub use screen::{Cursor, CursorShape, Modes, MouseMode, ScreenState};
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

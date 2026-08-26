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
//! [`ScreenState`] carries eight rules that are **enforced rather than
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
//! | I7 | the screen fits inside the cell and side caps | [`TermSize::check_bounds`] |
//! | I8 | painted text is text, not control, and bounded | [`text::check_cell_text`] |
//!
//! I1 to I3 are properties of one state, so [`ScreenState::validate`] can see
//! them and every constructor calls it. I5 and I6 are properties of a
//! *transition* — one state in isolation carries no history — so they live in
//! [`ScreenState::validate_transition`]. I4 is enforced by the strongest
//! mechanism available: the field does not exist, and an exhaustive struct
//! literal in the test suite stops anyone adding it back without noticing.
//!
//! I7 and I8 are the two that also have to be enforced **before** the state
//! exists, because for both of them building the offending state IS the
//! damage. `validate` checks them again on a built state so that a violating
//! `ScreenState` cannot exist however it was constructed, but the enforcement
//! that matters is upstream of the allocation: `TermSize::check_bounds` before
//! the cell buffer is allocated, and `text::check_cell_text` before a run's
//! cells are cloned `repeat + 1` times across a row.
//!
//! I8 is also the only invariant about *content*. It exists because the trust
//! model is asymmetric — the client renders the host's cells by writing them
//! to the user's real terminal, so a cell holding `\x1b]52;c;…\x07` is the
//! host reaching through the client to the user's clipboard. See [`text`].
//!
//! ## Where the transition check actually runs
//!
//! `oxutrm-sync`'s `Receiver::on_frame` calls it after applying a diff, with
//! the pre-application state as `previous`, through the `SyncState`
//! trait — which is also why the trait carries a `validate_transition` whose
//! default implementation is plain `validate`. This sentence is load-bearing
//! and it used to be false: the crate documented the call and nothing made
//! it, so I5 and I6 were enforced nowhere while `tests/invariants.rs` called
//! the checker directly and reported green. A test that calls a checker is
//! not evidence that the production path does.
//!
//! A transition that fails is a **rejected frame, not a fatal error**. The
//! receiver applies to a clone, so a rejection leaves the state and the ack
//! untouched and the session continues; the host and client loops log the
//! reason and take the next frame.
//!
//! The counters this rests on are monotonic at the source, so enforcement
//! cannot strand a healthy session: `HostTerm` accumulates `bell` and
//! `scrollback_len` with `saturating_add` and never resets either. In
//! particular `scrollback_len` is **not** `Term::history_size()`, which
//! saturates at the ring's capacity and falls when the emulator is reset —
//! it is a synthesized counter that only ever climbs, and the fetch path
//! clamps a request against the history actually still held.
//!
//! Nothing here clamps. A cursor outside the screen is rejected, because a
//! clamped cursor is a state that validates while the two ends quietly
//! disagree about where the caret is.

pub mod cell;
pub mod error;
pub mod frame;
pub mod ids;
pub mod keymat;
pub mod screen;
pub mod signal;
pub mod stream;
pub mod text;
pub mod types;

pub use cell::{Attrs, Cell, CellText, Color};
pub use error::ApplyError;
pub use frame::{FLAG_ZSTD, Frame};
pub use ids::SessionId;
pub use keymat::{Psk, SpkiSha256, WIRE_KEY_B64_LEN, WIRE_KEY_LEN};
pub use screen::{Cursor, CursorShape, Modes, MouseMode, ScreenState};
pub use signal::{Signal, read_signal, write_signal};
pub use stream::{ControlMsg, ScrollbackReq};
pub use text::{check_cell_text, check_title, fit_cell_text, fit_title, is_control_scalar};
pub use types::{
    Candidate, CandidateKind, MAX_CELL_TEXT, MAX_SCREEN_CELLS, MAX_SCREEN_DIM, MAX_TITLE, NatType,
    PathDescription, Rung, TermSize, TerminalCaps, TextField,
};

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

#![forbid(unsafe_code)]

//! The terminal itself: a PTY, an `alacritty_terminal` emulator driving it,
//! and the `ScreenState` snapshot that everything downstream replicates.
//!
//! The same emulator runs on both ends, which is what lets the client render
//! from authoritative state rather than approximating it. This crate also
//! answers what the local terminal can display, and what `TERM` an emulated
//! child should be given.
//!
//! # The state, and its invariants
//!
//! [`ScreenState`] is pure data — no emulator, no PTY, no I/O — and it carries
//! six rules that are **enforced rather than documented**:
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
//! disagree about where the caret is — which costs far more to find later than
//! a loud rejection costs now.

mod cell;
mod error;
mod screen;

pub use cell::{Attrs, Cell, CellText, Color};
pub use error::ApplyError;
pub use screen::{Cursor, CursorShape, Modes, MouseMode, ScreenState};

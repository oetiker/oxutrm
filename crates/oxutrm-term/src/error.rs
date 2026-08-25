//! The error a state or a diff is rejected with.
//!
//! # Why this type lives here and not in `oxutrm-sync`
//!
//! The interface contract lists `ApplyError` under `oxutrm-sync`, but
//! `oxutrm-sync` already depends on `oxutrm-term` (it needs [`ScreenState`],
//! [`Cell`] and [`Cursor`] to define its diffs), while `ScreenState::validate`
//! has to return this type. Those two facts are a dependency cycle, which
//! Cargo forbids.
//!
//! It is defined in the crate that owns the thing being validated, and
//! `oxutrm-sync` re-exports it:
//!
//! ```ignore
//! pub use oxutrm_term::ApplyError;
//! ```
//!
//! so `oxutrm_sync::ApplyError` — the contract's own spelling — keeps
//! resolving for every consumer, and moving the definition again later costs
//! one line.
//!
//! [`ScreenState`]: crate::ScreenState
//! [`Cell`]: crate::Cell
//! [`Cursor`]: crate::Cursor

/// Why a state or a diff was refused.
///
/// Every variant means the same thing operationally: **nothing was applied**.
/// There is no partial application, because a half-applied diff is a state
/// that validates while describing a screen neither end is looking at.
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum ApplyError {
    #[error("diff base {base} does not match current state {current}")]
    BaseMismatch { base: u64, current: u64 },

    #[error("diff refers to row {row} outside {rows} rows")]
    OutOfBounds { row: u16, rows: u16 },

    #[error("decode: {0}")]
    Decode(String),

    /// **I1.** `cells.len()` must equal `rows * cols` exactly — not at least.
    /// Every access is a computed row-major offset, so a short vector panics
    /// on the first read past the end and a long one silently addresses the
    /// wrong cell forever. The second failure is far more expensive to find.
    #[error("cells length {len} does not match {rows}x{cols}")]
    LengthMismatch { len: usize, rows: u16, cols: u16 },

    /// **I2.** The cursor must sit on a cell that exists. Rejected, never
    /// clamped: a clamped cursor produces a state that looks healthy while
    /// the two ends quietly disagree about where the caret is.
    #[error("cursor ({row},{col}) outside {rows}x{cols}")]
    CursorOutOfBounds { row: u16, col: u16, rows: u16, cols: u16 },

    /// **I3.** Sequence zero is the full-state sentinel — the value a diff
    /// carries to mean "this is not a diff at all". A real state numbered
    /// zero would be indistinguishable from that request.
    #[error("sequence 0 is the full-state sentinel, never a real state")]
    SeqZero,

    /// **I5.** The bell is a monotonic counter and the client rings once per
    /// increment, so a decrease would either lose bells or replay every bell
    /// in the session's history.
    #[error("bell went backwards: was {was}, now {now}")]
    BellWentBackwards { was: u32, now: u32 },

    /// **I6.** Lines that have scrolled off do not come back.
    #[error("scrollback shrank: was {was}, now {now}")]
    ScrollbackShrank { was: u64, now: u64 },
}

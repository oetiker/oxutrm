//! The error a state or a diff is rejected with.
//!
//! # Why this type lives here
//!
//! It travels with the thing it describes. [`ScreenState`] is a **wire type** —
//! it is the replicated state, serialised and sent — so it belongs beside
//! [`Frame`] and [`Signal`], and its validation error belongs beside it.
//!
//! Putting it anywhere else creates a cycle or breaks a boundary that matters
//! more than tidiness:
//!
//! * In `oxutrm-sync`: `sync` needs [`ScreenState`] to define its diffs, and
//!   `ScreenState::validate` must return this error. That is a cycle, and
//!   Cargo rejects it.
//! * In `oxutrm-term`: `term` owns `alacritty_terminal`, which drags in a PTY,
//!   `polling` and `signal-hook` with no feature flag to exclude them. Any
//!   crate reaching for this error would then have a PTY in its dependency
//!   tree — including `oxutrm-sync`, whose whole value is that it has no I/O
//!   at all. The boundary would be gone quietly, and nobody would notice until
//!   they wondered why the pure crate needs `signal-hook`.
//!
//! Here, `oxutrm-sync` depends on this crate alone and the boundary holds **by
//! construction** rather than by anyone remembering it.
//!
//! [`ScreenState`]: crate::ScreenState
//! [`Frame`]: crate::Frame
//! [`Signal`]: crate::Signal

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
    CursorOutOfBounds {
        row: u16,
        col: u16,
        rows: u16,
        cols: u16,
    },

    /// A run in a `RowPatch` writes past the end of its row.
    ///
    /// Truncating instead would leave a screen that still validates — the
    /// length is unchanged — while disagreeing with the host about what is
    /// painted on it.
    #[error("a run on row {row} reaches column {end_col}, outside {cols} columns")]
    RunOverflowsRow { row: u16, end_col: usize, cols: u16 },

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

    /// **I7.** The screen is larger than [`MAX_SCREEN_CELLS`] cells, or one of
    /// its dimensions exceeds [`MAX_SCREEN_DIM`].
    ///
    /// Unlike every other invariant here, this one has to be checked *before*
    /// the state is built. `rows` and `cols` are `u16`, so a diff of a few
    /// bytes can name 4.29e9 cells; allocating that and then rejecting it is
    /// not a rejection, it is the attack. See [`TermSize::check_bounds`].
    ///
    /// [`MAX_SCREEN_CELLS`]: crate::types::MAX_SCREEN_CELLS
    /// [`MAX_SCREEN_DIM`]: crate::types::MAX_SCREEN_DIM
    /// [`TermSize::check_bounds`]: crate::TermSize::check_bounds
    #[error(
        "screen {rows}x{cols} exceeds the maximum of {} cells / {} per side",
        crate::types::MAX_SCREEN_CELLS,
        crate::types::MAX_SCREEN_DIM
    )]
    ScreenTooLarge { rows: u16, cols: u16 },
}

//! The authoritative picture of a terminal, and the rules it must obey.

use serde::{Deserialize, Serialize};

use crate::{ApplyError, Cell};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cursor {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    pub shape: CursorShape,
}

/// How much mouse reporting the remote application asked for.
///
/// The client enables the matching mode on the user's real terminal, so that
/// the local terminal's expectations match the remote application's.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub enum MouseMode {
    #[default]
    Off,
    Press,
    PressRelease,
    ButtonMotion,
    AnyMotion,
}

/// Terminal modes the client has to mirror locally to behave correctly.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Modes {
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub app_cursor: bool,
    pub app_keypad: bool,
}

/// Everything the client needs in order to paint the screen.
///
/// This is a **value**, not a handle: it is cloned into a ring of recent
/// states, diffed against whatever the peer last acknowledged, and never
/// mutated in a way that assumes a single reader — which is what leaves the
/// door open to read-only observers later.
///
/// There is no `icon` field, and that is deliberate: `vte` silently drops
/// OSC 1, so an icon field could only ever hold a value oxutrm invented.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScreenState {
    /// Starts at 1. Zero is reserved as the full-state sentinel. Resets to 1
    /// at every attach.
    pub seq: u64,
    pub rows: u16,
    pub cols: u16,
    /// `rows * cols`, row-major, length exact. See [`ApplyError::LengthMismatch`].
    pub cells: Vec<Cell>,
    pub cursor: Cursor,
    pub modes: Modes,
    /// From OSC 0 and OSC 2 only.
    pub title: String,
    /// A monotonic counter, never a flag. The client rings once per increment,
    /// so a datagram lost in transit costs nothing: the next state still
    /// reports the higher number. A flag would simply be lost.
    pub bell: u32,
    /// How many lines have scrolled off for good. The lines themselves never
    /// travel in a datagram — they are fetched on a stream — so this number is
    /// the client's only way to know how much history exists.
    pub scrollback_len: u64,
}

impl ScreenState {
    /// A blank screen of the given size, at sequence 1.
    ///
    /// Returns an error rather than a value because every constructor
    /// validates: a type whose invariants are checked everywhere except at
    /// construction is a type whose invariants are checked nowhere.
    pub fn blank(rows: u16, cols: u16) -> Result<ScreenState, ApplyError> {
        // I7 BEFORE the allocation below, not after it via `validate`. A
        // caller that took these dimensions from a peer — `ClientHello` does —
        // would otherwise allocate the whole hostile screen and only then be
        // told it was too big.
        crate::TermSize { rows, cols }.check_bounds()?;
        let state = ScreenState {
            seq: 1,
            rows,
            cols,
            cells: vec![Cell::blank(); rows as usize * cols as usize],
            cursor: Cursor {
                row: 0,
                col: 0,
                visible: true,
                shape: CursorShape::Block,
            },
            modes: Modes::default(),
            title: String::new(),
            bell: 0,
            scrollback_len: 0,
        };
        state.validate()?;
        Ok(state)
    }

    /// The cell at `(row, col)`.
    ///
    /// # Panics
    ///
    /// If the position is outside the screen. Every caller computes the same
    /// row-major offset, so an out-of-range access is a bug in the caller
    /// rather than bad data — but it panics with the coordinates rather than
    /// as a bare slice index, because "index out of bounds: 47" says nothing
    /// about which cell anyone wanted.
    pub fn cell(&self, row: u16, col: u16) -> &Cell {
        assert!(
            row < self.rows && col < self.cols,
            "cell ({row},{col}) is outside {}x{}",
            self.rows,
            self.cols
        );
        &self.cells[row as usize * self.cols as usize + col as usize]
    }

    /// One whole row, left to right.
    ///
    /// # Panics
    ///
    /// If `row >= self.rows`, with the same reasoning as [`ScreenState::cell`].
    pub fn row(&self, row: u16) -> &[Cell] {
        assert!(row < self.rows, "row {row} is outside {} rows", self.rows);
        let width = self.cols as usize;
        let start = row as usize * width;
        &self.cells[start..start + width]
    }

    /// Check every invariant that one state can carry on its own: **I1**
    /// (exact length), **I2** (cursor in bounds) and **I3** (sequence not the
    /// sentinel).
    ///
    /// Called by every constructor, and by the sync layer after it applies a
    /// diff. A comment is not a constraint anyone checks; this is.
    ///
    /// It takes `&self` and never mutates. In particular it does **not** clamp
    /// an out-of-range cursor: clamping turns a detectable desynchronisation
    /// into a session that looks healthy while the two ends drift apart.
    pub fn validate(&self) -> Result<(), ApplyError> {
        // I7 first. It is the only invariant here that also has to be enforced
        // BEFORE a state is built, so checking it again on a built one is
        // belt-and-braces: it means no oversized `ScreenState` can exist at
        // all, however it was constructed.
        crate::TermSize {
            rows: self.rows,
            cols: self.cols,
        }
        .check_bounds()?;

        // I1. Exactly, not at least. A long vector never panics - it just
        // makes every computed offset address the wrong cell.
        let expected = self.rows as usize * self.cols as usize;
        if self.cells.len() != expected {
            return Err(ApplyError::LengthMismatch {
                len: self.cells.len(),
                rows: self.rows,
                cols: self.cols,
            });
        }

        // I3. Zero is the full-state sentinel.
        if self.seq == 0 {
            return Err(ApplyError::SeqZero);
        }

        // I2. The degenerate 0x0 screen has no cell for the cursor to sit on,
        // so it is exempt; every other size must place the cursor on a real
        // position.
        if expected != 0 && (self.cursor.row >= self.rows || self.cursor.col >= self.cols) {
            return Err(ApplyError::CursorOutOfBounds {
                row: self.cursor.row,
                col: self.cursor.col,
                rows: self.rows,
                cols: self.cols,
            });
        }

        Ok(())
    }

    /// Check the invariants that only exist **between** two states: **I5**
    /// (the bell never goes backwards) and **I6** (scrollback never shrinks).
    ///
    /// `validate` cannot see either of these, because one state in isolation
    /// carries no history.
    ///
    /// The production caller is `oxutrm_sync::Receiver::on_frame`, which runs
    /// it after applying a diff to a clone, with the pre-application state as
    /// `previous`. A failure therefore **rejects the frame** and leaves the
    /// receiver's state and ack untouched; it never ends the session.
    ///
    /// Validates `self` on its own first, so a transition check can never let
    /// a malformed state through.
    ///
    /// Note what is deliberately *not* checked here: `seq` ordering, sizes,
    /// and content. States legitimately resize, and the sequence relationship
    /// belongs to the frame that carried the diff, not to the states.
    pub fn validate_transition(&self, previous: &ScreenState) -> Result<(), ApplyError> {
        self.validate()?;

        // I5. The client rings once per increment. Going backwards would
        // either swallow bells or, on the way back up, ring once for every
        // bell in the session's history.
        if self.bell < previous.bell {
            return Err(ApplyError::BellWentBackwards {
                was: previous.bell,
                now: self.bell,
            });
        }

        // I6. Lines that have scrolled off do not come back.
        if self.scrollback_len < previous.scrollback_len {
            return Err(ApplyError::ScrollbackShrank {
                was: previous.scrollback_len,
                now: self.scrollback_len,
            });
        }

        Ok(())
    }
}

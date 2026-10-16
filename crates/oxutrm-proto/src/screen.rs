//! The authoritative picture of a terminal, and the rules it must obey.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{ApplyError, Cell, Clipboard};

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
    /// The latest OSC 52 copy and how many there have been. See
    /// [`crate::clipboard`].
    pub clipboard: Clipboard,
    /// The OSC 8 hyperlinks the cells refer to: link id to URI. Exactly the
    /// ids some cell holds, on the host; at most [`crate::MAX_LINKS`] (**I10**).
    /// See [`crate::link`].
    pub links: BTreeMap<u16, Arc<str>>,
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
            clipboard: Clipboard::default(),
            links: BTreeMap::new(),
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

    /// At most `max` of this screen, cut the way the session preview shows
    /// it (preview spec §3.2): the leftmost columns, and the rows ending at
    /// the cursor's row while the cursor is visible -- in a shell, the prompt
    /// and the output above it -- else the top rows, where a full-screen
    /// program that hides its cursor keeps its header.
    ///
    /// Only the cells and the cursor are carried: the title, the modes, the
    /// bell, the scrollback count and the clipboard are a session's, not a
    /// picture's, and nothing that shows a window may act on them. Links are
    /// dropped too, every cell's and the table: a picture shown inside
    /// another session's screen must not resolve its ids against that
    /// session's table, and a preview is no place to click. A wide glyph whose right
    /// half falls outside the cut becomes a space in its own colours, as the
    /// renderer's `repair_wide_pairs` makes it: half a glyph is not a glyph.
    ///
    /// The window of a state that validates validates.
    #[must_use]
    pub fn window(&self, max: crate::TermSize) -> ScreenState {
        let rows = self.rows.min(max.rows);
        let cols = self.cols.min(max.cols);
        let top = if self.cursor.visible {
            // Saturating: a screen with no cells is exempt from I2, so its
            // cursor can sit at u16::MAX and still validate.
            self.cursor
                .row
                .saturating_add(1)
                .saturating_sub(rows)
                .min(self.rows - rows)
        } else {
            0
        };
        let mut cells = Vec::with_capacity(usize::from(rows) * usize::from(cols));
        for r in top..top + rows {
            let row = self.row(r);
            cells.extend(row[..usize::from(cols)].iter().map(|c| Cell {
                link: 0,
                ..c.clone()
            }));
            let cut_pair = cols > 0
                && cols < self.cols
                && row[usize::from(cols)]
                    .attrs
                    .contains(crate::Attrs::WIDE_CONT);
            if cut_pair && let Some(last) = cells.last_mut() {
                last.text = crate::CellText::const_new(" ");
                last.attrs = crate::Attrs::empty();
            }
        }
        let (row, col) = if rows == 0 || cols == 0 {
            (0, 0)
        } else {
            (
                self.cursor.row.saturating_sub(top).min(rows - 1),
                self.cursor.col.min(cols - 1),
            )
        };
        ScreenState {
            seq: 1,
            rows,
            cols,
            cells,
            cursor: Cursor {
                row,
                col,
                ..self.cursor
            },
            modes: Modes::default(),
            title: String::new(),
            bell: 0,
            scrollback_len: 0,
            clipboard: Clipboard::default(),
            links: BTreeMap::new(),
        }
    }

    /// Check every invariant that one state can carry on its own: **I1**
    /// (exact length), **I2** (cursor in bounds), **I3** (sequence not the
    /// sentinel), **I8** (painted text is text, not control) and **I10**
    /// (links are printable and exist).
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

        // I8. The only invariant here about CONTENT rather than shape, and the
        // reason it is worth a linear scan of a state that is about to be
        // painted anyway: the client renders these cells by writing them to
        // the user's real terminal, so a cell holding `\x1b]52;c;...\x07` is
        // the host reaching through the client to the user's clipboard.
        //
        // Checking it here makes a violating `ScreenState` impossible to hold
        // however it was constructed, and puts I8 on `Receiver::on_frame`'s
        // path for free by way of `validate_transition`. It is NOT where the
        // invariant is cheap, though: by the time a state exists its cells have
        // already been cloned. The enforcement that bounds the allocation is
        // the one in `ScreenState::apply`, before the run expansion.
        for cell in &self.cells {
            crate::check_cell_text(&cell.text)?;
        }
        crate::check_title(&self.title)?;
        crate::check_clipboard(&self.clipboard)?;

        // I10. The table first, so a cell's id is only ever looked up in a
        // table that is itself acceptable.
        crate::check_links(&self.links)?;
        for cell in &self.cells {
            if cell.link != 0 && !self.links.contains_key(&cell.link) {
                return Err(ApplyError::UnknownLink { id: cell.link });
            }
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
    /// Note what is deliberately *not* checked here: `seq` ordering and sizes.
    /// States legitimately resize, and the sequence relationship belongs to
    /// the frame that carried the diff, not to the states. Content is not
    /// absent from the contract — I8 covers it — but it is a property of one
    /// state, so `validate` above owns it and this function inherits it.
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

        // I9. The client writes a copy when the serial rises. Going backwards
        // and up again would write a copy it has already written.
        if self.clipboard.serial < previous.clipboard.serial {
            return Err(ApplyError::ClipboardWentBackwards {
                was: previous.clipboard.serial,
                now: self.clipboard.serial,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Attrs, CellText, Color, TermSize};

    const PANE: TermSize = TermSize { cols: 68, rows: 13 };

    /// A 120x40 shell: a log line on every row, the prompt on `prompt`.
    fn shell(prompt: u16) -> ScreenState {
        let mut s = ScreenState::blank(40, 120).unwrap();
        for r in 0..40 {
            let text = if r == prompt {
                "oetiker@thinlinc:~$ journalctl -f -u ssh".to_string()
            } else {
                format!("Oct 16 11:41:{r:02} thinlinc sshd[4121]: Accepted publickey for oetiker")
            };
            put(&mut s, r, 0, &text);
        }
        s.cursor.row = prompt;
        s.cursor.col = 41;
        s
    }

    fn put(s: &mut ScreenState, row: u16, col: u16, text: &str) {
        let cols = usize::from(s.cols);
        for (i, ch) in text.chars().enumerate() {
            let at = usize::from(row) * cols + usize::from(col) + i;
            s.cells[at].text = CellText::new(ch.to_string());
        }
    }

    fn line(s: &ScreenState, row: u16) -> String {
        s.row(row)
            .iter()
            .map(|c| c.text.as_str())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    #[test]
    fn the_window_ends_on_the_cursors_row() {
        let w = shell(30).window(PANE);
        assert_eq!((w.rows, w.cols), (13, 68));
        assert!(
            line(&w, 12).starts_with("oetiker@thinlinc:~$"),
            "{}",
            line(&w, 12)
        );
        assert!(
            line(&w, 0).starts_with("Oct 16 11:41:18"),
            "{}",
            line(&w, 0)
        );
        assert_eq!((w.cursor.row, w.cursor.col), (12, 41));
        w.validate().unwrap();
    }

    #[test]
    fn a_cursor_near_the_top_keeps_the_top_rows() {
        let w = shell(3).window(PANE);
        assert!(
            line(&w, 0).starts_with("Oct 16 11:41:00"),
            "{}",
            line(&w, 0)
        );
        assert_eq!(w.cursor.row, 3);
        w.validate().unwrap();
    }

    #[test]
    fn a_cursor_on_the_last_row_shows_the_bottom() {
        let w = shell(39).window(PANE);
        assert!(line(&w, 12).starts_with("oetiker@thinlinc:~$"));
        assert!(
            line(&w, 0).starts_with("Oct 16 11:41:27"),
            "{}",
            line(&w, 0)
        );
        w.validate().unwrap();
    }

    #[test]
    fn a_hidden_cursor_shows_the_top() {
        let mut s = shell(30);
        s.cursor.visible = false;
        let w = s.window(PANE);
        assert!(
            line(&w, 0).starts_with("Oct 16 11:41:00"),
            "{}",
            line(&w, 0)
        );
        assert!(!w.cursor.visible);
        assert_eq!(
            w.cursor.row, 12,
            "a hidden cursor is clamped into the window"
        );
        w.validate().unwrap();
    }

    #[test]
    fn a_screen_smaller_than_the_window_is_copied_whole() {
        let mut s = ScreenState::blank(10, 40).unwrap();
        put(&mut s, 9, 0, "$ make check");
        s.cursor.row = 9;
        s.cursor.col = 12;
        let w = s.window(PANE);
        assert_eq!((w.rows, w.cols), (10, 40));
        assert_eq!(w.cells, s.cells);
        assert_eq!((w.cursor.row, w.cursor.col), (9, 12));
    }

    #[test]
    fn only_the_cells_and_the_cursor_are_carried() {
        let mut s = shell(30);
        s.title = "vim notes.md".to_string();
        s.bell = 4;
        s.scrollback_len = 900;
        s.seq = 77;
        s.modes.alt_screen = true;
        s.modes.bracketed_paste = true;
        s.clipboard = Clipboard {
            serial: 3,
            copy: Some(crate::ClipboardCopy {
                target: crate::ClipboardTarget::Clipboard,
                text: "hunter2".into(),
            }),
        };
        let w = s.window(PANE);
        assert_eq!(w.title, "");
        assert_eq!((w.seq, w.bell, w.scrollback_len), (1, 0, 0));
        assert_eq!(w.modes, Modes::default());
        assert_eq!(w.clipboard, Clipboard::default(), "no copy, no serial");
    }

    #[test]
    fn a_window_carries_no_links() {
        let mut s = shell(30);
        s.links.insert(1, "https://example.org/".into());
        let prompt = usize::from(30u16) * 120;
        for cell in &mut s.cells[prompt..prompt + 8] {
            cell.link = 1;
        }
        s.validate().unwrap();
        let w = s.window(PANE);
        assert!(w.links.is_empty(), "no table");
        assert!(w.cells.iter().all(|c| c.link == 0), "no cell keeps an id");
        assert!(line(&w, 12).starts_with("oetiker@"), "the text stays");
        w.validate().unwrap();
    }

    #[test]
    fn a_cell_whose_link_is_not_in_the_table_is_refused() {
        let mut s = ScreenState::blank(2, 4).unwrap();
        s.cells[5].link = 3;
        assert_eq!(s.validate(), Err(ApplyError::UnknownLink { id: 3 }));
        s.links.insert(3, "http://a".into());
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_state_with_a_bad_link_table_is_refused() {
        let mut s = ScreenState::blank(2, 4).unwrap();
        s.links.insert(0, "http://a".into());
        assert_eq!(s.validate(), Err(ApplyError::LinkIdZero));
        let mut s = ScreenState::blank(2, 4).unwrap();
        s.links.insert(1, "http://a\x1b\\".into());
        assert!(matches!(
            s.validate(),
            Err(ApplyError::BadLinkUri { id: 1, .. })
        ));
        let mut s = ScreenState::blank(2, 4).unwrap();
        for k in 1..=crate::MAX_LINKS as u16 + 1 {
            s.links.insert(k, "http://a".into());
        }
        assert!(matches!(s.validate(), Err(ApplyError::TooManyLinks { .. })));
    }

    #[test]
    fn a_wide_glyph_cut_in_half_becomes_a_space_in_its_colours() {
        let mut s = ScreenState::blank(4, 80).unwrap();
        // "構築" from column 66: its second glyph straddles the cut at 68.
        for (col, glyph) in [(66u16, "構"), (68, "築")] {
            let at = usize::from(col);
            s.cells[at] = Cell {
                text: CellText::new(glyph),
                fg: Color::Idx(2),
                bg: Color::Rgb(0, 0, 64),
                attrs: Attrs::BOLD,
                link: 0,
            };
            s.cells[at + 1] = Cell {
                text: CellText::const_new(""),
                fg: Color::Idx(2),
                bg: Color::Rgb(0, 0, 64),
                attrs: Attrs::WIDE_CONT,
                link: 0,
            };
        }
        // Column 67 is the first glyph's right half: inside, kept whole.
        let w = s.window(TermSize { cols: 67, rows: 4 });
        assert_eq!(
            w.cell(0, 66).text.as_str(),
            " ",
            "a lead cut from its right half"
        );
        assert_eq!(w.cell(0, 66).bg, Color::Rgb(0, 0, 64));
        assert_eq!(w.cell(0, 66).attrs, Attrs::empty());
        let w = s.window(TermSize { cols: 68, rows: 4 });
        assert_eq!(w.cell(0, 66).text.as_str(), "構", "a whole pair is kept");
        assert!(w.cell(0, 67).attrs.contains(Attrs::WIDE_CONT));
        w.validate().unwrap();
    }

    #[test]
    fn an_empty_screen_or_window_is_empty_and_valid() {
        let s = ScreenState::blank(0, 0).unwrap();
        let w = s.window(PANE);
        assert_eq!((w.rows, w.cols, w.cells.len()), (0, 0, 0));
        w.validate().unwrap();
        let w = shell(30).window(TermSize { cols: 0, rows: 0 });
        assert_eq!((w.rows, w.cols, w.cursor.row, w.cursor.col), (0, 0, 0, 0));
        w.validate().unwrap();
    }

    /// A screen with no cells is exempt from the cursor bound (I2), so a
    /// peer can send one with its cursor anywhere at all: the window of it
    /// is still empty and valid.
    #[test]
    fn a_screen_with_no_cells_and_its_cursor_at_the_end_has_an_empty_window() {
        for (rows, cols) in [(0, 0), (0, 80), (24, 0)] {
            let mut s = ScreenState::blank(rows, cols).unwrap();
            s.cursor.row = u16::MAX;
            s.cursor.col = u16::MAX;
            s.cursor.visible = true;
            s.validate().unwrap();
            let w = s.window(PANE);
            assert_eq!(w.cells.len(), 0, "{rows}x{cols}");
            w.validate().unwrap();
        }
    }
}

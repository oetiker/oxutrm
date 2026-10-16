//! A cap on the zero-width marks one cell collects, applied before the
//! emulator stores them.
//!
//! `Term::input` pushes every width-0 scalar onto the cell before the cursor,
//! and puts no limit on how many: a program that prints combining marks in a
//! loop grows one cell's mark list by four bytes per mark, until the OOM
//! killer acts. `cell_text` in `host.rs` already cuts what a snapshot
//! *shows* to [`MAX_CELL_TEXT`]; this stops the emulator *holding* more.
//!
//! It works on the parser's calls, not on the byte stream, because only the
//! emulator knows which cell a mark lands on: the cursor moves under CSI, and
//! a counter on the bytes that reset at every escape would let `CSI C CSI D`
//! between bursts of marks pile them onto the same cell forever.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::index::Point;
use alacritty_terminal::term::cell::Flags;
use oxutrm_proto::MAX_CELL_TEXT;
use unicode_width::UnicodeWidthChar as _;

use crate::grid::cell_at;

/// Whether `c` may go on to `Term::input`.
///
/// `false` only for a zero-width `c` that would take the cell it lands on
/// past [`MAX_CELL_TEXT`] bytes, the same bound `cell_text` cuts a snapshot
/// to, so nothing dropped here was ever going to be shown.
pub(crate) fn mark_fits<T: EventListener>(term: &Term<T>, c: char) -> bool {
    // The same crate and version `Term::input` asks; a disagreement here
    // would drop a character the emulator meant to print.
    if c.width() != Some(0) {
        return true;
    }
    let Some(cell) = cell_at(term, landing(term)) else {
        return true;
    };
    let held: usize = cell.c.len_utf8()
        + cell
            .zerowidth()
            .unwrap_or_default()
            .iter()
            .map(|m| m.len_utf8())
            .sum::<usize>();
    held + c.len_utf8() <= MAX_CELL_TEXT
}

/// The cell `Term::input` puts a zero-width character on: the one before the
/// cursor, unless the cursor is waiting to wrap at the last column, and the
/// first half of a wide character rather than its spacer.
fn landing<T: EventListener>(term: &Term<T>) -> Point {
    let cursor = &term.grid().cursor;
    let mut point = cursor.point;
    if !cursor.input_needs_wrap {
        point.column.0 = point.column.saturating_sub(1);
    }
    if cell_at(term, point).is_some_and(|cell| cell.flags.contains(Flags::WIDE_CHAR_SPACER)) {
        point.column.0 = point.column.saturating_sub(1);
    }
    point
}

#[cfg(test)]
mod tests {
    use super::*;

    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;
    use oxutrm_proto::TermSize;

    use crate::blink::{BlinkPlane, BlinkTap};
    use crate::grid::GridSize;
    use crate::listener::EventSink;

    struct Emu {
        term: Term<EventSink>,
        blink: BlinkPlane,
        scrolled: u64,
        parser: Processor,
    }

    impl Emu {
        fn new(cols: u16) -> Emu {
            let dims = GridSize::new(TermSize { cols, rows: 3 }, 10).expect("size");
            Emu {
                term: Term::new(Config::default(), &dims, EventSink::new()),
                blink: BlinkPlane::default(),
                scrolled: 0,
                parser: Processor::new(),
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let mut tap = BlinkTap::new(&mut self.term, &mut self.blink, &mut self.scrolled);
            self.parser.advance(&mut tap, bytes);
        }

        /// Base character and marks of the cell at row 0, `col`.
        fn cell(&self, col: usize) -> String {
            let cell = cell_at(&self.term, Point::new(Line(0), Column(col))).expect("cell");
            let mut s = String::from(cell.c);
            s.extend(cell.zerowidth().unwrap_or_default());
            s
        }
    }

    const ACUTE: char = '\u{301}';

    fn marks(n: usize) -> String {
        std::iter::repeat_n(ACUTE, n).collect()
    }

    /// How many 2-byte marks fit after a 1-byte base.
    const FIT: usize = (MAX_CELL_TEXT - 1) / 2;

    #[test]
    fn marks_past_the_cap_are_not_stored() {
        let mut e = Emu::new(10);
        e.feed(format!("e{}", marks(10_000)).as_bytes());
        assert_eq!(e.cell(0), format!("e{}", marks(FIT)));
    }

    #[test]
    fn marks_with_no_base_are_capped_as_well() {
        let mut e = Emu::new(10);
        e.feed(marks(10_000).as_bytes());
        assert!(
            e.cell(0).len() <= MAX_CELL_TEXT,
            "{} bytes",
            e.cell(0).len()
        );
    }

    #[test]
    fn moving_the_cursor_between_bursts_does_not_get_past_the_cap() {
        let mut e = Emu::new(10);
        e.feed(b"e");
        for _ in 0..1_000 {
            e.feed(format!("{}\x1b[C\x1b[D", marks(10)).as_bytes());
        }
        assert_eq!(e.cell(0), format!("e{}", marks(FIT)));
    }

    #[test]
    fn marks_on_a_wide_character_and_at_the_last_column_are_capped() {
        // A wide character takes its marks on its first cell.
        let mut e = Emu::new(10);
        e.feed(format!("\u{4e2d}{}", marks(10_000)).as_bytes());
        let want = format!("\u{4e2d}{}", marks((MAX_CELL_TEXT - 3) / 2));
        assert_eq!(e.cell(0), want);
        // At the last column the cursor waits to wrap and does not move on.
        let mut e = Emu::new(3);
        e.feed(format!("abc{}", marks(10_000)).as_bytes());
        assert_eq!(e.cell(2), format!("c{}", marks(FIT)));
    }

    #[test]
    fn a_real_grapheme_cluster_is_stored_whole() {
        let clusters = [
            "e\u{302}\u{301}",
            "\u{5d0}\u{5b8}\u{5bc}\u{591}",
            "\u{f40}\u{f90}\u{fb5}\u{f72}",
            "1\u{fe0f}\u{20e3}",
        ];
        for cluster in clusters {
            let mut e = Emu::new(10);
            e.feed(cluster.as_bytes());
            assert_eq!(e.cell(0), cluster);
        }
    }

    #[test]
    fn a_mark_that_reaches_the_cap_exactly_is_kept() {
        // A 2-byte base and 15 marks of 2 bytes are 32 bytes: the 15th is
        // kept, the 16th is not.
        let mut e = Emu::new(10);
        e.feed(format!("\u{e9}{}", marks(16)).as_bytes());
        assert_eq!(e.cell(0), format!("\u{e9}{}", marks(15)));
        assert_eq!(e.cell(0).len(), MAX_CELL_TEXT);
    }
}

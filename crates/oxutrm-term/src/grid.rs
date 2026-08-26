//! Dimensions, and the one checked way to read a cell.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Point};
use alacritty_terminal::term::cell::Cell as VteCell;

use oxutrm_proto::{ApplyError, TermSize};

/// A screen size the emulator will accept.
///
/// `Dimensions` is a trait, not a struct, and `Term::new`/`Term::resize` take
/// anything implementing it. `alacritty_terminal::term::test::TermSize` exists
/// but is test scaffolding in someone else's crate; three trivial methods are
/// cheaper than depending on that.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct GridSize {
    pub screen_lines: usize,
    pub columns: usize,
    pub history: usize,
}

impl GridSize {
    /// **The choke point where a peer-chosen `TermSize` becomes an
    /// allocation**, so I7 is enforced here and it is fallible for that
    /// reason alone.
    ///
    /// The client sends its window size to the host, in `ClientHello` and
    /// again on every resize. `Term::new` and `Term::resize` allocate
    /// `(screen_lines + history) * columns` emulator cells from whatever this
    /// returns, with no bound of their own. `rows` and `cols` are `u16`, so an
    /// unchecked size means a client can ask a host it has merely connected to
    /// for hundreds of gigabytes — the same memory bomb as the resize arm of
    /// `ScreenState::apply`, pointing the other way down the wire, and worse
    /// because `history` multiplies it.
    ///
    /// Returning a `Result` rather than clamping is deliberate and matches I2:
    /// a clamped size means the two ends silently disagree about how big the
    /// screen is. A refused one is visible.
    pub fn new(size: TermSize, history: usize) -> Result<GridSize, ApplyError> {
        size.check_bounds()?;
        Ok(GridSize {
            screen_lines: size.rows as usize,
            columns: size.cols as usize,
            history,
        })
    }
}

impl Dimensions for GridSize {
    fn total_lines(&self) -> usize {
        self.screen_lines + self.history
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Read one cell, or `None` if the point is outside the grid.
///
/// **This is the only place in oxutrm that indexes the emulator's grid, and it
/// must stay that way.** `Grid`'s `Index<Point>` carries a `debug_assert` and
/// nothing else: out of range it panics in a debug build and reads whatever
/// memory is next along in a release build. A release-only garbage read is
/// close to undiagnosable, so every access is clamped first and compared
/// against what was asked for.
pub fn cell_at<T: EventListener>(term: &Term<T>, point: Point) -> Option<&VteCell> {
    let clamped = point.grid_clamp(term, Boundary::Grid);
    // The clamp moved it, so the point was never in the grid.
    if clamped != point {
        return None;
    }
    Some(&term.grid()[point])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::term_with;
    use alacritty_terminal::index::{Column, Line};

    #[test]
    fn dimensions_are_what_the_emulator_asks_for() {
        let g = GridSize::new(TermSize { cols: 80, rows: 24 }, 1_000).expect("80x24 is legal");
        assert_eq!(g.columns(), 80);
        assert_eq!(g.screen_lines(), 24);
        assert_eq!(g.total_lines(), 1_024);
        assert_eq!(
            g.history_size(),
            1_000,
            "provided by the trait, from the two above"
        );
        assert_eq!(g.last_column(), Column(79));
        assert_eq!(g.bottommost_line(), Line(23));
        assert_eq!(g.topmost_line(), Line(-1_000));
    }

    /// The test whose absence let a `// FAULT INJECTION` marker ship in place
    /// of the check itself, in the very commit that added I7. `GridSize::new`
    /// was made fallible, given a doc comment naming it the choke point, and
    /// its caller updated to `.expect(...)` — and the body checked nothing.
    /// Nothing failed, because nothing here ever asked it to refuse.
    ///
    /// This is the host-facing direction of I7: a hostile CLIENT sends the
    /// size, and `history` multiplies whatever it asks for.
    #[test]
    fn an_over_cap_size_is_refused_rather_than_allocated() {
        for (rows, cols, why) in [
            (u16::MAX, u16::MAX, "the unbounded resize bomb itself"),
            (2_049, 1, "rows alone past MAX_SCREEN_DIM"),
            (1, 2_049, "cols alone past MAX_SCREEN_DIM"),
            (1_024, 1_024, "each dimension legal, the product is not"),
        ] {
            let err = GridSize::new(TermSize { cols, rows }, 1_000)
                .expect_err(&format!("{rows}x{cols} must be refused: {why}"));
            assert!(
                matches!(err, ApplyError::ScreenTooLarge { .. }),
                "{rows}x{cols} ({why}) gave {err:?}, not ScreenTooLarge"
            );
        }
    }

    /// `history` is not part of I7's arithmetic, so a legal size stays legal
    /// however deep the scrollback. Pinned because the obvious over-correction
    /// is to fold `history` into the cell count and start refusing ordinary
    /// terminals.
    #[test]
    fn a_legal_size_survives_a_deep_history() {
        let g = GridSize::new(
            TermSize {
                cols: 400,
                rows: 120,
            },
            100_000,
        )
        .expect("a 4K display at a 6px font, with deep scrollback");
        assert_eq!(g.total_lines(), 100_120);
    }

    #[test]
    fn a_cell_inside_the_grid_reads_back() {
        let term = term_with(4, 8, b"hi");
        let c = cell_at(&term, Point::new(Line(0), Column(0))).expect("in range");
        assert_eq!(c.c, 'h');
    }

    #[test]
    fn every_boundary_is_checked_rather_than_clamped_into_a_wrong_cell() {
        // Index<Point> has only a debug_assert: out of range it panics in
        // debug and reads garbage in release. None is the only safe answer,
        // and a clamp would silently return a DIFFERENT cell's contents.
        let term = term_with(4, 8, b"");
        // Inside.
        assert!(cell_at(&term, Point::new(Line(0), Column(0))).is_some());
        assert!(
            cell_at(&term, Point::new(Line(3), Column(7))).is_some(),
            "last cell"
        );
        // One past, in each direction.
        assert!(
            cell_at(&term, Point::new(Line(4), Column(0))).is_none(),
            "past the last row"
        );
        assert!(
            cell_at(&term, Point::new(Line(0), Column(8))).is_none(),
            "past the last column"
        );
        assert!(cell_at(&term, Point::new(Line(3), Column(8))).is_none());
        assert!(cell_at(&term, Point::new(Line(4), Column(8))).is_none());
        // Far out, both signs.
        assert!(cell_at(&term, Point::new(Line(9_999), Column(0))).is_none());
        assert!(cell_at(&term, Point::new(Line(0), Column(9_999))).is_none());
        assert!(
            cell_at(&term, Point::new(Line(-9_999), Column(0))).is_none(),
            "below the oldest history line"
        );
    }

    #[test]
    fn history_lines_are_reachable_with_a_negative_line() {
        // Scrollback is native and O(1): Line is a signed i32 and negative
        // reaches history. Line(-1) is the most recently scrolled-off line.
        let mut bytes = Vec::new();
        for i in 0..8u8 {
            bytes.extend_from_slice(&[b'a' + i, b'\r', b'\n']);
        }
        let term = term_with(2, 8, &bytes);
        assert!(term.history_size() > 0, "lines must have scrolled off");
        assert!(cell_at(&term, Point::new(Line(-1), Column(0))).is_some());
    }
}

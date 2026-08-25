//! Dimensions, and the one checked way to read a cell.

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Boundary, Point};
use alacritty_terminal::term::cell::Cell as VteCell;

use oxutrm_proto::TermSize;

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
    pub fn new(size: TermSize, history: usize) -> GridSize {
        GridSize {
            screen_lines: size.rows as usize,
            columns: size.cols as usize,
            history,
        }
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
        let g = GridSize::new(TermSize { cols: 80, rows: 24 }, 1_000);
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

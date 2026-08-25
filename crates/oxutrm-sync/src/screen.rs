//! Diffing one screen against another.

use serde::{Deserialize, Serialize};

use oxutrm_proto::{ApplyError, Cell, Cursor, Modes, ScreenState, TermSize};

use crate::SyncState;

/// A stretch of one row.
///
/// The `cells` sequence is emitted **`repeat + 1` times consecutively**,
/// starting at `start_col`. So `repeat == 0` means "emit `cells` exactly
/// once", which is the common case, and the field costs one varint byte for
/// the ability to collapse a run of identical cells — a blank line, a rule, a
/// progress bar — into almost nothing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Run {
    pub start_col: u16,
    pub repeat: u16,
    pub cells: Vec<Cell>,
}

impl Run {
    /// How many columns this run writes.
    fn width(&self) -> usize {
        self.cells.len() * (self.repeat as usize + 1)
    }
}

/// The changed parts of one row. Rows that did not change are absent entirely.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RowPatch {
    pub row: u16,
    pub runs: Vec<Run>,
}

/// What changed between two screens.
///
/// `base` and `target` are deliberately **not** here: they live in the
/// [`Frame`](oxutrm_proto::Frame) that carries this, and duplicating them
/// would create two sources of truth that could disagree.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenDiff {
    pub resize: Option<TermSize>,
    pub rows: Vec<RowPatch>,
    pub cursor: Option<Cursor>,
    pub modes: Option<Modes>,
    pub title: Option<String>,
    pub bell: Option<u32>,
    pub scrollback_len: Option<u64>,
}

/// Turn one row's worth of cells into runs, emitting only what changed.
///
/// `base` is `None` for a full state, where every cell counts as changed.
fn row_runs(target: &[Cell], base: Option<&[Cell]>) -> Vec<Run> {
    let mut runs = Vec::new();
    let mut col = 0usize;

    while col < target.len() {
        // Skip over cells that already match.
        if let Some(base) = base {
            if col < base.len() && target[col] == base[col] {
                col += 1;
                continue;
            }
        }

        // A maximal span of changed cells, run-length encoded within itself.
        let span_start = col;
        let mut literal: Vec<Cell> = Vec::new();
        let mut literal_start = span_start;

        while col < target.len() {
            let unchanged = match base {
                Some(base) => col < base.len() && target[col] == base[col],
                None => false,
            };
            if unchanged {
                break;
            }

            // How far does this exact cell repeat?
            let mut run_len = 1usize;
            while col + run_len < target.len() && target[col + run_len] == target[col] {
                let still_changed = match base {
                    Some(base) => {
                        !(col + run_len < base.len()
                            && target[col + run_len] == base[col + run_len])
                    }
                    None => true,
                };
                if !still_changed {
                    break;
                }
                run_len += 1;
            }

            if run_len >= 2 {
                // Worth collapsing. Flush whatever literal was accumulating.
                if !literal.is_empty() {
                    runs.push(Run {
                        start_col: literal_start as u16,
                        repeat: 0,
                        cells: std::mem::take(&mut literal),
                    });
                }
                runs.push(Run {
                    start_col: col as u16,
                    repeat: (run_len - 1) as u16,
                    cells: vec![target[col].clone()],
                });
                col += run_len;
                literal_start = col;
            } else {
                if literal.is_empty() {
                    literal_start = col;
                }
                literal.push(target[col].clone());
                col += 1;
            }
        }

        if !literal.is_empty() {
            runs.push(Run {
                start_col: literal_start as u16,
                repeat: 0,
                cells: literal,
            });
        }
    }

    runs
}

impl SyncState for ScreenState {
    type Diff = ScreenDiff;

    fn seq(&self) -> u64 {
        self.seq
    }

    fn set_seq(&mut self, seq: u64) {
        self.seq = seq;
    }

    fn validate(&self) -> Result<(), ApplyError> {
        ScreenState::validate(self)
    }

    /// I5 and I6 — the bell never goes backwards, scrollback never shrinks.
    ///
    /// `ScreenState::validate_transition` validates `self` on its own first,
    /// so I1 to I3 are still checked here exactly as before.
    fn validate_transition(&self, previous: &Self) -> Result<(), ApplyError> {
        ScreenState::validate_transition(self, previous)
    }

    fn diff_from(&self, base: &Self) -> ScreenDiff {
        let resized = self.rows != base.rows || self.cols != base.cols;

        let mut rows = Vec::new();
        for r in 0..self.rows {
            let target = self.row(r);
            // After a resize the receiver's buffer is reallocated blank, so
            // every row has to be sent in full - there is nothing to diff
            // against.
            let base_row = if resized || r >= base.rows {
                None
            } else {
                Some(base.row(r))
            };
            let runs = row_runs(target, base_row);
            if !runs.is_empty() {
                rows.push(RowPatch { row: r, runs });
            }
        }

        ScreenDiff {
            resize: resized.then_some(TermSize {
                cols: self.cols,
                rows: self.rows,
            }),
            rows,
            cursor: (self.cursor != base.cursor).then_some(self.cursor),
            modes: (self.modes != base.modes).then_some(self.modes),
            title: (self.title != base.title).then(|| self.title.clone()),
            bell: (self.bell != base.bell).then_some(self.bell),
            scrollback_len: (self.scrollback_len != base.scrollback_len)
                .then_some(self.scrollback_len),
        }
    }

    fn full_diff(&self) -> ScreenDiff {
        let mut rows = Vec::new();
        for r in 0..self.rows {
            let runs = row_runs(self.row(r), None);
            if !runs.is_empty() {
                rows.push(RowPatch { row: r, runs });
            }
        }
        ScreenDiff {
            // Always present: the receiver may be any size, or may be a fresh
            // client with nothing at all.
            resize: Some(TermSize {
                cols: self.cols,
                rows: self.rows,
            }),
            rows,
            cursor: Some(self.cursor),
            modes: Some(self.modes),
            title: Some(self.title.clone()),
            bell: Some(self.bell),
            scrollback_len: Some(self.scrollback_len),
        }
    }

    fn apply(&mut self, base: u64, target: u64, d: &ScreenDiff) -> Result<(), ApplyError> {
        // `base == 0` is the full-state sentinel: it builds on nothing, so
        // whatever we currently hold is irrelevant.
        if base != 0 && base != self.seq {
            return Err(ApplyError::BaseMismatch {
                base,
                current: self.seq,
            });
        }

        // The resize comes first, because every row index below is relative to
        // the NEW geometry. The buffer is reallocated blank rather than
        // reshaped: a full state's row patches then cover it completely, and a
        // resize diff's do too.
        if let Some(size) = d.resize {
            self.rows = size.rows;
            self.cols = size.cols;
            self.cells = vec![Cell::blank(); size.rows as usize * size.cols as usize];
        }

        let cols = self.cols as usize;
        for patch in &d.rows {
            if patch.row >= self.rows {
                return Err(ApplyError::OutOfBounds {
                    row: patch.row,
                    rows: self.rows,
                });
            }
            let row_start = patch.row as usize * cols;
            for run in &patch.runs {
                let start = run.start_col as usize;
                let end = start + run.width();
                // Truncating instead would leave a screen that still validates
                // - the length is unchanged - while disagreeing with the host
                // about what is painted on it.
                if end > cols {
                    return Err(ApplyError::RunOverflowsRow {
                        row: patch.row,
                        end_col: end,
                        cols: self.cols,
                    });
                }
                let mut col = start;
                for _ in 0..=run.repeat {
                    for cell in &run.cells {
                        self.cells[row_start + col] = cell.clone();
                        col += 1;
                    }
                }
            }
        }

        if let Some(c) = d.cursor {
            self.cursor = c;
        }
        if let Some(m) = d.modes {
            self.modes = m;
        }
        if let Some(t) = &d.title {
            self.title.clone_from(t);
        }
        if let Some(b) = d.bell {
            self.bell = b;
        }
        if let Some(s) = d.scrollback_len {
            self.scrollback_len = s;
        }

        self.seq = target;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::Color;

    fn cell(ch: &str) -> Cell {
        Cell {
            text: ch.into(),
            ..Cell::blank()
        }
    }

    #[test]
    fn a_run_emits_its_cells_repeat_plus_one_times() {
        // The one piece of arithmetic in the wire format that is easy to get
        // off by one, so it is pinned for 0, 1 and 5.
        struct Case {
            repeat: u16,
            want: &'static str,
        }
        let cases = [
            Case {
                repeat: 0,
                want: "ab",
            },
            Case {
                repeat: 1,
                want: "abab",
            },
            Case {
                repeat: 5,
                want: "abababababab",
            },
        ];

        for c in cases {
            let mut s = ScreenState::blank(1, 12).expect("blank");
            let d = ScreenDiff {
                resize: None,
                rows: vec![RowPatch {
                    row: 0,
                    runs: vec![Run {
                        start_col: 0,
                        repeat: c.repeat,
                        cells: vec![cell("a"), cell("b")],
                    }],
                }],
                cursor: None,
                modes: None,
                title: None,
                bell: None,
                scrollback_len: None,
            };
            s.apply(1, 2, &d).expect("apply");
            let painted: String = s
                .row(0)
                .iter()
                .take(c.want.len())
                .map(|c| c.text.as_str())
                .collect();
            assert_eq!(
                painted,
                c.want,
                "repeat {} must emit {} times",
                c.repeat,
                c.repeat + 1
            );
            assert_eq!(
                Run {
                    start_col: 0,
                    repeat: c.repeat,
                    cells: vec![cell("a"), cell("b")]
                }
                .width(),
                c.want.len(),
                "width() must agree with what apply paints"
            );
        }
    }

    #[test]
    fn an_unchanged_row_produces_no_patch_at_all() {
        let base = ScreenState::blank(3, 4).expect("blank");
        let mut target = base.clone();
        target.cells[4].text = "X".into(); // row 1
        let d = target.diff_from(&base);
        assert_eq!(d.rows.len(), 1, "only the row that changed travels");
        assert_eq!(d.rows[0].row, 1);
        assert!(d.resize.is_none());
        assert!(d.cursor.is_none());
        assert!(d.title.is_none());
    }

    #[test]
    fn a_repeated_cell_collapses_into_one_run() {
        let base = ScreenState::blank(1, 10).expect("blank");
        let mut target = base.clone();
        for c in target.cells.iter_mut() {
            c.text = "-".into();
        }
        let d = target.diff_from(&base);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].runs.len(), 1, "ten identical cells are one run");
        assert_eq!(
            d.rows[0].runs[0].repeat, 9,
            "ten emissions means repeat == 9"
        );
        assert_eq!(d.rows[0].runs[0].cells.len(), 1);
    }

    #[test]
    fn scattered_changes_stay_separate_runs() {
        let base = ScreenState::blank(1, 10).expect("blank");
        let mut target = base.clone();
        target.cells[1].text = "a".into();
        target.cells[7].text = "b".into();
        let d = target.diff_from(&base);
        assert_eq!(d.rows[0].runs.len(), 2, "two islands of change, two runs");
        assert_eq!(d.rows[0].runs[0].start_col, 1);
        assert_eq!(d.rows[0].runs[1].start_col, 7);
    }

    #[test]
    fn a_diff_round_trips_through_apply() {
        let base = ScreenState::blank(4, 8).expect("blank");
        let mut target = base.clone();
        target.seq = 2;
        for (i, c) in target.cells.iter_mut().enumerate() {
            c.text = char::from(b'a' + (i % 26) as u8).to_string().into();
            c.fg = Color::Idx(i as u8);
        }
        target.title = "t".to_owned();
        target.bell = 4;

        let d = target.diff_from(&base);
        let mut got = base.clone();
        got.apply(1, 2, &d).expect("apply");
        assert_eq!(got, target);
    }

    #[test]
    fn a_resize_sends_every_row_because_the_buffer_is_reallocated() {
        let base = ScreenState::blank(2, 2).expect("blank");
        let mut target = ScreenState::blank(3, 5).expect("blank");
        target.seq = 2;
        for c in target.cells.iter_mut() {
            c.text = "z".into();
        }
        let d = target.diff_from(&base);
        assert_eq!(d.resize, Some(TermSize { cols: 5, rows: 3 }));
        assert_eq!(
            d.rows.len(),
            3,
            "after a resize there is nothing to diff against"
        );

        let mut got = base.clone();
        got.apply(1, 2, &d).expect("apply");
        assert_eq!(got, target);
    }

    #[test]
    fn a_full_diff_applies_onto_a_screen_of_any_shape() {
        let mut target = ScreenState::blank(2, 3).expect("blank");
        target.seq = 9;
        target.title = "full".to_owned();
        target.cells[0].text = "q".into();

        let d = target.full_diff();
        let mut got = ScreenState::blank(40, 40).expect("blank");
        got.apply(0, 9, &d).expect("a full state builds on nothing");
        assert_eq!(got, target);
    }

    #[test]
    fn apply_sets_the_target_sequence_number() {
        let base = ScreenState::blank(1, 1).expect("blank");
        let mut got = base.clone();
        let d = base.diff_from(&base);
        got.apply(1, 77, &d).expect("apply");
        assert_eq!(got.seq, 77);
    }

    #[test]
    fn a_shrinking_resize_drops_the_cells_that_no_longer_exist() {
        let mut base = ScreenState::blank(4, 4).expect("blank");
        for c in base.cells.iter_mut() {
            c.text = "x".into();
        }
        let mut target = ScreenState::blank(2, 2).expect("blank");
        target.seq = 2;

        let d = target.diff_from(&base);
        let mut got = base.clone();
        got.apply(1, 2, &d).expect("apply");
        assert_eq!(got.cells.len(), 4);
        assert_eq!(got, target);
    }
}

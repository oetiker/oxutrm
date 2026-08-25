//! The host's terminal: a PTY, the emulator reading it, and the snapshot
//! everything downstream replicates.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell as VteCell, Flags};
use alacritty_terminal::term::{Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as VteCursorShape, Processor, Rgb};

use oxutrm_proto::{
    Attrs, Cell, CellText, Cursor, CursorShape, Modes, MouseMode, ScreenState, TermSize,
};

use crate::blink::{BlinkPlane, BlinkTap};
use crate::grid::{GridSize, cell_at};
use crate::listener::EventSink;
use crate::palette::{PALETTE_LEN, palette, to_proto_color};
use crate::pty::Pty;

/// How much is read from the PTY in one `read`.
const READ_CHUNK: usize = 64 * 1024;

/// The most one [`HostTerm::poll`] will drain before returning.
///
/// **Without this the loop does not terminate.** `poll` drains until the PTY
/// is empty, and a child that writes faster than we read - `yes`, a `find /`,
/// a cat of a huge file - keeps it non-empty indefinitely. The caller never
/// gets control back, so no frame is ever emitted and the screen freezes:
/// precisely the "falls behind" failure the whole design exists to prevent,
/// arriving through the one path that bypasses it.
///
/// 64 KiB per call at an 8 ms tick is 8 MB/s, far more than any terminal
/// produces meaningfully, and it bounds the work per turn either way.
const READ_BUDGET: usize = 64 * 1024;

/// PTY + emulator. Owns the child process.
pub struct HostTerm {
    pty: Pty,
    term: Term<EventSink>,
    parser: Processor,
    events: EventSink,
    blink: BlinkPlane,
    palette: [Rgb; PALETTE_LEN],

    size: TermSize,
    history: usize,

    /// Accumulated across the session, because `ScreenState::bell` is
    /// monotonic and the sink's counter resets on every drain.
    bell: u32,
    title: String,

    /// Synthesized, because the emulator has no monotonic scrolled-off
    /// counter and `history_size()` saturates at capacity. Counted at the
    /// source instead, in [`crate::blink::BlinkTap`].
    scrollback_len: u64,

    exited: Option<i32>,
}

impl HostTerm {
    pub fn spawn(
        shell: &str,
        args: &[String],
        env: &[(String, String)],
        size: TermSize,
        scrollback: usize,
    ) -> anyhow::Result<HostTerm> {
        let pty = Pty::spawn(shell, args, env, size)?;
        let events = EventSink::new();
        let dims = GridSize::new(size, scrollback);
        let config = Config {
            scrolling_history: scrollback,
            // Accept paste requests as well as copies; the default is
            // OnlyCopy, which silently drops half of OSC 52.
            osc52: Osc52::CopyPaste,
            ..Config::default()
        };
        let term = Term::new(config, &dims, events.clone());

        Ok(HostTerm {
            pty,
            term,
            parser: Processor::new(),
            events,
            blink: BlinkPlane::default(),
            palette: palette(),
            size,
            history: scrollback,
            bell: 0,
            title: String::new(),
            scrollback_len: 0,
            exited: None,
        })
    }

    /// Write user input to the PTY.
    pub fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.pty.write_input(bytes)
    }

    /// Resize the PTY and the emulator.
    ///
    /// `Term::resize` genuinely reflows the primary grid, losslessly in both
    /// directions, so no content is rewrapped by hand here. The alternate
    /// grid never reflows and has no history, which is `alacritty_terminal`'s
    /// behaviour and correct: a full-screen application redraws itself.
    pub fn resize(&mut self, size: TermSize) -> anyhow::Result<()> {
        self.pty.resize(size)?;
        self.term.resize(GridSize::new(size, self.history));
        self.size = size;
        // Reflow moves content, so recorded blink positions no longer mean
        // anything. Clearing is honest; shifting them would paint blink on
        // cells that never had it.
        self.blink.clear();
        Ok(())
    }

    /// Drain whatever the PTY has ready, without blocking.
    ///
    /// Returns true when the screen changed, which is the question
    /// `ScreenDiff` asks. That answer comes from `Term::damage()` — the
    /// emulator's own per-line dirty ranges — rather than from comparing two
    /// whole grids.
    ///
    /// **Damage is an input handed to the sync engine, never something the
    /// sync engine asks for.** `oxutrm-sync` has no idea an emulator exists.
    pub fn poll(&mut self) -> anyhow::Result<bool> {
        let mut buf = vec![0u8; READ_CHUNK];
        let mut changed = false;
        let mut drained = 0usize;

        loop {
            // Bounded on purpose: see READ_BUDGET. Whatever is left stays in
            // the PTY buffer and is picked up next turn, which is exactly the
            // coalescing behaviour - the emulator has already applied
            // everything read so far, so the snapshot is current either way.
            if drained >= READ_BUDGET {
                break;
            }
            let n = self.pty.read_ready(&mut buf)?;
            if n == 0 {
                break;
            }
            drained += n;
            {
                // The tap counts scrolled-off lines as they happen. Measuring
                // `history_size()` growth afterwards does NOT work: it
                // saturates at capacity, so on a busy terminal it stops
                // counting within seconds of the ring filling.
                let mut tap =
                    BlinkTap::new(&mut self.term, &mut self.blink, &mut self.scrollback_len);
                self.parser.advance(&mut tap, &buf[..n]);
            }
            changed = true;
        }

        let signals = self.events.drain();
        if let Some(t) = signals.title {
            self.title = t;
            changed = true;
        }
        if signals.bells > 0 {
            self.bell = self.bell.saturating_add(signals.bells);
            changed = true;
        }
        if let Some(code) = signals.child_exit {
            self.exited = Some(code);
        }

        // The emulator's own answer to "did anything move". Reset so the next
        // poll reports only what is new.
        let damaged = matches!(
            self.term.damage(),
            alacritty_terminal::term::TermDamage::Full
        ) || changed;
        self.term.reset_damage();

        Ok(damaged)
    }

    /// Build a state carrying the given sequence number.
    pub fn snapshot(&self, seq: u64) -> ScreenState {
        let rows = self.size.rows;
        let cols = self.size.cols;
        let mut cells = Vec::with_capacity(rows as usize * cols as usize);

        for row in 0..rows {
            for col in 0..cols {
                let point = Point::new(Line(row as i32), Column(col as usize));
                let cell = match cell_at(&self.term, point) {
                    Some(c) => self.convert(c, point),
                    // Outside the emulator's grid: the sizes disagree, which
                    // can happen for one poll after a resize. A blank keeps
                    // the length invariant, which matters more than the cell.
                    None => Cell::blank(),
                };
                cells.push(cell);
            }
        }

        let cursor_point = self.term.grid().cursor.point;
        let cursor = Cursor {
            row: cursor_point.line.0.clamp(0, rows.saturating_sub(1) as i32) as u16,
            col: (cursor_point.column.0 as u16).min(cols.saturating_sub(1)),
            visible: self.term.mode().contains(TermMode::SHOW_CURSOR),
            shape: match self.term.cursor_style().shape {
                VteCursorShape::Block | VteCursorShape::HollowBlock => CursorShape::Block,
                VteCursorShape::Underline => CursorShape::Underline,
                VteCursorShape::Beam => CursorShape::Bar,
                // The enum is non-exhaustive from our side; a block is the
                // least surprising thing to draw for anything new.
                _ => CursorShape::Block,
            },
        };

        ScreenState {
            seq,
            rows,
            cols,
            cells,
            cursor,
            modes: self.modes(),
            title: self.title.clone(),
            bell: self.bell,
            scrollback_len: self.scrollback_len,
        }
    }

    /// Scrollback lines `[from, to)` as rendered cell rows.
    ///
    /// Scrollback is native and O(1): `Line` is signed and negative reaches
    /// history, with `Line(-1)` the most recently scrolled-off line. Nothing
    /// here mutates the viewport.
    pub fn scrollback(&self, from: u64, to: u64) -> Vec<Vec<Cell>> {
        let history = self.term.history_size() as u64;
        let mut out = Vec::new();
        if history == 0 || from >= to {
            return out;
        }

        // `from` counts from the OLDEST line the client knows about, while
        // the grid counts backwards from the newest. Anything older than the
        // ring still holds is simply gone.
        let oldest = self.scrollback_len.saturating_sub(history);
        for want in from..to {
            if want < oldest || want >= self.scrollback_len {
                continue;
            }
            let back = self.scrollback_len - want; // 1 == most recent
            let line = Line(-(back as i32));
            let mut row = Vec::with_capacity(self.size.cols as usize);
            for col in 0..self.size.cols {
                let point = Point::new(line, Column(col as usize));
                row.push(match cell_at(&self.term, point) {
                    Some(c) => self.convert(c, point),
                    None => Cell::blank(),
                });
            }
            out.push(row);
        }
        out
    }

    pub fn child_exited(&mut self) -> Option<i32> {
        if self.exited.is_none() {
            self.exited = self.pty.child_exited();
        }
        self.exited
    }

    /// The size the emulator is currently at.
    pub fn size(&self) -> TermSize {
        self.size
    }

    /// The child's process id. Only the tests ask, to prove the child really
    /// is killed when the terminal is dropped.
    #[cfg(test)]
    pub fn child_pid(&self) -> u32 {
        self.pty.child_pid()
    }

    fn modes(&self) -> Modes {
        let m = self.term.mode();
        Modes {
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            mouse: mouse_mode(m),
            app_cursor: m.contains(TermMode::APP_CURSOR),
            app_keypad: m.contains(TermMode::APP_KEYPAD),
        }
    }

    fn convert(&self, cell: &VteCell, point: Point) -> Cell {
        Cell {
            text: cell_text(cell),
            fg: to_proto_color(cell.fg, self.term.colors(), &self.palette),
            bg: to_proto_color(cell.bg, self.term.colors(), &self.palette),
            attrs: attrs_of(cell.flags) | blink_of(&self.blink, point),
        }
    }
}

fn blink_of(blink: &BlinkPlane, point: Point) -> Attrs {
    if blink.is_blinking(point) {
        Attrs::BLINK
    } else {
        Attrs::empty()
    }
}

/// The text of one cell, combining marks included.
///
/// Combining marks are **not** separate cells in `alacritty_terminal`: they
/// hang off the base cell in `zerowidth()`. Dropping them would turn an
/// accented character into a bare one, and treating them as cells would shift
/// the whole row.
fn cell_text(cell: &VteCell) -> CellText {
    // The right-hand half of a wide character carries a space in `c`. Sending
    // that space would make a renderer paint one and shift the row, so the
    // continuation cell carries no text at all - `Attrs::WIDE_CONT` says what
    // it is.
    if cell.flags.contains(Flags::WIDE_CHAR_SPACER)
        || cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return CellText::const_new("");
    }

    match cell.zerowidth() {
        Some(marks) if !marks.is_empty() => {
            let mut text = CellText::new(cell.c.to_string());
            for m in marks {
                text.push(*m);
            }
            text
        }
        _ => {
            let mut buf = [0u8; 4];
            CellText::new(cell.c.encode_utf8(&mut buf))
        }
    }
}

fn attrs_of(flags: Flags) -> Attrs {
    let mut a = Attrs::empty();
    if flags.contains(Flags::BOLD) {
        a |= Attrs::BOLD;
    }
    if flags.contains(Flags::ITALIC) {
        a |= Attrs::ITALIC;
    }
    if flags.contains(Flags::INVERSE) {
        a |= Attrs::INVERSE;
    }
    if flags.contains(Flags::DIM) {
        a |= Attrs::DIM;
    }
    // Native flags, unlike blink.
    if flags.contains(Flags::HIDDEN) {
        a |= Attrs::HIDDEN;
    }
    if flags.contains(Flags::STRIKEOUT) {
        a |= Attrs::STRIKE;
    }
    // v1 flattens all five underline styles onto one. Styles and per-cell
    // underline colour (SGR 58/59) are a later milestone; drawing a curly
    // underline as a straight one is a smaller lie than drawing none.
    if flags.intersects(Flags::ALL_UNDERLINES) {
        a |= Attrs::UNDERLINE;
    }
    if flags.contains(Flags::WIDE_CHAR_SPACER) || flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
        a |= Attrs::WIDE_CONT;
    }
    a
}

fn mouse_mode(m: &TermMode) -> MouseMode {
    // Most specific first: the modes are cumulative, and reporting the least
    // specific one would drop motion events the application asked for.
    if m.contains(TermMode::MOUSE_MOTION) {
        MouseMode::AnyMotion
    } else if m.contains(TermMode::MOUSE_DRAG) {
        MouseMode::ButtonMotion
    } else if m.contains(TermMode::MOUSE_REPORT_CLICK) {
        MouseMode::PressRelease
    } else {
        MouseMode::Off
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn size() -> TermSize {
        TermSize { cols: 40, rows: 10 }
    }

    fn sh(script: &str, size: TermSize) -> HostTerm {
        HostTerm::spawn(
            "/bin/sh",
            &["-c".to_owned(), script.to_owned()],
            &[],
            size,
            200,
        )
        .expect("spawn")
    }

    /// Poll until `f` is satisfied, or give up. There is a real process on
    /// the other end, so nothing here may assume it has already run.
    fn poll_until(t: &mut HostTerm, budget: Duration, f: impl Fn(&HostTerm) -> bool) -> bool {
        let deadline = Instant::now() + budget;
        while Instant::now() < deadline {
            t.poll().expect("poll");
            if f(t) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    fn row_text(s: &ScreenState, row: u16) -> String {
        s.row(row)
            .iter()
            .map(|c| c.text.as_str())
            .collect::<String>()
    }

    #[test]
    fn output_from_the_child_lands_on_the_screen() {
        let mut t = sh("printf hello", size());
        assert!(
            poll_until(&mut t, Duration::from_secs(10), |t| {
                row_text(&t.snapshot(1), 0).starts_with("hello")
            }),
            "got {:?}",
            row_text(&t.snapshot(1), 0)
        );
    }

    #[test]
    fn a_snapshot_always_satisfies_the_state_invariants() {
        // Everything downstream assumes this. If snapshot could produce a
        // state that fails validate(), the sync engine would reject the
        // host's own screen.
        let mut t = sh("printf 'one\\ntwo\\nthree\\n'", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            row_text(&t.snapshot(1), 0).starts_with("one")
        });
        for seq in 1..4u64 {
            let s = t.snapshot(seq);
            assert_eq!(s.validate(), Ok(()), "seq {seq}");
            assert_eq!(s.cells.len(), size().rows as usize * size().cols as usize);
            assert_eq!(s.seq, seq);
        }
    }

    #[test]
    fn the_sequence_number_is_whatever_the_caller_asked_for() {
        let t = sh("exit 0", size());
        assert_eq!(t.snapshot(42).seq, 42);
    }

    #[test]
    fn a_title_reaches_the_snapshot() {
        // OSC 2. It arrives as an Event, not as grid content.
        let mut t = sh("printf '\\033]2;oxutrm-title\\007'; sleep 5", size());
        assert!(
            poll_until(&mut t, Duration::from_secs(10), |t| t.snapshot(1).title
                == "oxutrm-title"),
            "got {:?}",
            t.snapshot(1).title
        );
    }

    #[test]
    fn there_is_no_icon_because_vte_drops_osc_1() {
        // OSC 1 has no arm in osc_dispatch and no Handler method. Setting it
        // must change nothing at all - which is why ScreenState has no field
        // for it to change.
        let mut t = sh("printf '\\033]1;an-icon\\007'; printf x; sleep 5", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            row_text(&t.snapshot(1), 0).starts_with('x')
        });
        assert_eq!(
            t.snapshot(1).title,
            "",
            "OSC 1 must not have set the title either"
        );
    }

    #[test]
    fn the_bell_counts_up_and_never_resets() {
        let mut t = sh("printf '\\a\\a\\a'; sleep 5", size());
        assert!(
            poll_until(&mut t, Duration::from_secs(10), |t| t.snapshot(1).bell >= 3),
            "got {}",
            t.snapshot(1).bell
        );
        let after = t.snapshot(1).bell;
        // A poll with nothing new must not lose the count.
        t.poll().expect("poll");
        assert_eq!(
            t.snapshot(1).bell,
            after,
            "the bell is a counter, not a flag"
        );
    }

    #[test]
    fn scrollback_length_keeps_climbing_past_the_ring_capacity() {
        // history_size() SATURATES, so a naive read would stop counting once
        // the ring filled. The count is accumulated per advance instead.
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "i=0; while [ $i -lt 120 ]; do echo line$i; i=$((i+1)); done; sleep 5".to_owned(),
            ],
            &[],
            TermSize { cols: 40, rows: 4 },
            16, // a deliberately tiny ring
        )
        .expect("spawn");

        assert!(
            poll_until(&mut t, Duration::from_secs(15), |t| t
                .snapshot(1)
                .scrollback_len
                > 40),
            "got {}",
            t.snapshot(1).scrollback_len
        );
        let len = t.snapshot(1).scrollback_len;
        assert!(
            len > 16,
            "scrollback_len {len} did not exceed the 16-line ring, so it is just history_size()"
        );
    }

    #[test]
    fn scrollback_lines_come_back_and_are_the_right_width() {
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "i=0; while [ $i -lt 30 ]; do echo row$i; i=$((i+1)); done; sleep 5".to_owned(),
            ],
            &[],
            TermSize { cols: 20, rows: 4 },
            200,
        )
        .expect("spawn");
        poll_until(&mut t, Duration::from_secs(15), |t| {
            t.snapshot(1).scrollback_len >= 20
        });

        let total = t.snapshot(1).scrollback_len;
        let lines = t.scrollback(total.saturating_sub(5), total);
        assert!(!lines.is_empty(), "expected some history back");
        for line in &lines {
            assert_eq!(line.len(), 20, "a scrollback row is a full screen width");
        }
    }

    #[test]
    fn asking_for_scrollback_that_does_not_exist_returns_nothing() {
        let t = sh("sleep 5", size());
        assert!(t.scrollback(0, 10).is_empty(), "nothing has scrolled yet");
        assert!(t.scrollback(5, 5).is_empty(), "an empty range is empty");
        assert!(
            t.scrollback(9, 3).is_empty(),
            "a reversed range is not a panic"
        );
        assert!(t.scrollback(u64::MAX - 1, u64::MAX).is_empty());
    }

    #[test]
    fn a_resize_changes_the_snapshot_and_keeps_it_valid() {
        let mut t = sh("sleep 5", size());
        t.poll().expect("poll");
        assert_eq!(t.snapshot(1).cols, 40);

        let bigger = TermSize {
            cols: 100,
            rows: 30,
        };
        t.resize(bigger).expect("resize");
        let s = t.snapshot(2);
        assert_eq!((s.cols, s.rows), (100, 30));
        assert_eq!(s.cells.len(), 3_000);
        assert_eq!(s.validate(), Ok(()));
        assert_eq!(t.size(), bigger);

        // And back down again: resize reflows losslessly in both directions.
        t.resize(size()).expect("resize");
        let s = t.snapshot(3);
        assert_eq!((s.cols, s.rows), (40, 10));
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_wide_character_occupies_two_cells_and_the_second_is_marked() {
        // The continuation cell must NOT be a space: a renderer that painted
        // one would shift every column after it.
        let mut t = sh("printf '\\344\\275\\240x'; sleep 5", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).cell(0, 0).text == "\u{4f60}"
        });
        let s = t.snapshot(1);
        assert_eq!(s.cell(0, 0).text, "\u{4f60}");
        assert!(!s.cell(0, 0).attrs.contains(Attrs::WIDE_CONT));
        assert!(
            s.cell(0, 1).attrs.contains(Attrs::WIDE_CONT),
            "the right half must be flagged"
        );
        assert_ne!(s.cell(0, 1).text, " ", "and must not be a space");
        assert_eq!(
            s.cell(0, 2).text,
            "x",
            "the next character starts at column 2"
        );
    }

    #[test]
    fn a_combining_mark_stays_in_the_cell_it_belongs_to() {
        // Combining marks are not separate cells in alacritty_terminal: they
        // hang off the base cell. Treating them as cells would shift the row;
        // dropping them would turn e-acute into a bare e.
        let mut t = sh("printf 'e\\314\\201z'; sleep 5", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).cell(0, 1).text == "z"
        });
        let s = t.snapshot(1);
        assert_eq!(
            s.cell(0, 0).text,
            "e\u{0301}",
            "base plus combining acute, one cell"
        );
        assert_eq!(
            s.cell(0, 1).text,
            "z",
            "and the next character did not shift"
        );
    }

    #[test]
    fn the_native_attributes_arrive() {
        // Bold, italic, underline, inverse, strike and hidden are all native
        // flags in alacritty_terminal.
        let mut t = sh("printf '\\033[1;3;4;7;9;8mA\\033[0mB'; sleep 5", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).cell(0, 1).text == "B"
        });
        let s = t.snapshot(1);
        let a = s.cell(0, 0).attrs;
        for (flag, name) in [
            (Attrs::BOLD, "bold"),
            (Attrs::ITALIC, "italic"),
            (Attrs::UNDERLINE, "underline"),
            (Attrs::INVERSE, "inverse"),
            (Attrs::STRIKE, "strike"),
            (Attrs::HIDDEN, "hidden"),
        ] {
            assert!(a.contains(flag), "{name} missing from {a:?}");
        }
        assert_eq!(s.cell(0, 1).attrs, Attrs::empty(), "SGR 0 reset everything");
    }

    #[test]
    fn blink_is_recovered_even_though_the_emulator_discards_it() {
        // vte parses SGR 5 into Attr::BlinkSlow and Term::terminal_attribute
        // throws it away. Without the BlinkTap newtype this attribute would
        // be unreachable.
        let mut t = sh("printf '\\033[5mA\\033[25mB'; sleep 5", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).cell(0, 1).text == "B"
        });
        let s = t.snapshot(1);
        assert!(
            s.cell(0, 0).attrs.contains(Attrs::BLINK),
            "SGR 5 must set blink"
        );
        assert!(
            !s.cell(0, 1).attrs.contains(Attrs::BLINK),
            "SGR 25 must cancel it again"
        );
    }

    #[test]
    fn every_underline_style_flattens_onto_one_attribute() {
        // v1 maps all five. Drawing a curly underline as a straight one is a
        // smaller lie than drawing none at all.
        // NOT 21: vte maps that to Attr::CancelBold. ECMA-48 says doubly
        // underlined, but the "bold off" reading won long ago and vte follows
        // it. Double underline is 4:2.
        for sgr in ["4", "4:2", "4:3", "4:4", "4:5"] {
            let mut t = sh(&format!("printf '\\033[{sgr}mU'; sleep 5"), size());
            poll_until(&mut t, Duration::from_secs(10), |t| {
                t.snapshot(1).cell(0, 0).text == "U"
            });
            assert!(
                t.snapshot(1).cell(0, 0).attrs.contains(Attrs::UNDERLINE),
                "SGR {sgr} did not produce an underline"
            );
        }
    }

    #[test]
    fn colours_arrive_as_indices_and_as_rgb() {
        let mut t = sh(
            "printf '\\033[31mR\\033[38;2;1;2;3mT\\033[0mZ'; sleep 5",
            size(),
        );
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).cell(0, 2).text == "Z"
        });
        let s = t.snapshot(1);
        assert_eq!(
            s.cell(0, 0).fg,
            oxutrm_proto::Color::Idx(1),
            "SGR 31 is palette red"
        );
        assert_eq!(s.cell(0, 1).fg, oxutrm_proto::Color::Rgb(1, 2, 3));
        assert_eq!(
            s.cell(0, 2).fg,
            oxutrm_proto::Color::Default,
            "reset leaves the client its own theme"
        );
    }

    #[test]
    fn modes_are_reported() {
        let mut t = sh(
            "printf '\\033[?2004h\\033[?1h\\033[?1000h'; printf m; sleep 5",
            size(),
        );
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).modes.bracketed_paste
        });
        let m = t.snapshot(1).modes;
        assert!(m.bracketed_paste, "DECSET 2004");
        assert!(m.app_cursor, "DECSET 1");
        assert_eq!(m.mouse, MouseMode::PressRelease, "DECSET 1000");
        assert!(!m.alt_screen);
    }

    #[test]
    fn the_alternate_screen_is_reported() {
        let mut t = sh("printf '\\033[?1049h'; printf a; sleep 5", size());
        assert!(
            poll_until(&mut t, Duration::from_secs(10), |t| t
                .snapshot(1)
                .modes
                .alt_screen),
            "DECSET 1049 must show as alt_screen"
        );
    }

    #[test]
    fn the_cursor_is_inside_the_screen_and_visible_by_default() {
        let mut t = sh("printf abc; sleep 5", size());
        poll_until(&mut t, Duration::from_secs(10), |t| {
            t.snapshot(1).cursor.col > 0
        });
        let s = t.snapshot(1);
        assert!(s.cursor.row < s.rows && s.cursor.col < s.cols);
        assert!(s.cursor.visible);
        assert_eq!(
            s.validate(),
            Ok(()),
            "an out-of-range cursor would fail here"
        );
    }

    #[test]
    fn a_hidden_cursor_is_reported_hidden() {
        let mut t = sh("printf '\\033[?25l'; printf x; sleep 5", size());
        assert!(
            poll_until(&mut t, Duration::from_secs(10), |t| !t
                .snapshot(1)
                .cursor
                .visible),
            "DECTCEM off must hide the cursor"
        );
    }

    #[test]
    fn input_reaches_the_child_and_comes_back_as_output() {
        let mut t = sh("read line; printf 'echo:%s' \"$line\"", size());
        t.write_input(b"ping\n").expect("write");
        assert!(
            poll_until(&mut t, Duration::from_secs(10), |t| {
                t.snapshot(1)
                    .cells
                    .iter()
                    .map(|c| c.text.as_str())
                    .collect::<String>()
                    .contains("echo:ping")
            }),
            "got {:?}",
            row_text(&t.snapshot(1), 0)
        );
    }

    #[test]
    fn an_exit_code_comes_back() {
        let mut t = sh("exit 5", size());
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            t.poll().expect("poll");
            if let Some(code) = t.child_exited() {
                assert_eq!(code, 5);
                break;
            }
            assert!(Instant::now() < deadline, "the child never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn poll_returns_even_when_the_child_never_stops_writing() {
        // The bug this guards is not subtle in effect: `poll` used to drain
        // until the PTY was empty, and `yes` keeps it non-empty forever, so
        // the call never returned. No frame is emitted, the screen freezes,
        // and it looks like a hang rather than a fall-behind.
        let mut t = sh("yes oxutrm-flood", size());
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_output = false;

        while Instant::now() < deadline {
            let started = Instant::now();
            t.poll().expect("poll");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "poll took {:?} against an endless writer",
                started.elapsed()
            );
            if t.snapshot(1).cells.iter().any(|c| c.text == "x") {
                saw_output = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        assert!(saw_output, "the emulator never processed any of the flood");

        // And it stays bounded once the flood is in full flow, which is when
        // the old implementation would never have returned at all.
        for turn in 0..20 {
            let started = Instant::now();
            t.poll().expect("poll");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "poll took {:?} on turn {turn}",
                started.elapsed()
            );
        }
    }

    #[test]
    fn dropping_the_terminal_kills_the_child() {
        // std's Child does not kill on drop. Without an explicit kill, every
        // abandoned session leaves a shell holding a pty nobody reads.
        let t = sh("yes oxutrm-orphan", size());
        let pid = t.child_pid();
        assert!(
            std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the child should be running"
        );
        drop(t);

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the child {pid} outlived the HostTerm that owned it");
    }

    #[test]
    fn a_quiet_terminal_polls_without_blocking() {
        let mut t = sh("sleep 5", size());
        let started = Instant::now();
        for _ in 0..5 {
            t.poll().expect("poll");
        }
        assert!(started.elapsed() < Duration::from_secs(1), "poll blocked");
    }
}

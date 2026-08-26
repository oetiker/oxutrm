//! The host's terminal: a PTY, the emulator reading it, and the snapshot
//! everything downstream replicates.

use alacritty_terminal::Term;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell as VteCell, Flags};
use alacritty_terminal::term::{Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as VteCursorShape, Processor, Rgb};

use oxutrm_proto::{
    Attrs, Cell, CellText, Cursor, CursorShape, MAX_CELL_TEXT, Modes, MouseMode, ScreenState,
    TermSize, fit_cell_text, fit_title, is_control_scalar,
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
        // Before the PTY, not after: `size` came from the client and this is
        // the host. Spawning a shell and then discovering the geometry was
        // hostile means a process to clean up as well as an error to report.
        let dims = GridSize::new(size, scrollback)?;
        let pty = Pty::spawn(shell, args, env, size)?;
        let events = EventSink::new();
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
        // I7 first, and before the ioctl: a refused resize must leave the PTY
        // and the emulator agreeing with each other about the old size.
        let dims = GridSize::new(size, self.history)?;
        self.pty.resize(size)?;
        self.term.resize(dims);
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
        screen_state_of(
            &self.term,
            &self.blink,
            &self.palette,
            self.size,
            StateMeta {
                seq,
                title: self.title.clone(),
                bell: self.bell,
                scrollback_len: self.scrollback_len,
            },
        )
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
                    Some(c) => {
                        convert_cell(c, point, &self.blink, self.term.colors(), &self.palette)
                    }
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
}

/// Convert what the emulator holds into a [`ScreenState`].
///
/// A free function rather than a method so that emulation fidelity can be
/// tested against a bare `Term` - no PTY, no child process, no timing. That
/// matters: the golden tests are the only thing standing between us and a
/// silent change in how the emulator renders, and they should not depend on
/// a shell starting fast enough.
pub(crate) struct StateMeta {
    pub seq: u64,
    pub title: String,
    pub bell: u32,
    pub scrollback_len: u64,
}

pub(crate) fn screen_state_of<T: alacritty_terminal::event::EventListener>(
    term: &Term<T>,
    blink: &BlinkPlane,
    palette: &[Rgb; PALETTE_LEN],
    size: TermSize,
    meta: StateMeta,
) -> ScreenState {
    let rows = size.rows;
    let cols = size.cols;
    let mut cells = Vec::with_capacity(rows as usize * cols as usize);

    for row in 0..rows {
        for col in 0..cols {
            let point = Point::new(Line(row as i32), Column(col as usize));
            cells.push(match cell_at(term, point) {
                Some(c) => convert_cell(c, point, blink, term.colors(), palette),
                // Outside the emulator's grid: the sizes disagree, which can
                // happen for one poll after a resize. A blank keeps the length
                // invariant, which matters more than the cell.
                None => Cell::blank(),
            });
        }
    }

    let cursor_point = term.grid().cursor.point;
    let cursor = Cursor {
        row: cursor_point.line.0.clamp(0, rows.saturating_sub(1) as i32) as u16,
        col: (cursor_point.column.0 as u16).min(cols.saturating_sub(1)),
        visible: term.mode().contains(TermMode::SHOW_CURSOR),
        shape: match term.cursor_style().shape {
            VteCursorShape::Block | VteCursorShape::HollowBlock => CursorShape::Block,
            VteCursorShape::Underline => CursorShape::Underline,
            VteCursorShape::Beam => CursorShape::Bar,
            _ => CursorShape::Block,
        },
    };

    ScreenState {
        seq: meta.seq,
        rows,
        cols,
        cells,
        cursor,
        modes: modes_of(term.mode()),
        // I8, maintained rather than checked — same reasoning as `cell_text`,
        // and here the input is even less under our control: the title is the
        // payload of an OSC 0/2 written by whatever program the user ran, and
        // `alacritty_terminal` hands it over unexamined. Fitted at the point
        // the state is BUILT rather than where `HostTerm` records it, so that
        // every path that constructs a `ScreenState` in this crate — the
        // golden harness included — inherits the guarantee.
        title: fit_title(meta.title),
        bell: meta.bell,
        scrollback_len: meta.scrollback_len,
    }
}

pub(crate) fn convert_cell(
    cell: &VteCell,
    point: Point,
    blink: &BlinkPlane,
    overrides: &alacritty_terminal::term::color::Colors,
    palette: &[Rgb; PALETTE_LEN],
) -> Cell {
    Cell {
        text: cell_text(cell),
        fg: to_proto_color(cell.fg, overrides, palette),
        bg: to_proto_color(cell.bg, overrides, palette),
        attrs: attrs_of(cell.flags) | blink_of(blink, point),
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
///
/// # This is where I8 is *maintained*, not merely checked
///
/// Every consumer of a [`ScreenState`] rejects a cell that breaks I8, and a
/// rejection is the right answer there — the peer is not trusted. Here it
/// would be the wrong answer twice over. Sanitising on the host does nothing
/// for the client's safety: a hostile host simply would not run this code, so
/// the client's own check is what protects the user. What this buys is
/// **liveness**. A host that emits a state its own peer refuses is a session
/// that freezes with no log line explaining why, and `alacritty_terminal` can
/// genuinely produce such a state: it puts **no cap at all** on how many
/// zero-width marks it stacks onto one base character (`Term::input` pushes
/// every width-0 scalar onto the previous cell, unbounded), so a program that
/// prints combining marks in a loop grows one cell without limit. That is not
/// hypothetical hostility — it is what `cat` of a file full of stacked
/// diacritics does.
///
/// So the bound is applied at the source, and applied while the string is
/// being built rather than after: filtering a megabyte of marks down to 32
/// bytes still costs the megabyte if it was assembled first.
/// [`fit_cell_text`] then backstops the result, which is what makes the
/// property total rather than merely likely — including for `cell.c` itself,
/// which `alacritty_terminal` never sets to a control today.
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

    let text = match cell.zerowidth() {
        Some(marks) if !marks.is_empty() => {
            let mut text = CellText::new(cell.c.to_string());
            for m in marks {
                // Stop at the cap instead of assembling the whole stack and
                // trimming it afterwards. `break`, not `continue`: the marks
                // are ordered, and keeping a later one after dropping an
                // earlier one would reorder the cluster.
                if is_control_scalar(*m) || text.len() + m.len_utf8() > MAX_CELL_TEXT {
                    break;
                }
                text.push(*m);
            }
            text
        }
        _ => {
            let mut buf = [0u8; 4];
            CellText::new(cell.c.encode_utf8(&mut buf))
        }
    };
    fit_cell_text(text)
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

fn modes_of(m: &TermMode) -> Modes {
    Modes {
        alt_screen: m.contains(TermMode::ALT_SCREEN),
        bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
        mouse: mouse_mode(m),
        app_cursor: m.contains(TermMode::APP_CURSOR),
        app_keypad: m.contains(TermMode::APP_KEYPAD),
    }
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

    /// One emulator cell holding `base` with `marks` stacked on it, exactly as
    /// `Term::input` builds one.
    fn vte_cell(base: char, marks: &[char]) -> VteCell {
        let mut cell = VteCell {
            c: base,
            ..VteCell::default()
        };
        for m in marks {
            cell.push_zerowidth(*m);
        }
        cell
    }

    /// **I8 is MAINTAINED here, and it has to be — an honest host really can
    /// produce a cell the peer would reject.**
    ///
    /// `alacritty_terminal` puts no cap on `zerowidth()`: `Term::input` pushes
    /// every width-0 scalar onto the previous cell, forever. So a program that
    /// prints combining marks in a loop — a `cat` of stacked diacritics, not
    /// an attack — grows one cell without limit, and if the host shipped that
    /// state the client would refuse the frame and the session would freeze
    /// with nothing in any log to explain it. Fitting at the source is what
    /// keeps a rejection on the client an event that only a hostile host can
    /// cause.
    #[test]
    fn a_cell_the_emulator_can_over_fill_is_fitted_at_the_source() {
        let cell = vte_cell('e', &['\u{301}'; 500]);
        // The emulator held 1001 bytes in one cell, with no complaint.
        assert_eq!(cell.zerowidth().expect("marks").len(), 500);

        let text = cell_text(&cell);
        assert!(
            text.len() <= MAX_CELL_TEXT,
            "the host must not emit {} bytes in one cell",
            text.len()
        );
        oxutrm_proto::check_cell_text(&text).expect("what the host emits must be acceptable");
        assert!(
            text.starts_with('e'),
            "the base character is what must survive: {text:?}"
        );
    }

    /// The over-correction guard for the same code path. Fitting must be
    /// invisible to every cell that was already legal — which is all of them.
    #[test]
    fn a_real_grapheme_cluster_passes_through_the_host_untouched() {
        let clusters: [(char, &[char]); 6] = [
            ('e', &['\u{301}']),
            ('e', &['\u{302}', '\u{301}']),
            ('\u{5d0}', &['\u{5b8}', '\u{5bc}', '\u{591}']),
            ('\u{f40}', &['\u{f90}', '\u{fb5}', '\u{f72}']),
            ('🦀', &['\u{fe0f}']),
            ('1', &['\u{fe0f}', '\u{20e3}']),
        ];
        for (base, marks) in clusters {
            let mut want = String::from(base);
            want.extend(marks.iter());
            assert_eq!(
                cell_text(&vte_cell(base, marks)).as_str(),
                want,
                "a real cluster must survive intact"
            );
        }
    }

    /// A plain character is the overwhelming majority of every screen, and it
    /// must come through the fitting untouched as well.
    #[test]
    fn ordinary_characters_are_unchanged_by_the_fitting() {
        for c in ['x', ' ', 'é', '\u{4f60}', '🦀'] {
            assert_eq!(cell_text(&vte_cell(c, &[])).as_str(), c.to_string());
        }
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

    /// **I7 on the host side, which is the side that matters.**
    ///
    /// The size in a resize is chosen by the CLIENT and handed to
    /// `Term::resize`, which allocates `(rows + history) * cols` emulator
    /// cells with no bound of its own. History multiplies it, so this is the
    /// same memory bomb as the resize arm of `ScreenState::apply` and strictly
    /// worse: a client that has merely connected can ask a host for hundreds
    /// of gigabytes.
    ///
    /// 1024x1024 is over the cap and small enough to run this test RED
    /// without taking the machine down; it was, and red it resizes happily.
    #[test]
    fn a_client_cannot_resize_the_host_beyond_the_cap() {
        let mut t = sh("sleep 5", size());
        let err = t
            .resize(TermSize {
                cols: 1024,
                rows: 1024,
            })
            .expect_err("a screen that large must be refused");
        assert!(
            err.to_string().contains("exceeds the maximum"),
            "expected the I7 error, got: {err}"
        );
        assert_eq!(
            t.size(),
            size(),
            "a refused resize must leave the PTY and the emulator agreeing"
        );
        // And the session is still usable — a rejected resize is not fatal.
        t.resize(TermSize { cols: 60, rows: 20 })
            .expect("an ordinary resize still works");
        assert_eq!(t.size(), TermSize { cols: 60, rows: 20 });
    }

    /// The spawn path takes the client's size too, from `ClientHello`.
    #[test]
    fn a_host_terminal_cannot_be_spawned_beyond_the_cap() {
        let got = HostTerm::spawn(
            "/bin/sh",
            &["-c".to_owned(), "true".to_owned()],
            &[],
            TermSize {
                cols: u16::MAX,
                rows: u16::MAX,
            },
            200,
        );
        let err = got.err().expect("a screen that large must be refused");
        assert!(
            err.to_string().contains("exceeds the maximum"),
            "expected the I7 error, got: {err}"
        );
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

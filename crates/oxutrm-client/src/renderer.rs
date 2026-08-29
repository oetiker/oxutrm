//! The **second** diff.
//!
//! The sync engine already diffed host state against host state; that answered
//! "what changed on the remote screen". This answers a different question:
//! given what is *currently painted on the user's physical terminal*, what is
//! the least ANSI that makes it match the desired [`ScreenState`]?
//!
//! Keeping that model of the painted screen is what makes local scrollback and
//! a locally drawn status pane possible later: they can be painted over the
//! screen and then undone with [`Renderer::invalidate`], without the host ever
//! learning that anything happened.

use std::io::Write;

use oxutrm_proto::{
    Attrs, Cell, Color, Cursor, CursorShape, Modes, MouseMode, ScreenState, TermSize, TerminalCaps,
};

use crate::color::down_convert;
use crate::overlay::Overlay;

/// What the terminal is believed to be showing right now.
struct Painted {
    cells: Vec<Cell>,
    cursor: Cursor,
    modes: Modes,
    title: String,
    bell: u32,
}

pub struct Renderer {
    size: TermSize,
    caps: TerminalCaps,
    /// `None` means nothing is known about the terminal, so the next render
    /// repaints everything.
    painted: Option<Painted>,
    /// Layer 1: locally drawn UI composited over the remote framebuffer.
    ///
    /// Held here rather than passed to `render` so that setting it is what a
    /// caller does, and painting stays one call. It is composited into a local
    /// cell buffer and **never** into a `ScreenState`: the authoritative state
    /// is what the sync engine acks and diffs, and layer 1 must not reach it.
    overlay: Option<Overlay>,
}

impl Renderer {
    pub fn new(size: TermSize, caps: TerminalCaps) -> Renderer {
        Renderer {
            size,
            caps,
            painted: None,
            overlay: None,
        }
    }

    /// The user's window changed size. Nothing painted survives that, so the
    /// model is dropped rather than reshaped.
    pub fn resize(&mut self, size: TermSize) {
        self.size = size;
        self.painted = None;
    }

    /// Forget what is painted; the next render repaints everything.
    pub fn invalidate(&mut self) {
        self.painted = None;
    }

    /// Put local UI over the screen, or take it away.
    ///
    /// Deliberately **not** paired with `invalidate`. Both painting and
    /// removing the overlay are ordinary diffs against the model, which is the
    /// whole reason layer 1 goes through the renderer instead of being written
    /// to the terminal directly -- see `ClientSession::announce`, which has to
    /// invalidate precisely because it writes outside the model.
    pub fn set_overlay(&mut self, overlay: Option<Overlay>) {
        self.overlay = overlay;
    }

    pub fn size(&self) -> TermSize {
        self.size
    }

    /// Reconcile the terminal with `s`, writing the minimal ANSI to `w`.
    pub fn render<W: Write>(&mut self, w: &mut W, s: &ScreenState) -> std::io::Result<()> {
        let mut out: Vec<u8> = Vec::new();

        // A state of a different shape than the terminal cannot be painted
        // incrementally: every column index would mean something else.
        let shape_changed = s.rows != self.size.rows || s.cols != self.size.cols;
        if shape_changed {
            self.size = TermSize {
                cols: s.cols,
                rows: s.rows,
            };
            self.painted = None;
        }

        let mut full = self.painted.is_none();

        // `\x1b[?1049h`/`l` swaps the physical screen buffer. Everything the
        // model claims is painted belongs to the buffer being left, so from
        // here on there is nothing to diff against: the bell is read before
        // the model is dropped, and every later pass runs as a full repaint.
        let buffer_swapped = self
            .painted
            .as_ref()
            .is_some_and(|p| p.modes.alt_screen != s.modes.alt_screen);

        // Read before the model can be dropped below, emitted after everything
        // else: a bell is not worth a cursor move, and `write_cursor` restates
        // the position whenever anything at all was emitted before it.
        let ring = self.should_ring(s);

        // Modes go first, because the swap has to happen before the repaint
        // that fills the buffer it swapped to.
        self.write_modes(&mut out, s, full);
        if buffer_swapped {
            self.painted = None;
            full = true;
        }

        self.write_title(&mut out, s, full);

        // Layer 1 is stamped into a copy of the cells, and the cursor is
        // hidden under it: a caret sitting inside a drawn box reads as a bug.
        // `Cow` so a session with no overlay -- which is every healthy session
        // -- clones nothing.
        let cells = self.composite(s);
        let cursor = match self.overlay {
            None => s.cursor,
            Some(_) => Cursor {
                visible: false,
                ..s.cursor
            },
        };

        self.write_cells(&mut out, &cells, s.rows, s.cols, full);
        self.write_cursor(&mut out, cursor, full);
        if ring {
            out.push(0x07);
        }

        // Synchronized output. The terminal shows the whole repaint at once
        // instead of mid-tear, which matters most where layer 1 paints a box
        // over live content.
        //
        // Unconditional, and that is not laziness: a conforming terminal
        // ignores a private mode it does not know, so there is nothing to
        // detect, nothing to negotiate and no capability to carry. Guarded only
        // on emptiness, because a render that changes nothing must write
        // nothing -- otherwise every quiet pacing tick costs two escape
        // sequences.
        if !out.is_empty() {
            let mut wrapped = Vec::with_capacity(out.len() + 16);
            wrapped.extend_from_slice(b"\x1b[?2026h");
            wrapped.append(&mut out);
            wrapped.extend_from_slice(b"\x1b[?2026l");
            out = wrapped;
        }

        // Only a write that completed makes the model true. Committing it
        // before the bytes are out would leave every later diff computed
        // against a screen that was never painted, with no way back — and a
        // rejected frame must cost a repaint, never a session.
        match w.write_all(&out) {
            Ok(()) => {
                self.painted = Some(Painted {
                    cells: cells.into_owned(),
                    cursor,
                    modes: s.modes,
                    title: s.title.clone(),
                    bell: s.bell,
                });
                Ok(())
            }
            Err(e) => {
                self.painted = None;
                Err(e)
            }
        }
    }

    /// The screen with layer 1 stamped on top, clipped to the screen.
    ///
    /// Clipping rather than asserting: a window can shrink between a notice
    /// being laid out and being painted, and a resize is not a reason to panic
    /// in the middle of a repaint.
    fn composite<'a>(&self, s: &'a ScreenState) -> std::borrow::Cow<'a, [Cell]> {
        let Some(o) = self.overlay.as_ref() else {
            return std::borrow::Cow::Borrowed(&s.cells);
        };

        let mut cells = s.cells.clone();
        let cols = s.cols as usize;
        for r in 0..o.rows {
            let screen_row = o.row.saturating_add(r);
            if screen_row >= s.rows {
                break;
            }
            for c in 0..o.cols {
                let screen_col = o.col.saturating_add(c);
                if screen_col >= s.cols {
                    break;
                }
                let from = r as usize * o.cols as usize + c as usize;
                let to = screen_row as usize * cols + screen_col as usize;
                cells[to] = o.cells[from].clone();
            }
            repair_wide_pairs(&mut cells, cols, screen_row as usize);
        }
        std::borrow::Cow::Owned(cells)
    }

    // ---- modes -------------------------------------------------------------

    /// On a full repaint every mode is *stated*, in whichever direction.
    ///
    /// "Nothing is known about the terminal" is not "the terminal is in the
    /// default state": emitting only the modes that want turning on is what
    /// left a resized-then-quit vim behind on the alternate buffer, still
    /// reporting the mouse, with the user's shell unusable. A mode we cannot
    /// vouch for has to be said out loud, and `l` is as much a statement as
    /// `h`.
    fn write_modes(&self, out: &mut Vec<u8>, s: &ScreenState, full: bool) {
        let before = self.painted.as_ref().map(|p| p.modes);

        let changed = |f: fn(&Modes) -> bool| match before {
            Some(b) if !full => f(&s.modes) != f(&b),
            _ => true,
        };

        if changed(|m| m.alt_screen) {
            out.extend_from_slice(if s.modes.alt_screen {
                b"\x1b[?1049h"
            } else {
                b"\x1b[?1049l"
            });
        }
        // Bracketed paste is only offered if this terminal understands it;
        // enabling it on one that does not would leak `ESC[200~` into the
        // user's input.
        if self.caps.bracketed_paste && changed(|m| m.bracketed_paste) {
            out.extend_from_slice(if s.modes.bracketed_paste {
                b"\x1b[?2004h"
            } else {
                b"\x1b[?2004l"
            });
        }

        let mouse_changed = match before {
            Some(b) if !full => s.modes.mouse != b.mouse,
            // `write_mouse` clears all three tracking modes before enabling
            // the wanted one, so stating `Off` is exactly the sequence that
            // stops a terminal nobody can vouch for from reporting.
            _ => true,
        };
        if mouse_changed {
            self.write_mouse(out, s.modes.mouse);
        }
    }

    /// The remote application asked for a particular level of mouse reporting;
    /// the local terminal is put into the matching mode so that what it sends
    /// is what that application expects to read.
    fn write_mouse(&self, out: &mut Vec<u8>, mode: MouseMode) {
        // Always clear all three tracking modes first: they are not nested,
        // and leaving an old one on would produce reports nobody asked for.
        out.extend_from_slice(b"\x1b[?1003l\x1b[?1002l\x1b[?1000l");
        let enable: &[u8] = match mode {
            MouseMode::Off => b"",
            MouseMode::Press | MouseMode::PressRelease => b"\x1b[?1000h",
            MouseMode::ButtonMotion => b"\x1b[?1002h",
            MouseMode::AnyMotion => b"\x1b[?1003h",
        };
        out.extend_from_slice(enable);
        if mode == MouseMode::Off {
            out.extend_from_slice(b"\x1b[?1006l");
        } else if self.caps.mouse_sgr {
            // SGR encoding, without which coordinates past column 223 cannot
            // be expressed at all.
            out.extend_from_slice(b"\x1b[?1006h");
        }
    }

    fn write_title(&self, out: &mut Vec<u8>, s: &ScreenState, full: bool) {
        let changed = match self.painted.as_ref() {
            Some(p) if !full => p.title != s.title,
            _ => !s.title.is_empty(),
        };
        if changed {
            out.extend_from_slice(b"\x1b]0;");
            out.extend_from_slice(s.title.as_bytes());
            out.push(0x07);
        }
    }

    // ---- cells -------------------------------------------------------------

    fn write_cells(&self, out: &mut Vec<u8>, cells: &[Cell], rows: u16, cols: u16, full: bool) {
        if full {
            out.extend_from_slice(b"\x1b[H\x1b[2J");
        }

        let cols = cols as usize;
        // The pen is tracked across the whole pass but never across passes: a
        // terminal we have not written to since is not a terminal whose SGR
        // state we can still vouch for.
        let mut pen: Option<Vec<u8>> = None;

        for row in 0..rows {
            let base = row as usize * cols;
            let desired = &cells[base..base + cols];
            let previous = self.painted.as_ref().and_then(|p| {
                if p.cells.len() == cells.len() {
                    Some(&p.cells[base..base + cols])
                } else {
                    None
                }
            });

            for (start, end) in changed_runs(desired, previous, full) {
                out.extend_from_slice(format!("\x1b[{};{}H", row + 1, start + 1).as_bytes());
                for cell in &desired[start..end] {
                    // The right-hand half of a double-width character. The
                    // preceding glyph already occupies this column; emitting
                    // anything here would shift every column after it.
                    if cell.attrs.contains(Attrs::WIDE_CONT) {
                        continue;
                    }
                    let want = self.sgr_for(cell);
                    if pen.as_deref() != Some(want.as_slice()) {
                        out.extend_from_slice(&want);
                        pen = Some(want);
                    }
                    out.extend_from_slice(cell.text.as_bytes());
                }
            }
        }

        // Leave the pen at the default, so anything drawn afterwards — a
        // status pane, the shell after we exit — starts from a known state.
        // Skipped when the last cell already left it there, which is the
        // common case for ordinary text.
        if pen.is_some_and(|p| p != b"\x1b[0m") {
            out.extend_from_slice(b"\x1b[0m");
        }
    }

    /// The full SGR sequence for a cell, colours already folded to what this
    /// terminal can show. Always reset-then-set, so one sequence fully
    /// describes the pen rather than depending on what came before.
    fn sgr_for(&self, cell: &Cell) -> Vec<u8> {
        let mut p = String::from("\x1b[0");
        let a = cell.attrs;

        let fg = down_convert(cell.fg, &self.caps);
        let bg = down_convert(cell.bg, &self.caps);

        // On a terminal with no bright half, a bright foreground is shown the
        // way it always has been: bold plus the base colour. That promotion is
        // the renderer's job, not the palette's.
        let promote_bold =
            self.caps.colors < 16 && matches!(fg, Color::Idx(i) if (8..16).contains(&i));

        if a.contains(Attrs::BOLD) || promote_bold {
            p.push_str(";1");
        }
        if a.contains(Attrs::DIM) {
            p.push_str(";2");
        }
        if a.contains(Attrs::ITALIC) {
            p.push_str(";3");
        }
        // v1 maps every underline variant onto plain underline. Styles and
        // per-cell underline colour are a later milestone.
        if a.contains(Attrs::UNDERLINE) {
            p.push_str(";4");
        }
        if a.contains(Attrs::BLINK) {
            p.push_str(";5");
        }
        if a.contains(Attrs::INVERSE) {
            p.push_str(";7");
        }
        if a.contains(Attrs::HIDDEN) {
            p.push_str(";8");
        }
        if a.contains(Attrs::STRIKE) {
            p.push_str(";9");
        }

        push_color(&mut p, fg, true, self.caps.colors);
        push_color(&mut p, bg, false, self.caps.colors);

        p.push('m');
        p.into_bytes()
    }

    // ---- cursor and bell ---------------------------------------------------

    fn write_cursor(&self, out: &mut Vec<u8>, cursor: Cursor, full: bool) {
        let before = self.painted.as_ref().map(|p| p.cursor).filter(|_| !full);

        if before.map(|b| b.shape) != Some(cursor.shape) {
            let n = match cursor.shape {
                CursorShape::Block => 2,
                CursorShape::Underline => 4,
                CursorShape::Bar => 6,
            };
            out.extend_from_slice(format!("\x1b[{n} q").as_bytes());
        }

        // Position is written whenever anything was painted, because painting
        // left the caret wherever the last glyph ended.
        let moved = before.map(|b| (b.row, b.col)) != Some((cursor.row, cursor.col));
        if moved || !out.is_empty() {
            out.extend_from_slice(
                format!("\x1b[{};{}H", cursor.row + 1, cursor.col + 1).as_bytes(),
            );
        }

        if before.map(|b| b.visible) != Some(cursor.visible) {
            out.extend_from_slice(if cursor.visible {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            });
        }
    }

    /// `bell` is a monotonic counter, so the terminal rings on an *increase*.
    ///
    /// A decrease can only mean a fresh session's state, never fifty bells
    /// undone — ringing on it would be the reset bug the counter exists to
    /// prevent. One ring per increase rather than one per unit, because a
    /// burst that scrolled past deserves one bell, not fifty.
    fn should_ring(&self, s: &ScreenState) -> bool {
        self.painted.as_ref().is_some_and(|p| s.bell > p.bell)
    }
}

fn push_color(p: &mut String, c: Color, foreground: bool, colors: u32) {
    match c {
        // Already covered by the leading reset.
        Color::Default => {}
        Color::Idx(i) if i < 8 => {
            let base = if foreground { 30 } else { 40 };
            p.push_str(&format!(";{}", base + u16::from(i)));
        }
        Color::Idx(i) if i < 16 => {
            if colors < 16 {
                // No bright half here; the bold promotion above carries it,
                // and the base colour is what actually gets emitted.
                let base = if foreground { 30 } else { 40 };
                p.push_str(&format!(";{}", base + u16::from(i - 8)));
            } else {
                let base = if foreground { 90 } else { 100 };
                p.push_str(&format!(";{}", base + u16::from(i - 8)));
            }
        }
        Color::Idx(i) => {
            let base = if foreground { 38 } else { 48 };
            p.push_str(&format!(";{base};5;{i}"));
        }
        Color::Rgb(r, g, b) => {
            let base = if foreground { 38 } else { 48 };
            p.push_str(&format!(";{base};2;{r};{g};{b}"));
        }
    }
}

/// Is this the left half of a double-width glyph?
///
/// Measured from the text, not asserted from a flag: `WIDE_CONT` marks the
/// right half and nothing marks the left, because nothing needed to until the
/// overlay arrived and started landing between the two.
fn is_wide_lead(cell: &Cell) -> bool {
    use unicode_width::UnicodeWidthStr as _;
    !cell.attrs.contains(Attrs::WIDE_CONT) && cell.text.width() == 2
}

/// Put back the invariant the overlay can break: a double-width glyph occupies
/// exactly two columns, the second of them a `WIDE_CONT`.
///
/// The overlay is a rectangle and a wide glyph is two columns, so either edge
/// of the box can land in the middle of one. Both halves of that are a stray
/// half-glyph at the box edge, on any CJK-heavy screen:
///
/// - The **left** edge can land on the continuation of a remote glyph whose
///   lead is outside the box. The lead survives, still two columns wide, so
///   the terminal paints it over the box's first column and shifts the rest
///   of the row right by one.
/// - The **right** edge can overwrite a lead whose continuation is outside the
///   box -- and so can the screen's own right edge, clipping the box's last
///   column. `write_cells` skips a `WIDE_CONT`, so nothing is ever painted
///   over that column and the right half of the old glyph stays there.
///
/// Both are the same broken pair seen from opposite sides, so the repair is
/// symmetrical: a continuation whose predecessor is not a wide lead becomes a
/// blank, and a wide lead whose continuation is gone becomes a blank. The
/// cell's own colours are kept, so a blank inside the box still wears the
/// box's background.
///
/// The whole row is scanned rather than just the two edge columns. It is one
/// pass over a row of a grid that has just been cloned wholesale, and on a
/// well-formed screen it changes nothing outside the damage.
fn repair_wide_pairs(cells: &mut [Cell], cols: usize, row: usize) {
    let base = row * cols;
    for c in 0..cols {
        let i = base + c;
        let broken = if cells[i].attrs.contains(Attrs::WIDE_CONT) {
            c == 0 || !is_wide_lead(&cells[i - 1])
        } else if is_wide_lead(&cells[i]) {
            c + 1 >= cols || !cells[base + c + 1].attrs.contains(Attrs::WIDE_CONT)
        } else {
            continue;
        };

        if broken {
            // A space and not empty text: `write_cells` emits a cell's text
            // verbatim, so an empty one would write nothing at all and shift
            // every column after it -- the very fault being repaired.
            cells[i].text = oxutrm_proto::CellText::const_new(" ");
            cells[i].attrs = Attrs::empty();
        }
    }
}

/// Half-open column ranges that need repainting.
///
/// A run that begins on a `WIDE_CONT` cell is extended one column to the left,
/// so the double-width glyph is always redrawn as a unit. Repositioning onto
/// the second half of a wide character and painting from there would leave the
/// terminal showing half a glyph.
fn changed_runs(desired: &[Cell], previous: Option<&[Cell]>, full: bool) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    let mut start: Option<usize> = None;

    for col in 0..desired.len() {
        let dirty = match previous {
            Some(prev) if !full => desired[col] != prev[col],
            // On a full repaint the screen was just cleared, so only cells
            // that differ from blank need writing at all.
            _ => desired[col] != Cell::blank(),
        };
        if dirty {
            if start.is_none() {
                let mut s = col;
                if desired[s].attrs.contains(Attrs::WIDE_CONT) && s > 0 {
                    s -= 1;
                }
                start = Some(s);
            }
        } else if let Some(s) = start.take() {
            runs.push((s, col));
        }
    }
    if let Some(s) = start {
        runs.push((s, desired.len()));
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::CellText;

    fn caps(colors: u32) -> TerminalCaps {
        TerminalCaps {
            truecolor: colors >= 16_777_216,
            colors,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "test".to_string(),
        }
    }

    /// What a full repaint says about the modes when every one of them is off.
    /// Nothing is known about the terminal, so every mode is stated rather than
    /// assumed — see [`Renderer::write_modes`].
    const MODES_ALL_OFF: &str = "\u{1b}[?1049l\u{1b}[?2004l\
                                 \u{1b}[?1003l\u{1b}[?1002l\u{1b}[?1000l\u{1b}[?1006l";

    fn size() -> TermSize {
        TermSize { cols: 5, rows: 2 }
    }

    fn blank_state() -> ScreenState {
        ScreenState::blank(2, 5).expect("2x5 is a valid screen")
    }

    fn cell(text: &str) -> Cell {
        Cell {
            text: CellText::from(text),
            ..Cell::blank()
        }
    }

    /// Render once from a known base so the interesting assertion is about the
    /// SECOND render, which is the incremental path.
    fn primed(caps: TerminalCaps, base: &ScreenState) -> Renderer {
        let mut r = Renderer::new(size(), caps);
        let mut sink: Vec<u8> = Vec::new();
        r.render(&mut sink, base).expect("priming render");
        r
    }

    fn render(r: &mut Renderer, s: &ScreenState) -> String {
        let mut out: Vec<u8> = Vec::new();
        r.render(&mut out, s).expect("render");
        String::from_utf8(out).expect("renderer emitted invalid UTF-8")
    }

    #[test]
    fn a_single_changed_cell_repaints_only_that_cell() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[6] = cell("X"); // row 1, col 1

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[2;2H\u{1b}[0mX\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn an_unchanged_state_emits_nothing_at_all() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);
        assert_eq!(render(&mut r, &base), "");
    }

    #[test]
    fn adjacent_changed_cells_become_one_run_with_one_move() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[1] = cell("a");
        next.cells[2] = cell("b");
        next.cells[3] = cell("c");

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[1;2H\u{1b}[0mabc\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn a_changed_attribute_run_emits_one_sgr_for_the_whole_run() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        for (i, ch) in ["a", "b"].iter().enumerate() {
            next.cells[i] = Cell {
                text: CellText::from(*ch),
                attrs: Attrs::BOLD | Attrs::UNDERLINE,
                ..Cell::blank()
            };
        }

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[1;1H\u{1b}[0;1;4mab\u{1b}[0m\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn truecolor_is_folded_for_a_16_colour_terminal() {
        let base = blank_state();
        let mut r = primed(caps(16), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = Cell {
            text: CellText::from("R"),
            fg: Color::Rgb(255, 0, 0),
            bg: Color::Rgb(0, 0, 255),
            ..Cell::blank()
        };

        let out = render(&mut r, &next);
        // Pure red lands on 31, not on bright red 91: xterm renders bright red
        // as (255,85,85), a salmon that is further from (255,0,0) than
        // (170,0,0) is. Nearest-in-sRGB is the rule, and this is where the
        // rule leads — a surprise worth recording rather than special-casing.
        assert_eq!(
            out,
            "\u{1b}[?2026h\u{1b}[1;1H\u{1b}[0;31;44mR\u{1b}[0m\u{1b}[1;1H\u{1b}[?2026l"
        );
        assert!(
            !out.contains("38;2"),
            "truecolor leaked to a 16-colour terminal"
        );
    }

    #[test]
    fn truecolor_survives_untouched_on_a_truecolor_terminal() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = Cell {
            text: CellText::from("R"),
            fg: Color::Rgb(255, 0, 0),
            ..Cell::blank()
        };

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[1;1H\u{1b}[0;38;2;255;0;0mR\u{1b}[0m\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn a_bright_index_becomes_bold_plus_the_base_colour_on_an_8_colour_terminal() {
        let base = blank_state();
        let mut r = primed(caps(8), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = Cell {
            text: CellText::from("B"),
            fg: Color::Idx(9), // bright red
            ..Cell::blank()
        };

        // Bold, then base red (31) — never 91, which this terminal cannot show.
        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[1;1H\u{1b}[0;1;31mB\u{1b}[0m\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn a_wide_character_emits_its_glyph_once_and_nothing_for_the_continuation() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = cell("世");
        next.cells[1] = Cell {
            text: CellText::from(""),
            attrs: Attrs::WIDE_CONT,
            ..Cell::blank()
        };
        next.cells[2] = cell("!");

        let out = render(&mut r, &next);
        assert_eq!(
            out,
            "\u{1b}[?2026h\u{1b}[1;1H\u{1b}[0m世!\u{1b}[1;1H\u{1b}[?2026l"
        );
        assert_eq!(
            out.matches('世').count(),
            1,
            "the wide glyph was painted more than once"
        );
    }

    /// A run beginning on a continuation cell must reach back to the glyph, or
    /// the terminal is left showing half a character.
    #[test]
    fn a_run_starting_on_a_continuation_cell_reaches_back_to_the_glyph() {
        let mut base = blank_state();
        base.cells[0] = cell("世");
        base.cells[1] = Cell {
            text: CellText::from(""),
            attrs: Attrs::WIDE_CONT,
            ..Cell::blank()
        };
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        // Only the continuation cell changes.
        next.cells[1] = Cell {
            text: CellText::from(""),
            attrs: Attrs::WIDE_CONT | Attrs::BOLD,
            ..Cell::blank()
        };

        let out = render(&mut r, &next);
        assert!(
            out.starts_with("\u{1b}[?2026h\u{1b}[1;1H"),
            "run did not reach back: {out:?}"
        );
        assert!(out.contains('世'), "the glyph was not redrawn: {out:?}");
    }

    #[test]
    fn a_cursor_move_alone_emits_only_the_move() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cursor.row = 1;
        next.cursor.col = 3;

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[2;4H\u{1b}[?2026l"
        );
    }

    #[test]
    fn hiding_the_cursor_emits_the_hide_sequence_once() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cursor.visible = false;

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[?25l\u{1b}[?2026l"
        );
        // And it is not repeated when nothing changes again.
        let mut again = next.clone();
        again.seq = 3;
        assert_eq!(render(&mut r, &again), "");
    }

    #[test]
    fn a_cursor_shape_change_emits_the_shape_sequence() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cursor.shape = CursorShape::Bar;

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[6 q\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn invalidate_forces_a_full_repaint() {
        let mut base = blank_state();
        base.cells[0] = cell("A");
        let mut r = primed(caps(16_777_216), &base);

        // Nothing changed, so nothing would be emitted...
        let mut again = base.clone();
        again.seq = 2;
        assert_eq!(render(&mut r, &again), "");

        // ...until the model is dropped.
        r.invalidate();
        let mut third = base.clone();
        third.seq = 3;
        // A full repaint also restates every mode, the cursor shape and the
        // cursor visibility: with no model of the terminal, none of them can
        // be assumed.
        assert_eq!(
            render(&mut r, &third),
            format!(
                "\u{1b}[?2026h{MODES_ALL_OFF}\u{1b}[H\u{1b}[2J\u{1b}[1;1H\u{1b}[0mA\u{1b}[2 q\u{1b}[1;1H\u{1b}[?25h\u{1b}[?2026l"
            )
        );
    }

    #[test]
    fn the_first_render_clears_the_screen_and_paints_only_non_blank_cells() {
        let mut r = Renderer::new(size(), caps(16_777_216));
        let mut s = blank_state();
        s.cells[7] = cell("Q");

        assert_eq!(
            render(&mut r, &s),
            format!(
                "\u{1b}[?2026h{MODES_ALL_OFF}\u{1b}[H\u{1b}[2J\u{1b}[2;3H\u{1b}[0mQ\u{1b}[2 q\u{1b}[1;1H\u{1b}[?25h\u{1b}[?2026l"
            )
        );
    }

    #[test]
    fn resize_drops_the_model_so_the_next_render_repaints() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);
        r.resize(TermSize { cols: 5, rows: 2 });

        let mut next = base.clone();
        next.seq = 2;
        let out = render(&mut r, &next);
        assert!(
            out.starts_with(&format!("\u{1b}[?2026h{MODES_ALL_OFF}")),
            "{out:?}"
        );
        assert!(out.contains("\u{1b}[H\u{1b}[2J"), "{out:?}");
    }

    /// A state of a different shape cannot be painted incrementally: every
    /// column index would mean something else.
    #[test]
    fn a_state_of_a_different_shape_forces_a_repaint_and_adopts_the_new_size() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut wider = ScreenState::blank(3, 8).expect("3x8 is valid");
        wider.seq = 2;
        wider.cells[0] = cell("W");

        assert_eq!(
            render(&mut r, &wider),
            format!(
                "\u{1b}[?2026h{MODES_ALL_OFF}\u{1b}[H\u{1b}[2J\u{1b}[1;1H\u{1b}[0mW\u{1b}[2 q\u{1b}[1;1H\u{1b}[?25h\u{1b}[?2026l"
            )
        );
        assert_eq!(r.size(), TermSize { cols: 8, rows: 3 });
    }

    #[test]
    fn a_title_change_emits_osc_0() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.title = "vim README.md".to_string();

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}]0;vim README.md\u{07}\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    #[test]
    fn bracketed_paste_and_mouse_modes_are_mirrored_locally() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.modes.bracketed_paste = true;
        next.modes.mouse = MouseMode::ButtonMotion;

        assert_eq!(
            render(&mut r, &next),
            "\u{1b}[?2026h\u{1b}[?2004h\u{1b}[?1003l\u{1b}[?1002l\u{1b}[?1000l\u{1b}[?1002h\u{1b}[?1006h\u{1b}[1;1H\u{1b}[?2026l"
        );
    }

    /// A terminal that cannot do SGR mouse encoding must not be told to.
    #[test]
    fn sgr_mouse_encoding_is_not_enabled_on_a_terminal_that_lacks_it() {
        let base = blank_state();
        let mut c = caps(16_777_216);
        c.mouse_sgr = false;
        c.bracketed_paste = false;
        let mut r = primed(c, &base);

        let mut next = base.clone();
        next.seq = 2;
        next.modes.bracketed_paste = true;
        next.modes.mouse = MouseMode::Press;

        let out = render(&mut r, &next);
        assert!(
            !out.contains("1006"),
            "SGR mouse mode was enabled anyway: {out:?}"
        );
        assert!(
            !out.contains("2004"),
            "bracketed paste was enabled anyway: {out:?}"
        );
    }

    #[test]
    fn the_bell_rings_once_when_the_counter_rises() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.bell = 1;
        assert_eq!(render(&mut r, &next), "\u{1b}[?2026h\u{07}\u{1b}[?2026l");

        // Same count: silence.
        let mut same = next.clone();
        same.seq = 3;
        assert_eq!(render(&mut r, &same), "");
    }

    /// The bug the monotonic counter exists to prevent: a reset must never
    /// ring the terminal once for every bell in the session's history.
    #[test]
    fn a_reset_counter_never_rings() {
        let mut base = blank_state();
        base.bell = 50;
        let mut r = primed(caps(16_777_216), &base);

        let mut reset = base.clone();
        reset.seq = 2;
        reset.bell = 0;
        assert_eq!(render(&mut r, &reset), "");
    }

    /// The bug this exists to prevent, in the user's words: resize the window
    /// while in vim, quit vim, and the shell is unusable — still on the
    /// alternate buffer, still sending mouse reports on every twitch.
    ///
    /// The resize invalidates the model, so the next render is a full repaint
    /// with nothing known about the terminal. "Nothing known" is not "off": a
    /// mode whose desired value is false has to be *said*, or whatever the
    /// terminal happens to be in stays.
    #[test]
    fn a_full_repaint_turns_off_the_modes_it_cannot_vouch_for() {
        let mut vim = blank_state();
        vim.modes.alt_screen = true;
        vim.modes.bracketed_paste = true;
        vim.modes.mouse = MouseMode::AnyMotion;
        let mut r = primed(caps(16_777_216), &vim);

        // The window was resized: everything painted is gone, and so is every
        // claim about what mode the terminal is in.
        r.invalidate();

        let mut shell = vim.clone();
        shell.seq = 2;
        shell.modes.alt_screen = false;
        shell.modes.bracketed_paste = false;
        shell.modes.mouse = MouseMode::Off;

        let out = render(&mut r, &shell);
        assert!(
            out.contains("\u{1b}[?1049l"),
            "the terminal was left on the alternate buffer: {out:?}"
        );
        assert!(
            out.contains("\u{1b}[?2004l"),
            "bracketed paste was never turned off: {out:?}"
        );
        assert!(
            out.contains("\u{1b}[?1003l"),
            "mouse reporting was never turned off: {out:?}"
        );
    }

    /// `\x1b[?1049h`/`l` swaps the physical screen buffer under us. Cells that
    /// match the model are not on the buffer we just switched to, so a diff
    /// against that model paints nothing and the user sees the *other*
    /// application's screen.
    #[test]
    fn crossing_the_alternate_buffer_repaints_rather_than_diffing() {
        let mut alt = blank_state();
        alt.modes.alt_screen = true;
        alt.cells[0] = cell("V");
        let mut r = primed(caps(16_777_216), &alt);

        // Leaving the alternate buffer. `V` is unchanged in the model, so an
        // incremental diff would emit nothing for it at all.
        let mut normal = alt.clone();
        normal.seq = 2;
        normal.modes.alt_screen = false;

        let out = render(&mut r, &normal);
        let leave = out
            .find("\u{1b}[?1049l")
            .expect("the buffer swap was never emitted");
        let clear = out.find("\u{1b}[H\u{1b}[2J").unwrap_or_else(|| {
            panic!("the buffer swapped but the screen was not repainted: {out:?}")
        });
        assert!(
            leave < clear,
            "the repaint went to the buffer we were leaving: {out:?}"
        );
        assert!(
            out.contains('V'),
            "a cell equal to the model was never painted onto the new buffer: {out:?}"
        );
    }

    /// A writer that refuses everything.
    struct FailingWriter;

    impl Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "gone"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// A writer that takes the first byte and then stops accepting anything,
    /// which is how `write_all` reports a truncated write.
    struct ShortWriter {
        taken: usize,
    }

    impl Write for ShortWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.taken == 0 && !buf.is_empty() {
                self.taken = 1;
                return Ok(1);
            }
            Ok(0)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    /// The model says "this is on the screen". It may only say so once the
    /// bytes that would put it there have actually gone out.
    #[test]
    fn a_failed_write_does_not_leave_the_model_claiming_a_painted_screen() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = cell("X");
        assert!(r.render(&mut FailingWriter, &next).is_err(), "write failed");

        // Whatever comes next cannot be a diff against a screen that was never
        // painted; the only safe move is to repaint.
        let mut after = next.clone();
        after.seq = 3;
        let out = render(&mut r, &after);
        assert!(
            out.contains("\u{1b}[H\u{1b}[2J"),
            "the model survived a failed write: {out:?}"
        );
        assert!(out.contains('X'), "the cell was never repainted: {out:?}");
    }

    #[test]
    fn a_short_write_does_not_leave_the_model_claiming_a_painted_screen() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = cell("X");
        let err = r
            .render(&mut ShortWriter { taken: 0 }, &next)
            .expect_err("a truncated write is an error");
        assert_eq!(err.kind(), std::io::ErrorKind::WriteZero);

        let mut after = next.clone();
        after.seq = 3;
        let out = render(&mut r, &after);
        assert!(
            out.contains("\u{1b}[H\u{1b}[2J"),
            "the model survived a truncated write: {out:?}"
        );
    }

    #[test]
    fn every_underline_variant_is_plain_underline_in_v1() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cells[0] = Cell {
            text: CellText::from("u"),
            attrs: Attrs::UNDERLINE,
            ..Cell::blank()
        };

        let out = render(&mut r, &next);
        assert!(out.contains("\u{1b}[0;4m"), "expected plain SGR 4: {out:?}");
        for style in ["4:1", "4:2", "4:3", "4:4", "4:5", "21m"] {
            assert!(!out.contains(style), "emitted an underline style: {out:?}");
        }
    }

    /// A 3x1 red overlay at row 1, col 2 of an 8x3 screen.
    fn test_overlay(row: u16, col: u16, text: &str) -> crate::Overlay {
        let cells = text
            .chars()
            .map(|c| Cell {
                text: oxutrm_proto::CellText::new(c.to_string()),
                fg: Color::Idx(1),
                bg: Color::Default,
                attrs: Attrs::empty(),
            })
            .collect::<Vec<_>>();
        crate::Overlay {
            row,
            col,
            rows: 1,
            cols: cells.len() as u16,
            cells,
        }
    }

    #[test]
    fn an_overlay_paints_over_the_screen_beneath_it() {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps(16_777_216));
        let mut screen = ScreenState::blank(3, 8).unwrap();
        for (i, c) in "abcdefgh".chars().enumerate() {
            screen.cells[8 + i].text = oxutrm_proto::CellText::new(c.to_string());
        }

        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();

        r.set_overlay(Some(test_overlay(1, 2, "XYZ")));
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        let painted = String::from_utf8_lossy(&out).to_string();

        assert!(
            painted.contains("XYZ"),
            "the overlay was not painted: {painted:?}"
        );
        assert!(
            !painted.contains("abcdefgh"),
            "the whole row was repainted, not just the covered columns: {painted:?}"
        );
    }

    /// The property the whole approach rests on: removing the overlay is an
    /// ordinary diff back to the authoritative screen. No repaint, no
    /// `invalidate`, and the cells underneath come back exactly.
    #[test]
    fn removing_an_overlay_restores_the_cells_beneath_it() {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps(16_777_216));
        let mut screen = ScreenState::blank(3, 8).unwrap();
        for (i, c) in "abcdefgh".chars().enumerate() {
            screen.cells[8 + i].text = oxutrm_proto::CellText::new(c.to_string());
        }

        r.render(&mut Vec::new(), &screen).unwrap();
        r.set_overlay(Some(test_overlay(1, 2, "XYZ")));
        r.render(&mut Vec::new(), &screen).unwrap();

        r.set_overlay(None);
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        let painted = String::from_utf8_lossy(&out).to_string();

        assert!(
            painted.contains("cde"),
            "the covered cells were not restored: {painted:?}"
        );
        assert!(
            !painted.contains("\x1b[2J"),
            "restoring should be a diff, not a full repaint: {painted:?}"
        );
    }

    #[test]
    fn an_overlay_hides_the_cursor_and_restoring_brings_it_back() {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps(16_777_216));
        let mut screen = ScreenState::blank(3, 8).unwrap();
        screen.cursor.visible = true;

        r.render(&mut Vec::new(), &screen).unwrap();

        r.set_overlay(Some(test_overlay(1, 2, "XYZ")));
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[?25l"),
            "the cursor was not hidden under the overlay"
        );

        r.set_overlay(None);
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[?25h"),
            "the cursor did not come back"
        );
    }

    /// An overlay wider or taller than the screen must clip, not panic and not
    /// write past the row. A window can shrink between the notice being built
    /// and being painted.
    #[test]
    fn an_overlay_larger_than_the_screen_is_clipped() {
        let mut r = Renderer::new(TermSize { cols: 4, rows: 2 }, caps(16_777_216));
        let screen = ScreenState::blank(2, 4).unwrap();

        r.set_overlay(Some(test_overlay(1, 2, "XYZABC")));
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();

        let painted = String::from_utf8_lossy(&out).to_string();
        assert!(painted.contains("XY"), "nothing was painted: {painted:?}");
        assert!(
            !painted.contains("ABC"),
            "painted past the screen: {painted:?}"
        );
    }

    /// A screen whose row 1 holds `text` starting at `col`, as a double-width
    /// glyph and its continuation.
    fn screen_with_wide_glyph_at(col: usize) -> ScreenState {
        let mut screen = ScreenState::blank(3, 8).expect("3x8 is a valid screen");
        screen.cells[8 + col] = Cell {
            text: CellText::new("世"),
            ..Cell::blank()
        };
        screen.cells[8 + col + 1] = Cell {
            text: CellText::const_new(""),
            attrs: Attrs::WIDE_CONT,
            ..Cell::blank()
        };
        screen
    }

    fn composited(screen: &ScreenState, o: crate::Overlay) -> Vec<Cell> {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps(16_777_216));
        r.set_overlay(Some(o));
        r.composite(screen).into_owned()
    }

    /// The box's left edge landing on the second column of a remote glyph.
    ///
    /// The left half is outside the box and survives, still two columns wide,
    /// so the terminal paints it over the box's first column and shifts the
    /// whole row right by one -- on any CJK-heavy screen, at the edge of the
    /// box the user is being asked to read.
    #[test]
    fn an_overlay_edge_on_a_glyphs_second_column_removes_the_orphaned_left_half() {
        let screen = screen_with_wide_glyph_at(0);
        let cells = composited(&screen, test_overlay(1, 1, "XYZ"));

        assert!(
            !is_wide_lead(&cells[8]),
            "half a glyph was left beside the box, and it is two columns wide: \
             {:?}",
            cells[8].text
        );
        assert_eq!(cells[8].text.as_str(), " ");
        assert_eq!(cells[9].text.as_str(), "X", "the box moved");
    }

    /// And the other side of the same broken pair: the box overwrites the
    /// left half and the continuation is left behind. `write_cells` skips a
    /// `WIDE_CONT`, so nothing ever repaints that column and the right half of
    /// the old glyph stays on the screen for the life of the box.
    #[test]
    fn an_overlay_edge_on_a_glyphs_first_column_removes_the_orphaned_right_half() {
        let screen = screen_with_wide_glyph_at(4);
        // Columns 2, 3 and 4 -- so the glyph's lead at 4 goes and its
        // continuation at 5 does not.
        let cells = composited(&screen, test_overlay(1, 2, "XYZ"));

        assert!(
            !cells[8 + 5].attrs.contains(Attrs::WIDE_CONT),
            "a continuation cell was left with nothing in front of it, so the \
             column it occupies can never be repainted"
        );
        assert_eq!(cells[8 + 5].text.as_str(), " ");
    }

    /// The box's own wide glyph, with its continuation clipped away by the
    /// screen's right edge. Painting the lead alone puts two columns of glyph
    /// into one column of screen.
    #[test]
    fn a_wide_glyph_clipped_by_the_screen_edge_is_not_left_half_painted() {
        let screen = ScreenState::blank(3, 8).unwrap();
        // The overlay's last cell lands in column 7, the last one there is.
        let cells = composited(&screen, test_overlay(1, 6, "X\u{4e16}"));

        assert!(
            !is_wide_lead(&cells[8 + 7]),
            "a double-width glyph was left in the last column with its other \
             half off the screen: {:?}",
            cells[8 + 7].text
        );
    }

    /// And a pair the box does not touch is left exactly as the host sent it.
    /// A repair that blanked working glyphs would be the same bug with a
    /// different sign.
    #[test]
    fn a_wide_pair_the_overlay_does_not_touch_survives() {
        let screen = screen_with_wide_glyph_at(6);
        let cells = composited(&screen, test_overlay(1, 1, "XYZ"));

        assert_eq!(
            cells[8 + 6].text.as_str(),
            "世",
            "a glyph the box never reached was blanked"
        );
        assert!(cells[8 + 7].attrs.contains(Attrs::WIDE_CONT));
    }

    #[test]
    fn a_repaint_is_wrapped_in_synchronized_output() {
        let mut r = Renderer::new(TermSize { cols: 4, rows: 1 }, caps(16_777_216));
        let mut screen = ScreenState::blank(1, 4).unwrap();
        screen.cells[0].text = oxutrm_proto::CellText::new("x");

        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        let painted = String::from_utf8_lossy(&out).to_string();

        assert!(painted.starts_with("\x1b[?2026h"), "no begin: {painted:?}");
        assert!(painted.ends_with("\x1b[?2026l"), "no end: {painted:?}");
    }

    /// A render that changes nothing must write nothing at all. Bracketing an
    /// empty payload would turn every quiet pacing tick into two escape
    /// sequences on the wire to the user's terminal.
    #[test]
    fn a_render_that_paints_nothing_writes_nothing() {
        let mut r = Renderer::new(TermSize { cols: 4, rows: 1 }, caps(16_777_216));
        let screen = ScreenState::blank(1, 4).unwrap();

        r.render(&mut Vec::new(), &screen).unwrap();
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();

        assert!(
            out.is_empty(),
            "wrote {:?} for an unchanged screen",
            String::from_utf8_lossy(&out)
        );
    }
}

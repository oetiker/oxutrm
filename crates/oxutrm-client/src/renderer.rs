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
}

impl Renderer {
    pub fn new(size: TermSize, caps: TerminalCaps) -> Renderer {
        Renderer {
            size,
            caps,
            painted: None,
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

        let full = self.painted.is_none();
        self.write_modes(&mut out, s, full);
        self.write_title(&mut out, s, full);
        self.write_cells(&mut out, s, full);
        self.write_cursor(&mut out, s, full);
        self.write_bell(&mut out, s);

        self.painted = Some(Painted {
            cells: s.cells.clone(),
            cursor: s.cursor,
            modes: s.modes,
            title: s.title.clone(),
            bell: s.bell,
        });

        w.write_all(&out)
    }

    // ---- modes -------------------------------------------------------------

    fn write_modes(&self, out: &mut Vec<u8>, s: &ScreenState, full: bool) {
        let before = self.painted.as_ref().map(|p| p.modes);

        let changed = |f: fn(&Modes) -> bool| match before {
            Some(b) if !full => f(&s.modes) != f(&b),
            _ => f(&s.modes),
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
            _ => s.modes.mouse != MouseMode::Off,
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

    fn write_cells(&self, out: &mut Vec<u8>, s: &ScreenState, full: bool) {
        if full {
            out.extend_from_slice(b"\x1b[H\x1b[2J");
        }

        let cols = s.cols as usize;
        // The pen is tracked across the whole pass but never across passes: a
        // terminal we have not written to since is not a terminal whose SGR
        // state we can still vouch for.
        let mut pen: Option<Vec<u8>> = None;

        for row in 0..s.rows {
            let base = row as usize * cols;
            let desired = &s.cells[base..base + cols];
            let previous = self.painted.as_ref().and_then(|p| {
                if p.cells.len() == s.cells.len() {
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

    fn write_cursor(&self, out: &mut Vec<u8>, s: &ScreenState, full: bool) {
        let before = self.painted.as_ref().map(|p| p.cursor).filter(|_| !full);

        if before.map(|b| b.shape) != Some(s.cursor.shape) {
            let n = match s.cursor.shape {
                CursorShape::Block => 2,
                CursorShape::Underline => 4,
                CursorShape::Bar => 6,
            };
            out.extend_from_slice(format!("\x1b[{n} q").as_bytes());
        }

        // Position is written whenever anything was painted, because painting
        // left the caret wherever the last glyph ended.
        let moved = before.map(|b| (b.row, b.col)) != Some((s.cursor.row, s.cursor.col));
        if moved || !out.is_empty() {
            out.extend_from_slice(
                format!("\x1b[{};{}H", s.cursor.row + 1, s.cursor.col + 1).as_bytes(),
            );
        }

        if before.map(|b| b.visible) != Some(s.cursor.visible) {
            out.extend_from_slice(if s.cursor.visible {
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
    fn write_bell(&self, out: &mut Vec<u8>, s: &ScreenState) {
        if let Some(p) = self.painted.as_ref()
            && s.bell > p.bell
        {
            out.push(0x07);
        }
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

        assert_eq!(render(&mut r, &next), "\u{1b}[2;2H\u{1b}[0mX\u{1b}[1;1H");
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

        assert_eq!(render(&mut r, &next), "\u{1b}[1;2H\u{1b}[0mabc\u{1b}[1;1H");
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
            "\u{1b}[1;1H\u{1b}[0;1;4mab\u{1b}[0m\u{1b}[1;1H"
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
        assert_eq!(out, "\u{1b}[1;1H\u{1b}[0;31;44mR\u{1b}[0m\u{1b}[1;1H");
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
            "\u{1b}[1;1H\u{1b}[0;38;2;255;0;0mR\u{1b}[0m\u{1b}[1;1H"
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
            "\u{1b}[1;1H\u{1b}[0;1;31mB\u{1b}[0m\u{1b}[1;1H"
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
        assert_eq!(out, "\u{1b}[1;1H\u{1b}[0m世!\u{1b}[1;1H");
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
            out.starts_with("\u{1b}[1;1H"),
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

        assert_eq!(render(&mut r, &next), "\u{1b}[2;4H");
    }

    #[test]
    fn hiding_the_cursor_emits_the_hide_sequence_once() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);

        let mut next = base.clone();
        next.seq = 2;
        next.cursor.visible = false;

        assert_eq!(render(&mut r, &next), "\u{1b}[?25l");
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

        assert_eq!(render(&mut r, &next), "\u{1b}[6 q\u{1b}[1;1H");
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
        // A full repaint also restates cursor shape and visibility: with no
        // model of the terminal, neither can be assumed.
        assert_eq!(
            render(&mut r, &third),
            "\u{1b}[H\u{1b}[2J\u{1b}[1;1H\u{1b}[0mA\u{1b}[2 q\u{1b}[1;1H\u{1b}[?25h"
        );
    }

    #[test]
    fn the_first_render_clears_the_screen_and_paints_only_non_blank_cells() {
        let mut r = Renderer::new(size(), caps(16_777_216));
        let mut s = blank_state();
        s.cells[7] = cell("Q");

        assert_eq!(
            render(&mut r, &s),
            "\u{1b}[H\u{1b}[2J\u{1b}[2;3H\u{1b}[0mQ\u{1b}[2 q\u{1b}[1;1H\u{1b}[?25h"
        );
    }

    #[test]
    fn resize_drops_the_model_so_the_next_render_repaints() {
        let base = blank_state();
        let mut r = primed(caps(16_777_216), &base);
        r.resize(TermSize { cols: 5, rows: 2 });

        let mut next = base.clone();
        next.seq = 2;
        assert!(render(&mut r, &next).starts_with("\u{1b}[H\u{1b}[2J"));
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
            "\u{1b}[H\u{1b}[2J\u{1b}[1;1H\u{1b}[0mW\u{1b}[2 q\u{1b}[1;1H\u{1b}[?25h"
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
            "\u{1b}]0;vim README.md\u{07}\u{1b}[1;1H"
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
            "\u{1b}[?2004h\u{1b}[?1003l\u{1b}[?1002l\u{1b}[?1000l\u{1b}[?1002h\u{1b}[?1006h\u{1b}[1;1H"
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
        assert_eq!(render(&mut r, &next), "\u{07}");

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
}

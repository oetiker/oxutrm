//! Recovering the blink attribute that `alacritty_terminal` throws away.
//!
//! `vte` parses SGR 5, 6 and 25 into [`Attr::BlinkSlow`], [`Attr::BlinkFast`]
//! and [`Attr::CancelBlink`] — and then `Term::terminal_attribute` matches
//! those three arms and does nothing with them. The information reaches the
//! emulator and is dropped on the floor.
//!
//! So the parser is not handed the `Term` directly. It is handed [`BlinkTap`],
//! a newtype that implements the whole of [`vte::ansi::Handler`], forwards
//! all 71 methods to the `Term` unchanged, and intercepts exactly two:
//! `terminal_attribute`, to track whether blink is currently on, and `input`,
//! to record the cursor position of every character written while it is.
//!
//! **The forwarding is generated, not typed.** 71 hand-copied signatures is
//! 71 chances to get an argument order wrong in a way that compiles, so they
//! were extracted from `vte`'s own source. Keep it that way if the trait
//! grows.
//!
//! # What scrolling does to the plane
//!
//! Blink is recorded against an absolute grid [`Point`], whose `line` shifts
//! as content scrolls off the top: today's line 0 is tomorrow's line -1. The
//! plane is shifted by the same amount, using the history growth that
//! [`crate::HostTerm`] already has to measure for `scrollback_len`. Points
//! that fall out of the grid entirely are dropped, because nothing can render
//! them any more.

use std::collections::HashSet;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Boundary, Line, Point};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::cursor_icon::CursorIcon;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

/// Which cells are blinking, and whether blink is on right now.
/// A cell identified by the two numbers that locate it. `Point` itself is not
/// `Hash`, so it cannot be a key.
type Key = (i32, usize);

fn key(p: Point) -> Key {
    (p.line.0, p.column.0)
}

#[derive(Default, Debug)]
pub struct BlinkPlane {
    cells: HashSet<Key>,
    active: bool,
}

impl BlinkPlane {
    pub fn is_blinking(&self, point: Point) -> bool {
        self.cells.contains(&key(point))
    }

    /// Content scrolled off the top: every recorded point moves up by `lines`.
    ///
    /// Points that fall past the top of the grid are discarded rather than
    /// kept at a line nothing can address.
    pub fn scrolled(&mut self, lines: i32, topmost: Line) {
        if lines == 0 {
            return;
        }
        self.cells = self
            .cells
            .iter()
            .filter_map(|(line, column)| {
                let line = line - lines;
                (Line(line) >= topmost).then_some((line, *column))
            })
            .collect();
    }

    /// A resize reflows the grid, so recorded positions no longer mean
    /// anything. Clearing is the honest answer; guessing would paint blink on
    /// cells that never had it.
    pub fn clear(&mut self) {
        self.cells.clear();
    }
}

/// The parser's view of the terminal: a `Term`, the blink plane, and a count
/// of lines that scrolled off the top.
pub struct BlinkTap<'a, T> {
    term: &'a mut Term<T>,
    blink: &'a mut BlinkPlane,
    /// Incremented for every line pushed into history. See
    /// [`BlinkTap::linefeed`] for why this cannot be measured afterwards.
    scrolled: &'a mut u64,
}

impl<'a, T: EventListener> BlinkTap<'a, T> {
    pub fn new(
        term: &'a mut Term<T>,
        blink: &'a mut BlinkPlane,
        scrolled: &'a mut u64,
    ) -> BlinkTap<'a, T> {
        BlinkTap {
            term,
            blink,
            scrolled,
        }
    }

    fn on_alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    fn note_scroll(&mut self, lines: u64) {
        if lines == 0 {
            return;
        }
        *self.scrolled = self.scrolled.saturating_add(lines);
        let topmost = self.term.topmost_line();
        self.blink
            .scrolled(lines.min(i32::MAX as u64) as i32, topmost);
    }
}

impl<T: EventListener> Handler for BlinkTap<'_, T> {
    /// Intercepted: record where this character landed if blink is on.
    ///
    /// The cursor is read BEFORE forwarding, because `Term::input` advances
    /// it — reading afterwards would tag the cell to the right.
    #[inline]
    fn input(&mut self, a0: char) {
        if self.blink.active {
            let point = self.term.grid().cursor.point;
            let point = point.grid_clamp(self.term, Boundary::Grid);
            self.blink.cells.insert(key(point));
        } else {
            let point = self.term.grid().cursor.point;
            let point = point.grid_clamp(self.term, Boundary::Grid);
            self.blink.cells.remove(&key(point));
        }
        self.term.input(a0);
    }

    /// Intercepted: this is the only path that pushes a line into history,
    /// and the only place a scroll can still be counted once the history ring
    /// is full.
    ///
    /// `history_size()` **saturates** at capacity, so measuring its growth
    /// stops working the moment the ring fills — which on a busy terminal is
    /// within seconds. There is no monotonic scrolled-off counter anywhere in
    /// the crate.
    ///
    /// The cursor tells us instead, exactly and for free: on a linefeed the
    /// cursor moves down one line *unless* it is already at the bottom of the
    /// scrolling region, in which case it stays put and the grid scrolls. So
    /// "the cursor did not move" **is** "a line scrolled off", saturated or
    /// not.
    ///
    /// The alternate screen has no history and its content never becomes
    /// scrollback, so scrolls there are not counted.
    #[inline]
    fn linefeed(&mut self) {
        let before = self.term.grid().cursor.point.line;
        let alt = self.on_alt_screen();
        self.term.linefeed();
        if !alt && self.term.grid().cursor.point.line == before {
            self.note_scroll(1);
        }
    }

    /// Intercepted for the same reason as `linefeed`.
    ///
    /// CSI S scrolls the region up. When the region starts at the top of the
    /// screen the displaced lines enter history; when it does not, they are
    /// discarded. The region is not visible from out here, so this counts
    /// them — an over-count in the rare case of an explicit CSI S against a
    /// region that does not start at row 0, and correct otherwise.
    #[inline]
    fn scroll_up(&mut self, a0: usize) {
        let alt = self.on_alt_screen();
        self.term.scroll_up(a0);
        if !alt {
            self.note_scroll(a0 as u64);
        }
    }

    /// Intercepted: SGR 5, 6 and 25 reach `Term` and are discarded there, so
    /// they are recorded here on the way past. `Reset` (SGR 0) cancels blink
    /// like every other attribute.
    #[inline]
    fn terminal_attribute(&mut self, a0: Attr) {
        match a0 {
            Attr::BlinkSlow | Attr::BlinkFast => self.blink.active = true,
            Attr::CancelBlink | Attr::Reset => self.blink.active = false,
            _ => {}
        }
        self.term.terminal_attribute(a0);
    }

    #[inline]
    fn set_title(&mut self, a0: Option<String>) {
        self.term.set_title(a0);
    }

    #[inline]
    fn set_cursor_style(&mut self, a0: Option<CursorStyle>) {
        self.term.set_cursor_style(a0);
    }

    #[inline]
    fn set_cursor_shape(&mut self, a0: CursorShape) {
        self.term.set_cursor_shape(a0);
    }

    #[inline]
    fn goto(&mut self, a0: i32, a1: usize) {
        self.term.goto(a0, a1);
    }

    #[inline]
    fn goto_line(&mut self, a0: i32) {
        self.term.goto_line(a0);
    }

    #[inline]
    fn goto_col(&mut self, a0: usize) {
        self.term.goto_col(a0);
    }

    #[inline]
    fn insert_blank(&mut self, a0: usize) {
        self.term.insert_blank(a0);
    }

    #[inline]
    fn move_up(&mut self, a0: usize) {
        self.term.move_up(a0);
    }

    #[inline]
    fn move_down(&mut self, a0: usize) {
        self.term.move_down(a0);
    }

    #[inline]
    fn identify_terminal(&mut self, a0: Option<char>) {
        self.term.identify_terminal(a0);
    }

    #[inline]
    fn device_status(&mut self, a0: usize) {
        self.term.device_status(a0);
    }

    #[inline]
    fn move_forward(&mut self, a0: usize) {
        self.term.move_forward(a0);
    }

    #[inline]
    fn move_backward(&mut self, a0: usize) {
        self.term.move_backward(a0);
    }

    #[inline]
    fn move_down_and_cr(&mut self, a0: usize) {
        self.term.move_down_and_cr(a0);
    }

    #[inline]
    fn move_up_and_cr(&mut self, a0: usize) {
        self.term.move_up_and_cr(a0);
    }

    #[inline]
    fn put_tab(&mut self, a0: u16) {
        self.term.put_tab(a0);
    }

    #[inline]
    fn backspace(&mut self) {
        self.term.backspace();
    }

    #[inline]
    fn carriage_return(&mut self) {
        self.term.carriage_return();
    }

    #[inline]
    fn bell(&mut self) {
        self.term.bell();
    }

    #[inline]
    fn substitute(&mut self) {
        self.term.substitute();
    }

    #[inline]
    fn newline(&mut self) {
        self.term.newline();
    }

    #[inline]
    fn set_horizontal_tabstop(&mut self) {
        self.term.set_horizontal_tabstop();
    }

    #[inline]
    fn scroll_down(&mut self, a0: usize) {
        self.term.scroll_down(a0);
    }

    #[inline]
    fn insert_blank_lines(&mut self, a0: usize) {
        self.term.insert_blank_lines(a0);
    }

    #[inline]
    fn delete_lines(&mut self, a0: usize) {
        self.term.delete_lines(a0);
    }

    #[inline]
    fn erase_chars(&mut self, a0: usize) {
        self.term.erase_chars(a0);
    }

    #[inline]
    fn delete_chars(&mut self, a0: usize) {
        self.term.delete_chars(a0);
    }

    #[inline]
    fn move_backward_tabs(&mut self, a0: u16) {
        self.term.move_backward_tabs(a0);
    }

    #[inline]
    fn move_forward_tabs(&mut self, a0: u16) {
        self.term.move_forward_tabs(a0);
    }

    #[inline]
    fn save_cursor_position(&mut self) {
        self.term.save_cursor_position();
    }

    #[inline]
    fn restore_cursor_position(&mut self) {
        self.term.restore_cursor_position();
    }

    #[inline]
    fn clear_line(&mut self, a0: LineClearMode) {
        self.term.clear_line(a0);
    }

    #[inline]
    fn clear_screen(&mut self, a0: ClearMode) {
        self.term.clear_screen(a0);
    }

    #[inline]
    fn clear_tabs(&mut self, a0: TabulationClearMode) {
        self.term.clear_tabs(a0);
    }

    #[inline]
    fn set_tabs(&mut self, a0: u16) {
        self.term.set_tabs(a0);
    }

    #[inline]
    fn reset_state(&mut self) {
        self.term.reset_state();
    }

    #[inline]
    fn reverse_index(&mut self) {
        self.term.reverse_index();
    }

    #[inline]
    fn set_mode(&mut self, a0: Mode) {
        self.term.set_mode(a0);
    }

    #[inline]
    fn unset_mode(&mut self, a0: Mode) {
        self.term.unset_mode(a0);
    }

    #[inline]
    fn report_mode(&mut self, a0: Mode) {
        self.term.report_mode(a0);
    }

    #[inline]
    fn set_private_mode(&mut self, a0: PrivateMode) {
        self.term.set_private_mode(a0);
    }

    #[inline]
    fn unset_private_mode(&mut self, a0: PrivateMode) {
        self.term.unset_private_mode(a0);
    }

    #[inline]
    fn report_private_mode(&mut self, a0: PrivateMode) {
        self.term.report_private_mode(a0);
    }

    #[inline]
    fn set_scrolling_region(&mut self, a0: usize, a1: Option<usize>) {
        self.term.set_scrolling_region(a0, a1);
    }

    #[inline]
    fn set_keypad_application_mode(&mut self) {
        self.term.set_keypad_application_mode();
    }

    #[inline]
    fn unset_keypad_application_mode(&mut self) {
        self.term.unset_keypad_application_mode();
    }

    #[inline]
    fn set_active_charset(&mut self, a0: CharsetIndex) {
        self.term.set_active_charset(a0);
    }

    #[inline]
    fn configure_charset(&mut self, a0: CharsetIndex, a1: StandardCharset) {
        self.term.configure_charset(a0, a1);
    }

    #[inline]
    fn set_color(&mut self, a0: usize, a1: Rgb) {
        self.term.set_color(a0, a1);
    }

    #[inline]
    fn dynamic_color_sequence(&mut self, a0: String, a1: usize, a2: &str) {
        self.term.dynamic_color_sequence(a0, a1, a2);
    }

    #[inline]
    fn reset_color(&mut self, a0: usize) {
        self.term.reset_color(a0);
    }

    #[inline]
    fn clipboard_store(&mut self, a0: u8, a1: &[u8]) {
        self.term.clipboard_store(a0, a1);
    }

    #[inline]
    fn clipboard_load(&mut self, a0: u8, a1: &str) {
        self.term.clipboard_load(a0, a1);
    }

    #[inline]
    fn decaln(&mut self) {
        self.term.decaln();
    }

    #[inline]
    fn push_title(&mut self) {
        self.term.push_title();
    }

    #[inline]
    fn pop_title(&mut self) {
        self.term.pop_title();
    }

    #[inline]
    fn text_area_size_pixels(&mut self) {
        self.term.text_area_size_pixels();
    }

    #[inline]
    fn text_area_size_chars(&mut self) {
        self.term.text_area_size_chars();
    }

    #[inline]
    fn set_hyperlink(&mut self, a0: Option<Hyperlink>) {
        self.term.set_hyperlink(a0);
    }

    #[inline]
    fn set_mouse_cursor_icon(&mut self, a0: CursorIcon) {
        self.term.set_mouse_cursor_icon(a0);
    }

    #[inline]
    fn report_keyboard_mode(&mut self) {
        self.term.report_keyboard_mode();
    }

    #[inline]
    fn push_keyboard_mode(&mut self, a0: KeyboardModes) {
        self.term.push_keyboard_mode(a0);
    }

    #[inline]
    fn pop_keyboard_modes(&mut self, a0: u16) {
        self.term.pop_keyboard_modes(a0);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, a0: KeyboardModes, a1: KeyboardModesApplyBehavior) {
        self.term.set_keyboard_mode(a0, a1);
    }

    #[inline]
    fn set_modify_other_keys(&mut self, a0: ModifyOtherKeys) {
        self.term.set_modify_other_keys(a0);
    }

    #[inline]
    fn report_modify_other_keys(&mut self) {
        self.term.report_modify_other_keys();
    }

    #[inline]
    fn set_scp(&mut self, a0: ScpCharPath, a1: ScpUpdateMode) {
        self.term.set_scp(a0, a1);
    }
}

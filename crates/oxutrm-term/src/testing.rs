//! Shared test scaffolding: an emulator with no PTY behind it.

use alacritty_terminal::Term;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;

use oxutrm_proto::TermSize;

use crate::grid::GridSize;
use crate::listener::EventSink;

/// An emulator of the given size, fed `bytes`, with no PTY and no child.
///
/// Most of this crate is a pure function of what the emulator holds, so most
/// of it can be tested without spawning anything.
pub fn term_with(rows: u16, cols: u16, bytes: &[u8]) -> Term<EventSink> {
    let dims = GridSize::new(TermSize { cols, rows }, 100);
    let mut term = Term::new(Config::default(), &dims, EventSink::new());
    let mut parser: Processor = Processor::new();
    parser.advance(&mut term, bytes);
    term
}

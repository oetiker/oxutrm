//! What one character position holds.

use serde::{Deserialize, Serialize};

/// A colour, in the three forms a terminal can express one.
///
/// `Default` is not a fourth colour: it means "whatever the renderer's default
/// is", and it survives all the way to the client so that a terminal with a
/// themed background paints its own rather than a guess made on the host.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Color {
    Default,
    /// An index into the 256-colour palette.
    Idx(u8),
    Rgb(u8, u8, u8),
}

bitflags::bitflags! {
    /// Everything about a cell that is not colour or text.
    ///
    /// The eight SGR attributes fill exactly one byte; `WIDE_CONT` is a ninth
    /// bit, which is why this is a `u16`. It is structural rather than an SGR
    /// attribute — no escape sequence sets it.
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
    pub struct Attrs: u16 {
        const BOLD      = 0b0000_0001;
        const ITALIC    = 0b0000_0010;
        const UNDERLINE = 0b0000_0100;
        const INVERSE   = 0b0000_1000;
        const BLINK     = 0b0001_0000;
        const STRIKE    = 0b0010_0000;
        const DIM       = 0b0100_0000;
        const HIDDEN    = 0b1000_0000;
        /// Right-hand half of a double-width character
        /// (`alacritty_terminal`'s `Flags::WIDE_CHAR_SPACER`).
        ///
        /// Represented explicitly rather than as a space: a renderer that
        /// painted a space here would shift every column after it.
        const WIDE_CONT = 0b0001_0000_0000;
    }
}

/// The text of one cell.
///
/// Inline for up to 24 bytes, so a cell holding one ASCII character — or one
/// character plus a combining mark, or one CJK ideograph — allocates
/// **nothing**. This matters because the design keeps a ring of 32 states:
/// with `String`, an 80x24 session would hold roughly 61,000 live heap
/// allocations for a screen that is mostly spaces.
///
/// The wire encoding is identical to `String`'s — both serialise as a str — so
/// this alias can be changed in one line without a protocol change.
pub type CellText = compact_str::CompactString;

/// One character position on the screen.
///
/// `text` is a small string rather than a `char` so that grapheme clusters and
/// combining marks survive intact; an accented character that arrived as two
/// code points is stored as two code points and rendered as one glyph.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cell {
    pub text: CellText,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
}

impl Cell {
    /// An empty position: one space, default colours, no attributes.
    pub fn blank() -> Cell {
        Cell {
            text: CellText::const_new(" "),
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
        }
    }
}

impl Default for Cell {
    fn default() -> Cell {
        Cell::blank()
    }
}

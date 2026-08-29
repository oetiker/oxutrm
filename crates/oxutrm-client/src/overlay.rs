//! Layer 1: local UI, converted into cells the renderer can composite.
//!
//! The client paints two layers. Layer 0 is the remote framebuffer, which the
//! host owns. Layer 1 is this: a notice, and later a session picker or a config
//! screen, drawn locally and never sent anywhere. It is composited into the
//! renderer's grid *before* the diff, so drawing it and removing it are both
//! ordinary diffs.
//!
//! `ratatui` is used **headlessly** -- widgets render into a bare `Buffer` and
//! this module converts that into `oxutrm_proto::Cell`. Nothing here touches a
//! terminal; `Renderer` remains the only thing in the tree that does.

use oxutrm_proto::{Attrs, Cell, CellText, Color};
use ratatui::buffer::Buffer;
use ratatui::style::{Color as RColor, Modifier};
use unicode_width::UnicodeWidthStr as _;

/// A rectangle of locally drawn cells, and where it sits on the screen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Overlay {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
    /// `rows * cols`, row-major.
    pub cells: Vec<Cell>,
}

/// Convert a rendered ratatui buffer into cells the renderer can composite.
///
/// **The wide-character rule is the whole reason this is not a `map`.**
/// `ratatui` represents a double-width glyph as the glyph followed by a plain
/// space; oxutrm represents it as the glyph followed by empty text carrying
/// `Attrs::WIDE_CONT`, because the renderer skips a continuation cell and
/// painting a space there would shift every column to its right
/// (`oxutrm_proto`'s `cell.rs`, and `Renderer::write_cells`). So the width is
/// measured here rather than trusted from either side.
pub fn overlay_from_buffer(buf: &Buffer, row: u16, col: u16) -> Overlay {
    let area = buf.area();
    let mut cells = Vec::with_capacity(area.width as usize * area.height as usize);

    for y in area.top()..area.bottom() {
        // Set by the column that owns a wide glyph, consumed by the next one.
        // Reset per row: a glyph cannot straddle the right edge.
        let mut continuation: Option<(Color, Color)> = None;

        for x in area.left()..area.right() {
            if let Some((fg, bg)) = continuation.take() {
                cells.push(Cell {
                    text: CellText::const_new(""),
                    fg,
                    bg,
                    attrs: Attrs::WIDE_CONT,
                });
                continue;
            }

            let c = &buf[(x, y)];
            let fg = color_of(c.fg);
            let bg = color_of(c.bg);

            // `width` and not `chars().count()`: a grapheme cluster of several
            // code points still occupies one or two columns.
            if c.symbol().width() == 2 {
                continuation = Some((fg, bg));
            }

            cells.push(Cell {
                // `fit_cell_text` and not the raw symbol: `MAX_CELL_TEXT` is 32
                // bytes, a receiver rejects a longer one, and layer 1 is a
                // producer like any other.
                text: oxutrm_proto::fit_cell_text(CellText::new(c.symbol())),
                fg,
                bg,
                attrs: attrs_of(c.modifier),
            });
        }
    }

    Overlay {
        row,
        col,
        rows: area.height,
        cols: area.width,
        cells,
    }
}

/// ratatui's sixteen named colours are the ANSI palette, so they become palette
/// indices rather than guessed RGB: the terminal's own theme should win, and
/// `color::down_convert` already knows how to degrade an index.
fn color_of(c: RColor) -> Color {
    match c {
        RColor::Reset => Color::Default,
        RColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
        RColor::Indexed(i) => Color::Idx(i),
        RColor::Black => Color::Idx(0),
        RColor::Red => Color::Idx(1),
        RColor::Green => Color::Idx(2),
        RColor::Yellow => Color::Idx(3),
        RColor::Blue => Color::Idx(4),
        RColor::Magenta => Color::Idx(5),
        RColor::Cyan => Color::Idx(6),
        RColor::Gray => Color::Idx(7),
        RColor::DarkGray => Color::Idx(8),
        RColor::LightRed => Color::Idx(9),
        RColor::LightGreen => Color::Idx(10),
        RColor::LightYellow => Color::Idx(11),
        RColor::LightBlue => Color::Idx(12),
        RColor::LightMagenta => Color::Idx(13),
        RColor::LightCyan => Color::Idx(14),
        RColor::White => Color::Idx(15),
    }
}

/// Both blink rates collapse onto one attribute, matching what the host does
/// with `alacritty_terminal`'s flags.
fn attrs_of(m: Modifier) -> Attrs {
    let mut a = Attrs::empty();
    if m.contains(Modifier::BOLD) {
        a |= Attrs::BOLD;
    }
    if m.contains(Modifier::DIM) {
        a |= Attrs::DIM;
    }
    if m.contains(Modifier::ITALIC) {
        a |= Attrs::ITALIC;
    }
    if m.contains(Modifier::UNDERLINED) {
        a |= Attrs::UNDERLINE;
    }
    if m.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
        a |= Attrs::BLINK;
    }
    if m.contains(Modifier::REVERSED) {
        a |= Attrs::INVERSE;
    }
    if m.contains(Modifier::HIDDEN) {
        a |= Attrs::HIDDEN;
    }
    if m.contains(Modifier::CROSSED_OUT) {
        a |= Attrs::STRIKE;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget as _};

    fn buffer_of(width: u16, height: u16, lines: Vec<Line<'static>>) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines).render(area, &mut buf);
        buf
    }

    #[test]
    fn plain_text_converts_one_cell_per_column() {
        let buf = buffer_of(4, 1, vec![Line::from("ab")]);
        let o = overlay_from_buffer(&buf, 3, 7);

        assert_eq!((o.row, o.col, o.rows, o.cols), (3, 7, 1, 4));
        assert_eq!(o.cells.len(), 4);
        assert_eq!(o.cells[0].text, "a");
        assert_eq!(o.cells[1].text, "b");
        assert_eq!(o.cells[2].text, " ");
    }

    /// The trap this module exists for. ratatui puts a SPACE in the column
    /// after a double-width glyph; oxutrm puts `WIDE_CONT` and no text. Copying
    /// the space through shifts every column to the right of it.
    #[test]
    fn a_wide_glyph_gets_a_flagged_continuation_and_not_a_space() {
        let buf = buffer_of(4, 1, vec![Line::from("a\u{4e16}b")]);
        let o = overlay_from_buffer(&buf, 0, 0);

        assert_eq!(o.cells[0].text, "a");
        assert_eq!(o.cells[1].text, "\u{4e16}");
        assert!(
            !o.cells[1].attrs.contains(Attrs::WIDE_CONT),
            "the glyph itself is not a continuation"
        );
        assert_eq!(
            o.cells[2].text, "",
            "the continuation carries no text; a space would shift the row"
        );
        assert!(
            o.cells[2].attrs.contains(Attrs::WIDE_CONT),
            "the right half must be flagged"
        );
        assert_eq!(o.cells[3].text, "b", "the column after must not be shifted");
    }

    #[test]
    fn a_continuation_inherits_the_glyphs_colours() {
        let style = Style::default()
            .fg(RColor::Rgb(1, 2, 3))
            .bg(RColor::Indexed(9));
        let buf = buffer_of(3, 1, vec![Line::from(Span::styled("\u{4e16}", style))]);
        let o = overlay_from_buffer(&buf, 0, 0);

        assert_eq!(o.cells[1].fg, o.cells[0].fg);
        assert_eq!(o.cells[1].bg, o.cells[0].bg);
    }

    #[test]
    fn colours_map_across_all_three_kinds() {
        let cases = [
            (RColor::Reset, Color::Default),
            (RColor::Rgb(10, 20, 30), Color::Rgb(10, 20, 30)),
            (RColor::Indexed(200), Color::Idx(200)),
            (RColor::Red, Color::Idx(1)),
            (RColor::LightRed, Color::Idx(9)),
            (RColor::White, Color::Idx(15)),
        ];
        for (from, want) in cases {
            let buf = buffer_of(
                1,
                1,
                vec![Line::from(Span::styled("x", Style::default().fg(from)))],
            );
            let o = overlay_from_buffer(&buf, 0, 0);
            assert_eq!(o.cells[0].fg, want, "mapping {from:?}");
        }
    }

    #[test]
    fn every_modifier_maps_to_an_attribute() {
        let cases = [
            (Modifier::BOLD, Attrs::BOLD),
            (Modifier::DIM, Attrs::DIM),
            (Modifier::ITALIC, Attrs::ITALIC),
            (Modifier::UNDERLINED, Attrs::UNDERLINE),
            (Modifier::SLOW_BLINK, Attrs::BLINK),
            (Modifier::RAPID_BLINK, Attrs::BLINK),
            (Modifier::REVERSED, Attrs::INVERSE),
            (Modifier::HIDDEN, Attrs::HIDDEN),
            (Modifier::CROSSED_OUT, Attrs::STRIKE),
        ];
        for (from, want) in cases {
            let style = Style::default().add_modifier(from);
            let buf = buffer_of(1, 1, vec![Line::from(Span::styled("x", style))]);
            let o = overlay_from_buffer(&buf, 0, 0);
            assert!(o.cells[0].attrs.contains(want), "mapping {from:?}");
        }
    }

    /// `MAX_CELL_TEXT` is 32 bytes and load-bearing: a longer cell text is
    /// rejected by the receiver's validation. `fit_cell_text` is the producer's
    /// repair, and layer 1 is a producer.
    #[test]
    fn an_overlong_grapheme_cluster_is_fitted_rather_than_emitted_whole() {
        let long: String = std::iter::once('e')
            .chain(std::iter::repeat_n('\u{301}', 40))
            .collect();
        let buf = buffer_of(2, 1, vec![Line::from(long)]);
        let o = overlay_from_buffer(&buf, 0, 0);

        assert!(
            o.cells[0].text.len() <= oxutrm_proto::MAX_CELL_TEXT,
            "cell text {} bytes exceeds MAX_CELL_TEXT",
            o.cells[0].text.len()
        );
    }
}

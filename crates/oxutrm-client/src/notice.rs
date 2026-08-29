//! What layer 1 says, and how big it is.
//!
//! Phase 1 deliberately makes no promise about reconnection, because nothing
//! reconnects yet. The notice reports what the client can observe -- silence,
//! counters -- and nothing it cannot know. In particular it never claims the
//! session is safe: a dead network and a crashed host are indistinguishable
//! from here.

use oxutrm_proto::TermSize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget as _, Wrap};

use crate::overlay::{Overlay, overlay_from_buffer};

/// Below this the box is dropped for a single line: a box that does not fit is
/// worse than a line that does.
pub const MIN_BOX: TermSize = TermSize { cols: 20, rows: 6 };

/// One piece of local UI, as content rather than as pixels.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Notice {
    pub headline: String,
    pub body: Vec<String>,
    /// `(keys, what it does)`, rendered as a two-column list.
    pub keys: Vec<(String, String)>,
}

/// Lay a notice out for this screen, as cells ready to composite.
///
/// Sizing is content-driven and then clamped, rather than a fixed box: the
/// held-input notice is much taller than the silence one, and a fixed box
/// would either truncate it or leave the common case mostly empty.
pub fn layout_notice(n: &Notice, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        return single_line(n, size);
    }

    let lines = notice_lines(n);
    // Two columns of border plus two of padding.
    let widest = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let cols = widest.saturating_add(4).clamp(MIN_BOX.cols, size.cols);
    // Two rows of border.
    let rows = (lines.len() as u16).saturating_add(2).clamp(3, size.rows);

    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" oxutrm ");
    let inner = block.inner(area);
    block.render(area, &mut buf);
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, &mut buf);

    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

/// Headline, blank, body, blank, keys -- with the blanks dropped when the part
/// they separate is empty, so a notice with no keys has no trailing gap.
fn notice_lines(n: &Notice) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        n.headline.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];

    if !n.body.is_empty() {
        lines.push(Line::from(""));
        lines.extend(n.body.iter().map(|b| Line::from(b.clone())));
    }

    if !n.keys.is_empty() {
        lines.push(Line::from(""));
        let widest = n
            .keys
            .iter()
            .map(|(k, _)| k.chars().count())
            .max()
            .unwrap_or(0);
        for (keys, what) in &n.keys {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{keys:<widest$}  "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(what.clone()),
            ]));
        }
    }

    lines
}

/// The fallback for a screen too small for a box.
///
/// Reverse video and the top row, because the bottom rows are where the cursor
/// usually is and covering those is what the box was centred to avoid.
fn single_line(n: &Notice, size: TermSize) -> Overlay {
    let area = Rect::new(0, 0, size.cols.max(1), 1);
    let mut buf = Buffer::empty(area);
    Paragraph::new(Line::from(Span::styled(
        format!("oxutrm: {}", n.headline),
        Style::default().add_modifier(Modifier::REVERSED),
    )))
    .render(area, &mut buf);
    overlay_from_buffer(&buf, 0, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice() -> Notice {
        Notice {
            headline: "no reply from host".to_string(),
            body: vec!["silent for 6s".to_string(), "sent 14 - lost 9".to_string()],
            keys: vec![(
                "Ctrl-\\ q".to_string(),
                "close oxutrm here; the shell keeps running".to_string(),
            )],
        }
    }

    fn text_of(o: &Overlay) -> String {
        let mut s = String::new();
        for r in 0..o.rows {
            for c in 0..o.cols {
                let cell = &o.cells[r as usize * o.cols as usize + c as usize];
                s.push_str(if cell.text.is_empty() { "" } else { &cell.text });
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn a_notice_is_centred_on_the_screen() {
        let o = layout_notice(&notice(), TermSize { cols: 80, rows: 24 });

        assert_eq!(o.col, (80 - o.cols) / 2, "not horizontally centred");
        assert_eq!(o.row, (24 - o.rows) / 2, "not vertically centred");
    }

    #[test]
    fn a_notice_never_exceeds_the_screen() {
        for (cols, rows) in [(80u16, 24u16), (20, 6), (200, 60), (24, 8)] {
            let o = layout_notice(&notice(), TermSize { cols, rows });
            assert!(
                o.cols <= cols && o.rows <= rows,
                "{o:?} exceeds {cols}x{rows}"
            );
            assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
        }
    }

    #[test]
    fn the_headline_and_the_keys_are_both_in_the_box() {
        let o = layout_notice(&notice(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);

        assert!(text.contains("no reply from host"), "{text}");
        assert!(text.contains("Ctrl-\\ q"), "{text}");
        assert!(text.contains("the shell keeps running"), "{text}");
    }

    /// A box that does not fit is worse than a line that does.
    #[test]
    fn a_screen_too_small_for_a_box_gets_one_line() {
        let o = layout_notice(&notice(), TermSize { cols: 18, rows: 4 });

        assert_eq!(o.rows, 1, "expected the single-line fallback");
        assert_eq!(o.row, 0, "the fallback goes on the top row");
        assert_eq!(o.cols, 18, "the fallback spans the width");
        assert!(text_of(&o).contains("no reply"), "{}", text_of(&o));
    }

    /// One column and one row is absurd and must still not panic: a terminal
    /// reports 1x1 transiently while some emulators tear down.
    #[test]
    fn a_one_by_one_screen_does_not_panic() {
        let o = layout_notice(&notice(), TermSize { cols: 1, rows: 1 });
        assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
    }
}

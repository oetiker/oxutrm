//! The status popup, laid out as layer-1 cells.
//!
//! [`PopupView`] is what the popup says, as plain data the session builds on
//! every lap the popup is open; [`layout_popup`] turns it into an [`Overlay`]
//! the renderer composites over the remote screen. Pure: no clock, no
//! terminal, no session.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use oxutrm_proto::TermSize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Padding, Paragraph, Sparkline, Widget as _, Wrap,
};

use crate::overlay::{Overlay, overlay_from_buffer};

/// Below this the box is dropped for a single line: a box that does not fit
/// is worse than a line that does.
pub const MIN_BOX: TermSize = TermSize { cols: 20, rows: 6 };

/// The largest box, whatever the screen: wide enough for a log line, small
/// enough to leave the shell visible around it.
pub const MAX_BOX: TermSize = TermSize { cols: 72, rows: 24 };

/// How much of a reason is shown.
///
/// A reason is an error chain built partly from a remote's stderr, so its
/// length is not ours to choose. The popup wraps and clips, so a long one
/// cannot overflow anything -- but it can push everything else out of the
/// box. The useful part of an ssh failure is at the front.
pub const REASON_SHOWN: usize = 120;

/// The label in front of the RTT sparkline.
const SPARK_LABEL: &str = "rtt ";

/// The state the popup reports, and the colour it reports it in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Marker {
    /// The host is answering.
    #[default]
    Live,
    /// A reply is owed and none has come.
    Silent,
    /// Silent long enough that the client is rebuilding the link.
    Recovering,
    /// The host answers again after an outage the popup opened for.
    LiveAgain,
}

impl Marker {
    fn color(self) -> Color {
        match self {
            Marker::Live | Marker::LiveAgain => Color::Green,
            Marker::Silent => Color::Yellow,
            Marker::Recovering => Color::Red,
        }
    }
}

/// One entry of the key bar.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyHint {
    pub key: String,
    pub label: String,
    /// Drawn dimmed when false: a key that is coming, not one that works.
    pub enabled: bool,
}

/// What the popup says, as content rather than as cells.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PopupView {
    /// Drawn into the top border.
    pub title: String,
    pub marker: Marker,
    /// The first line inside the box, e.g. `● SILENT`.
    pub marker_text: String,
    /// What was typed while the link was down, and what can be done with it.
    pub held: Vec<String>,
    /// What the rebuild loop is doing, while it runs.
    pub recovering: Vec<String>,
    /// Link quality and identity, one row each.
    pub status: Vec<String>,
    /// The standby, when the host offered one.
    pub standby: Vec<String>,
    /// Round-trip time in milliseconds, oldest first; `None` is a gap.
    pub spark: Vec<Option<u64>>,
    /// The activity log, oldest first.
    pub log: Vec<String>,
    pub keys: Vec<KeyHint>,
}

/// Lay the popup out for this screen, as cells ready to composite.
///
/// `min(cols - 4, 72)` by `min(rows - 2, 24)`, centred. Top to bottom: the
/// marker, the held section, the recovering section, the status block, the
/// standby block, the sparkline, then the log filling whatever is left with
/// the newest entry at the bottom. The key bar always has the last row inside
/// the border, because it is how the popup is left.
pub fn layout_popup(v: &PopupView, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        return single_line(v, size);
    }
    // At least 16x4 here, so the inner area below is at least 12x2.
    let cols = (size.cols - 4).min(MAX_BOX.cols);
    let rows = (size.rows - 2).min(MAX_BOX.rows);
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(format!(" {} ", v.title));
    let inner = block.inner(area);
    block.render(area, &mut buf);

    let bar = Rect {
        y: inner.bottom() - 1,
        height: 1,
        ..inner
    };
    Paragraph::new(key_line(&v.keys)).render(bar, &mut buf);

    let mut body = Rect {
        height: inner.height - 1,
        ..inner
    };
    let marker = [Line::from(Span::styled(
        v.marker_text.clone(),
        Style::default()
            .fg(v.marker.color())
            .add_modifier(Modifier::BOLD),
    ))];
    body = place(&marker, body, &mut buf);
    for section in [&v.held, &v.recovering, &v.status, &v.standby] {
        body = place(&plain(section), body, &mut buf);
    }
    body = place_spark(&v.spark, body, &mut buf);
    place_log(&v.log, body, &mut buf);

    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

fn plain(lines: &[String]) -> Vec<Line<'static>> {
    lines.iter().map(|l| Line::from(l.clone())).collect()
}

/// Render `lines`, wrapped, at the top of `area`; return the part of `area`
/// left below them.
fn place(lines: &[Line<'static>], area: Rect, buf: &mut Buffer) -> Rect {
    if lines.is_empty() || area.height == 0 {
        return area;
    }
    let used = wrapped_row_count(lines, area.width, area.height);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(
            Rect {
                height: used,
                ..area
            },
            buf,
        );
    Rect {
        y: area.y + used,
        height: area.height - used,
        ..area
    }
}

/// One row: a label, then one cell per sample, the newest at the right.
fn place_spark(spark: &[Option<u64>], area: Rect, buf: &mut Buffer) -> Rect {
    let label = SPARK_LABEL.len() as u16;
    if area.height == 0 || area.width <= label || spark.iter().all(Option::is_none) {
        return area;
    }
    Paragraph::new(SPARK_LABEL).render(
        Rect {
            width: label,
            height: 1,
            ..area
        },
        buf,
    );
    let width = (area.width - label) as usize;
    let shown = &spark[spark.len().saturating_sub(width)..];
    let max = shown.iter().flatten().copied().max().unwrap_or(1).max(1);
    Sparkline::default()
        .data(shown.to_vec())
        .max(max)
        .absent_value_symbol(" ")
        .render(
            Rect {
                x: area.x + label,
                width: area.width - label,
                height: 1,
                ..area
            },
            buf,
        );
    Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    }
}

/// The log fills `area` from the bottom up, newest last; entries that do not
/// fit give way oldest first. A long entry wraps and takes the rows it needs.
fn place_log(log: &[String], area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let mut kept: Vec<(Line<'static>, u16)> = Vec::new();
    let mut used = 0u16;
    for entry in log.iter().rev() {
        let line = Line::from(entry.clone());
        let rows = wrapped_row_count(std::slice::from_ref(&line), area.width, area.height).max(1);
        if used + rows > area.height {
            break;
        }
        used += rows;
        kept.push((line, rows));
    }
    let mut y = area.bottom() - used;
    for (line, rows) in kept.into_iter().rev() {
        Paragraph::new(line).wrap(Wrap { trim: false }).render(
            Rect {
                y,
                height: rows,
                ..area
            },
            buf,
        );
        y += rows;
    }
}

fn key_line(keys: &[KeyHint]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let (key, label) = if k.enabled {
            (
                Style::default().add_modifier(Modifier::BOLD),
                Style::default(),
            )
        } else {
            let dim = Style::default().add_modifier(Modifier::DIM);
            (dim, dim)
        };
        spans.push(Span::styled(k.key.clone(), key));
        spans.push(Span::styled(format!(" {}", k.label), label));
    }
    Line::from(spans)
}

/// The fallback for a screen too small for a box.
///
/// Reverse video and the top row, because the bottom rows are where the
/// cursor usually is and covering those is what the box was centred to avoid.
fn single_line(v: &PopupView, size: TermSize) -> Overlay {
    let area = Rect::new(0, 0, size.cols.max(1), 1);
    let mut buf = Buffer::empty(area);
    Paragraph::new(Line::from(Span::styled(
        format!("oxutrm: {}", v.marker_text),
        Style::default().add_modifier(Modifier::REVERSED),
    )))
    .render(area, &mut buf);
    overlay_from_buffer(&buf, 0, 0)
}

/// How many rows `lines` needs when wrapped at `width`, capped at `height`.
///
/// `Paragraph::line_count` would answer this directly, but sits behind
/// ratatui's `unstable-rendered-line-info` feature, and enabling an unstable
/// feature to size a box is a worse trade than this: render the same wrap
/// into a scratch buffer and read back the last row it touched. By
/// construction it cannot disagree with ratatui's own wrapping -- it *is*
/// ratatui's own wrapping. Callers never end a section on a blank line, so a
/// run of untouched rows at the bottom means "wrapping stopped here".
pub(crate) fn wrapped_row_count(lines: &[Line<'static>], width: u16, height: u16) -> u16 {
    if width == 0 || height == 0 {
        return 0;
    }
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(area, &mut buf);
    (0..height)
        .rev()
        .find(|&y| (0..width).any(|x| buf[(x, y)].symbol() != " "))
        .map_or(0, |y| y + 1)
}

/// One line of `reason`, safe to put in a cell and short enough to read.
///
/// The first line only: ssh says why it failed first and pads afterwards, so a
/// multi-line reason's later lines are the least useful part of it. Cut on a
/// character boundary rather than a byte one -- a reason carries a remote's
/// stderr, which can be anything at all.
///
/// Made legible BEFORE the cut, so the cap bounds what actually reaches the
/// screen rather than what went into the escaping.
pub fn summarised(reason: &str) -> String {
    let line = legible(reason.lines().next().unwrap_or("").trim());
    if line.chars().count() <= REASON_SHOWN {
        return line;
    }
    let kept: String = line.chars().take(REASON_SHOWN).collect();
    format!("{kept}...")
}

/// `line` with every control scalar shown rather than emitted.
///
/// **This string is a remote's stderr**, relayed through ssh and an error
/// chain and handed to the renderer. Anything in it that a terminal acts on --
/// C0 (0x00-0x1F and DEL), and C1 (U+0080-U+009F, of which U+009B is CSI and
/// terminals in UTF-8 mode obey it) -- is untrusted input landing as a control
/// sequence. What saved this before was incidental: ratatui's
/// `Buffer::set_stringn` skips zero-width graphemes, so an ESC happened never
/// to become a cell. That is a property of a third-party crate and not a
/// decision anybody here made.
///
/// # Why a second escaper rather than the one that already exists
///
/// `linkstate::render_held` does this same job for held input, and it lives in
/// the ROOT crate -- which DEPENDS on this one, so it cannot be called from
/// here, and moving it across is restructuring rather than a fix. The
/// alternative was to sanitise at the call site in the root crate, before the
/// reason reaches the popup. This is the better half of that trade: it leaves
/// the escaping owned by the function that already owns "make this reason
/// fit", so no future caller can reintroduce the hole by not knowing about it.
/// The two escapers deliberately use the same vocabulary -- `^X`, `^?`, `<9B>` --
/// so a user who has seen one recognises the other. `render_held` is not a
/// drop-in either way: it takes bytes and applies the held-input cap, and this
/// takes a `&str` and applies [`REASON_SHOWN`].
pub fn legible(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for ch in line.chars() {
        match ch {
            // Every C0 except the ones `lines()` and `trim()` have already
            // removed, plus DEL.
            '\u{0}'..='\u{1f}' => {
                out.push('^');
                out.push((ch as u8 + b'@') as char);
            }
            '\u{7f}' => out.push_str("^?"),
            '\u{80}'..='\u{9f}' => out.push_str(&format!("<{:02X}>", ch as u32)),
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{Attrs, Color as PColor};

    fn view() -> PopupView {
        PopupView {
            title: "oxutrm \u{b7} bastion".to_string(),
            marker: Marker::Silent,
            marker_text: "\u{25cf} SILENT".to_string(),
            held: vec![],
            recovering: vec![],
            status: vec![
                "silent for 6s".to_string(),
                "rtt \u{2014} \u{b7} min 30 / avg 35 / max 52 ms".to_string(),
            ],
            standby: vec!["standby: searching\u{2026}".to_string()],
            spark: vec![Some(30), Some(40), None, Some(52)],
            log: (1..=30).map(|i| format!("event-{i:02}")).collect(),
            keys: vec![
                KeyHint {
                    key: "q".to_string(),
                    label: "quit".to_string(),
                    enabled: true,
                },
                KeyHint {
                    key: "c".to_string(),
                    label: "config".to_string(),
                    enabled: false,
                },
            ],
        }
    }

    fn row(o: &Overlay, r: u16) -> String {
        (0..o.cols)
            .map(|c| {
                o.cells[r as usize * o.cols as usize + c as usize]
                    .text
                    .to_string()
            })
            .collect()
    }

    fn text_of(o: &Overlay) -> String {
        (0..o.rows)
            .map(|r| row(o, r))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn cell_of<'a>(o: &'a Overlay, r: u16, ch: &str) -> &'a oxutrm_proto::Cell {
        let start = r as usize * o.cols as usize;
        o.cells[start..start + o.cols as usize]
            .iter()
            .find(|c| c.text == ch)
            .unwrap_or_else(|| panic!("no {ch:?} on row {r}: {:?}", row(o, r)))
    }

    #[test]
    fn a_popup_is_centred_and_capped() {
        for (cols, rows, want) in [
            (80u16, 24u16, (72u16, 22u16, 1u16, 4u16)),
            (200, 60, (72, 24, 18, 64)),
            (40, 12, (36, 10, 1, 2)),
        ] {
            let o = layout_popup(&view(), TermSize { cols, rows });
            assert_eq!((o.cols, o.rows, o.row, o.col), want, "on {cols}x{rows}");
        }
    }

    #[test]
    fn a_popup_never_exceeds_the_screen() {
        for (cols, rows) in [(80u16, 24u16), (20, 6), (21, 7), (200, 60), (24, 8)] {
            let o = layout_popup(&view(), TermSize { cols, rows });
            assert!(
                o.cols <= cols && o.rows <= rows,
                "{o:?} exceeds {cols}x{rows}"
            );
            assert!(o.row + o.rows <= rows && o.col + o.cols <= cols);
            assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
        }
    }

    /// The marker is the first row inside the border, the key bar the last:
    /// asserted together, so a layout that put everything on one row could
    /// not pass either.
    #[test]
    fn the_marker_leads_and_the_key_bar_closes_the_box() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        assert!(row(&o, 1).contains("\u{25cf} SILENT"), "{}", text_of(&o));
        assert!(!row(&o, 1).contains("quit"), "{}", text_of(&o));
        assert!(row(&o, o.rows - 2).contains("q quit"), "{}", text_of(&o));
        assert!(row(&o, o.rows - 2).contains("c config"), "{}", text_of(&o));
    }

    #[test]
    fn the_newest_log_entry_is_at_the_bottom_and_the_oldest_give_way() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        assert!(row(&o, o.rows - 3).contains("event-30"), "{text}");
        assert!(row(&o, o.rows - 4).contains("event-29"), "{text}");
        assert!(
            !text.contains("event-01"),
            "thirty entries fitted into a box with fewer rows than that: {text}"
        );
    }

    /// On a short screen the held section -- the question the user has to
    /// answer -- survives, and the status block is what gives way.
    #[test]
    fn the_held_section_survives_a_short_screen() {
        let mut v = view();
        v.held = vec![
            "You typed 10 bytes while offline:".to_string(),
            "make test\u{21b5}".to_string(),
        ];
        let tall = text_of(&layout_popup(&v, TermSize { cols: 80, rows: 24 }));
        assert!(
            tall.contains("silent for 6s"),
            "the fixture shows no status: {tall}"
        );

        // 41 columns: the 33-character held line fits the 33-column interior
        // on one row; at 40 it wraps and the second line is clipped.
        let short = text_of(&layout_popup(&v, TermSize { cols: 41, rows: 8 }));
        assert!(short.contains("make test"), "{short}");
        assert!(short.contains("q quit"), "the key bar was clipped: {short}");
        assert!(
            !short.contains("silent for 6s"),
            "nothing gave way: {short}"
        );
    }

    #[test]
    fn the_marker_is_coloured_by_state() {
        for (marker, want) in [
            (Marker::Live, PColor::Idx(2)),
            (Marker::LiveAgain, PColor::Idx(2)),
            (Marker::Silent, PColor::Idx(3)),
            (Marker::Recovering, PColor::Idx(1)),
        ] {
            let v = PopupView { marker, ..view() };
            let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
            assert_eq!(cell_of(&o, 1, "\u{25cf}").fg, want, "for {marker:?}");
        }
    }

    #[test]
    fn a_key_not_offered_yet_is_dimmed_and_one_that_works_is_not() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        let bar = o.rows - 2;
        assert!(cell_of(&o, bar, "c").attrs.contains(Attrs::DIM));
        let q = cell_of(&o, bar, "q");
        assert!(!q.attrs.contains(Attrs::DIM));
        assert!(q.attrs.contains(Attrs::BOLD));
    }

    /// An outage second has no RTT, and must read as a gap rather than as a
    /// zero-height bar, which would claim a very fast link.
    #[test]
    fn an_outage_second_is_a_gap_in_the_sparkline() {
        let v = PopupView {
            spark: vec![Some(10), None, Some(10)],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        // The status row starts with "rtt " too; it is the one with "ms".
        let line = (0..o.rows)
            .map(|r| row(&o, r))
            .find(|l| l.contains(SPARK_LABEL) && !l.contains("ms"))
            .expect("no sparkline row");
        let at = line.find(SPARK_LABEL).unwrap() + SPARK_LABEL.len();
        let bars: Vec<char> = line[at..].chars().take(3).collect();
        assert_ne!(bars[0], ' ', "{line:?}");
        assert_eq!(bars[1], ' ', "the outage second was drawn: {line:?}");
        assert_ne!(bars[2], ' ', "{line:?}");
    }

    /// The bars are scaled to the largest sample shown, so the biggest one is
    /// a full cell and a quarter of it is visibly lower.
    #[test]
    fn the_sparkline_is_scaled_to_its_largest_sample() {
        let v = PopupView {
            spark: vec![Some(10), Some(40)],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let line = (0..o.rows)
            .map(|r| row(&o, r))
            .find(|l| l.contains(SPARK_LABEL) && !l.contains("ms"))
            .expect("no sparkline row");
        let at = line.find(SPARK_LABEL).unwrap() + SPARK_LABEL.len();
        let bars: Vec<char> = line[at..].chars().take(2).collect();
        assert_eq!(
            bars[1], '\u{2588}',
            "the largest sample is not a full bar: {line:?}"
        );
        assert!(
            bars[0] != ' ' && bars[0] < bars[1],
            "the smaller sample is not a lower bar: {line:?}"
        );
    }

    #[test]
    fn a_screen_below_the_minimum_gets_one_line() {
        let o = layout_popup(&view(), TermSize { cols: 19, rows: 5 });
        assert_eq!((o.rows, o.row, o.col, o.cols), (1, 0, 0, 19));
        assert!(
            text_of(&o).contains("oxutrm: \u{25cf} SILENT"),
            "{}",
            text_of(&o)
        );
        assert!(o.cells[0].attrs.contains(Attrs::INVERSE));
    }

    /// A terminal reports 1x1 transiently while some emulators tear down.
    #[test]
    fn a_one_by_one_screen_does_not_panic() {
        let o = layout_popup(&view(), TermSize { cols: 1, rows: 1 });
        assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
    }

    #[test]
    fn snapshot_80x24() {
        insta::assert_snapshot!(text_of(&layout_popup(
            &view(),
            TermSize { cols: 80, rows: 24 }
        )));
    }

    #[test]
    fn snapshot_40x12() {
        insta::assert_snapshot!(text_of(&layout_popup(
            &view(),
            TermSize { cols: 40, rows: 12 }
        )));
    }

    #[test]
    fn snapshot_19x5() {
        insta::assert_snapshot!(text_of(&layout_popup(
            &view(),
            TermSize { cols: 19, rows: 5 }
        )));
    }

    /// The reason is a REMOTE's stderr. A terminal must never act on it.
    #[test]
    fn control_scalars_in_a_reason_are_shown_rather_than_emitted() {
        let shown = summarised("ssh said: \u{1b}[2Jcleared\u{9b}31m and \u{7} rang");
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(shown.contains("^[") && shown.contains("^G"), "{shown:?}");
        assert!(shown.contains("<9B>"), "{shown:?}");
        assert!(
            shown.contains("ssh said:") && shown.contains("cleared"),
            "{shown:?}"
        );
    }

    /// ssh says why it failed on its first line and pads afterwards.
    #[test]
    fn only_the_first_line_of_a_reason_is_shown() {
        let shown = summarised("Permission denied (publickey).\nbanner line nobody needs");
        assert!(shown.contains("Permission denied"), "{shown}");
        assert!(!shown.contains("banner line"), "{shown}");
    }

    /// Against `REASON_SHOWN` itself, plus the ellipsis: a loose bound would
    /// pass for every cap between the two and pin none of them.
    #[test]
    fn a_long_reason_is_cut_at_the_cap_and_keeps_its_front() {
        let shown = summarised(&format!("ssh said: {}", "noise ".repeat(400)));
        assert_eq!(shown.chars().count(), REASON_SHOWN + "...".len(), "{shown}");
        assert!(shown.starts_with("ssh said:"), "{shown}");
        let short = summarised("ssh said: short");
        assert_eq!(short, "ssh said: short", "a short reason was cut");
    }
}

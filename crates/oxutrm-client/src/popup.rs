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

/// The cells between the marker and the rest of the header line.
const HEADER_GAP: &str = "   ";

/// The cells between the rtt text and its sparkline.
const SPARK_GAP: u16 = 2;

/// The cells between an attempt row's label and its state.
const ATTEMPT_GAP: u16 = 3;

/// The cells between a log entry's time and its text.
const LOG_GAP: u16 = 2;

/// The title of the rule above the standby section.
const STANDBY_RULE: &str = "standby";

/// The title of the rule above the activity log.
const RECENT_RULE: &str = "recent";

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

/// A row of two columns: a label, and a text that wraps under itself, so a
/// continuation line visibly belongs to its row and the label column stays
/// clear.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Row {
    pub label: String,
    pub text: String,
}

impl Row {
    pub fn new(label: impl Into<String>, text: impl Into<String>) -> Row {
        Row {
            label: label.into(),
            text: text.into(),
        }
    }
}

/// What the popup says, as content rather than as cells.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PopupView {
    /// Drawn into the top border.
    pub title: String,
    pub marker: Marker,
    /// The start of the first line inside the box, e.g. `● SILENT`, drawn
    /// in the marker's colour.
    pub marker_text: String,
    /// The rest of the first line, after the marker: how the link is
    /// reached, or how long it has been silent.
    pub header: String,
    /// During an outage, one row per means of recovery, each with its own
    /// state and clock.
    pub attempts: Vec<Row>,
    /// What was typed while the link was down, and what can be done with it.
    pub held: Vec<String>,
    /// The rtt row's text; the sparkline fills the rest of that row.
    pub rtt: String,
    /// Round-trip time in milliseconds, oldest first; `None` is a gap.
    pub spark: Vec<Option<u64>>,
    /// Link quality below the rtt row, one row each.
    pub quality: Vec<String>,
    /// The standby section, when the host offered one.
    pub standby: Vec<String>,
    /// The activity log, oldest first: a time, and what happened.
    pub log: Vec<Row>,
    pub keys: Vec<KeyHint>,
    /// What a screen below [`MIN_BOX`] shows on its one line, longest
    /// first: the first that fits the width is used, and the last is cut
    /// when none does. Empty means `oxutrm: <marker_text>`.
    pub line: Vec<String>,
}

/// Lay the popup out for this screen, as cells ready to composite.
///
/// `min(cols - 4, 72)` by `min(rows - 2, 24)`, centred. Top to bottom: the
/// header (marker and what follows it), the attempts block, the held
/// section, the rtt row with its sparkline, the quality rows, then the
/// `standby` and `recent` sections, each under a rule drawn into the
/// border, and a plain rule above the key bar. The key bar always has the
/// last row inside the border, because it is how the popup is left.
///
/// A short screen gives way in this order of priority: header, held
/// section (the question the user has to answer), attempts, rtt, quality,
/// the plain rule, the standby section, the log. A section's rule is drawn
/// only with at least one row of the section under it. The log takes the
/// rows left, its newest entry last and its oldest giving way.
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
    body = place(&[header_line(v)], body, &mut buf);

    // The held section is placed below the attempts but sized before them,
    // so on a short screen it is the attempts that give way.
    let held = plain(&v.held);
    let held_rows = wrapped_row_count(&held, body.width, body.height);
    let above_held = Rect {
        height: body.height - held_rows,
        ..body
    };
    let after_attempts = place_rows(&v.attempts, ATTEMPT_GAP, above_held, &mut buf);
    body = Rect {
        height: body.bottom() - after_attempts.y,
        ..after_attempts
    };
    body = place(&held, body, &mut buf);

    body = place_rtt(&v.rtt, &v.spark, body, &mut buf);
    body = place(&plain(&v.quality), body, &mut buf);

    if body.height == 0 {
        return overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2);
    }
    rule(&mut buf, body.bottom() - 1, cols, None);
    body.height -= 1;

    if !v.standby.is_empty() && body.height >= 2 {
        rule(&mut buf, body.y, cols, Some(STANDBY_RULE));
        body = place(&plain(&v.standby), below(body, 1), &mut buf);
    }
    if !v.log.is_empty() && body.height >= 2 {
        rule(&mut buf, body.y, cols, Some(RECENT_RULE));
        place_log(&v.log, below(body, 1), &mut buf);
    }

    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

/// `area` without its top `n` rows.
fn below(area: Rect, n: u16) -> Rect {
    let n = n.min(area.height);
    Rect {
        y: area.y + n,
        height: area.height - n,
        ..area
    }
}

fn header_line(v: &PopupView) -> Line<'static> {
    let mut spans = vec![Span::styled(
        v.marker_text.clone(),
        Style::default()
            .fg(v.marker.color())
            .add_modifier(Modifier::BOLD),
    )];
    if !v.header.is_empty() {
        spans.push(Span::raw(HEADER_GAP));
        spans.push(Span::raw(v.header.clone()));
    }
    Line::from(spans)
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
    below(area, used)
}

/// A horizontal rule across the whole box at `y`, joined into both sides of
/// the border, with `title` drawn into it the way the top border carries
/// the popup's.
fn rule(buf: &mut Buffer, y: u16, cols: u16, title: Option<&str>) {
    let last = cols - 1;
    buf[(0, y)].set_symbol("\u{251c}");
    for x in 1..last {
        buf[(x, y)].set_symbol("\u{2500}");
    }
    buf[(last, y)].set_symbol("\u{2524}");
    if let Some(t) = title {
        buf.set_stringn(
            1,
            y,
            format!(" {t} "),
            usize::from(cols.saturating_sub(2)),
            Style::default(),
        );
    }
}

/// The column a block of rows' texts start in: the widest label, then `gap`.
/// Zero when that leaves the text too little room to be worth a column, so
/// a narrow box runs label and text together instead.
fn text_column(rows: &[Row], gap: u16, width: u16) -> u16 {
    let widest = rows
        .iter()
        .map(|r| Line::from(r.label.as_str()).width())
        .max()
        .unwrap_or(0);
    let col = u16::try_from(widest)
        .unwrap_or(u16::MAX)
        .saturating_add(gap);
    if col.saturating_add(8) > width {
        0
    } else {
        col
    }
}

/// How many rows `row` takes at `col` in `width`, capped at `height`.
fn row_height(row: &Row, col: u16, width: u16, height: u16) -> u16 {
    if col == 0 {
        let line = Line::from(format!("{} {}", row.label, row.text));
        return wrapped_row_count(&[line], width, height).max(1);
    }
    wrapped_row_count(&[Line::from(row.text.clone())], width - col, height).max(1)
}

/// Draw `row` at the top of `area`, `height` rows of it.
fn draw_row(row: &Row, col: u16, area: Rect, height: u16, buf: &mut Buffer) {
    if col == 0 {
        let line = Line::from(format!("{} {}", row.label, row.text));
        Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .render(Rect { height, ..area }, buf);
        return;
    }
    buf.set_stringn(
        area.x,
        area.y,
        &row.label,
        usize::from(col),
        Style::default(),
    );
    Paragraph::new(Line::from(row.text.clone()))
        .wrap(Wrap { trim: false })
        .render(
            Rect {
                x: area.x + col,
                width: area.width - col,
                height,
                ..area
            },
            buf,
        );
}

/// Rows at the top of `area`, each text hanging under itself; what does not
/// fit is cut from the bottom. Returns the part of `area` left below.
fn place_rows(rows: &[Row], gap: u16, mut area: Rect, buf: &mut Buffer) -> Rect {
    let col = text_column(rows, gap, area.width);
    for row in rows {
        if area.height == 0 {
            break;
        }
        let h = row_height(row, col, area.width, area.height);
        draw_row(row, col, area, h, buf);
        area = below(area, h);
    }
    area
}

/// The rtt row: its text, then one cell per sample filling the rest of the
/// row, the newest at the right. Returns the part of `area` left below.
fn place_rtt(text: &str, spark: &[Option<u64>], area: Rect, buf: &mut Buffer) -> Rect {
    let samples = spark.iter().any(Option::is_some);
    if area.height == 0 || (text.is_empty() && !samples) {
        return area;
    }
    let row = Rect { height: 1, ..area };
    Paragraph::new(text.to_string()).render(row, buf);
    let used = u16::try_from(Line::from(text).width())
        .unwrap_or(u16::MAX)
        .saturating_add(SPARK_GAP);
    if samples && used < area.width {
        let width = area.width - used;
        let shown = &spark[spark.len().saturating_sub(usize::from(width))..];
        let max = shown.iter().flatten().copied().max().unwrap_or(1).max(1);
        Sparkline::default()
            .data(shown.to_vec())
            .max(max)
            .absent_value_symbol(" ")
            .render(
                Rect {
                    x: area.x + used,
                    width,
                    ..row
                },
                buf,
            );
    }
    below(area, 1)
}

/// The log fills `area` from the top, newest last; entries that do not fit
/// give way oldest first. A long entry wraps under its own text and takes
/// the rows it needs.
fn place_log(log: &[Row], area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let col = text_column(log, LOG_GAP, area.width);
    let mut kept: Vec<(&Row, u16)> = Vec::new();
    let mut used = 0u16;
    for entry in log.iter().rev() {
        let rows = row_height(entry, col, area.width, area.height);
        if used + rows > area.height {
            break;
        }
        used += rows;
        kept.push((entry, rows));
    }
    let mut at = area;
    for (entry, rows) in kept.into_iter().rev() {
        draw_row(entry, col, at, rows, buf);
        at = below(at, rows);
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
    let marker = [format!("oxutrm: {}", v.marker_text)];
    let texts = if v.line.is_empty() {
        &marker[..]
    } else {
        &v.line[..]
    };
    let text = texts
        .iter()
        .find(|t| Line::from(t.as_str()).width() <= usize::from(area.width))
        .or(texts.last())
        .cloned()
        .unwrap_or_default();
    Paragraph::new(Line::from(Span::styled(
        text,
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
            title: "oxutrm \u{b7} bastion \u{b7} f00dcafe".to_string(),
            marker: Marker::Silent,
            marker_text: "\u{25cf} SILENT".to_string(),
            header: "silent 6 s".to_string(),
            attempts: vec![
                Row::new("standby probe", "probing\u{2026}"),
                Row::new("ssh rebuild", "in 14 s"),
            ],
            held: vec![],
            rtt: "rtt   \u{2014}  (30\u{2013}52)".to_string(),
            spark: vec![Some(30), Some(40), None, Some(52)],
            quality: vec!["loss  0.0 %  \u{2191} 0 B/s  \u{2193} 0 B/s".to_string()],
            standby: vec!["searching\u{2026}".to_string()],
            log: (1..=30)
                .map(|i| Row::new("14:13", format!("event-{i:02}")))
                .collect(),
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
            line: vec![],
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

    /// The first row containing `what`.
    fn find(o: &Overlay, what: &str) -> Option<u16> {
        (0..o.rows).find(|&r| row(o, r).contains(what))
    }

    /// A rule: joined into both sides of the border, `─` in between, and
    /// `title` drawn into it when there is one.
    fn is_rule(line: &str, title: Option<&str>) -> bool {
        let chars: Vec<char> = line.chars().collect();
        let (Some(&first), Some(&last)) = (chars.first(), chars.last()) else {
            return false;
        };
        if first != '\u{251c}' || last != '\u{2524}' {
            return false;
        }
        let middle: String = chars[1..chars.len() - 1].iter().collect();
        match title {
            Some(t) => {
                let head = format!(" {t} ");
                middle.starts_with(&head) && middle[head.len()..].chars().all(|c| c == '\u{2500}')
            }
            None => middle.chars().all(|c| c == '\u{2500}'),
        }
    }

    fn rule_row(o: &Overlay, title: Option<&str>) -> Option<u16> {
        (0..o.rows).find(|&r| is_rule(&row(o, r), title))
    }

    /// The bars right after the rtt text and its gap, `n` of them.
    fn bars(o: &Overlay, v: &PopupView, n: usize) -> Vec<char> {
        let r = find(o, &v.rtt).unwrap_or_else(|| panic!("no rtt row: {}", text_of(o)));
        let line = row(o, r);
        let at = line.find(&v.rtt).unwrap() + v.rtt.len();
        line[at..]
            .chars()
            .skip(SPARK_GAP as usize)
            .take(n)
            .collect()
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

    /// The marker and the rest of the header share the first row inside the
    /// border, the key bar has the last: asserted together, so a layout
    /// that put everything on one row could not pass either.
    #[test]
    fn the_header_leads_and_the_key_bar_closes_the_box() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        assert!(
            row(&o, 1).contains("\u{25cf} SILENT   silent 6 s"),
            "{}",
            text_of(&o)
        );
        assert!(!row(&o, 1).contains("quit"), "{}", text_of(&o));
        assert!(row(&o, o.rows - 2).contains("q quit"), "{}", text_of(&o));
        assert!(row(&o, o.rows - 2).contains("c config"), "{}", text_of(&o));
    }

    /// The sections are told apart by rules drawn into the border: one
    /// titled for each section, and a plain one right above the key bar.
    #[test]
    fn the_sections_are_separated_by_rules_in_the_border() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        let standby = rule_row(&o, Some("standby")).unwrap_or_else(|| panic!("{text}"));
        let recent = rule_row(&o, Some("recent")).unwrap_or_else(|| panic!("{text}"));
        let plain = rule_row(&o, None).unwrap_or_else(|| panic!("{text}"));
        assert_eq!(
            plain,
            o.rows - 3,
            "the plain rule is not above the keys: {text}"
        );
        assert_eq!(
            find(&o, "searching"),
            Some(standby + 1),
            "the standby section is not under its rule: {text}"
        );
        assert!(standby < recent && recent < plain, "{text}");
    }

    #[test]
    fn the_newest_log_entry_is_at_the_bottom_and_the_oldest_give_way() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        assert!(row(&o, o.rows - 4).contains("event-30"), "{text}");
        assert!(row(&o, o.rows - 5).contains("event-29"), "{text}");
        assert!(
            !text.contains("event-01"),
            "thirty entries fitted into a box with fewer rows than that: {text}"
        );
    }

    /// A few entries sit right under their rule, not at the far end of an
    /// empty section.
    #[test]
    fn a_short_log_starts_right_under_its_rule() {
        let v = PopupView {
            log: vec![Row::new("14:13", "only-one")],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let recent = rule_row(&o, Some("recent")).unwrap();
        assert_eq!(find(&o, "only-one"), Some(recent + 1), "{}", text_of(&o));
    }

    /// The time column stays clear: a wrapped entry's continuation starts
    /// under its own text, so it visibly belongs to it.
    #[test]
    fn a_wrapped_log_entry_hangs_under_its_text() {
        let long = format!("FIRST {} LAST", "word ".repeat(20));
        let v = PopupView {
            log: vec![Row::new("14:13", long)],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let r = find(&o, "FIRST").unwrap_or_else(|| panic!("{}", text_of(&o)));
        let first = row(&o, r);
        let next = row(&o, r + 1);
        let time_at = first
            .find("14:13")
            .expect("no time on the entry's first row");
        let text_at = first.find("FIRST").unwrap();
        assert_eq!(
            text_at - time_at,
            "14:13".len() + LOG_GAP as usize,
            "{first:?}"
        );
        assert!(
            next[time_at..text_at].chars().all(|c| c == ' '),
            "the continuation runs into the time column: {next:?}"
        );
        assert_ne!(
            next[text_at..].chars().next(),
            Some(' '),
            "the continuation does not start under the text: {next:?}"
        );
        assert!(next.contains("word") || next.contains("LAST"), "{next:?}");
    }

    /// The attempts block lines its states up in one column, and a reason
    /// row with no label hangs in that same column.
    #[test]
    fn the_attempts_line_their_states_up() {
        let v = PopupView {
            attempts: vec![
                Row::new("standby probe", "no answer"),
                Row::new("ssh rebuild", "attempt 1 failed"),
                Row::new("", "REASON"),
            ],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let col = |what: &str| {
            let r = find(&o, what).unwrap_or_else(|| panic!("no {what:?}: {}", text_of(&o)));
            row(&o, r).find(what).unwrap()
        };
        let want = col("standby probe") + "standby probe".len() + ATTEMPT_GAP as usize;
        assert_eq!(col("no answer"), want, "{}", text_of(&o));
        assert_eq!(col("attempt 1 failed"), want, "{}", text_of(&o));
        assert_eq!(col("REASON"), want, "{}", text_of(&o));
    }

    /// On a short screen the held section -- the question the user has to
    /// answer -- survives, and the attempts and everything below give way.
    #[test]
    fn the_held_section_survives_a_short_screen() {
        let mut v = view();
        v.held = vec![
            "You typed 10 bytes while offline:".to_string(),
            "make test\u{21b5}".to_string(),
        ];
        let tall = text_of(&layout_popup(&v, TermSize { cols: 80, rows: 24 }));
        assert!(
            tall.contains("ssh rebuild") && tall.contains("rtt"),
            "the fixture shows no attempts: {tall}"
        );

        // 41 columns: the 33-character held line fits the 33-column interior
        // on one row; at 40 it wraps and the second line is clipped.
        let short = text_of(&layout_popup(&v, TermSize { cols: 41, rows: 8 }));
        assert!(short.contains("make test"), "{short}");
        assert!(short.contains("q quit"), "the key bar was clipped: {short}");
        assert!(!short.contains("ssh rebuild"), "nothing gave way: {short}");
        assert!(
            !short.contains("event-") && !short.contains("\u{251c}"),
            "the body was not exhausted, a log entry or rule is shown: {short}"
        );
    }

    /// A rule costs a row, and one with nothing under it says nothing: it
    /// goes with its section. One row short of the standby section's two,
    /// neither shows; given the row, the section does, and the log does not.
    #[test]
    fn a_rule_goes_with_its_section_when_no_row_is_left_under_it() {
        let v = PopupView {
            attempts: vec![],
            ..view()
        };
        // Header, rtt, loss, one free row, the plain rule, the key bar: six
        // inside the border, so eight for the box and ten for the screen.
        let short = layout_popup(&v, TermSize { cols: 80, rows: 10 });
        let text = text_of(&short);
        assert!(rule_row(&short, None).is_some(), "{text}");
        assert!(rule_row(&short, Some("standby")).is_none(), "{text}");
        assert!(!text.contains("searching"), "{text}");

        let more = layout_popup(&v, TermSize { cols: 80, rows: 11 });
        let text = text_of(&more);
        assert!(rule_row(&more, Some("standby")).is_some(), "{text}");
        assert!(text.contains("searching"), "{text}");
        assert!(rule_row(&more, Some("recent")).is_none(), "{text}");
        assert!(!text.contains("event-"), "{text}");
    }

    /// The order of the body, top to bottom. Wide log entries mean a log
    /// drawn first would show past the shorter rows, so drawing order is
    /// guarded too.
    #[test]
    fn the_sections_run_in_the_documented_order() {
        let v = PopupView {
            attempts: vec![Row::new("ATTEMPT-LABEL", "x")],
            held: vec!["HELD-LINE".to_string()],
            rtt: "RTT-LINE".to_string(),
            quality: vec!["QUALITY-LINE".to_string()],
            standby: vec!["STANDBY-LINE".to_string()],
            log: (1..=30)
                .map(|i| Row::new("14:13", format!("{:.<40} LOG-ENTRY-{i:02}", "")))
                .collect(),
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        let at = |what: &str| find(&o, what).unwrap_or_else(|| panic!("no {what:?} in: {text}"));
        let order = [
            at("SILENT"),
            at("ATTEMPT-LABEL"),
            at("HELD-LINE"),
            at("RTT-LINE"),
            at("QUALITY-LINE"),
            rule_row(&o, Some("standby")).unwrap(),
            at("STANDBY-LINE"),
            rule_row(&o, Some("recent")).unwrap(),
            at("LOG-ENTRY"),
            rule_row(&o, None).unwrap(),
            at("q quit"),
        ];
        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "rows {order:?} are not strictly increasing: {text}"
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
            // The marker's colour stops at the marker.
            assert_ne!(cell_of(&o, 1, "6").fg, want, "for {marker:?}");
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
        let b = bars(&o, &v, 3);
        assert_ne!(b[0], ' ', "{b:?}");
        assert_eq!(b[1], ' ', "the outage second was drawn: {b:?}");
        assert_ne!(b[2], ' ', "{b:?}");
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
        let b = bars(&o, &v, 2);
        assert_eq!(
            b[1], '\u{2588}',
            "the largest sample is not a full bar: {b:?}"
        );
        assert!(
            b[0] != ' ' && b[0] < b[1],
            "the smaller sample is not a lower bar: {b:?}"
        );
    }

    /// More samples than room: the sparkline fills the rtt row to its end,
    /// the newest sample at the right, and the oldest give way.
    #[test]
    fn the_sparkline_fills_the_rest_of_the_rtt_row() {
        // Rising, so the newest is the largest and the only full bar.
        let v = PopupView {
            spark: (1..=200).map(Some).collect(),
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let r = find(&o, &v.rtt).unwrap();
        let line: Vec<char> = row(&o, r).chars().collect();
        // Border, padding: the last bar is two cells in from the end.
        let last = line[line.len() - 3];
        assert_eq!(
            last, '\u{2588}',
            "the newest sample is not at the right: {line:?}"
        );
        assert_eq!(line[line.len() - 2], ' ', "{line:?}");
        let b = bars(&o, &v, 1);
        assert_ne!(
            b[0], ' ',
            "the sparkline does not start after the text: {line:?}"
        );
        assert_eq!(
            find(&o, "\u{2588}"),
            Some(r),
            "the sparkline has a row of its own: {}",
            text_of(&o)
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

    /// Final review, Minor 1: a single line that has something to ask
    /// shows the longest of its texts that fits, so a narrow screen keeps
    /// the keys rather than the start of a sentence.
    #[test]
    fn the_single_line_shows_the_longest_text_that_fits() {
        let v = PopupView {
            line: vec![
                "oxutrm: \u{25cf} LIVE \u{b7} send typed input? s send / d drop".to_string(),
                "s send / d drop".to_string(),
            ],
            ..view()
        };
        let wide = layout_popup(&v, TermSize { cols: 80, rows: 5 });
        assert_eq!(
            row(&wide, 0).trim_end(),
            "oxutrm: \u{25cf} LIVE \u{b7} send typed input? s send / d drop"
        );
        let narrow = layout_popup(&v, TermSize { cols: 19, rows: 5 });
        assert_eq!(row(&narrow, 0).trim_end(), "s send / d drop");
        assert!(narrow.cells[0].attrs.contains(Attrs::INVERSE));
        // Nothing fits: the last is cut, as the marker always was.
        let tiny = layout_popup(&v, TermSize { cols: 6, rows: 5 });
        assert_eq!(row(&tiny, 0), "s send");
    }

    /// A terminal reports 1x1 transiently while some emulators tear down.
    #[test]
    fn a_one_by_one_screen_does_not_panic() {
        let o = layout_popup(&view(), TermSize { cols: 1, rows: 1 });
        assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
    }

    /// Every size from the minimum box up lays out without a panic: the
    /// rules, the hanging rows and the reservation arithmetic all subtract.
    #[test]
    fn every_small_box_lays_out() {
        let mut v = view();
        v.held = vec!["You typed 10 bytes while offline:".to_string()];
        v.attempts.push(Row::new(
            "",
            "a reason long enough to wrap twice in a narrow box",
        ));
        for cols in MIN_BOX.cols..40 {
            for rows in MIN_BOX.rows..16 {
                let o = layout_popup(&v, TermSize { cols, rows });
                assert!(
                    row(&o, o.rows - 2).contains("q"),
                    "{cols}x{rows}: {}",
                    text_of(&o)
                );
            }
        }
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

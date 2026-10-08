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
/// `min(cols - 4, 72)` wide and as tall as what it says, up to
/// `min(rows - 2, 24)`; centred either way. Top to bottom: the header
/// (marker and what follows it), the attempts block, the held section, the
/// rtt row with its sparkline, the quality rows, then the `standby` and
/// `recent` sections, each under a rule drawn into the border, and a plain
/// rule above the key bar. The key bar always has the last row inside the
/// border, because it is how the popup is left.
///
/// A short screen gives way in this order of priority: header, held
/// section (the question the user has to answer), attempts, rtt, quality,
/// the plain rule, the standby section, the log. While there is held text
/// the header keeps to one row, cut rather than wrapped, so it cannot take
/// a row the held text needs. A section's rule is drawn only with at least
/// one row of the section under it. The log takes the rows left, its newest
/// entry last and its oldest giving way.
pub fn layout_popup(v: &PopupView, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        return single_line(v, size);
    }
    // At least 16x4 here, so the inner area below is at least 12x2.
    let cols = (size.cols - 4).min(MAX_BOX.cols);
    let cap = (size.rows - 2).min(MAX_BOX.rows);
    // Laid out once at the tallest the screen allows to learn how many rows
    // the content takes, then again at that height: a box that fits its
    // content lays it out exactly as the tall one did.
    let (_, used) = draw(v, cols, cap);
    let rows = used.saturating_add(2).clamp(4, cap);
    let (buf, _) = draw(v, cols, rows);
    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

/// The box drawn `cols` by `rows`, and how many rows inside its border the
/// content needs: everything drawn, the plain rule and the key bar.
fn draw(v: &PopupView, cols: u16, rows: u16) -> (Buffer, u16) {
    let (mut buf, above_bar) = framed(&v.title, key_line(&v.keys), cols, rows);
    let mut body = above_bar;
    body = if v.held.is_empty() {
        place(&[header_line(v)], body, &mut buf)
    } else {
        place_one(header_line(v), body, &mut buf)
    };

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
        return (buf, above_bar.height + 1);
    }
    rule(&mut buf, body.bottom() - 1, cols, None);
    body.height -= 1;
    // Where the content ends: the plain rule moves up to it in a box that
    // is not as tall as the screen allows.
    let mut end = body.y;

    if !v.standby.is_empty() && body.height >= 2 {
        rule(&mut buf, body.y, cols, Some(STANDBY_RULE));
        body = place(&plain(&v.standby), below(body, 1), &mut buf);
        end = body.y;
    }
    if !v.log.is_empty() && body.height >= 2 {
        rule(&mut buf, body.y, cols, Some(RECENT_RULE));
        let log = below(body, 1);
        end = log.y + place_log(&v.log, log, &mut buf);
    }

    // The content, the plain rule, the key bar.
    (buf, end - above_bar.y + 2)
}

/// What the box's frame takes of its width: the border and a cell of
/// padding either side.
const FRAME_COLS: u16 = 4;

/// The box every popup screen is drawn in, `cols` by `rows`: a rounded
/// border with `title` drawn into its top, a cell of padding either side,
/// and `keys` on the last row inside -- how the screen is left, so the
/// last row to give way. Returns the area inside the border above the key
/// bar. At least `FRAME_COLS` by 3.
fn framed(title: &str, keys: Line<'static>, cols: u16, rows: u16) -> (Buffer, Rect) {
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(format!(" {title} "));
    let inner = block.inner(area);
    block.render(area, &mut buf);
    let bar = inner.bottom() - 1;
    Paragraph::new(keys).render(
        Rect {
            y: bar,
            height: 1,
            ..inner
        },
        &mut buf,
    );
    (
        buf,
        Rect {
            height: inner.height - 1,
            ..inner
        },
    )
}

/// `line` on the top row of `area`, cut at its width; return the part of
/// `area` left below it.
fn place_one(line: Line<'static>, area: Rect, buf: &mut Buffer) -> Rect {
    if area.height == 0 {
        return area;
    }
    Paragraph::new(line).render(Rect { height: 1, ..area }, buf);
    below(area, 1)
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
        return wrapped_row_count(&[run_together(row)], width, height).max(1);
    }
    wrapped_row_count(&[Line::from(row.text.clone())], width - col, height).max(1)
}

/// `row` as one line, for a box with no room for a label column: the label,
/// a space, the text -- or the text alone when there is no label.
fn run_together(row: &Row) -> Line<'static> {
    if row.label.is_empty() {
        Line::from(row.text.clone())
    } else {
        Line::from(format!("{} {}", row.label, row.text))
    }
}

/// Draw `row` at the top of `area`, `height` rows of it.
fn draw_row(row: &Row, col: u16, area: Rect, height: u16, buf: &mut Buffer) {
    if col == 0 {
        Paragraph::new(run_together(row))
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
/// row, the newest at the right edge -- with fewer samples than cells, the
/// empty cells are on the left. Returns the part of `area` left below.
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
        // ratatui draws its data from the left; padded in front, the newest
        // sample lands in the last cell.
        let mut data = vec![None; usize::from(width) - shown.len()];
        data.extend_from_slice(shown);
        Sparkline::default()
            .data(data)
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
/// the rows it needs. Returns how many rows it took.
fn place_log(log: &[Row], area: Rect, buf: &mut Buffer) -> u16 {
    if area.height == 0 {
        return 0;
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
    used
}

fn key_line(keys: &[KeyHint]) -> Line<'static> {
    hint_line(keys, "  ", |_| true)
}

/// The key bar in `width` cells: whole when it fits, else every key but
/// the last without its label -- the last is how the screen is left, `q
/// back` or `q quit`, and says which -- else the last alone.
fn fitted_key_line(keys: &[KeyHint], width: u16) -> Line<'static> {
    let whole = key_line(keys);
    if whole.width() <= usize::from(width) {
        return whole;
    }
    let last = keys.len().saturating_sub(1);
    let short = hint_line(keys, " ", |i| i == last);
    if short.width() <= usize::from(width) {
        return short;
    }
    // The last resort: the way out, whole, and nothing else.
    key_line(&keys[last..])
}

/// `keys` with `gap` between them, the labels of those `labelled` says.
/// A key in bold, a disabled one and its label dimmed.
fn hint_line(
    keys: &[KeyHint],
    gap: &'static str,
    labelled: impl Fn(usize) -> bool,
) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(gap));
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
        if labelled(i) {
            spans.push(Span::styled(format!(" {}", k.label), label));
        }
    }
    Line::from(spans)
}

/// The fallback for a screen too small for a box.
///
/// Reverse video and the top row, because the bottom rows are where the
/// cursor usually is and covering those is what the box was centred to avoid.
fn single_line(v: &PopupView, size: TermSize) -> Overlay {
    let width = usize::from(size.cols.max(1));
    let marker = [format!("oxutrm: {}", v.marker_text)];
    let texts = if v.line.is_empty() {
        &marker[..]
    } else {
        &v.line[..]
    };
    let text = texts
        .iter()
        .find(|t| Line::from(t.as_str()).width() <= width)
        .or(texts.last())
        .cloned()
        .unwrap_or_default();
    reversed_line(text, size)
}

/// `text` in reverse video on the top row, for a screen too small for a
/// box.
fn reversed_line(text: String, size: TermSize) -> Overlay {
    let area = Rect::new(0, 0, size.cols.max(1), 1);
    let mut buf = Buffer::empty(area);
    Paragraph::new(Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::REVERSED),
    )))
    .render(area, &mut buf);
    overlay_from_buffer(&buf, 0, 0)
}

/// One row of the config screen.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ConfigRow {
    pub name: String,
    /// The value, or the text field while one is open on the row.
    pub value: String,
    /// Changed on the screen and not saved: drawn as `*`.
    pub changed: bool,
    /// `host` or `global`; empty for the built-in default.
    pub origin: String,
    /// When a change takes effect, when that is not now.
    pub note: String,
    /// The value is an open text field: when it is cut, its end -- where
    /// the cursor is -- stays in view rather than its start.
    pub field: bool,
}

/// Rows under a rule with `name` drawn into it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ConfigSection {
    pub name: String,
    pub rows: Vec<ConfigRow>,
    /// A plain list, like the `stun_servers` sub-list: each row is an
    /// index and a value that gets the rest of the line, with no origin or
    /// note columns to make room for.
    pub list: bool,
}

/// What the config screen says, as content rather than as cells.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct ConfigView {
    /// Drawn into the top border.
    pub title: String,
    /// The first line inside the box -- warnings, an outage -- or empty.
    pub header: String,
    pub sections: Vec<ConfigSection>,
    /// The row the cursor is on, counting rows across all sections.
    pub cursor: usize,
    /// The line above the key bar: the selected setting's help and range,
    /// or why a change was refused.
    pub help: String,
    pub keys: Vec<KeyHint>,
}

impl ConfigView {
    /// The row the cursor is on.
    fn selected(&self) -> Option<&ConfigRow> {
        self.sections.iter().flat_map(|s| &s.rows).nth(self.cursor)
    }
}

/// What layer 1's popup shows: the status view, the config screen or the
/// session selector.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Popup {
    Status(PopupView),
    Config(ConfigView),
    Sessions(SessionsView),
}

/// Lay out whichever view the popup shows.
pub fn layout(p: &Popup, size: TermSize) -> Overlay {
    match p {
        Popup::Status(v) => layout_popup(v, size),
        Popup::Config(v) => layout_config(v, size),
        Popup::Sessions(v) => layout_sessions(v, size),
    }
}

/// One row of the session selector, its columns as text.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SessionRow {
    /// The name, else the start of the id; or the name being typed.
    pub name: String,
    /// The shell's basename.
    pub shell: String,
    /// `HH:MM` today, else `Mon DD`.
    pub started: String,
    /// `120×40`.
    pub size: String,
    /// `this`, `in use`, `?`, `old version`, or nothing.
    pub mark: String,
    /// A session that cannot be switched to: drawn dimmed.
    pub dimmed: bool,
    /// The name is an open text field: cut at its start, not its end.
    pub field: bool,
}

/// What the session selector says, as content rather than as cells.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SessionsView {
    /// Drawn into the top border.
    pub title: String,
    /// The first line inside the box -- an outage -- or empty.
    pub header: String,
    pub rows: Vec<SessionRow>,
    /// The last row, after the sessions: `+ new session`.
    pub new_row: String,
    /// A row of `rows`, or `rows.len()` for the new row.
    pub cursor: usize,
    /// The line under the list: a question, why something was refused, or
    /// that the list is on its way. Empty for none.
    pub line: String,
    /// `line` is a question the next key answers: on a short screen it
    /// keeps its row before anything else above the key bar.
    pub question: bool,
    pub keys: Vec<KeyHint>,
    /// What a screen below [`MIN_BOX`] shows on its one line.
    pub small: String,
}

/// The widest the selector's name column grows: a name's own limit.
const SESSION_NAME_COLS: usize = 24;
/// The narrowest: the start of an id.
const SESSION_NAME_MIN: usize = 8;
/// The widest the shell column grows: a basename longer than this is cut.
const SESSION_SHELL_COLS: usize = 12;
/// The narrowest it is cut to when nothing else is left to give way.
const SESSION_SHELL_MIN: usize = 4;
/// The gap between the selector's columns.
const SESSION_GAP: &str = "  ";

/// The selector's column widths in cells, from what its rows say. A
/// column given up to make room is `None`.
struct SessionCols {
    name: usize,
    shell: Option<usize>,
    started: Option<usize>,
    size: Option<usize>,
    /// The widest mark; 0 when no row has one.
    mark: usize,
}

impl SessionCols {
    fn of(rows: &[SessionRow]) -> SessionCols {
        let widest =
            |f: fn(&SessionRow) -> &str| rows.iter().map(|r| cells(f(r))).max().unwrap_or(0);
        SessionCols {
            name: widest(|r| &r.name).clamp(SESSION_NAME_MIN, SESSION_NAME_COLS),
            shell: Some(widest(|r| &r.shell).min(SESSION_SHELL_COLS)),
            started: Some(widest(|r| &r.started)),
            size: Some(widest(|r| &r.size)),
            mark: widest(|r| &r.mark),
        }
    }

    /// How wide the widest row is, its marker included.
    fn width(&self) -> usize {
        let gap = SESSION_GAP.len();
        let after = |w: Option<usize>| w.map_or(0, |w| gap + w);
        2 + self.name
            + after(self.shell)
            + after(self.started)
            + after(self.size)
            + after(Some(self.mark).filter(|&m| m > 0))
    }

    /// Narrowed to `width` cells so the mark -- `this`, `in use`, `old
    /// version`, the column that says what switching would do -- stays in
    /// view: the name gives way first, down to the start of an id, then
    /// the size, then the start time, then the shell, cut and then gone.
    fn fitted(mut self, width: usize) -> SessionCols {
        let over = |c: &SessionCols| c.width().saturating_sub(width);
        self.name -= over(&self).min(self.name - SESSION_NAME_MIN);
        if over(&self) > 0 {
            self.size = None;
        }
        if over(&self) > 0 {
            self.started = None;
        }
        if let Some(shell) = self.shell {
            let cut = over(&self).min(shell - SESSION_SHELL_MIN.min(shell));
            self.shell = Some(shell - cut);
        }
        if over(&self) > 0 {
            self.shell = None;
        }
        self
    }

    /// One row as text: the marker, then the columns, each padded or cut
    /// to its width in cells.
    fn line(&self, r: &SessionRow, selected: bool) -> String {
        let marker = if selected { '\u{25b8}' } else { ' ' };
        let mut text = format!(
            "{marker} {}",
            padded(&cut(&r.name, self.name, r.field), self.name)
        );
        for (value, cols) in [
            (&r.shell, self.shell),
            (&r.started, self.started),
            (&r.size, self.size),
        ] {
            if let Some(cols) = cols {
                text.push_str(SESSION_GAP);
                text.push_str(&padded(&cut(value, cols, false), cols));
            }
        }
        text.push_str(SESSION_GAP);
        text.push_str(&r.mark);
        text.trim_end().to_string()
    }
}

/// Lay the session selector out for this screen (switcher spec §3.2): the
/// popup's box rules -- as wide and as tall as what it says, capped at
/// `min(cols - 4, 72)` by `min(rows - 2, 24)`, centred. The header, the
/// rows, `+ new session`, the line under the list, then a plain rule and
/// the key bar. When the rows do not fit, the list scrolls so the cursor's
/// row is in view; when they are too wide, their columns give way, the
/// mark last. Below [`MIN_BOX`], one reverse-video line.
pub fn layout_sessions(v: &SessionsView, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        return reversed_line(v.small.clone(), size);
    }
    let cols = SessionCols::of(&v.rows);
    let new_marker = if v.cursor >= v.rows.len() {
        '\u{25b8}'
    } else {
        ' '
    };
    let new_row = format!("{new_marker} {}", v.new_row);
    let width = [
        cols.width(),
        cells(&new_row),
        cells(&v.header),
        cells(&v.line),
        // The title, a space either side, fits the top border as the
        // content fits between the padding.
        cells(&v.title),
        key_line(&v.keys).width(),
    ]
    .into_iter()
    .max()
    .unwrap_or(0);
    let wanted = u16::try_from(width)
        .unwrap_or(u16::MAX)
        .saturating_add(FRAME_COLS);
    let box_cols = wanted.min((size.cols - 4).min(MAX_BOX.cols));
    let cols = cols.fitted(usize::from(box_cols - FRAME_COLS));
    let mut lines: Vec<String> = v
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| cols.line(r, i == v.cursor))
        .collect();
    lines.push(new_row);
    let cap = (size.rows - 2).min(MAX_BOX.rows);
    let inside = lines.len()
        + usize::from(!v.header.is_empty())
        + usize::from(!v.line.is_empty())
        // The rule and the key bar.
        + 2;
    let box_rows = u16::try_from(inside)
        .unwrap_or(u16::MAX)
        .saturating_add(2)
        .min(cap);
    let buf = draw_sessions(v, &lines, box_cols, box_rows);
    overlay_from_buffer(&buf, (size.rows - box_rows) / 2, (size.cols - box_cols) / 2)
}

/// The selector in a box `cols` by `rows`, at least 4 rows: the key bar
/// and a plain rule from the bottom, the header from the top, the line
/// under the list above the rule, and the list in what is left. A
/// question under the list is what the next key answers: on a short
/// screen it keeps its row before the rule, the header or the list do.
fn draw_sessions(v: &SessionsView, lines: &[String], cols: u16, rows: u16) -> Buffer {
    let keys = fitted_key_line(&v.keys, cols.saturating_sub(FRAME_COLS));
    let (mut buf, mut body) = framed(&v.title, keys, cols, rows);
    let ask = v.question && !v.line.is_empty();
    let put_line = |body: &mut Rect, buf: &mut Buffer| {
        // Cut visibly, never silently.
        let line = cut(&v.line, usize::from(body.width), false);
        Paragraph::new(Line::from(line)).render(
            Rect {
                y: body.bottom() - 1,
                height: 1,
                ..*body
            },
            buf,
        );
        body.height -= 1;
    };
    // The rule only with a row of the list left above it, and the
    // question's row.
    if body.height > 2 * u16::from(ask) {
        rule(&mut buf, body.bottom() - 1, cols, None);
        body.height -= 1;
    }
    if ask {
        put_line(&mut body, &mut buf);
    }
    if !v.header.is_empty() && (!ask || body.height >= 2) {
        body = place_one(Line::from(v.header.clone()), body, &mut buf);
    }
    if !ask && !v.line.is_empty() && body.height >= 2 {
        put_line(&mut body, &mut buf);
    }
    let height = usize::from(body.height);
    let at = v.cursor.min(lines.len().saturating_sub(1));
    // Scrolled just enough for the cursor's row to be the last one shown.
    let start = (at + 1).saturating_sub(height);
    for (i, text) in lines.iter().enumerate().skip(start).take(height) {
        let y = body.y + u16::try_from(i - start).unwrap_or(u16::MAX);
        let mut style = Style::default();
        if i == at {
            style = style.add_modifier(Modifier::BOLD);
        }
        if v.rows.get(i).is_some_and(|r| r.dimmed) {
            style = style.add_modifier(Modifier::DIM);
        }
        Paragraph::new(Line::from(Span::styled(text.clone(), style))).render(
            Rect {
                y,
                height: 1,
                ..body
            },
            &mut buf,
        );
    }
    buf
}

/// The config screen's column widths: the name, then the value.
const NAME_COLS: usize = 18;
const VALUE_COLS: usize = 18;
/// The origin column, after the `*`.
const ORIGIN_COLS: usize = 7;
/// The index column of a list section such as the `stun_servers` sub-list.
const INDEX_COLS: usize = 4;

/// Lay the config screen out for this screen: the same box as the status
/// view, `min(cols - 4, 72)` by up to `min(rows - 2, 24)`, centred. The
/// header, then each section under a rule drawn into the border, then a
/// plain rule, the help line and the key bar. When the rows do not fit, the
/// list scrolls so the cursor's row is in view.
pub fn layout_config(v: &ConfigView, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        let text = match v.selected() {
            Some(r) => format!("oxutrm config \u{b7} {} {}", r.name, r.value),
            None => "oxutrm config".to_string(),
        };
        return reversed_line(text, size);
    }
    let cols = (size.cols - 4).min(MAX_BOX.cols);
    let cap = (size.rows - 2).min(MAX_BOX.rows);
    let lines = config_lines(v).len() + usize::from(!v.header.is_empty());
    // Inside the border: the content, then the rule, the help, the keys.
    let wanted = u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(3 + 2);
    let rows = wanted.min(cap);
    let buf = draw_config(v, cols, rows);
    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

/// One line of the config screen's list.
enum ConfigLine<'a> {
    Rule(&'a str),
    /// A row, whether the cursor is on it, and whether its section is a
    /// plain list.
    Row(&'a ConfigRow, bool, bool),
}

fn config_lines(v: &ConfigView) -> Vec<ConfigLine<'_>> {
    let mut lines = Vec::new();
    let mut n = 0;
    for s in &v.sections {
        lines.push(ConfigLine::Rule(&s.name));
        for r in &s.rows {
            lines.push(ConfigLine::Row(r, n == v.cursor, s.list));
            n += 1;
        }
    }
    lines
}

/// How many cells `text` takes on the screen: a CJK character takes two,
/// so every column here is measured in cells, never in characters.
fn cells(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

/// `text` padded with spaces to `cols` cells.
fn padded(text: &str, cols: usize) -> String {
    format!("{text}{}", " ".repeat(cols.saturating_sub(cells(text))))
}

/// `text` in at most `cols` cells. A cut is marked with `…`: at the end,
/// or -- for an open text field, whose cursor is at the end and must stay
/// in view while typing -- at the start. A wide character that would
/// straddle the cut goes with it, so the result can be a cell short.
fn cut(text: &str, cols: usize, keep_end: bool) -> String {
    if cells(text) <= cols {
        return text.to_string();
    }
    if cols == 0 {
        return String::new();
    }
    let room = cols - 1;
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    let mut take = |c: char| {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w > room {
            return false;
        }
        used += w;
        kept.push(c);
        true
    };
    if keep_end {
        for c in text.chars().rev() {
            if !take(c) {
                break;
            }
        }
        kept.reverse();
        format!("\u{2026}{}", kept.into_iter().collect::<String>())
    } else {
        for c in text.chars() {
            if !take(c) {
                break;
            }
        }
        format!("{}\u{2026}", kept.into_iter().collect::<String>())
    }
}

/// `text` padded or cut to `cols` cells, with a space after a cut so the
/// next column stays apart.
fn column(text: &str, cols: usize, keep_end: bool) -> String {
    if cells(text) <= cols {
        padded(text, cols)
    } else {
        let room = cols.saturating_sub(1);
        format!("{} ", padded(&cut(text, room, keep_end), room))
    }
}

/// One row as a line `width` characters wide: a setting's name, value,
/// pending mark, origin and note; or, in a list section, an index and a
/// value that gets the rest of the line.
fn row_line(r: &ConfigRow, selected: bool, list: bool, width: usize) -> Line<'static> {
    let marker = if selected { '\u{25b8}' } else { ' ' };
    let text = if list {
        format!(
            "{marker} {}{}",
            column(&r.name, INDEX_COLS, false),
            cut(&r.value, width.saturating_sub(2 + INDEX_COLS), r.field)
        )
    } else {
        format!(
            "{marker} {}{}{} {}{}",
            column(&r.name, NAME_COLS, false),
            column(&r.value, VALUE_COLS, r.field),
            if r.changed { '*' } else { ' ' },
            column(&r.origin, ORIGIN_COLS, false),
            r.note
        )
    };
    let style = if selected {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    Line::from(Span::styled(text.trim_end().to_string(), style))
}

fn draw_config(v: &ConfigView, cols: u16, rows: u16) -> Buffer {
    // From the bottom: the key bar, which is how the screen is left, then
    // the help line and a plain rule while there is room for them. At
    // least two rows inside the border here.
    let (mut buf, above_bar) = framed(&v.title, key_line(&v.keys), cols, rows);
    if above_bar.height >= 1 {
        Paragraph::new(v.help.clone()).render(
            Rect {
                y: above_bar.bottom() - 1,
                height: 1,
                ..above_bar
            },
            &mut buf,
        );
    }
    if above_bar.height >= 2 {
        rule(&mut buf, above_bar.bottom() - 2, cols, None);
    }
    let mut body = Rect {
        height: above_bar.height.saturating_sub(2),
        ..above_bar
    };
    if !v.header.is_empty() {
        body = place_one(Line::from(v.header.clone()), body, &mut buf);
    }
    let lines = config_lines(v);
    let at = lines
        .iter()
        .position(|l| matches!(l, ConfigLine::Row(_, true, _)))
        .unwrap_or(0);
    let height = usize::from(body.height);
    // Scrolled just enough for the cursor's row to be the last one shown.
    // The rule of the first row shown can scroll off the top: keeping it
    // would cost the cursor's row, which is then exactly one line too low.
    // The cursor's own rule, when its row opens a section, shows whenever
    // the list has two lines.
    let start = (at + 1).saturating_sub(height);
    for (i, line) in lines.iter().skip(start).take(height).enumerate() {
        let y = body.y + u16::try_from(i).unwrap_or(u16::MAX);
        match line {
            ConfigLine::Rule(name) => rule(&mut buf, y, cols, Some(name)),
            ConfigLine::Row(r, selected, list) => {
                Paragraph::new(row_line(r, *selected, *list, usize::from(body.width))).render(
                    Rect {
                        y,
                        height: 1,
                        ..body
                    },
                    &mut buf,
                );
            }
        }
    }
    buf
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

    /// The last `n` bars of the rtt row, the newest last: the sparkline
    /// ends at the right edge, two cells in from the end (padding, border).
    fn last_bars(o: &Overlay, v: &PopupView, n: usize) -> Vec<char> {
        let r = find(o, &v.rtt).unwrap_or_else(|| panic!("no rtt row: {}", text_of(o)));
        let line: Vec<char> = row(o, r).chars().collect();
        let end = line.len() - 2;
        line[end - n..end].to_vec()
    }

    #[test]
    fn a_popup_is_centred_and_capped() {
        for (cols, rows, want) in [
            (80u16, 24u16, (72u16, 22u16, 1u16, 4u16)),
            (200, 60, (72, 24, 18, 64)),
            // The fixture says less than a 36x10 box holds: the box is as
            // tall as its content, and still centred.
            (40, 12, (36, 9, 1, 2)),
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
        // The free row is not drawn: the box shrinks to what it shows.
        let short = layout_popup(&v, TermSize { cols: 80, rows: 10 });
        let text = text_of(&short);
        assert_eq!(short.rows, 7, "{text}");
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
        let b = last_bars(&o, &v, 3);
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
        let b = last_bars(&o, &v, 2);
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
                    row(&o, o.rows - 2).contains("q quit"),
                    "{cols}x{rows}: {}",
                    text_of(&o)
                );
            }
        }
    }

    /// Review I1. Under `Confirming` the header is the long live one, and
    /// on a mid-width screen it would wrap and take the row the typed bytes
    /// need: the question would be asked without showing what it sends. With
    /// held text present the header keeps to one row.
    #[test]
    fn a_long_header_does_not_push_the_held_text_out() {
        let v = PopupView {
            marker: Marker::Live,
            marker_text: "\u{25cf} LIVE".to_string(),
            header: "IPv4 punched \u{b7} up 1m \u{b7} link 2".to_string(),
            attempts: vec![],
            held: vec![
                "the host is answering again - deliver what you typed?".to_string(),
                "You typed 10 bytes while offline:".to_string(),
                "make test\u{21b5}".to_string(),
            ],
            ..view()
        };
        // 44 columns: a 36-column interior, which the 41-cell header does
        // not fit on one row.
        let o = layout_popup(&v, TermSize { cols: 44, rows: 10 });
        let text = text_of(&o);
        assert!(text.contains("make test"), "{text}");
        assert!(text.contains("You typed 10 bytes"), "{text}");
        assert!(text.contains("\u{25cf} LIVE"), "{text}");
        assert!(row(&o, o.rows - 2).contains("q quit"), "{text}");
    }

    /// Review M3. Fewer samples than cells: the newest still sits at the
    /// right edge, and the empty cells are on the left.
    #[test]
    fn a_short_sparkline_ends_at_the_right_edge() {
        let v = PopupView {
            spark: vec![Some(10), Some(20), Some(40)],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let r = find(&o, &v.rtt).unwrap();
        let line: Vec<char> = row(&o, r).chars().collect();
        // Border, padding: the last bar is two cells in from the end.
        assert_eq!(line[line.len() - 3], '\u{2588}', "{line:?}");
        assert!(
            line[line.len() - 5] != ' ' && line[line.len() - 5] < line[line.len() - 4],
            "the older samples are not just left of the newest: {line:?}"
        );
        assert_eq!(
            bars(&o, &v, 1),
            [' '],
            "the samples start at the left: {line:?}"
        );
    }

    /// Review M7. Too narrow for a label column, a row with no label starts
    /// at the left of the box, not one space in.
    #[test]
    fn a_narrow_unlabelled_row_has_no_leading_space() {
        let v = PopupView {
            attempts: vec![
                Row::new("standby probe", "no answer"),
                Row::new("", "REASON"),
            ],
            ..view()
        };
        // An 18-column interior: too narrow for the 16-column label column
        // and eight cells of text.
        let o = layout_popup(&v, TermSize { cols: 26, rows: 24 });
        let r = find(&o, "REASON").unwrap_or_else(|| panic!("{}", text_of(&o)));
        let line: String = row(&o, r).chars().skip(2).collect();
        assert!(line.starts_with("REASON"), "{line:?}");
        let labelled = find(&o, "standby probe").unwrap();
        assert!(
            row(&o, labelled)
                .chars()
                .skip(2)
                .collect::<String>()
                .starts_with("standby probe no"),
            "{}",
            text_of(&o)
        );
    }

    /// The box is as tall as what it says: a short log leaves no empty rows,
    /// and the shorter box is still centred.
    #[test]
    fn a_popup_with_little_to_say_shrinks_to_it() {
        let v = PopupView {
            log: vec![Row::new("14:13", "only-one")],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        // Header, two attempts, rtt, loss, standby rule and row, recent rule
        // and entry, plain rule, keys: eleven inside the border.
        assert_eq!(o.rows, 13, "{text}");
        assert_eq!((o.row, o.col), ((24 - 13) / 2, 4), "{text}");
        for r in 1..o.rows - 1 {
            let inside: String = row(&o, r).chars().skip(1).take(70).collect();
            assert!(!inside.trim().is_empty(), "row {r} is empty: {text}");
        }
        assert!(row(&o, o.rows - 2).contains("q quit"), "{text}");
        assert!(is_rule(&row(&o, o.rows - 3), None), "{text}");
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

    // ---- the config screen ----

    fn config_view() -> ConfigView {
        let row = |name: &str, value: &str, changed: bool, origin: &str, note: &str| ConfigRow {
            name: name.to_string(),
            value: value.to_string(),
            changed,
            origin: origin.to_string(),
            note: note.to_string(),
            field: false,
        };
        ConfigView {
            title: "oxutrm \u{b7} config \u{b7} thinlinc".to_string(),
            header: "config: 2 warnings".to_string(),
            sections: vec![
                ConfigSection {
                    name: "popup".to_string(),
                    rows: vec![
                        row("key", "ctrl-\\", false, "", ""),
                        row("auto_open_after", "2s", false, "", ""),
                        row("linger", "3s", false, "", ""),
                        row("splash", "on", false, "", "next connect"),
                    ],
                    list: false,
                },
                ConfigSection {
                    name: "recovery".to_string(),
                    rows: vec![
                        row("silent_after", "2s", false, "", ""),
                        row("rebuild_after", "30s", true, "host", ""),
                        row("connect_timeout", "10s", false, "", "next attempt"),
                    ],
                    list: false,
                },
                ConfigSection {
                    name: "network".to_string(),
                    rows: vec![
                        row("standby", "off", false, "host", ""),
                        row(
                            "stun_servers",
                            "stun.cloudflare.com:3478 +3",
                            false,
                            "",
                            "next attempt",
                        ),
                        row("port_mapping", "on", false, "", "next attempt"),
                        row("birthday", "on", false, "global", "next attempt"),
                    ],
                    list: false,
                },
            ],
            cursor: 5,
            help: "silence before an ssh rebuild starts \u{b7} 5s\u{2013}10m".to_string(),
            keys: [
                "\u{2191}\u{2193} move",
                "\u{23ce} edit",
                "x reset",
                "w save",
                "Esc back",
            ]
            .iter()
            .map(|k| {
                let (key, label) = k.split_once(' ').unwrap();
                KeyHint {
                    key: key.to_string(),
                    label: label.to_string(),
                    enabled: true,
                }
            })
            .collect(),
        }
    }

    #[test]
    fn the_config_screen_shows_its_sections_under_rules_and_marks_the_cursor() {
        let o = layout_config(&config_view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        for section in ["popup", "recovery", "network"] {
            assert!(
                rule_row(&o, Some(section)).is_some(),
                "no {section} rule:\n{text}"
            );
        }
        let at = find(&o, "rebuild_after").expect("no rebuild_after row");
        let line = row(&o, at);
        assert!(line.contains("\u{25b8} rebuild_after"), "{line}");
        assert!(
            line.contains("30s") && line.contains('*') && line.contains("host"),
            "{line}"
        );
        assert!(find(&o, "config: 2 warnings").is_some(), "{text}");
        assert_eq!(find(&o, "silence before an ssh rebuild"), Some(o.rows - 3));
        assert_eq!(find(&o, "Esc back"), Some(o.rows - 2));
    }

    #[test]
    fn the_config_screen_scrolls_to_keep_the_cursor_in_view() {
        let mut v = config_view();
        v.cursor = 10;
        let o = layout_config(&v, TermSize { cols: 40, rows: 12 });
        let text = text_of(&o);
        assert!(
            find(&o, "birthday").is_some(),
            "the cursor's row is not shown:\n{text}"
        );
        assert!(
            find(&o, "auto_open_after").is_none(),
            "nothing scrolled:\n{text}"
        );
        assert!(o.rows <= 10);
        v.cursor = 0;
        let o = layout_config(&v, TermSize { cols: 40, rows: 12 });
        assert!(find(&o, "key").is_some(), "{}", text_of(&o));
    }

    #[test]
    fn a_long_value_is_cut_and_the_columns_after_it_stay() {
        let mut v = config_view();
        v.sections[1].rows[1].value = "x".repeat(60);
        let o = layout_config(&v, TermSize { cols: 80, rows: 24 });
        let line = row(&o, find(&o, "rebuild_after").unwrap());
        assert!(line.contains('\u{2026}') && line.contains("host"), "{line}");
    }

    /// Review focus 5. Below the minimum box the config screen is one line
    /// naming the selected row, and no size panics.
    #[test]
    fn a_config_screen_below_the_minimum_is_one_line_and_no_size_panics() {
        let o = layout_config(&config_view(), TermSize { cols: 19, rows: 5 });
        assert_eq!(o.rows, 1);
        assert!(row(&o, 0).starts_with("oxutrm config"), "{}", row(&o, 0));
        for cols in 1..=30 {
            for rows in 1..=12 {
                let o = layout_config(&config_view(), TermSize { cols, rows });
                assert!(o.cols <= cols && o.rows <= rows, "{cols}x{rows}");
            }
        }
    }

    #[test]
    fn layout_lays_out_whichever_view_the_popup_shows() {
        let size = TermSize { cols: 80, rows: 24 };
        assert_eq!(
            layout(&Popup::Status(view()), size),
            layout_popup(&view(), size)
        );
        assert_eq!(
            layout(&Popup::Config(config_view()), size),
            layout_config(&config_view(), size)
        );
    }

    /// The four built-in servers, 22 to 24 characters each.
    const SERVERS: [&str; 4] = [
        "stun.cloudflare.com:3478",
        "stun.l.google.com:19302",
        "stun.nextcloud.com:443",
        "stun.sipgate.net:3478",
    ];

    /// The `stun_servers` sub-list, as the client builds it.
    fn servers_view() -> ConfigView {
        ConfigView {
            header: String::new(),
            sections: vec![ConfigSection {
                name: "network.stun_servers".to_string(),
                rows: SERVERS
                    .iter()
                    .enumerate()
                    .map(|(i, s)| ConfigRow {
                        name: (i + 1).to_string(),
                        value: s.to_string(),
                        ..ConfigRow::default()
                    })
                    .collect(),
                list: true,
            }],
            cursor: 0,
            ..config_view()
        }
    }

    #[test]
    fn the_servers_sub_list_shows_each_server_in_full() {
        let o = layout_config(&servers_view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        for s in SERVERS {
            assert!(find(&o, s).is_some(), "{s} is not shown whole:\n{text}");
        }
    }

    #[test]
    fn a_text_field_on_a_long_server_keeps_its_end_and_cursor_in_view() {
        let mut v = servers_view();
        let long = format!("stun.{}.example.org:3478", "x".repeat(80));
        v.sections[0].rows[1].value = format!("{long}\u{258f}");
        v.sections[0].rows[1].field = true;
        v.cursor = 1;
        let o = layout_config(&v, TermSize { cols: 80, rows: 24 });
        let line = row(&o, find(&o, "\u{258f}").expect("no cursor shown"));
        assert!(line.contains("example.org:3478\u{258f}"), "{line}");
        assert!(line.contains('\u{2026}'), "the cut is not marked: {line}");
        // An open field on a realistic server shows it whole.
        let mut v = servers_view();
        v.sections[0].rows[0].value = "stun.cloudflare.com:3478\u{258f}".to_string();
        v.sections[0].rows[0].field = true;
        let o = layout_config(&v, TermSize { cols: 80, rows: 24 });
        assert!(
            find(&o, "stun.cloudflare.com:3478\u{258f}").is_some(),
            "{}",
            text_of(&o)
        );
    }

    #[test]
    fn a_text_field_in_the_main_list_keeps_its_end_and_cursor_in_view() {
        let mut v = config_view();
        let r = &mut v.sections[1].rows[1];
        r.value = "1234567890abcdefghijXYZ\u{258f}".to_string();
        r.field = true;
        let o = layout_config(&v, TermSize { cols: 80, rows: 24 });
        let line = row(&o, find(&o, "rebuild_after").unwrap());
        assert!(line.contains("\u{2026}"), "{line}");
        assert!(
            line.contains("XYZ\u{258f}"),
            "the cursor is cut off: {line}"
        );
        assert!(line.contains("host"), "the columns after it moved: {line}");
    }

    #[test]
    fn snapshot_config_servers_80x24() {
        let mut v = servers_view();
        v.cursor = 2;
        v.sections[0].rows[2].value = "stun.nextcloud.com:443\u{258f}".to_string();
        v.sections[0].rows[2].field = true;
        insta::assert_snapshot!(text_of(&layout_config(&v, TermSize { cols: 80, rows: 24 })));
    }

    #[test]
    fn snapshot_config_80x24() {
        insta::assert_snapshot!(text_of(&layout_config(
            &config_view(),
            TermSize { cols: 80, rows: 24 }
        )));
    }

    #[test]
    fn snapshot_config_40x12() {
        insta::assert_snapshot!(text_of(&layout_config(
            &config_view(),
            TermSize { cols: 40, rows: 12 }
        )));
    }

    // ---- the session selector -------------------------------------------

    fn session_keys(back: &str) -> Vec<KeyHint> {
        [
            ("\u{23ce}", "switch"),
            ("n", "new"),
            ("r", "rename"),
            ("x", "kill"),
            ("q", back),
        ]
        .into_iter()
        .map(|(key, label)| KeyHint {
            key: key.to_string(),
            label: label.to_string(),
            enabled: true,
        })
        .collect()
    }

    fn session(name: &str, shell: &str, started: &str, size: &str, mark: &str) -> SessionRow {
        SessionRow {
            name: name.to_string(),
            shell: shell.to_string(),
            started: started.to_string(),
            size: size.to_string(),
            mark: mark.to_string(),
            ..SessionRow::default()
        }
    }

    /// The spec's picture: three sessions on thinlinc, `build` the one the
    /// client is in, an unnamed fish session in use by another client.
    fn sessions_view() -> SessionsView {
        SessionsView {
            title: "sessions on thinlinc".to_string(),
            header: String::new(),
            rows: vec![
                session("a3f9c01e", "fish", "Oct 05", "80\u{d7}24", "in use"),
                session("build", "bash", "09:14", "120\u{d7}40", "this"),
                session("logs", "zsh", "11:02", "120\u{d7}40", ""),
            ],
            new_row: "+ new session".to_string(),
            cursor: 1,
            line: String::new(),
            question: false,
            keys: session_keys("back"),
            small: "oxutrm sessions \u{b7} build".to_string(),
        }
    }

    #[test]
    fn the_selector_lines_up_its_columns_and_marks_the_cursor() {
        let o = layout_sessions(&sessions_view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        let build = row(&o, find(&o, "build").unwrap());
        let fish = row(&o, find(&o, "fish").unwrap());
        assert!(build.contains("\u{25b8} build"), "{text}");
        for col in ["bash", "09:14", "120\u{d7}40", "this"] {
            assert!(build.contains(col), "{col} missing: {text}");
        }
        // In characters: the marker is three bytes.
        let at = |line: &str, what: &str| line[..line.find(what).unwrap()].chars().count();
        assert_eq!(at(&build, "bash"), at(&fish, "fish"), "{text}");
        assert_eq!(at(&build, "09:14"), at(&fish, "Oct 05"), "{text}");
        assert!(find(&o, "+ new session").is_some(), "{text}");
        assert!(row(&o, o.rows - 2).contains("q back"), "{text}");
        assert!(is_rule(&row(&o, o.rows - 3), None), "{text}");
        // As wide as it needs and no wider, centred.
        assert!(o.cols < 72, "{text}");
        assert_eq!(o.col, (80 - o.cols) / 2);
    }

    #[test]
    fn a_session_that_cannot_be_switched_to_is_dimmed() {
        let mut v = sessions_view();
        v.rows[2].dimmed = true;
        let o = layout_sessions(&v, TermSize { cols: 80, rows: 24 });
        let r = find(&o, "logs").unwrap();
        assert!(cell_of(&o, r, "l").attrs.contains(Attrs::DIM));
        let b = find(&o, "build").unwrap();
        assert!(!cell_of(&o, b, "b").attrs.contains(Attrs::DIM));
    }

    #[test]
    fn a_long_list_scrolls_to_keep_the_cursor_in_view() {
        let mut v = sessions_view();
        v.rows = (0..30)
            .map(|i| session(&format!("job-{i:02}"), "bash", "09:14", "120\u{d7}40", ""))
            .collect();
        v.cursor = 27;
        let o = layout_sessions(&v, TermSize { cols: 80, rows: 12 });
        let text = text_of(&o);
        assert!(find(&o, "\u{25b8} job-27").is_some(), "{text}");
        assert!(row(&o, o.rows - 2).contains("q back"), "{text}");
    }

    #[test]
    fn a_name_being_typed_keeps_its_end_in_view() {
        let mut v = sessions_view();
        v.rows[1].name = format!("{}\u{258f}", "n".repeat(40));
        v.rows[1].field = true;
        let o = layout_sessions(&v, TermSize { cols: 80, rows: 24 });
        let line = row(&o, find(&o, "\u{258f}").expect("no cursor shown"));
        assert!(line.contains('\u{2026}'), "the cut is not marked: {line}");
        assert!(line.contains("bash"), "the columns after it moved: {line}");
    }

    #[test]
    fn snapshot_sessions_80x24() {
        insta::assert_snapshot!(text_of(&layout_sessions(
            &sessions_view(),
            TermSize { cols: 80, rows: 24 }
        )));
    }

    #[test]
    fn snapshot_sessions_question_72x24() {
        let mut v = sessions_view();
        v.cursor = 0;
        v.line = "take over a3f9c01e from its other client? y/n".to_string();
        v.question = true;
        v.keys = vec![
            KeyHint {
                key: "y".to_string(),
                label: "yes".to_string(),
                enabled: true,
            },
            KeyHint {
                key: "n".to_string(),
                label: "no".to_string(),
                enabled: true,
            },
        ];
        insta::assert_snapshot!(text_of(&layout_sessions(
            &v,
            TermSize { cols: 72, rows: 24 }
        )));
    }

    #[test]
    fn snapshot_sessions_at_connect_40x12() {
        let v = SessionsView {
            title: "sessions on thinlinc".to_string(),
            header: "no reply 4 s".to_string(),
            rows: vec![session("logs", "zsh", "11:02", "120\u{d7}40", "")],
            cursor: 0,
            line: "the link is down; try again once it is back".to_string(),
            keys: session_keys("quit"),
            ..sessions_view()
        };
        insta::assert_snapshot!(text_of(&layout_sessions(
            &v,
            TermSize { cols: 40, rows: 12 }
        )));
    }

    #[test]
    fn snapshot_sessions_19x5() {
        insta::assert_snapshot!(text_of(&layout_sessions(
            &sessions_view(),
            TermSize { cols: 19, rows: 5 }
        )));
    }

    #[test]
    fn no_size_panics() {
        for cols in 0..=90 {
            for rows in 0..=30 {
                let _ = layout_sessions(&sessions_view(), TermSize { cols, rows });
            }
        }
    }

    #[test]
    fn a_narrow_selector_keeps_the_key_that_leaves_it() {
        let o = layout_sessions(&sessions_view(), TermSize { cols: 30, rows: 12 });
        let bar = row(&o, o.rows - 2);
        assert!(bar.contains("q back"), "{}", text_of(&o));
    }

    #[test]
    fn snapshot_sessions_three_rows_40x12() {
        insta::assert_snapshot!(text_of(&layout_sessions(
            &sessions_view(),
            TermSize { cols: 40, rows: 12 }
        )));
    }

    /// A row too wide for the box gives way column by column -- the name
    /// first, the size, the start time -- and keeps its mark, which says
    /// what switching to it would do.
    #[test]
    fn a_row_too_wide_keeps_its_mark() {
        let mut v = sessions_view();
        v.rows[2].name = "nightly-integration-runs".to_string();
        v.rows[2].shell = "xonsh-with-plugins".to_string();
        v.rows[2].mark = "old version".to_string();
        let o = layout_sessions(&v, TermSize { cols: 60, rows: 24 });
        let text = text_of(&o);
        let line = row(&o, find(&o, "nightly").expect("no row"));
        assert!(line.contains("old version"), "{text}");
        assert!(line.contains('\u{2026}'), "the cut is not marked: {text}");
        let fish = row(&o, find(&o, "a3f9c01e").unwrap());
        assert!(fish.contains("in use"), "{text}");
        let at = |line: &str, what: &str| line[..line.find(what).unwrap()].chars().count();
        assert_eq!(at(&line, "old version"), at(&fish, "in use"), "{text}");
        // And in a box with room for little more than the name's minimum
        // and the mark, still the mark.
        let o = layout_sessions(&v, TermSize { cols: 31, rows: 24 });
        let line = row(&o, find(&o, "\u{2026}").expect("no cut row"));
        assert!(line.contains("old version"), "{}", text_of(&o));
    }

    #[test]
    fn the_smallest_selector_keeps_the_key_that_leaves_it_whole() {
        for cols in 20..=24 {
            let o = layout_sessions(&sessions_view(), TermSize { cols, rows: 12 });
            let bar = row(&o, o.rows - 2);
            assert!(bar.contains("q back"), "{cols}: {}", text_of(&o));
        }
    }

    /// On a screen too short for the list and a question, the question
    /// is what stays: the next key answers it.
    #[test]
    fn a_question_on_a_short_screen_keeps_its_row() {
        let mut v = sessions_view();
        v.line = "kill build? y/n".to_string();
        v.question = true;
        v.keys.truncate(2);
        for rows in 6..=7 {
            let o = layout_sessions(&v, TermSize { cols: 80, rows });
            let text = text_of(&o);
            assert!(find(&o, "kill build? y/n").is_some(), "{rows}: {text}");
        }
        let o = layout_sessions(&v, TermSize { cols: 80, rows: 7 });
        assert!(find(&o, "\u{25b8} build").is_some(), "{}", text_of(&o));
    }

    /// The short key bar dims a key that does nothing, as the whole one
    /// does.
    #[test]
    fn a_narrow_selectors_key_bar_still_dims_what_is_off() {
        let mut v = sessions_view();
        v.keys[3].enabled = false;
        let o = layout_sessions(&v, TermSize { cols: 30, rows: 12 });
        let bar = o.rows - 2;
        assert!(!row(&o, bar).contains("kill"), "{}", text_of(&o));
        assert!(cell_of(&o, bar, "x").attrs.contains(Attrs::DIM));
        assert!(!cell_of(&o, bar, "n").attrs.contains(Attrs::DIM));
    }

    /// A name of wide characters takes two cells a character: the columns
    /// after it stay aligned with every other row's.
    #[test]
    fn a_name_of_wide_characters_keeps_the_columns_aligned() {
        let mut v = sessions_view();
        v.rows[2].name = "\u{69cb}\u{7bc9}\u{30ed}\u{30b0}".to_string();
        let o = layout_sessions(&v, TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        let column_of = |r: u16, first: &str| {
            let start = r as usize * o.cols as usize;
            o.cells[start..start + o.cols as usize]
                .iter()
                .position(|c| c.text == first)
                .unwrap_or_else(|| panic!("no {first} on row {r}: {text}"))
        };
        let wide = find(&o, "zsh").unwrap();
        let narrow = find(&o, "bash").unwrap();
        // The start time's colon is the first on either row.
        assert_eq!(column_of(wide, ":"), column_of(narrow, ":"), "{text}");
    }
}

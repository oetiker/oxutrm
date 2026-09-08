//! What layer 1 says, and how big it is.
//!
//! A notice reports what the client can observe -- silence, counters, what an
//! attempt of its own produced -- and nothing it cannot know. In particular it
//! never claims the session is safe: a dead network and a crashed host are
//! indistinguishable from here.
//!
//! The `Silent` box therefore still promises nothing about reconnection, and
//! the guard in `session.rs` that forbids it the words holds. The `Recovering`
//! box is the exception and earns it: something IS reconnecting by the time it
//! is on the screen, and it names only what that something has actually done.

use std::time::Duration;

use oxutrm_proto::TermSize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Padding, Paragraph, Widget as _, Wrap};

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

/// How much of the last failure's reason is shown.
///
/// The reason is an error chain built partly from a remote's stderr, so its
/// length is not ours to choose. The box wraps and is clamped to the screen,
/// so a long one cannot overflow anything -- but it can fill the screen, and a
/// box that covers the terminal it is apologising for is not an improvement.
/// The useful part of an ssh failure is at the front.
const REASON_SHOWN: usize = 120;

/// The notice shown while the client is rebuilding the link itself.
///
/// `quiet` is how long the host has been silent, `attempt` is the rebuild
/// attempt number (zero-based, as counted by `Phase::Recovering`),
/// `next_try_in` is how long until the next attempt is due, and `last_failure`
/// is why the previous attempt did not work -- `None` before any attempt has
/// finished, which is the state the first few seconds of every outage are in.
///
/// `attempt` is rendered as `attempt + 1`: zero-based is right internally,
/// where it indexes `backoff`, but a person reading "reconnect attempt 0"
/// for the very first try would read it as a bug.
pub fn recovering_notice(
    quiet: Duration,
    attempt: u32,
    next_try_in: Duration,
    last_failure: Option<&str>,
) -> Notice {
    let mut body = vec![
        format!("host quiet for {}s", quiet.as_secs()),
        format!("reconnect attempt {}", attempt + 1),
        format!("next try in {}s", next_try_in.as_secs()),
    ];
    if let Some(why) = last_failure {
        body.push(format!("last attempt: {}", summarised(why)));
    }
    Notice {
        headline: "waiting for the network".to_string(),
        body,
        keys: vec![(
            "Ctrl-\\ q".to_string(),
            "closes oxutrm here; it does not touch the host".to_string(),
        )],
    }
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
fn summarised(reason: &str) -> String {
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
/// reason reaches [`recovering_notice`]. This is the better half of that
/// trade: it leaves the escaping owned by the function that already owns
/// "make this reason fit in the box", so no future caller of a public
/// `recovering_notice` can reintroduce the hole by not knowing about it. The
/// two escapers deliberately use the same vocabulary -- `^X`, `^?`, `<9B>` --
/// so a user who has seen one recognises the other. `render_held` is not a
/// drop-in either way: it takes bytes and applies the held-input cap, and this
/// takes a `&str` and applies [`REASON_SHOWN`].
fn legible(line: &str) -> String {
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
    // Two columns of border plus two of horizontal padding.
    let widest = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let cols = widest.saturating_add(4).clamp(MIN_BOX.cols, size.cols);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(" oxutrm ");

    // The row count cannot be read off `lines.len()`: `Wrap { trim: false }`
    // can turn one long line into several rendered rows, and a screen with
    // room to spare should grow the box to fit that rather than silently
    // clip it. Measuring the wrap by rendering it -- rather than
    // reimplementing word-wrap -- keeps this in agreement with what
    // actually gets painted, in every corner ratatui's own wrapper handles.
    let inner_width = block.inner(Rect::new(0, 0, cols, size.rows)).width;
    let wrapped = wrapped_row_count(&lines, inner_width, size.rows);
    // Two rows of border.
    let rows = wrapped.saturating_add(2).clamp(3, size.rows);

    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    let inner = block.inner(area);
    block.render(area, &mut buf);
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, &mut buf);

    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

/// How many rows `lines` needs when wrapped at `width`, capped at `height`.
///
/// `Paragraph::line_count` would answer this directly, but sits behind
/// ratatui's `unstable-rendered-line-info` feature, and enabling an
/// unstable feature to size a box is a worse trade than this: render the
/// same wrap into a scratch buffer and read back the last row it touched.
/// That is slightly blunt, but by construction it cannot disagree with
/// ratatui's own wrapping -- it *is* ratatui's own wrapping.
///
/// A run of untouched rows at the bottom unambiguously means "wrapping
/// stopped here": `notice_lines` never puts a blank separator line last, so
/// any blank row this function could see is sandwiched between two rows
/// that do have content, and is counted along with them.
fn wrapped_row_count(lines: &[Line<'static>], width: u16, height: u16) -> u16 {
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

    /// The words the client actually ships, and deliberately so. This fixture
    /// used to say "close oxutrm here; the shell keeps running" -- the exact
    /// claim the session's own guard now forbids, because a dead network and a
    /// crashed host are indistinguishable from here. It never reached a
    /// screen, being a test fixture, but it sat in the tree as a ready-made
    /// copy of the bug for anyone laying out a new box to paste.
    fn notice() -> Notice {
        Notice {
            headline: "no reply from host".to_string(),
            body: vec!["silent for 6s".to_string(), "sent 14 - lost 9".to_string()],
            keys: vec![(
                "Ctrl-\\ q".to_string(),
                "closes oxutrm here; it does not touch the host".to_string(),
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
        assert!(text.contains("it does not touch the host"), "{text}");
    }

    /// The `Recovering` notice reports the three things there is anything to
    /// say about: none of these phrases appear in the `Silent` notice above,
    /// so this could not pass against the old text by accident.
    #[test]
    fn the_recovering_notice_reports_quiet_time_attempt_and_countdown() {
        let n = recovering_notice(Duration::from_secs(23), 2, Duration::from_secs(5), None);
        let o = layout_notice(&n, TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);

        assert!(text.contains("waiting for the network"), "{text}");
        assert!(text.contains("host quiet for 23s"), "{text}");
        // The `attempt` argument is 2 (zero-based); the box must show the
        // human count, 3 -- proving the +1 actually happened, not just that
        // some number is printed.
        assert!(text.contains("reconnect attempt 3"), "{text}");
        assert!(text.contains("next try in 5s"), "{text}");
        assert!(text.contains("Ctrl-\\ q"), "{text}");
        // Nothing has failed yet, so there is nothing to report about a last
        // attempt -- and a box that said so anyway would be inventing one.
        assert!(!text.contains("last attempt"), "{text}");
    }

    /// Why the last attempt failed is the only thing in the box that says
    /// anything about the cause. "Permission denied (publickey)" under
    /// `BatchMode` and "Network is unreachable" are different afternoons.
    #[test]
    fn the_recovering_notice_reports_why_the_last_attempt_failed() {
        let n = recovering_notice(
            Duration::from_secs(23),
            2,
            Duration::from_secs(5),
            Some("ssh exited with status 255: Permission denied (publickey)"),
        );
        let o = layout_notice(&n, TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);

        assert!(
            text.contains("Permission denied (publickey)"),
            "the reason the last attempt failed never reached the box: {text}"
        );
    }

    /// The reason is built partly from a remote's stderr, so its length is not
    /// ours to choose. A box that filled the screen would cover the terminal
    /// it is apologising for.
    #[test]
    fn a_very_long_failure_reason_is_summarised_rather_than_shown_whole() {
        let long = format!("ssh said: {}", "noise ".repeat(400));
        let n = recovering_notice(
            Duration::from_secs(1),
            0,
            Duration::from_secs(1),
            Some(&long),
        );
        let shown = n.body.last().expect("the reason is the last body line");

        // Against `REASON_SHOWN` itself, plus the label and the ellipsis. A
        // loose bound -- "< 200" against a cap of 120 -- passes for every cap
        // between the two, which is to say it does not pin the cap at all.
        assert!(
            shown.chars().count() <= REASON_SHOWN + "last attempt: ".len() + "...".len(),
            "the reason was cut at something other than REASON_SHOWN ({REASON_SHOWN}): \
             {} characters of {shown}",
            shown.chars().count()
        );
        assert!(
            shown.contains("ssh said:"),
            "the front of the reason is the useful part and must survive: {shown}"
        );
    }

    /// ssh says why it failed on its first line and pads afterwards.
    #[test]
    fn only_the_first_line_of_a_failure_reason_is_shown() {
        let n = recovering_notice(
            Duration::from_secs(1),
            0,
            Duration::from_secs(1),
            Some("Permission denied (publickey).\nbanner line nobody needs"),
        );
        let shown = n.body.last().expect("the reason is the last body line");

        assert!(shown.contains("Permission denied"), "{shown}");
        assert!(
            !shown.contains("banner line"),
            "a multi-line reason was pasted into the box whole: {shown}"
        );
    }

    /// The reason is a REMOTE's stderr. A terminal must never act on it.
    ///
    /// Nothing here relies on the renderer dropping what it cannot paint:
    /// ratatui's `set_stringn` happens to skip zero-width graphemes, which is
    /// why an ESC never became a cell before this, but that is a third-party
    /// crate's behaviour and not a decision this code made. `is_control()`
    /// covers C0 and C1 alike -- U+0080-U+009F are `Cc` -- and U+009B is CSI,
    /// which a terminal in UTF-8 mode obeys exactly as it obeys `ESC [`.
    #[test]
    fn control_scalars_in_a_failure_reason_are_shown_rather_than_emitted() {
        let n = recovering_notice(
            Duration::from_secs(1),
            0,
            Duration::from_secs(1),
            Some("ssh said: \u{1b}[2Jcleared\u{9b}31m and \u{7} rang"),
        );
        let shown = n.body.last().expect("the reason is the last body line");

        assert!(
            !shown.chars().any(char::is_control),
            "an untrusted control scalar reached the notice: {shown:?}"
        );
        assert!(
            shown.contains("^[") && shown.contains("^G"),
            "a C0 scalar must be shown, not merely dropped: {shown:?}"
        );
        assert!(
            shown.contains("<9B>"),
            "a bare C1 CSI must be shown, not merely dropped: {shown:?}"
        );
        assert!(
            shown.contains("ssh said:") && shown.contains("cleared"),
            "the readable part of the reason must survive: {shown:?}"
        );
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

    /// On a 30-column screen the key line ("Ctrl-\ q  closes oxutrm here; it
    /// does not touch the host") is wider than the inner box and wraps across
    /// three rows. A box height read off the unwrapped line count budgets
    /// only one of those and silently clips the rest of the sentence saying
    /// what the key does -- which is the whole of what the box is offering.
    #[test]
    fn a_wrapped_key_line_still_fits_in_the_box() {
        let o = layout_notice(&notice(), TermSize { cols: 30, rows: 24 });
        let text = text_of(&o);

        // Not a plain `contains` of the whole sentence: at this width it wraps
        // across a row boundary (legitimately -- that row boundary is not the
        // bug), so the words land on separate lines. What the pre-fix code
        // drops is not a word boundary but an entire row, so checking that
        // both ends of the sentence survived -- rather than that they are
        // adjacent -- is what actually detects a dropped row instead of an
        // ordinary wrap point.
        assert!(text.contains("closes oxutrm"), "{text}");
        assert!(text.contains("the host"), "{text}");
    }
}

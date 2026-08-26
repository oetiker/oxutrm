//! **I8** — text that is painted is text, not control, and its length is
//! bounded before it is expanded.
//!
//! # Why this rule exists at all
//!
//! I1 to I7 constrain the *shape* of a screen: lengths, cursor, sequence,
//! dimensions. Not one of them says anything about what a cell may *contain*.
//! But the trust model is asymmetric on purpose — the host is a machine the
//! user connected to, possibly compromised, while the client runs on the
//! user's own machine — and the client renders a [`ScreenState`] by writing
//! escape sequences to the user's REAL terminal. Cell text is copied into that
//! byte stream verbatim.
//!
//! So a shape-only contract lets a hostile host put `\x1b]52;c;<base64>\x07`
//! in one cell and have the client write the user's clipboard for it. Or
//! `\x1b[?1049h`, or DECSTBM, or a BEL. The title is worse still: it is
//! interpolated between `\x1b]0;` and BEL, so a title containing a terminator
//! closes the OSC early and everything after it is a fresh command stream.
//!
//! # Reject, do not repair
//!
//! Every check here returns an error and changes nothing. Filtering the
//! control scalars out would paint a screen the host did not send — the row
//! would still be the right length, so it would still validate, while the two
//! ends disagreed about every column after the edit. That is the settled
//! reasoning behind I2's out-of-range cursor being *rejected* rather than
//! clamped, and it applies here unchanged. A refusal is visible in a log; a
//! silent repair is visible nowhere.
//!
//! A rejected frame never ends the session: `Receiver::on_frame` applies to a
//! clone, so a refusal leaves the state and the ack exactly as they were.
//!
//! # The order is the substance
//!
//! [`check_cell_text`] tests the length **first**. That is not micro-optimising
//! a cheap branch ahead of an expensive one: the length bound is the half of
//! I8 that stops an allocation, and the caller in `ScreenState::apply` must run
//! it *before* the `repeat` expansion clones the run's cells across the row.
//! Checking after the expansion is checking a machine that has already fallen
//! over — I7's own wording, for the same reason.
//!
//! # Maintaining, not just checking
//!
//! [`fit_cell_text`] and [`fit_title`] are the other side of the coin, and they
//! belong to the *producer* of a state, never to its consumer. An honest host
//! that emits a state its own peer will reject is a session that freezes for
//! reasons no log explains, and `alacritty_terminal` really can produce an
//! over-long cell: it puts no cap at all on how many zero-width marks it stacks
//! onto one base character, so a program printing combining marks in a loop
//! grows a single cell without limit. `oxutrm-term` therefore fits its text at
//! the source. This does nothing for the client's safety — a hostile host
//! simply would not call it — and it is not meant to. It is a liveness
//! property, not a security one.
//!
//! [`ScreenState`]: crate::ScreenState

use crate::{ApplyError, CellText, MAX_CELL_TEXT, MAX_TITLE, TextField};

/// Is this scalar a control rather than a glyph?
///
/// C0 (`U+0000..=U+001F`), DEL (`U+007F`) and C1 (`U+0080..=U+009F`). C1 is not
/// paranoia: a terminal in UTF-8 mode reads U+009B as CSI exactly as it reads
/// `\x1b[`, and U+009D as OSC, so a rule that only looked at bytes below 0x20
/// would let the whole command vocabulary through in two-byte UTF-8 form.
///
/// This is the same set as [`char::is_control`] today. It is spelled out
/// because the rule is normative here: it must not drift with a future
/// Unicode category revision, and a reader should be able to see the ranges
/// without going to look them up.
pub const fn is_control_scalar(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}' | '\u{80}'..='\u{9f}')
}

/// **I8** for one cell's text.
///
/// At most [`MAX_CELL_TEXT`] bytes, and no control scalar. Call this **before**
/// anything expands or clones the cell.
pub fn check_cell_text(text: &str) -> Result<(), ApplyError> {
    check_text(text, TextField::CellText)
}

/// **I8** for a window title.
///
/// At most [`MAX_TITLE`] bytes, and no control scalar — which is what stops a
/// title from closing the client's OSC 0 early and starting a command stream
/// of its own.
pub fn check_title(title: &str) -> Result<(), ApplyError> {
    check_text(title, TextField::Title)
}

fn check_text(text: &str, field: TextField) -> Result<(), ApplyError> {
    // Length FIRST. It is O(1) on a `str`, and it is the half of I8 that
    // bounds an allocation — see the module comment on ordering.
    let max = field.max_bytes();
    if text.len() > max {
        return Err(ApplyError::TextTooLong {
            field,
            len: text.len(),
            max,
        });
    }
    if let Some(c) = text.chars().find(|c| is_control_scalar(*c)) {
        return Err(ApplyError::ControlInText {
            field,
            scalar: c as u32,
        });
    }
    Ok(())
}

/// Cut one cell's text down to something [`check_cell_text`] accepts.
///
/// For **producers only**. Returns the input untouched on the overwhelmingly
/// common path, so this costs one length test and one scan of at most
/// [`MAX_CELL_TEXT`] bytes per cell.
///
/// The repair drops control scalars and then truncates on a `char` boundary.
/// Truncating a grapheme cluster loses combining marks, which is a visible
/// degradation — and it is still strictly better than the alternative, which
/// is emitting a state the peer refuses and freezing the session.
pub fn fit_cell_text(text: CellText) -> CellText {
    if check_cell_text(&text).is_ok() {
        return text;
    }
    CellText::new(fit(&text, MAX_CELL_TEXT))
}

/// Cut a title down to something [`check_title`] accepts. Producers only; see
/// [`fit_cell_text`].
pub fn fit_title(title: String) -> String {
    if check_title(&title).is_ok() {
        return title;
    }
    fit(&title, MAX_TITLE)
}

/// Drop the controls, then stop at the last `char` boundary that fits.
///
/// Byte-slicing a `str` at `max` would panic mid-character, and truncating
/// first would waste the work of filtering a megabyte of marks — so the two
/// happen in one pass.
fn fit(text: &str, max: usize) -> String {
    let mut out = String::with_capacity(text.len().min(max));
    for c in text.chars() {
        if is_control_scalar(c) {
            continue;
        }
        if out.len() + c.len_utf8() > max {
            break;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_control_set_is_exactly_c0_del_and_c1() {
        for c in '\u{0}'..='\u{1f}' {
            assert!(is_control_scalar(c), "U+{:04X} is C0", c as u32);
        }
        assert!(is_control_scalar('\u{7f}'), "DEL");
        for c in '\u{80}'..='\u{9f}' {
            assert!(is_control_scalar(c), "U+{:04X} is C1", c as u32);
        }
        // The scalars immediately outside each range are ordinary text.
        for c in [' ', '~', '\u{a0}', 'é', '中', '🦀'] {
            assert!(!is_control_scalar(c), "U+{:04X} is a glyph", c as u32);
        }
    }

    /// If this ever diverges, the explicit ranges above are the normative
    /// ones — but a divergence is worth knowing about.
    #[test]
    fn the_control_set_agrees_with_the_standard_library() {
        for u in 0u32..=0x2fffu32 {
            if let Some(c) = char::from_u32(u) {
                assert_eq!(
                    is_control_scalar(c),
                    c.is_control(),
                    "U+{u:04X} disagrees with char::is_control"
                );
            }
        }
    }

    #[test]
    fn fitting_ordinary_text_does_not_copy_or_change_it() {
        for s in ["a", " ", "é", "🦀", "e\u{301}"] {
            let fitted = fit_cell_text(CellText::new(s));
            assert_eq!(fitted.as_str(), s, "{s:?} is already legal");
        }
    }

    #[test]
    fn fitting_drops_controls_and_stops_on_a_char_boundary() {
        // Four-byte scalars, so a naive byte truncation at 32 would land
        // inside one and panic. Eight of them is exactly 32 bytes.
        let long = "🦀".repeat(20);
        let fitted = fit_cell_text(CellText::new(format!("\x1b{long}")));
        assert_eq!(fitted.len(), 32, "eight crabs is exactly the cap");
        assert_eq!(fitted.as_str(), "🦀".repeat(8));
        check_cell_text(&fitted).expect("a fitted cell must be acceptable");
    }

    #[test]
    fn fitting_a_title_keeps_it_acceptable() {
        let hostile = format!("safe\x07\x1b]52;c;AAAA\x07{}", "x".repeat(600));
        let fitted = fit_title(hostile);
        check_title(&fitted).expect("a fitted title must be acceptable");
        assert_eq!(fitted.len(), MAX_TITLE);
        assert!(fitted.starts_with("safe]52;c;AAAA"), "got {fitted:?}");
    }
}

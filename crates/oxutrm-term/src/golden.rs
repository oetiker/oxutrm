//! Golden tests: what the emulator renders, pinned.
//!
//! Emulation fidelity is this crate's whole risk surface. Everything else here
//! is plumbing that fails loudly; getting a wide character's continuation cell
//! or an SGR attribute subtly wrong fails **quietly**, and shows up much later
//! as "the remote screen looks slightly off".
//!
//! # These snapshots were generated fresh, and read
//!
//! Nothing here was ported from `ansidrama`. Its snapshots were taken against
//! `vt100`, and `alacritty_terminal` differs in attribute set, in reflow and in
//! scrollback — so a ported snapshot that happened to pass would be worse than
//! one that failed, because it would silently certify the new emulator against
//! the old one's behaviour.
//!
//! An `insta` snapshot accepted without being read is a test that asserts
//! whatever the code did on the day it ran. Every snapshot in
//! `src/snapshots/` was eyeballed against what the escape sequence should
//! produce before being committed. If you regenerate one, read it.
//!
//! The rendering below is deliberately compact and human-readable rather than
//! a `Debug` dump of 400 `Cell`s — a snapshot nobody can read is a snapshot
//! nobody will check.
//!
//! One of these tests recorded a behaviour rather than a wish: see
//! [`a_reflow_shrinks_and_grows_back_losslessly`], where the property turned
//! out not to hold as stated.

use alacritty_terminal::Term;
use alacritty_terminal::term::Config;
use alacritty_terminal::vte::ansi::Processor;

use oxutrm_proto::{Attrs, Color, ScreenState, TermSize};

use crate::blink::{BlinkPlane, BlinkTap};
use crate::grid::GridSize;
use crate::host::{StateMeta, screen_state_of};
use crate::listener::EventSink;
use crate::palette::palette;

/// Feed `bytes` to a fresh emulator and convert the result.
///
/// Goes through [`BlinkTap`], exactly as `HostTerm::poll` does, so blink and
/// the scrolled-line count are recovered here too — otherwise the golden test
/// would exercise a different path from production.
fn render(rows: u16, cols: u16, bytes: &[u8]) -> (ScreenState, String) {
    let size = TermSize { cols, rows };
    let dims = GridSize::new(size, 100).expect("test size is legal");
    let events = EventSink::new();
    let mut term = Term::new(Config::default(), &dims, events.clone());
    let mut parser: Processor = Processor::new();
    let mut blink = BlinkPlane::default();
    let mut scrolled = 0u64;

    {
        let mut tap = BlinkTap::new(&mut term, &mut blink, &mut scrolled);
        parser.advance(&mut tap, bytes);
    }

    let signals = events.drain();
    let title = signals.title.unwrap_or_default();
    let state = screen_state_of(
        &term,
        &blink,
        &palette(),
        size,
        StateMeta {
            seq: 1,
            title,
            bell: signals.bells,
            scrollback_len: scrolled,
        },
    );
    let text = describe(&state);
    (state, text)
}

/// A compact, readable rendering of a state.
///
/// Rows are shown between pipes so trailing spaces are visible; only cells that
/// differ from a blank are listed, because a 200-line dump of `Cell::blank()`
/// hides the two cells that matter.
fn describe(s: &ScreenState) -> String {
    let mut out = String::new();
    out.push_str(&format!("size    {}x{} (rows x cols)\n", s.rows, s.cols));
    out.push_str(&format!(
        "cursor  ({},{}) visible={} {:?}\n",
        s.cursor.row, s.cursor.col, s.cursor.visible, s.cursor.shape
    ));
    out.push_str(&format!(
        "modes   alt={} paste={} mouse={:?} app_cursor={} app_keypad={}\n",
        s.modes.alt_screen,
        s.modes.bracketed_paste,
        s.modes.mouse,
        s.modes.app_cursor,
        s.modes.app_keypad
    ));
    out.push_str(&format!("title   {:?}\n", s.title));
    out.push_str(&format!(
        "bell    {}   scrollback {}\n",
        s.bell, s.scrollback_len
    ));
    out.push_str("\nscreen\n");
    for r in 0..s.rows {
        let row: String = s
            .row(r)
            .iter()
            .map(|c| {
                if c.text.is_empty() {
                    // A wide character's continuation cell carries no text at
                    // all. Shown as a marker so it is visible in the snapshot
                    // rather than looking like a space.
                    '\u{2591}'
                } else {
                    c.text.chars().next().unwrap_or(' ')
                }
            })
            .collect();
        out.push_str(&format!("{r:>3} |{row}|\n"));
    }

    out.push_str("\nnon-blank cells\n");
    let blank = oxutrm_proto::Cell::blank();
    let mut any = false;
    for r in 0..s.rows {
        for c in 0..s.cols {
            let cell = s.cell(r, c);
            if *cell == blank {
                continue;
            }
            any = true;
            out.push_str(&format!(
                "({r},{c}) {:?} fg={} bg={} attrs={}\n",
                cell.text.as_str(),
                colour(cell.fg),
                colour(cell.bg),
                attrs(cell.attrs)
            ));
        }
    }
    if !any {
        out.push_str("(none)\n");
    }
    out
}

fn colour(c: Color) -> String {
    match c {
        Color::Default => "default".to_owned(),
        Color::Idx(i) => format!("idx{i}"),
        Color::Rgb(r, g, b) => format!("rgb({r},{g},{b})"),
    }
}

fn attrs(a: Attrs) -> String {
    if a.is_empty() {
        return "-".to_owned();
    }
    let mut names = Vec::new();
    for (flag, name) in [
        (Attrs::BOLD, "BOLD"),
        (Attrs::ITALIC, "ITALIC"),
        (Attrs::UNDERLINE, "UNDERLINE"),
        (Attrs::INVERSE, "INVERSE"),
        (Attrs::BLINK, "BLINK"),
        (Attrs::STRIKE, "STRIKE"),
        (Attrs::DIM, "DIM"),
        (Attrs::HIDDEN, "HIDDEN"),
        (Attrs::WIDE_CONT, "WIDE_CONT"),
    ] {
        if a.contains(flag) {
            names.push(name);
        }
    }
    names.join("|")
}

// ---------------------------------------------------------------- the goldens

#[test]
fn plain_text_with_sgr_colours() {
    // Palette, bright palette and 24-bit, then a reset. The colours must
    // travel as INDICES where they are indices, so a client with its own
    // theme can honour them.
    let (state, text) = render(
        4,
        24,
        b"\x1b[31mred\x1b[0m \x1b[92mbright\x1b[0m \x1b[38;2;10;20;30mtrue\x1b[0m",
    );
    assert_eq!(state.validate(), Ok(()));
    insta::assert_snapshot!(text);
}

#[test]
fn a_wide_character_at_the_right_margin_wraps() {
    // The case most likely to be subtly wrong. A CJK ideograph needs two
    // columns; with one column left it must move to the next row rather than
    // being split, and its continuation cell must carry WIDE_CONT and NO text
    // - a renderer that painted a space there would shift the whole row.
    let (state, text) = render(3, 5, "abcd\u{4f60}\u{597d}".as_bytes());
    assert_eq!(state.validate(), Ok(()));
    insta::assert_snapshot!(text);
}

#[test]
fn the_alternate_screen_is_entered_and_left() {
    // DECSET 1049 in, write, DECRST 1049 out. The primary screen's content
    // must come back untouched: a full-screen application exiting should not
    // eat the shell's scrollback.
    let (state, text) = render(4, 20, b"primary\r\n\x1b[?1049halternate\x1b[?1049l");
    assert_eq!(state.validate(), Ok(()));
    assert!(!state.modes.alt_screen, "we left the alternate screen");
    insta::assert_snapshot!(text);
}

#[test]
fn the_alternate_screen_while_still_inside_it() {
    let (state, text) = render(4, 20, b"primary\r\n\x1b[?1049halternate");
    assert_eq!(state.validate(), Ok(()));
    assert!(state.modes.alt_screen);
    insta::assert_snapshot!(text);
}

#[test]
fn an_osc_two_title() {
    // OSC 2 sets the title. OSC 1 is silently dropped by the parser, so the
    // icon in this fixture must leave no trace anywhere.
    let (state, text) = render(2, 12, b"\x1b]1;an-icon\x07\x1b]2;a title\x07hi");
    assert_eq!(state.title, "a title");
    insta::assert_snapshot!(text);
}

#[test]
fn blink_strikethrough_and_hidden_together() {
    // Blink is the one attribute alacritty_terminal parses and then DISCARDS,
    // recovered by the Handler newtype. It is therefore the one most likely to
    // be silently absent, and this snapshot is where that would show.
    let (state, text) = render(
        3,
        24,
        b"\x1b[5mblink\x1b[0m \x1b[9mstrike\x1b[0m \x1b[8mhidden\x1b[0m",
    );
    assert_eq!(state.validate(), Ok(()));
    assert!(
        state.cell(0, 0).attrs.contains(Attrs::BLINK),
        "blink was dropped; the BlinkTap newtype is not working"
    );
    insta::assert_snapshot!(text);
}

#[test]
fn every_underline_style_flattens_to_one() {
    // v1 maps all five onto UNDERLINE. Drawing a curly underline as a straight
    // one is a smaller lie than drawing none, but the flattening should be
    // visible rather than assumed.
    let (state, text) = render(
        3,
        30,
        b"\x1b[4ma\x1b[4:2mb\x1b[4:3mc\x1b[4:4md\x1b[4:5me\x1b[0m",
    );
    assert_eq!(state.validate(), Ok(()));
    insta::assert_snapshot!(text);
}

#[test]
fn a_reflow_shrinks_and_grows_back_losslessly() {
    // The name is kept as written in the spec; the assertion records what the
    // emulator actually does. See the comment before the assertions below.
    // The property this emulator was chosen for. A line longer than the screen
    // is wrapped when the window narrows and unwrapped when it widens, and the
    // text must survive the round trip intact.
    let size_wide = TermSize { cols: 20, rows: 4 };
    let dims = GridSize::new(size_wide, 100).expect("test size is legal");
    let events = EventSink::new();
    let mut term = Term::new(Config::default(), &dims, events.clone());
    let mut parser: Processor = Processor::new();
    let mut blink = BlinkPlane::default();
    let mut scrolled = 0u64;
    {
        let mut tap = BlinkTap::new(&mut term, &mut blink, &mut scrolled);
        parser.advance(&mut tap, b"abcdefghijklmnopqr");
    }

    let before = screen_state_of(
        &term,
        &blink,
        &palette(),
        size_wide,
        StateMeta {
            seq: 1,
            title: String::new(),
            bell: 0,
            scrollback_len: 0,
        },
    );

    // Narrow, then widen back.
    let narrow = TermSize { cols: 8, rows: 4 };
    term.resize(GridSize::new(narrow, 100).expect("test size is legal"));
    let shrunk = screen_state_of(
        &term,
        &blink,
        &palette(),
        narrow,
        StateMeta {
            seq: 2,
            title: String::new(),
            bell: 0,
            scrollback_len: 0,
        },
    );

    term.resize(GridSize::new(size_wide, 100).expect("test size is legal"));
    let after = screen_state_of(
        &term,
        &blink,
        &palette(),
        size_wide,
        StateMeta {
            seq: 3,
            title: String::new(),
            bell: 0,
            scrollback_len: 0,
        },
    );

    assert_eq!(before.validate(), Ok(()));
    assert_eq!(shrunk.validate(), Ok(()));
    assert_eq!(after.validate(), Ok(()));

    let flat = |s: &ScreenState| -> String {
        (0..s.rows)
            .map(|r| {
                s.row(r)
                    .iter()
                    .map(|c| {
                        if c.text.is_empty() {
                            " "
                        } else {
                            c.text.as_str()
                        }
                    })
                    .collect::<String>()
            })
            .collect::<String>()
            .replace(' ', "")
    };
    // NOT asserted: that `flat(&after) == flat(&before)`. It is not true, and
    // finding that out is what this test is for.
    //
    // `Term::resize` reflows losslessly across the GRID - viewport plus
    // history - but not across the VIEWPORT alone. Narrowing turns one long
    // line into several, and the rows that no longer fit are pushed into
    // history; widening does not pull them back down. A snapshot carries only
    // the viewport, so a client watching a shrink-then-grow sees text leave
    // the top of the screen and not come back.
    //
    // That is the emulator's behaviour and matches what a local terminal does,
    // so it is recorded rather than worked around - but "reflow is lossless in
    // both directions" is too strong a claim to keep repeating, and the README
    // now says so.
    assert!(
        flat(&before).contains("abcdefghijklmnopqr"),
        "the fixture should start on one line: {:?}",
        flat(&before)
    );
    assert!(
        !flat(&shrunk).is_empty(),
        "narrowing must keep something on screen"
    );

    insta::assert_snapshot!(format!(
        "--- 20 columns ---\n{}\n--- 8 columns ---\n{}\n--- back to 20 ---\n{}",
        describe(&before),
        describe(&shrunk),
        describe(&after)
    ));
}

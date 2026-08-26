//! The seven `ScreenState` invariants, each proved by its **rejection**.
//!
//! An invariant that is only documented is not an invariant. Every test here
//! builds a state that breaks one rule and asserts the exact error, because a
//! test that only checks the happy path passes just as well after someone
//! deletes the check.

use oxutrm_proto::{
    ApplyError, Attrs, Cell, CellText, Color, Cursor, CursorShape, MAX_SCREEN_CELLS,
    MAX_SCREEN_DIM, Modes, MouseMode, ScreenState, TermSize,
};

/// A valid 3x4 state, for tests that then break exactly one thing about it.
fn good() -> ScreenState {
    ScreenState::blank(3, 4).expect("3x4 blank is valid")
}

// ---------------------------------------------------------------- I1

#[test]
fn i1_a_short_cell_vector_is_rejected() {
    let mut s = good();
    s.cells.pop();
    assert_eq!(
        s.validate(),
        Err(ApplyError::LengthMismatch {
            len: 11,
            rows: 3,
            cols: 4
        })
    );
}

#[test]
fn i1_a_long_cell_vector_is_rejected_just_as_hard() {
    // The dangerous direction. A short vector panics on the first access past
    // the end, which is loud. A long one never panics: every index is a
    // computed row-major offset, so the extra cells are simply never read and
    // every access silently addresses the WRONG CELL. Rejecting both is the
    // whole point of "EXACTLY".
    let mut s = good();
    s.cells.push(Cell::blank());
    assert_eq!(
        s.validate(),
        Err(ApplyError::LengthMismatch {
            len: 13,
            rows: 3,
            cols: 4
        })
    );
}

#[test]
fn i1_a_zero_sized_screen_is_consistent_rather_than_special() {
    let s = ScreenState::blank(0, 0).expect("0x0 is degenerate but consistent");
    assert!(s.cells.is_empty());
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn i1_the_row_major_order_is_the_one_every_offset_assumes() {
    let mut s = good();
    // Mark (row 2, col 3): row-major offset is 2*4 + 3 == 11.
    s.cells[11].text = "X".into();
    assert_eq!(s.cell(2, 3).text, "X");
    assert_eq!(s.row(2).len(), 4);
    assert_eq!(s.row(2)[3].text, "X");
    // And nothing else moved: not the rest of that row, not any other row.
    assert_eq!(s.cell(2, 0).text, " ");
    assert_eq!(s.cell(0, 3).text, " ");
    assert_eq!(
        s.cells.iter().filter(|c| c.text == "X").count(),
        1,
        "exactly one cell was written"
    );
}

// ---------------------------------------------------------------- I2

#[test]
fn i2_a_cursor_past_the_last_row_is_rejected_not_clamped() {
    let mut s = good();
    s.cursor.row = 3; // rows == 3, so the last legal row is 2.
    assert_eq!(
        s.validate(),
        Err(ApplyError::CursorOutOfBounds {
            row: 3,
            col: 0,
            rows: 3,
            cols: 4
        })
    );
}

#[test]
fn i2_a_cursor_past_the_last_column_is_rejected_not_clamped() {
    let mut s = good();
    s.cursor.col = 4; // cols == 4, so the last legal column is 3.
    assert_eq!(
        s.validate(),
        Err(ApplyError::CursorOutOfBounds {
            row: 0,
            col: 4,
            rows: 3,
            cols: 4
        })
    );
}

#[test]
fn i2_clamping_is_not_an_acceptable_implementation() {
    // This test exists to fail loudly if someone "fixes" the rejection above
    // by clamping with a min(). Clamping produces a state that validates,
    // looks healthy, and is quietly desynchronised from the other end - the
    // most expensive class of bug this protocol can have.
    //
    // Three things are asserted, and a clamping implementation fails all of
    // them: it returns Ok, it reports the clamped coordinates rather than the
    // ones that were actually wrong, and it mutates the state it was handed.
    let mut s = good();
    s.cursor = Cursor {
        row: 99,
        col: 99,
        visible: true,
        shape: CursorShape::Block,
    };
    let before = s.cursor;

    assert_eq!(
        s.validate(),
        Err(ApplyError::CursorOutOfBounds {
            row: 99,
            col: 99,
            rows: 3,
            cols: 4
        }),
        "the error must carry the ORIGINAL out-of-range position, not a repaired one"
    );
    assert_eq!(
        s.cursor, before,
        "validate() takes &self and must not repair anything"
    );
    assert_eq!(s.cursor.row, 99, "still out of range: nothing was clamped");
}

#[test]
fn i2_the_last_addressable_cell_is_in_bounds() {
    let mut s = good();
    s.cursor.row = 2;
    s.cursor.col = 3;
    assert_eq!(
        s.validate(),
        Ok(()),
        "row 2, col 3 is the last legal position in 3x4"
    );
}

#[test]
fn i2_a_zero_sized_screen_has_no_legal_cursor_position() {
    // 0x0 has no cell to sit on, so `blank` is the only way to build one and
    // its cursor is 0,0 - which is out of bounds by the strict rule. The
    // degenerate case is allowed through deliberately, and this test pins
    // that decision so it cannot change by accident.
    let s = ScreenState::blank(0, 0).unwrap();
    assert_eq!(s.cursor.row, 0);
    assert_eq!(s.cursor.col, 0);
    assert_eq!(s.validate(), Ok(()));
}

// ---------------------------------------------------------------- I3

#[test]
fn i3_sequence_zero_is_the_sentinel_and_never_a_real_state() {
    let mut s = good();
    s.seq = 0;
    assert_eq!(s.validate(), Err(ApplyError::SeqZero));
}

#[test]
fn i3_a_fresh_state_starts_at_one() {
    assert_eq!(good().seq, 1, "attach resets to 1, not 0");
    assert_eq!(good().validate(), Ok(()));
}

// ---------------------------------------------------------------- I4

#[test]
fn i4_the_struct_has_no_icon_field() {
    // An exhaustive struct literal is the check. `ScreenState` has no
    // `#[non_exhaustive]`, so this fails to COMPILE the moment anyone adds a
    // field - including an `icon` someone adds back after seeing OSC 1 in a
    // terminal spec. vte silently drops OSC 1: there is no `b"1"` arm in
    // osc_dispatch and no Handler method, so an icon field could only ever
    // hold a value oxutrm invented.
    let s = ScreenState {
        seq: 1,
        rows: 1,
        cols: 1,
        cells: vec![Cell::blank()],
        cursor: Cursor {
            row: 0,
            col: 0,
            visible: true,
            shape: CursorShape::Block,
        },
        modes: Modes::default(),
        title: String::new(),
        bell: 0,
        scrollback_len: 0,
    };
    assert_eq!(s.validate(), Ok(()));
}

#[test]
fn i4_the_title_is_the_only_osc_derived_string() {
    let mut s = good();
    s.title = "vim ~/src".to_owned();
    assert_eq!(s.validate(), Ok(()));
    assert_eq!(s.title, "vim ~/src");
}

// ---------------------------------------------------------------- I5
//
// The I5 and I6 cases below call `validate_transition` DIRECTLY, and that
// proves only that the function computes the right answer. It says nothing
// about whether the path a state actually travels ever asks — and for the
// whole life of this crate it did not, while these tests reported green.
// `oxutrm-sync/tests/faults.rs` is where enforcement is proved, by pushing a
// backwards bell and a shrinking scrollback through `Receiver::on_frame`.
// Keep both: these pin the rule, that one pins the caller.

#[test]
fn i5_a_bell_that_goes_backwards_is_rejected() {
    // The bell is a COUNTER, not a flag, and this is why: the client rings
    // once per increment. A datagram that lost a flag would lose the bell
    // outright; a datagram that loses a counter loses nothing, because the
    // next state still reports the higher number. That only works if the
    // number never goes down.
    let mut before = good();
    before.bell = 7;
    let mut after = before.clone();
    after.seq = 2;
    after.bell = 6;

    assert_eq!(after.validate(), Ok(()), "it is a legal state on its own");
    assert_eq!(
        after.validate_transition(&before),
        Err(ApplyError::BellWentBackwards { was: 7, now: 6 }),
        "but not a legal successor"
    );
}

#[test]
fn i5_a_bell_may_stay_put_or_jump_by_more_than_one() {
    let mut before = good();
    before.bell = 7;

    let mut same = before.clone();
    same.seq = 2;
    assert_eq!(same.validate_transition(&before), Ok(()));

    // Several bells between two transmitted states collapse into one jump.
    let mut jumped = before.clone();
    jumped.seq = 2;
    jumped.bell = 11;
    assert_eq!(jumped.validate_transition(&before), Ok(()));
}

#[test]
fn i5_a_reset_bell_is_the_specific_case_that_must_never_pass() {
    // Resetting to zero would ring the terminal once for every bell in the
    // session's history the next time it climbed back up.
    let mut before = good();
    before.bell = 42;
    let mut after = before.clone();
    after.seq = 2;
    after.bell = 0;
    assert_eq!(
        after.validate_transition(&before),
        Err(ApplyError::BellWentBackwards { was: 42, now: 0 })
    );
}

// ---------------------------------------------------------------- I6

#[test]
fn i6_scrollback_length_never_shrinks() {
    // It counts lines that have scrolled off for good. The lines themselves
    // never travel in a datagram - they are fetched on a stream - so this
    // number is the client's only way to know how much history exists.
    let mut before = good();
    before.scrollback_len = 5_000;
    let mut after = before.clone();
    after.seq = 2;
    after.scrollback_len = 4_999;
    assert_eq!(
        after.validate_transition(&before),
        Err(ApplyError::ScrollbackShrank {
            was: 5_000,
            now: 4_999
        })
    );
}

#[test]
fn i6_scrollback_length_may_grow_by_any_amount() {
    let mut before = good();
    before.scrollback_len = 5_000;
    let mut after = before.clone();
    after.seq = 2;
    // `history_size()` saturates at capacity, so the accumulated count can
    // jump by a whole screenful between two transmitted states.
    after.scrollback_len = 5_000 + 10_000;
    assert_eq!(after.validate_transition(&before), Ok(()));
}

#[test]
fn i6_scrollback_length_is_wide_enough_for_a_long_lived_session() {
    let mut s = good();
    s.scrollback_len = u64::MAX;
    assert_eq!(
        s.validate(),
        Ok(()),
        "a u64 line counter cannot realistically overflow"
    );
}

// ------------------------------------------------- transition, in general

#[test]
fn a_valid_transition_still_validates_both_states() {
    let before = good();
    let mut after = before.clone();
    after.seq = 2;
    after.cells.pop();
    assert_eq!(
        after.validate_transition(&before),
        Err(ApplyError::LengthMismatch {
            len: 11,
            rows: 3,
            cols: 4
        }),
        "a transition check must not let a malformed state through"
    );
}

// ---------------------------------------------------------------- I7

/// I7 is the one invariant that must be checked BEFORE the state exists.
///
/// `rows` and `cols` are `u16` and a `Cell` is around 40 bytes, so the wire can
/// name 4.29e9 cells — about 170 GB — in a message of a few bytes. Every other
/// invariant here is checked on a built state, because building it is cheap.
/// Building this one is the attack, so `blank` and the resize arm of `apply`
/// both call [`TermSize::check_bounds`] first, and `validate` checks it again
/// so that no oversized state can exist however it was made.
#[test]
fn i7_a_screen_larger_than_the_cap_cannot_be_constructed() {
    // 1024x1024 is just over a million cells, well past the cap, and small
    // enough that this test run RED merely allocates 40 MB rather than
    // taking the machine down.
    assert_eq!(
        ScreenState::blank(1024, 1024),
        Err(ApplyError::ScreenTooLarge {
            rows: 1024,
            cols: 1024
        })
    );
}

#[test]
fn i7_the_largest_size_the_wire_can_name_is_refused() {
    // Safe to run only because the check exists. Do not prove this one red.
    assert_eq!(
        ScreenState::blank(u16::MAX, u16::MAX),
        Err(ApplyError::ScreenTooLarge {
            rows: u16::MAX,
            cols: u16::MAX
        })
    );
}

/// The per-side bound is not implied by the cell bound. A 65535x2 screen is
/// only 131,070 cells — under [`MAX_SCREEN_CELLS`] — and is still nonsense.
#[test]
fn i7_a_single_dimension_is_bounded_even_when_the_area_is_not() {
    let size = TermSize {
        rows: u16::MAX,
        cols: 2,
    };
    assert!(
        (size.rows as usize * size.cols as usize) < MAX_SCREEN_CELLS,
        "this test is only meaningful while the area is under the cell cap"
    );
    assert_eq!(
        size.check_bounds(),
        Err(ApplyError::ScreenTooLarge {
            rows: u16::MAX,
            cols: 2
        })
    );
}

/// I7 is checked before I1, so a state that breaks both reports the one that
/// would have cost something. Getting this order wrong is not cosmetic: it is
/// the difference between refusing an allocation and reporting it.
#[test]
fn i7_is_checked_before_the_length() {
    let mut s = good();
    s.rows = 4096;
    s.cols = 4096;
    // `cells` is still 12 long, so I1 is violated too.
    assert_eq!(
        s.validate(),
        Err(ApplyError::ScreenTooLarge {
            rows: 4096,
            cols: 4096
        })
    );
}

/// The cap is a ceiling, not a policy. The largest terminal anyone actually
/// runs must sit well inside it.
#[test]
fn i7_a_real_terminal_is_nowhere_near_the_cap() {
    // A 4K display at a 6-pixel font is about 400x120.
    ScreenState::blank(120, 400).expect("a 400x120 terminal is ordinary");
    // Both sides are constants, so the claim is checkable at compile time and
    // clippy insists it be checked there. That is the stronger form anyway: a
    // ceiling lowered below an ordinary terminal should fail the build rather
    // than one test run.
    const { assert!(MAX_SCREEN_DIM > 400) };
    const { assert!(MAX_SCREEN_CELLS > 400 * 120 * 4) };
}

#[test]
fn a_resize_between_two_states_is_a_legal_transition() {
    let before = good();
    let mut after = ScreenState::blank(10, 20).unwrap();
    after.seq = 2;
    after.bell = before.bell;
    after.scrollback_len = before.scrollback_len;
    assert_eq!(after.validate_transition(&before), Ok(()));
}

// ------------------------------------------------------------ value types

#[test]
fn a_blank_cell_is_one_space_with_nothing_set() {
    let c = Cell::blank();
    assert_eq!(c.text, " ");
    assert_eq!(c.fg, Color::Default);
    assert_eq!(c.bg, Color::Default);
    assert_eq!(c.attrs, Attrs::empty());
    assert_eq!(
        c,
        Cell::default(),
        "Default and blank() must not drift apart"
    );
}

#[test]
fn a_cell_holding_a_grapheme_cluster_still_allocates_nothing() {
    // The reason CellText is a CompactString and not a String: the design
    // keeps a ring of 32 states, so an 80x24 session would otherwise hold
    // roughly 61,000 live heap allocations.
    let base = Cell {
        text: "e\u{0301}".into(),
        ..Cell::blank()
    };
    assert_eq!(base.text.len(), 3, "e + combining acute is three bytes");
    assert!(
        !base.text.is_heap_allocated(),
        "up to 24 bytes stays inline"
    );

    let wide = Cell {
        text: "\u{4f60}".into(),
        ..Cell::blank()
    };
    assert!(!wide.text.is_heap_allocated());
}

#[test]
fn the_wide_continuation_flag_is_the_ninth_bit() {
    // Eight SGR attributes fit in a byte; WIDE_CONT is structural rather than
    // an SGR attribute, which is why Attrs is a u16 and not a u8.
    assert_eq!(Attrs::WIDE_CONT.bits(), 0b0001_0000_0000);
    assert!(Attrs::WIDE_CONT.bits() > 0xFF);

    let all_sgr = Attrs::BOLD
        | Attrs::ITALIC
        | Attrs::UNDERLINE
        | Attrs::INVERSE
        | Attrs::BLINK
        | Attrs::STRIKE
        | Attrs::DIM
        | Attrs::HIDDEN;
    assert_eq!(
        all_sgr.bits(),
        0xFF,
        "the eight SGR attributes fill exactly one byte"
    );
    assert!(!all_sgr.contains(Attrs::WIDE_CONT));
}

#[test]
fn the_right_half_of_a_wide_character_is_explicit_not_a_space() {
    // Spec §8.2: wide-character continuation cells are represented
    // explicitly. A renderer that saw a space here would emit one and shift
    // everything after it by a column.
    let lead = Cell {
        text: "\u{4f60}".into(),
        ..Cell::blank()
    };
    let cont = Cell {
        text: CellText::new(""),
        attrs: Attrs::WIDE_CONT,
        ..Cell::blank()
    };
    assert!(!lead.attrs.contains(Attrs::WIDE_CONT));
    assert!(cont.attrs.contains(Attrs::WIDE_CONT));
    assert_ne!(cont.text, " ", "a continuation cell is not a space");
}

#[test]
fn modes_default_to_everything_off() {
    let m = Modes::default();
    assert!(!m.alt_screen);
    assert!(!m.bracketed_paste);
    assert!(!m.app_cursor);
    assert!(!m.app_keypad);
    assert_eq!(m.mouse, MouseMode::Off);
}

#[test]
fn a_blank_screen_is_blank_everywhere() {
    let s = ScreenState::blank(4, 6).unwrap();
    assert_eq!(s.rows, 4);
    assert_eq!(s.cols, 6);
    assert_eq!(s.cells.len(), 24);
    assert!(s.cells.iter().all(|c| *c == Cell::blank()));
    assert_eq!(s.bell, 0);
    assert_eq!(s.scrollback_len, 0);
    assert_eq!(s.title, "");
    assert!(s.cursor.visible);
    assert_eq!(s.modes, Modes::default());
}

#[test]
fn every_row_is_reachable_and_the_right_width() {
    let s = ScreenState::blank(4, 6).unwrap();
    for r in 0..4u16 {
        assert_eq!(s.row(r).len(), 6, "row {r}");
    }
}

// --------------------------------------------------------- wire compatibility

#[test]
fn cell_text_encodes_byte_identically_to_a_string() {
    // The contract claims CellText can be swapped for String in one line
    // without a protocol change. That is only true if both encode the same
    // bytes, so it is checked rather than asserted in a comment.
    #[derive(serde::Serialize)]
    struct AsString {
        text: String,
    }
    #[derive(serde::Serialize)]
    struct AsCellText {
        text: CellText,
    }

    for sample in [
        "",
        " ",
        "x",
        "e\u{0301}",
        "\u{4f60}",
        "a rather longer run of text",
    ] {
        let a = postcard::to_stdvec(&AsString {
            text: sample.to_owned(),
        })
        .unwrap();
        let b = postcard::to_stdvec(&AsCellText {
            text: sample.into(),
        })
        .unwrap();
        assert_eq!(a, b, "encoding differs for {sample:?}");
    }
}

#[test]
fn a_screen_state_survives_a_postcard_round_trip() {
    let mut s = good();
    s.title = "vim ~/src".to_owned();
    s.bell = 3;
    s.scrollback_len = 1_234;
    s.cells[5] = Cell {
        text: "\u{4f60}".into(),
        fg: Color::Rgb(1, 2, 3),
        bg: Color::Idx(42),
        attrs: Attrs::BOLD | Attrs::WIDE_CONT,
    };
    s.modes = Modes {
        alt_screen: true,
        mouse: MouseMode::AnyMotion,
        ..Modes::default()
    };

    let bytes = postcard::to_stdvec(&s).unwrap();
    let back: ScreenState = postcard::from_bytes(&bytes).unwrap();
    assert_eq!(back, s);
    assert_eq!(back.validate(), Ok(()));
}

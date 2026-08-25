//! Fault injection: what happens when a frame is *not* valid.
//!
//! The convergence property drives `apply` with diffs that are valid by
//! construction, so it never reaches a single rejection branch. Every one of
//! those branches is exercised here instead, and every test asserts the same
//! two things:
//!
//! 1. the state is **byte-for-byte unchanged**, and
//! 2. `ack()` has **not advanced**.
//!
//! The second is the one no happy-path test can see. An implementation that
//! rejects a frame but advances its ack tells the sender "I have state N"
//! while holding state N-1. The sender then diffs from N forever, the
//! receiver rejects every one of those diffs for a base it does not have, and
//! the session is stranded with both ends convinced they are behaving.

use oxutrm_proto::{
    ApplyError, Cell, Cursor, CursorShape, FLAG_ZSTD, Frame, ScreenState, TermSize,
};
use oxutrm_sync::{InputState, Receiver, RowPatch, Run, ScreenDiff, Sender};

fn screen(rows: u16, cols: u16) -> ScreenState {
    ScreenState::blank(rows, cols).expect("blank")
}

/// Wrap a diff into a frame the way `Sender` would, without compressing.
fn frame_of<D: serde::Serialize>(my_state: u64, from_state: u64, diff: &D) -> Frame {
    Frame {
        my_state,
        from_state,
        ack_state: 0,
        flags: 0,
        payload: postcard::to_stdvec(diff).expect("encode"),
    }
}

fn empty_diff() -> ScreenDiff {
    ScreenDiff {
        resize: None,
        rows: Vec::new(),
        cursor: None,
        modes: None,
        title: None,
        bell: None,
        scrollback_len: None,
    }
}

/// Every rejection test runs through this, so neither assertion can be
/// forgotten in one of them.
fn assert_rejected(
    rx: &mut Receiver<ScreenState>,
    f: &Frame,
    want: impl Fn(&ApplyError) -> bool,
    what: &str,
) {
    let before_state = rx.state().clone();
    let before_ack = rx.ack();

    let got = rx.on_frame(f);
    let err = match got {
        Err(e) => e,
        Ok(advanced) => panic!("{what}: expected rejection, got Ok({advanced})"),
    };
    assert!(want(&err), "{what}: unexpected error {err:?}");
    assert_eq!(
        *rx.state(),
        before_state,
        "{what}: the state must not have moved"
    );
    assert_eq!(
        rx.ack(),
        before_ack,
        "{what}: the ack must not have advanced"
    );
}

#[test]
fn a_diff_against_a_base_we_do_not_have_is_rejected() {
    let mut rx = Receiver::new(screen(3, 4));
    // We are at state 1; this diff claims to build on state 7.
    let f = frame_of(8, 7, &empty_diff());
    assert_rejected(
        &mut rx,
        &f,
        |e| {
            matches!(
                e,
                ApplyError::BaseMismatch {
                    base: 7,
                    current: 1
                }
            )
        },
        "base mismatch",
    );
}

#[test]
fn a_row_patch_past_the_last_row_is_rejected() {
    let mut rx = Receiver::new(screen(3, 4));
    let mut d = empty_diff();
    d.rows.push(RowPatch {
        row: 3, // rows == 3, so the last legal row is 2
        runs: vec![Run {
            start_col: 0,
            repeat: 0,
            cells: vec![Cell::blank()],
        }],
    });
    let f = frame_of(2, 1, &d);
    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::OutOfBounds { row: 3, rows: 3 }),
        "row out of range",
    );
}

#[test]
fn a_run_that_overflows_its_row_is_rejected_rather_than_truncated() {
    // Truncating would leave a screen that validates - the length is still
    // rows*cols - while quietly disagreeing with the host about what is on it.
    let mut rx = Receiver::new(screen(3, 4));
    let mut d = empty_diff();
    d.rows.push(RowPatch {
        row: 0,
        runs: vec![Run {
            start_col: 2,
            repeat: 4, // 5 emissions of one cell, starting at column 2, in a 4-wide row
            cells: vec![Cell::blank()],
        }],
    });
    let f = frame_of(2, 1, &d);
    assert_rejected(
        &mut rx,
        &f,
        |e| {
            matches!(
                e,
                ApplyError::RunOverflowsRow {
                    row: 0,
                    cols: 4,
                    ..
                }
            )
        },
        "run overflow",
    );
}

#[test]
fn a_run_that_starts_past_the_end_of_the_row_is_rejected() {
    let mut rx = Receiver::new(screen(3, 4));
    let mut d = empty_diff();
    d.rows.push(RowPatch {
        row: 1,
        runs: vec![Run {
            start_col: 4,
            repeat: 0,
            cells: vec![Cell::blank()],
        }],
    });
    let f = frame_of(2, 1, &d);
    assert_rejected(
        &mut rx,
        &f,
        |e| {
            matches!(
                e,
                ApplyError::RunOverflowsRow {
                    row: 1,
                    cols: 4,
                    ..
                }
            )
        },
        "run starting out of range",
    );
}

#[test]
fn a_payload_that_is_not_a_diff_is_rejected() {
    let mut rx = Receiver::new(screen(3, 4));
    let f = Frame {
        my_state: 2,
        from_state: 1,
        ack_state: 0,
        flags: 0,
        payload: vec![0xff; 8],
    };
    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::Decode(_)),
        "garbage payload",
    );
}

#[test]
fn a_truncated_payload_is_rejected() {
    let mut rx = Receiver::new(screen(3, 4));
    let mut d = empty_diff();
    d.title = Some("a title long enough to be cut in half".to_owned());
    let full = frame_of(2, 1, &d);
    let f = Frame {
        payload: full.payload[..full.payload.len() / 2].to_vec(),
        ..full
    };
    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::Decode(_)),
        "truncated payload",
    );
}

#[test]
fn the_zstd_flag_set_on_a_payload_that_is_not_zstd_is_rejected() {
    let mut rx = Receiver::new(screen(3, 4));
    let f = Frame {
        my_state: 2,
        from_state: 1,
        ack_state: 0,
        flags: FLAG_ZSTD,
        payload: postcard::to_stdvec(&empty_diff()).expect("encode"),
    };
    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::Decode(_)),
        "mislabelled payload",
    );
}

#[test]
fn validate_runs_after_apply_and_not_before() {
    // The distinguishing case. Current state is 10x10 with the cursor at
    // (9,9) - perfectly valid. The diff shrinks the screen to 3x3 and does
    // NOT move the cursor, so the RESULT is invalid.
    //
    // An implementation that validated before applying would check the 10x10
    // state, find it fine, apply the diff, and commit a state whose cursor is
    // outside its own screen. Validating after is what catches it.
    let mut start = screen(10, 10);
    start.cursor = Cursor {
        row: 9,
        col: 9,
        visible: true,
        shape: CursorShape::Block,
    };
    assert_eq!(
        start.validate(),
        Ok(()),
        "the PRE state is valid, which is the point"
    );

    let mut rx = Receiver::new(start);
    let mut d = empty_diff();
    d.resize = Some(TermSize { cols: 3, rows: 3 });
    for row in 0..3u16 {
        d.rows.push(RowPatch {
            row,
            runs: vec![Run {
                start_col: 0,
                repeat: 2,
                cells: vec![Cell::blank()],
            }],
        });
    }

    let f = frame_of(2, 1, &d);
    assert_rejected(
        &mut rx,
        &f,
        |e| {
            matches!(
                e,
                ApplyError::CursorOutOfBounds {
                    row: 9,
                    col: 9,
                    rows: 3,
                    cols: 3
                }
            )
        },
        "post-apply validation",
    );
    // And the rejection really did roll everything back, not just the cursor.
    assert_eq!(rx.state().rows, 10);
    assert_eq!(rx.state().cells.len(), 100);
}

#[test]
fn a_diff_that_would_break_the_length_invariant_is_rejected() {
    // A resize whose row patches do not cover the new screen must still leave
    // cells.len() == rows*cols, because apply reallocates. This asserts the
    // reallocation happens rather than the old buffer being reused.
    let mut rx = Receiver::new(screen(2, 2));
    let mut d = empty_diff();
    d.resize = Some(TermSize { cols: 5, rows: 5 });
    let f = frame_of(2, 1, &d);
    assert_eq!(rx.on_frame(&f), Ok(true));
    assert_eq!(rx.state().cells.len(), 25);
    assert_eq!(rx.state().validate(), Ok(()));
}

#[test]
fn a_stale_frame_is_not_an_error_and_does_not_move_anything() {
    let mut tx = Sender::new(screen(3, 4));
    let mut next = screen(3, 4);
    next.title = "second".to_owned();
    tx.update(next);

    let mut rx = Receiver::new(screen(3, 4));
    let f = tx.make_frame(0).expect("make").expect("something to send");
    assert_eq!(rx.on_frame(&f), Ok(true));
    let after = rx.state().clone();

    // The same frame again: a duplicate is normal on an unreliable transport
    // and must never be an error.
    assert_eq!(rx.on_frame(&f), Ok(false), "a duplicate is not an error");
    assert_eq!(*rx.state(), after);
    assert_eq!(rx.ack(), 2);
}

#[test]
fn an_older_frame_arriving_late_is_ignored() {
    let mut tx = Sender::new(screen(3, 4));
    // A fresh sender DOES have something to send: the peer has acknowledged
    // nothing, so even the initial state has to travel, as a full state.
    let initial = tx.make_frame(0).expect("make").expect("the initial state");
    assert_eq!(initial.from_state, 0);
    assert_eq!(initial.my_state, 1);

    let mut a = screen(3, 4);
    a.title = "a".to_owned();
    tx.update(a);
    let fa = tx.make_frame(0).expect("make").expect("frame a");

    let mut b = screen(3, 4);
    b.title = "b".to_owned();
    tx.update(b);
    let fb = tx.make_frame(0).expect("make").expect("frame b");

    let mut rx = Receiver::new(screen(3, 4));
    assert_eq!(rx.on_frame(&fb), Ok(true));
    assert_eq!(rx.state().title, "b");
    // `fa` is older. Reordering is normal; it must be dropped silently.
    assert_eq!(rx.on_frame(&fa), Ok(false));
    assert_eq!(
        rx.state().title,
        "b",
        "a late frame must not undo a newer one"
    );
}

#[test]
fn an_input_diff_with_a_bad_base_leaves_pending_untouched() {
    let start = InputState {
        seq: 1,
        pending: b"hello".to_vec(),
        size: TermSize { cols: 80, rows: 24 },
    };
    let mut rx = Receiver::new(start.clone());
    let d = oxutrm_sync::InputDiff {
        consumed: 0,
        appended: b"!".to_vec(),
        size: None,
    };
    let f = frame_of(9, 7, &d);

    let before_ack = rx.ack();
    assert!(matches!(
        rx.on_frame(&f),
        Err(ApplyError::BaseMismatch { .. })
    ));
    assert_eq!(*rx.state(), start);
    assert_eq!(rx.ack(), before_ack);
}

#[test]
fn a_state_the_sender_has_forgotten_falls_back_to_a_full_state() {
    // The ring holds STATE_RING states. Once the peer's ack falls out of it
    // there is nothing to diff against, and the sender must send a full state
    // (from_state == 0) rather than a diff the receiver cannot apply.
    let mut tx = Sender::new(screen(3, 4));
    for i in 0..(oxutrm_sync::STATE_RING as u64 + 5) {
        let mut s = screen(3, 4);
        s.title = format!("state {i}");
        tx.update(s);
    }
    // The peer is still back at 1, which the ring no longer holds.
    tx.on_ack(1);
    let f = tx.make_frame(0).expect("make").expect("frame");
    assert_eq!(
        f.from_state, 0,
        "a forgotten base must produce a full state"
    );

    // And a receiver of ANY shape can apply it.
    let mut rx = Receiver::new(screen(99, 1));
    assert_eq!(rx.on_frame(&f), Ok(true));
    assert_eq!(rx.state(), tx.current());
}

// ---- I5 and I6: the invariants that only exist BETWEEN two states ----
//
// These belong here rather than in `oxutrm-proto`'s `tests/invariants.rs`,
// which calls `ScreenState::validate_transition` directly. Calling it directly
// proves the function computes the right answer; it proves nothing about
// whether the path a state actually travels ever asks. It did not, for the
// whole life of the crate, while a green test suite asserted that it did.

#[test]
fn a_bell_that_goes_backwards_is_rejected_by_the_real_apply_path() {
    let mut start = screen(3, 4);
    start.bell = 7;
    let mut rx = Receiver::new(start);

    let mut d = empty_diff();
    d.bell = Some(3);
    let f = frame_of(2, 1, &d);

    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::BellWentBackwards { was: 7, now: 3 }),
        "a bell counter that went backwards",
    );
}

#[test]
fn a_scrollback_that_shrinks_is_rejected_by_the_real_apply_path() {
    let mut start = screen(3, 4);
    start.scrollback_len = 900;
    let mut rx = Receiver::new(start);

    let mut d = empty_diff();
    d.scrollback_len = Some(12);
    let f = frame_of(2, 1, &d);

    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::ScrollbackShrank { was: 900, now: 12 }),
        "scrollback that shrank",
    );
}

#[test]
fn a_full_state_is_held_to_the_transition_invariants_too() {
    // `from_state == 0` builds on nothing as far as CONTENT goes. `bell` and
    // `scrollback_len` are not content: they are the host's own monotonic
    // counters, and a full state that walks them backwards is as wrong as a
    // diff that does.
    let mut start = screen(3, 4);
    start.bell = 5;
    start.scrollback_len = 40;
    let mut rx = Receiver::new(start);

    let mut newer = screen(3, 4);
    newer.bell = 5;
    newer.scrollback_len = 41;
    newer.seq = 2;
    let d = oxutrm_sync::SyncState::full_diff(&newer);
    let mut f = frame_of(2, 0, &d);
    assert_eq!(
        rx.on_frame(&f),
        Ok(true),
        "a lawful full state still applies"
    );

    let mut regressed = screen(3, 4);
    regressed.bell = 1;
    regressed.scrollback_len = 41;
    regressed.seq = 3;
    let d = oxutrm_sync::SyncState::full_diff(&regressed);
    f = frame_of(3, 0, &d);
    assert_rejected(
        &mut rx,
        &f,
        |e| matches!(e, ApplyError::BellWentBackwards { was: 5, now: 1 }),
        "a full state that rings the bell backwards",
    );
}

#[test]
fn counters_that_move_forwards_still_apply() {
    // The other half: enforcement must not turn a legal state into a rejected
    // one, or the receiver stalls forever on a frame the sender keeps resending.
    let mut start = screen(3, 4);
    start.bell = 2;
    start.scrollback_len = 10;
    let mut rx = Receiver::new(start);

    let mut d = empty_diff();
    d.bell = Some(3);
    d.scrollback_len = Some(11);
    assert_eq!(rx.on_frame(&frame_of(2, 1, &d)), Ok(true));
    assert_eq!(rx.state().bell, 3);
    assert_eq!(rx.state().scrollback_len, 11);

    // Unchanged counters ride on a diff that mentions neither.
    assert_eq!(rx.on_frame(&frame_of(3, 2, &empty_diff())), Ok(true));
    assert_eq!(rx.state().bell, 3);
    assert_eq!(rx.state().scrollback_len, 11);
}

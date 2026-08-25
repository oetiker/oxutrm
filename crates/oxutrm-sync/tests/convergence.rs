//! The property that matters.
//!
//! For any sequence of terminal output, and any subset of the resulting frames
//! dropped, duplicated or reordered, the receiver converges to the sender's
//! current state — and satisfies `validate()` at every step along the way.
//!
//! This is the whole justification for the state-diff design and for this
//! crate having no I/O: the property is exhaustively checkable with no socket,
//! no PTY and no clock.
//!
//! Note what this file does **not** cover. Every diff here is valid by
//! construction, so `apply`'s rejection branches are never reached. Those live
//! in `faults.rs`, and neither file substitutes for the other.

use oxutrm_proto::{
    Attrs, Cell, Color, Cursor, CursorShape, Modes, MouseMode, ScreenState, TermSize,
};
use oxutrm_sync::{InputState, Receiver, Sender, SyncState};
use proptest::prelude::*;

/// One thing that can happen to a terminal between two states.
#[derive(Clone, Debug)]
enum Edit {
    Write { row: u16, col: u16, ch: char },
    FillRow { row: u16, ch: char },
    MoveCursor { row: u16, col: u16 },
    Resize { rows: u16, cols: u16 },
    Bell,
    Title(String),
    Scroll(u16),
    ToggleAltScreen,
    Recolour { row: u16, col: u16 },
}

/// What the transport does to a frame.
#[derive(Clone, Copy, Debug)]
enum Delivery {
    Drop,
    Once,
    Twice,
    /// Held back and delivered after the next one, i.e. reordered.
    Delay,
}

fn edit_strategy() -> impl Strategy<Value = Edit> {
    prop_oneof![
        (0u16..8, 0u16..8, prop::char::range('a', 'z')).prop_map(|(row, col, ch)| Edit::Write {
            row,
            col,
            ch
        }),
        (0u16..8, prop::char::range('a', 'z')).prop_map(|(row, ch)| Edit::FillRow { row, ch }),
        (0u16..8, 0u16..8).prop_map(|(row, col)| Edit::MoveCursor { row, col }),
        (1u16..8, 1u16..8).prop_map(|(rows, cols)| Edit::Resize { rows, cols }),
        Just(Edit::Bell),
        "[a-z ]{0,12}".prop_map(Edit::Title),
        (1u16..40).prop_map(Edit::Scroll),
        Just(Edit::ToggleAltScreen),
        (0u16..8, 0u16..8).prop_map(|(row, col)| Edit::Recolour { row, col }),
    ]
}

fn delivery_strategy() -> impl Strategy<Value = Delivery> {
    prop_oneof![
        3 => Just(Delivery::Once),
        1 => Just(Delivery::Drop),
        1 => Just(Delivery::Twice),
        1 => Just(Delivery::Delay),
    ]
}

/// Apply one edit, producing a state that is valid by construction.
fn apply_edit(s: &ScreenState, e: &Edit) -> ScreenState {
    let mut next = s.clone();
    match e {
        Edit::Write { row, col, ch } => {
            if next.rows > 0 && next.cols > 0 {
                let r = row % next.rows;
                let c = col % next.cols;
                let idx = r as usize * next.cols as usize + c as usize;
                next.cells[idx].text = ch.to_string().into();
            }
        }
        Edit::FillRow { row, ch } => {
            if next.rows > 0 && next.cols > 0 {
                let r = (row % next.rows) as usize;
                let width = next.cols as usize;
                for c in 0..width {
                    next.cells[r * width + c].text = ch.to_string().into();
                }
            }
        }
        Edit::MoveCursor { row, col } => {
            if next.rows > 0 && next.cols > 0 {
                next.cursor.row = row % next.rows;
                next.cursor.col = col % next.cols;
            }
        }
        Edit::Resize { rows, cols } => {
            let mut resized = ScreenState::blank(*rows, *cols).expect("blank");
            // Carry over what still fits, the way a real reflow would.
            for r in 0..(*rows).min(next.rows) {
                for c in 0..(*cols).min(next.cols) {
                    let from = r as usize * next.cols as usize + c as usize;
                    let to = r as usize * *cols as usize + c as usize;
                    resized.cells[to] = next.cells[from].clone();
                }
            }
            resized.seq = next.seq;
            resized.title = next.title.clone();
            resized.bell = next.bell;
            resized.scrollback_len = next.scrollback_len;
            resized.modes = next.modes;
            resized.cursor = Cursor {
                row: next.cursor.row.min(rows.saturating_sub(1)),
                col: next.cursor.col.min(cols.saturating_sub(1)),
                ..next.cursor
            };
            next = resized;
        }
        Edit::Bell => next.bell = next.bell.saturating_add(1),
        Edit::Title(t) => next.title = t.clone(),
        Edit::Scroll(n) => next.scrollback_len = next.scrollback_len.saturating_add(*n as u64),
        Edit::ToggleAltScreen => next.modes.alt_screen = !next.modes.alt_screen,
        Edit::Recolour { row, col } => {
            if next.rows > 0 && next.cols > 0 {
                let r = row % next.rows;
                let c = col % next.cols;
                let idx = r as usize * next.cols as usize + c as usize;
                next.cells[idx].fg = Color::Idx((r as u8).wrapping_add(c as u8));
                next.cells[idx].bg = Color::Rgb(r as u8, c as u8, 7);
                next.cells[idx].attrs = Attrs::BOLD | Attrs::UNDERLINE;
            }
        }
    }
    next
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(160))]

    /// The convergence property, in full.
    #[test]
    fn the_receiver_converges_however_the_transport_misbehaves(
        edits in prop::collection::vec(edit_strategy(), 1..24),
        schedule in prop::collection::vec(delivery_strategy(), 1..24),
    ) {
        let start = ScreenState::blank(4, 6).expect("blank");
        let mut tx = Sender::new(start.clone());
        let mut rx = Receiver::new(start);

        let mut held: Option<oxutrm_proto::Frame> = None;

        for (i, edit) in edits.iter().enumerate() {
            let next = apply_edit(tx.current(), edit);
            prop_assert_eq!(next.validate(), Ok(()), "the test's own edit produced an invalid state");
            tx.update(next);

            // The sender always diffs against what the receiver last told it.
            tx.on_ack(rx.ack());
            let frame = match tx.make_frame(rx.ack()).expect("make_frame") {
                Some(f) => f,
                None => continue,
            };

            let how = schedule[i % schedule.len()];
            let mut to_deliver: Vec<oxutrm_proto::Frame> = Vec::new();
            match how {
                Delivery::Drop => {}
                Delivery::Once => to_deliver.push(frame),
                Delivery::Twice => {
                    to_deliver.push(frame.clone());
                    to_deliver.push(frame);
                }
                Delivery::Delay => {
                    // Reordering: this one waits for the next round.
                    if let Some(old) = held.replace(frame) {
                        to_deliver.push(old);
                    }
                }
            }
            if !matches!(how, Delivery::Delay) {
                if let Some(old) = held.take() {
                    // The delayed frame now arrives AFTER a newer one.
                    to_deliver.push(old);
                }
            }

            for f in to_deliver {
                // A frame whose base the receiver no longer holds is refused;
                // that is the transport's problem, not a convergence failure.
                let _ = rx.on_frame(&f);
                prop_assert_eq!(
                    rx.state().validate(),
                    Ok(()),
                    "the receiver's state must be valid after EVERY frame"
                );
            }
        }

        // The transport keeps trying. Convergence means: once frames get
        // through, the receiver ends up exactly where the sender is.
        for _ in 0..4 {
            tx.on_ack(rx.ack());
            match tx.make_frame(rx.ack()).expect("make_frame") {
                Some(f) => {
                    rx.on_frame(&f).expect("a frame built from the receiver's own ack must apply");
                }
                None => break,
            }
            prop_assert_eq!(rx.state().validate(), Ok(()));
        }

        prop_assert_eq!(rx.state(), tx.current(), "the receiver did not converge");
        prop_assert_eq!(rx.ack(), tx.current().seq());
    }

    /// The same property for the other direction, where the payload is a byte
    /// stream rather than a screen.
    #[test]
    fn input_state_converges_too(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..8), 1..20),
        consumes in prop::collection::vec(0usize..6, 1..20),
        schedule in prop::collection::vec(delivery_strategy(), 1..20),
    ) {
        let size = TermSize { cols: 80, rows: 24 };
        let start = InputState { seq: 1, pending: Vec::new(), size };
        let mut tx = Sender::new(start.clone());
        let mut rx = Receiver::new(start);

        for (i, chunk) in chunks.iter().enumerate() {
            let next = tx
                .current()
                .append(chunk, size)
                .consume(consumes[i % consumes.len()]);
            tx.update(next);
            tx.on_ack(rx.ack());
            let Some(frame) = tx.make_frame(rx.ack()).expect("make_frame") else { continue };

            match schedule[i % schedule.len()] {
                Delivery::Drop => {}
                Delivery::Once | Delivery::Delay => {
                    let _ = rx.on_frame(&frame);
                }
                Delivery::Twice => {
                    let _ = rx.on_frame(&frame);
                    let _ = rx.on_frame(&frame);
                }
            }
        }

        for _ in 0..4 {
            tx.on_ack(rx.ack());
            match tx.make_frame(rx.ack()).expect("make_frame") {
                Some(f) => {
                    rx.on_frame(&f).expect("a frame built from the receiver's own ack must apply");
                }
                None => break,
            }
        }

        prop_assert_eq!(rx.state(), tx.current());
    }

    /// Dropping every frame but the last still converges, because a diff is
    /// computed against the peer's acknowledged state rather than the previous
    /// one. This is the property that lets a runaway `yes` produce one frame
    /// rather than a backlog.
    #[test]
    fn a_single_frame_after_a_total_blackout_carries_everything(
        edits in prop::collection::vec(edit_strategy(), 1..30),
    ) {
        let start = ScreenState::blank(4, 6).expect("blank");
        let mut tx = Sender::new(start.clone());
        let mut rx = Receiver::new(start);

        for edit in &edits {
            let next = apply_edit(tx.current(), edit);
            tx.update(next);
            // Every frame is thrown away.
            let _ = tx.make_frame(0);
        }

        tx.on_ack(rx.ack());
        let f = tx.make_frame(rx.ack()).expect("make_frame").expect("one frame");
        rx.on_frame(&f).expect("apply");
        prop_assert_eq!(rx.state(), tx.current(), "one frame must carry the whole backlog");
    }
}

#[test]
fn a_screen_that_never_changes_produces_no_frames() {
    // Staying detached for a week costs no bandwidth (spec §9.3): if the peer
    // is up to date there is nothing to send, and `make_frame` says so.
    let s = ScreenState::blank(4, 6).expect("blank");
    let mut tx = Sender::new(s.clone());
    let mut rx = Receiver::new(s);

    // TWO frames first, and the count is the whole handshake. Acknowledgements
    // travel only on frames, so until the peer has heard our ack once there IS
    // something to say.
    //
    // Frame one is the full state, and it carries `ack_state == 0`: the
    // receiver has applied nothing of the sender's yet and must not claim
    // otherwise (contract R5). Both ends start holding a state numbered 1, and
    // acknowledging one's OWN invented 1 asks the sender to diff against a
    // screen the receiver has never seen.
    let hello = tx
        .make_frame(rx.ack())
        .expect("make_frame")
        .expect("the first ack has to travel somehow");
    assert_eq!(hello.from_state, 0, "the first frame of an attach is full");
    assert_eq!(hello.ack_state, 0, "we held nothing of the peer's yet");
    rx.on_frame(&hello).expect("apply");
    tx.on_ack(rx.ack());

    // Frame two exists purely to carry the acknowledgement that only became
    // TRUE when frame one landed. Nothing of the sender's own state moved.
    let ack_only = tx
        .make_frame(rx.ack())
        .expect("make_frame")
        .expect("the acknowledgement owed for frame one");
    assert_eq!(ack_only.my_state, hello.my_state, "no state moved");
    assert_eq!(ack_only.ack_state, 1);

    // From here, nothing changes and nothing is sent: the handshake is two
    // frames and then silence. Staying detached for a week costs no bandwidth
    // (spec §9.3).
    assert!(tx.make_frame(rx.ack()).expect("make_frame").is_none());
    assert!(tx.make_frame(rx.ack()).expect("make_frame").is_none());

    let mut next = tx.current().clone();
    next.bell = 1;
    tx.update(next);
    let f = tx
        .make_frame(rx.ack())
        .expect("make_frame")
        .expect("one bell to send");
    rx.on_frame(&f).expect("apply");

    tx.on_ack(rx.ack());

    // One more frame is owed, and it is not the state: the peer has moved to
    // state 2 and has not yet heard us say so. An acknowledgement can only
    // travel on a frame, so a side with nothing of its own to say still sends
    // exactly one — carrying an empty diff.
    let ack_only = tx
        .make_frame(rx.ack())
        .expect("make_frame")
        .expect("the ack for state 2 has not been sent yet");
    assert_eq!(
        ack_only.ack_state, 2,
        "the ack-only frame must carry the ack it exists to deliver"
    );
    assert_eq!(
        ack_only.my_state, ack_only.from_state,
        "an ack-only frame describes no change of our own"
    );

    // And then silence, for as long as nothing moves.
    assert!(
        tx.make_frame(rx.ack()).expect("make_frame").is_none(),
        "with the peer caught up and the ack delivered there is nothing left \
         to send"
    );
    assert!(tx.make_frame(rx.ack()).expect("make_frame").is_none());
}

#[test]
fn every_screen_field_survives_a_round_trip() {
    let start = ScreenState::blank(3, 4).expect("blank");
    let mut tx = Sender::new(start.clone());
    let mut rx = Receiver::new(start);

    let mut next = tx.current().clone();
    next.title = "an interesting title".to_owned();
    next.bell = 9;
    next.scrollback_len = 12_345;
    next.cursor = Cursor {
        row: 2,
        col: 3,
        visible: false,
        shape: CursorShape::Bar,
    };
    next.modes = Modes {
        alt_screen: true,
        bracketed_paste: true,
        mouse: MouseMode::ButtonMotion,
        app_cursor: true,
        app_keypad: true,
    };
    next.cells[5] = Cell {
        text: "\u{4f60}".into(),
        fg: Color::Rgb(1, 2, 3),
        bg: Color::Idx(200),
        attrs: Attrs::BOLD | Attrs::WIDE_CONT,
    };
    tx.update(next);

    let f = tx.make_frame(rx.ack()).expect("make_frame").expect("frame");
    rx.on_frame(&f).expect("apply");
    assert_eq!(rx.state(), tx.current());
}

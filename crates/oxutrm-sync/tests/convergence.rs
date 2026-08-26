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
//!
//! # Acknowledgements are part of the transport too
//!
//! For a long time everything here handed the sender `rx.ack()` in the same
//! breath as it asked for a frame, which is **zero ack latency** — and zero ack
//! latency is the one condition under which base drift cannot happen at all.
//! The `Delivery` model abused frames and left acknowledgements pristine, so
//! contract rule R4 — a diff applies against whichever state the receiver HELD
//! at `from_state`, not only against the one it holds now — had no property
//! covering it. `AckDelivery` below is the missing dimension: acks arrive late,
//! out of order, and sometimes not at all, independently of frames.

use oxutrm_proto::{
    ApplyError, Attrs, Cell, Color, Cursor, CursorShape, Modes, MouseMode, ScreenState, TermSize,
};
use oxutrm_sync::{InputState, Receiver, STATE_RING, Sender, SyncState};
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

/// What the transport does to an **acknowledgement**, independently of frames.
///
/// Acks are cumulative, so losing one is nearly free — the next one that lands
/// says everything it said. What is not free is losing or delaying them in
/// bulk: the sender then keeps diffing against an old base, and every frame it
/// sends names a state the receiver has already left. That is the regime R4
/// exists for, and until this enum existed no property test ever entered it.
#[derive(Clone, Copy, Debug)]
enum AckDelivery {
    /// Lost.
    Drop,
    /// Reaches the sender before it builds its next frame — the old behaviour,
    /// kept in the mix so the lagging cases are not the only ones covered.
    Now,
    /// Reaches the sender `n` rounds later. Because a *later* ack may be
    /// scheduled to arrive *sooner*, this also produces reordering, and
    /// `Sender::on_ack` being monotonic is what makes that survivable.
    After(u8),
}

fn delivery_strategy() -> impl Strategy<Value = Delivery> {
    prop_oneof![
        3 => Just(Delivery::Once),
        1 => Just(Delivery::Drop),
        1 => Just(Delivery::Twice),
        1 => Just(Delivery::Delay),
    ]
}

/// The exclusive upper bound on how many edits the ack-lag fixture makes.
///
/// Load-bearing: it is what turns "the sender should mostly be diffing" into
/// "the sender MUST be diffing, on every frame after the first ack". See
/// `the_ack_lag_fixture_stays_inside_the_senders_ring`.
const MAX_EDITS: usize = 24;

fn ack_delivery_strategy() -> impl Strategy<Value = AckDelivery> {
    prop_oneof![
        2 => Just(AckDelivery::Now),
        1 => Just(AckDelivery::Drop),
        // Weighted towards lag, because lag is the point. A round here is one
        // update of the sender's state, so `After(4)` is four generations of
        // base drift.
        3 => (1u8..5).prop_map(AckDelivery::After),
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

    /// The same property again, with the acknowledgement path abused too — and
    /// an assertion about **how** convergence is reached, not merely that it is.
    ///
    /// # Why the obvious version of this test proves nothing
    ///
    /// A full state carries `from_state == 0` and applies unconditionally (R3).
    /// So when the sender's diff path is broken it falls back to full states,
    /// the screens converge anyway, and a test that only asks "did they
    /// converge?" stays green while the entire diff mechanism is dead. That is
    /// this project's accidental-rescue pattern, and it has now happened three
    /// times. The assertion that tells the two regimes apart is the sender-side
    /// one in the loop: **once the sender has heard any acknowledgement at all,
    /// every frame it emits must be a diff against exactly that base.**
    ///
    /// That is unconditional rather than statistical, and the reason is
    /// arithmetic: the fixture makes at most `MAX_EDITS` updates, `MAX_EDITS` is
    /// smaller than `STATE_RING`, so the sender's ring cannot evict anything and
    /// the acknowledged base is always still there to diff from. There is no
    /// legitimate full state after the first ack lands. `the_ack_lag_fixture_
    /// stays_inside_the_senders_ring` guards that arithmetic.
    ///
    /// # And what this one still cannot see
    ///
    /// The *receiver's* half of R4. Here a frame may legitimately be refused
    /// with `BaseMismatch`, because reordering can deliver a newer frame first
    /// and the receiver then prunes the older frame's base — so "some frames
    /// were refused" cannot be an error, and a receiver that had forgotten its
    /// ring entirely would hide inside that allowance. Verified: with the
    /// receiver's ring lookup cut back to the newest entry, this test still
    /// passes. Catching that needs a link with no loss and no reordering, where
    /// a refusal has no excuse, which is what the deterministic companion test
    /// below is for. Neither replaces the other.
    #[test]
    fn the_receiver_converges_when_acknowledgements_lag_as_well(
        edits in prop::collection::vec(edit_strategy(), 1..MAX_EDITS),
        schedule in prop::collection::vec(delivery_strategy(), 1..24),
        ack_schedule in prop::collection::vec(ack_delivery_strategy(), 1..24),
    ) {
        let start = ScreenState::blank(4, 6).expect("blank");
        let mut tx = Sender::new(start.clone());
        let mut rx = Receiver::new(start);

        let mut held: Option<oxutrm_proto::Frame> = None;
        // Acks in flight, as (the round they arrive in, what they say).
        let mut in_flight: Vec<(usize, u64)> = Vec::new();
        // Mirrors `Sender::peer_saw`: the highest ack that has actually
        // arrived. It is what the sender knows, so it is what the sender is
        // told to put on the wire.
        let mut known_ack = 0u64;

        let mut applied_diffs = 0u64;
        let mut applied_fulls = 0u64;
        // Frames that applied against a base the receiver had ALREADY left —
        // the R4 case, which zero ack latency can never produce.
        let mut applied_over_drift = 0u64;

        for (i, edit) in edits.iter().enumerate() {
            // Acknowledgements minted in earlier rounds mature here. Draining
            // by due-round rather than in mint order is what produces
            // reordering for free: an ack minted later with a shorter delay
            // overtakes an earlier one.
            let mut arrived: Vec<u64> = Vec::new();
            in_flight.retain(|&(due, ack)| {
                if due <= i {
                    arrived.push(ack);
                    false
                } else {
                    true
                }
            });
            for ack in arrived {
                tx.on_ack(ack);
                known_ack = known_ack.max(ack);
            }

            let next = apply_edit(tx.current(), edit);
            prop_assert_eq!(next.validate(), Ok(()), "the test's own edit produced an invalid state");
            tx.update(next);

            // NOT `rx.ack()`. The sender may only act on what has reached it.
            let frame = match tx.make_frame(known_ack).expect("make_frame") {
                Some(f) => f,
                None => continue,
            };

            if known_ack > 0 {
                prop_assert_eq!(
                    frame.from_state,
                    known_ack,
                    "the sender abandoned the diff path while the acknowledged base {} was \
                     still in a ring of {} holding at most {} states: from_state={} \
                     (0 means it sent a whole screen instead)",
                    known_ack,
                    STATE_RING,
                    MAX_EDITS,
                    frame.from_state
                );
            }

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
                    if let Some(old) = held.replace(frame) {
                        to_deliver.push(old);
                    }
                }
            }
            if !matches!(how, Delivery::Delay) {
                if let Some(old) = held.take() {
                    to_deliver.push(old);
                }
            }

            for f in to_deliver {
                let seq_before = rx.state().seq();
                let outcome = rx.on_frame(&f);
                if let Err(e) = &outcome {
                    // The one legitimate rejection: reordering delivered a
                    // newer frame first, which proved the sender had moved on,
                    // so the older frame's base was pruned. Anything else —
                    // a decode failure, an out-of-bounds row, a broken
                    // transition invariant — would be a real defect that ack
                    // lag had uncovered.
                    prop_assert!(
                        matches!(e, ApplyError::BaseMismatch { .. }),
                        "a frame was refused for a reason other than a base the receiver \
                         no longer holds: {}",
                        e
                    );
                }
                if outcome == Ok(true) {
                    if f.from_state == 0 {
                        applied_fulls += 1;
                    } else {
                        applied_diffs += 1;
                        if f.from_state < seq_before {
                            applied_over_drift += 1;
                        }
                    }
                }
                prop_assert_eq!(
                    rx.state().validate(),
                    Ok(()),
                    "the receiver's state must be valid after EVERY frame"
                );
            }

            // The receiver acknowledges what it now holds, and the transport
            // decides when — or whether — the sender hears about it.
            match ack_schedule[i % ack_schedule.len()] {
                AckDelivery::Drop => {}
                AckDelivery::Now => in_flight.push((i, rx.ack())),
                AckDelivery::After(n) => in_flight.push((i + n as usize, rx.ack())),
            }
        }

        // The link comes good: everything still in flight lands, and the
        // sender keeps trying until there is nothing left to say.
        for (_, ack) in in_flight.drain(..) {
            tx.on_ack(ack);
        }
        for _ in 0..4 {
            tx.on_ack(rx.ack());
            match tx.make_frame(rx.ack()).expect("make_frame") {
                Some(f) => {
                    let seq_before = rx.state().seq();
                    if rx.on_frame(&f).expect("a frame built from the receiver's own ack must apply")
                    {
                        if f.from_state == 0 {
                            applied_fulls += 1;
                        } else {
                            applied_diffs += 1;
                            if f.from_state < seq_before {
                                applied_over_drift += 1;
                            }
                        }
                    }
                }
                None => break,
            }
            prop_assert_eq!(rx.state().validate(), Ok(()));
        }

        prop_assert_eq!(rx.state(), tx.current(), "the receiver did not converge under ack lag");
        prop_assert_eq!(rx.ack(), tx.current().seq());

        // The receiver's own instrument must agree with what this test watched
        // happen, EXACTLY. This was a `>=` while `Receiver::on_frame`
        // incremented the counters before `apply` and `validate_transition`
        // could refuse a frame, which made a refused diff count as an applied
        // one. It counts applications now, so the bound is an equality — and it
        // must stay one: a `>=` here cannot tell a healthy diff path from one
        // that refuses everything and is rescued by full states, which is the
        // single thing this counter exists to expose.
        let (diffs, fulls) = rx.applied_kinds();
        prop_assert!(
            diffs == applied_diffs && fulls == applied_fulls,
            "applied_kinds() reported ({diffs}, {fulls}) but {applied_diffs} diffs and \
             {applied_fulls} full states actually applied"
        );
        prop_assert!(
            applied_over_drift <= applied_diffs,
            "counting error in the test itself"
        );
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

/// The arithmetic that makes the ack-lag proptest's sender-side assertion
/// unconditional rather than statistical.
///
/// `Sender::update` pushes one state per call and caps the ring at
/// `STATE_RING`, so a fixture that makes fewer than `STATE_RING` updates can
/// never evict anything: whatever base an acknowledgement names is still there
/// to diff from. That is what licenses "a full state after the first ack is a
/// bug" instead of "a full state after the first ack is usually a bug". Raise
/// the fixture past the ring and the assertion becomes wrong — legitimate
/// full-state recovery (R3) would start firing — so this fails first and says
/// why.
#[test]
fn the_ack_lag_fixture_stays_inside_the_senders_ring() {
    // Evaluated at COMPILE time: it compares two constants, so there is no
    // reason to wait for a test run to hear about it. It keeps a test's name
    // anyway, because the rule deserves one. `assert!` in const context takes
    // no format arguments, so the numbers are in the doc comment above rather
    // than in the message.
    const {
        assert!(
            MAX_EDITS < STATE_RING,
            "the ack-lag fixture makes more updates than the sender's ring holds; past the \
             ring the sender legitimately falls back to full states (R3), and the fixture's \
             `from_state == known_ack` assertion becomes wrong"
        )
    }
}

/// Ack lag, pinned: a lossless link that delivers every frame instantly and
/// every acknowledgement four rounds late.
///
/// The proptest above says the property holds across a cloud of schedules. This
/// says what one schedule costs, in exact numbers, and it is the direct
/// property-level statement of contract rule **R4**: with the acknowledgement
/// four rounds behind, every frame but the attach's first names a base the
/// receiver has already left, and every one of them must still apply.
///
/// Both failure directions are covered, which is the reason for the exact
/// figures:
///
/// * break the **sender's** diff path and `from_state` becomes 0 on every
///   frame — the assertion in the loop fires, and `applied_kinds` reports
///   thirteen full states instead of one. Convergence alone would NOT notice,
///   because a full state applies unconditionally (R3): that is the rescue this
///   project has now been fooled by three times.
/// * break the **receiver's** ring — the R4 defect — and the drifted frames are
///   refused with `BaseMismatch`, so `on_frame` panics here rather than
///   quietly halving the delivered frame rate.
///
/// Neither is reachable at zero ack latency, which is all this file tested
/// before.
#[test]
fn a_lagging_acknowledgement_is_answered_with_diffs_and_not_with_rescues() {
    /// Rounds of acknowledgement latency. One round is one update of the
    /// sender's state, so this is four generations of base drift.
    const LAG: usize = 4;
    const ROUNDS: usize = 12;

    let start = ScreenState::blank(4, 6).expect("blank");
    let mut tx = Sender::new(start.clone());
    let mut rx = Receiver::new(start);

    // The attach handshake, at zero latency, so that the one legitimate full
    // state (R1) is out of the way and everything below is a diff or a defect.
    let hello = tx
        .make_frame(rx.ack())
        .expect("make_frame")
        .expect("a fresh sender owes the peer a full state");
    assert_eq!(hello.from_state, 0, "the first frame of an attach is full");
    rx.on_frame(&hello)
        .expect("the attach's full state must apply");
    tx.on_ack(rx.ack());
    let mut known_ack = rx.ack();
    assert_eq!(known_ack, 1);

    // (arrival round, what the ack says)
    let mut in_flight: Vec<(usize, u64)> = Vec::new();
    let mut drifted = 0u64;
    let mut worst_drift = 0u64;

    for round in 0..ROUNDS {
        let mut arrived: Vec<u64> = Vec::new();
        in_flight.retain(|&(due, ack)| {
            if due <= round {
                arrived.push(ack);
                false
            } else {
                true
            }
        });
        for ack in arrived {
            tx.on_ack(ack);
            known_ack = known_ack.max(ack);
        }

        let mut next = tx.current().clone();
        next.bell = next.bell.saturating_add(1);
        let cell = round % next.cells.len();
        next.cells[cell].text = "x".into();
        tx.update(next);

        // The sender knows only what has arrived.
        let f = tx
            .make_frame(known_ack)
            .expect("make_frame")
            .expect("our state moved, so a frame is owed");
        assert_eq!(
            f.from_state, known_ack,
            "round {round}: the sender sent a whole screen while the acknowledged base \
             {known_ack} was still in its ring — the diff path is not being used"
        );

        let seq_before = rx.state().seq();
        assert!(
            rx.on_frame(&f)
                .expect("nothing is lost or reordered here, so every frame must apply"),
            "round {round}: a frame carrying state {} was not applied over state {seq_before}",
            f.my_state
        );
        if f.from_state < seq_before {
            drifted += 1;
            worst_drift = worst_drift.max(seq_before - f.from_state);
        }
        assert_eq!(rx.state().validate(), Ok(()));

        in_flight.push((round + LAG, rx.ack()));
    }

    // Every round but the first stood on a base the receiver had already left:
    // the acknowledgement for round N does not arrive until round N + LAG, so
    // the base trails the held state by LAG - 1 generations once the pipe is
    // full. At zero ack latency this number is zero, which is exactly why the
    // old fixture could not see R4.
    assert_eq!(
        drifted,
        (ROUNDS - 1) as u64,
        "the fixture did not actually produce base drift"
    );
    assert_eq!(
        worst_drift,
        (LAG - 1) as u64,
        "the drift should settle at one less than the ack latency"
    );

    // How convergence was reached, not merely that it was.
    let (diffs, fulls) = rx.applied_kinds();
    assert_eq!(
        (diffs, fulls),
        (ROUNDS as u64, 1),
        "convergence came from {fulls} whole screens and {diffs} diffs; one full state is \
         the attach (R1) and everything after it must be a diff"
    );

    assert_eq!(rx.state(), tx.current(), "the ends did not converge");
    assert_eq!(rx.ack(), tx.current().seq());
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

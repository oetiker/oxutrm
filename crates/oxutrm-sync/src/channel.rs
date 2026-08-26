//! The two ends of one replicated value.

use std::collections::VecDeque;

use oxutrm_proto::{ApplyError, FLAG_ZSTD, Frame};

use crate::{STATE_RING, SyncState};

/// zstd level 1.
///
/// This runs on the keystroke path. Level 1 gives most of the ratio for a
/// fraction of the CPU of the default 3, and a screen diff is mostly runs of
/// identical bytes, which even level 1 crushes. Latency wins over ratio here.
const ZSTD_LEVEL: i32 = 1;

/// A ceiling on what one frame may decompress to.
///
/// The peer is authenticated, so this is not the main line of defence — but a
/// compression bomb is cheap to send and expensive to receive, and "the peer is
/// authenticated" is exactly the assumption that fails first. 64 MiB is far
/// above any real screen and far below anything that hurts.
const MAX_DECOMPRESSED: usize = 64 * 1024 * 1024;

/// Keeps a ring of recent states and emits diffs against the peer's ack.
///
/// The ring is what makes a lost datagram cost nothing: the next diff is
/// computed against the same acknowledged base, so it *contains* whatever the
/// lost one carried. When the peer's ack has fallen out of the ring there is
/// nothing to diff against and a full state goes instead.
pub struct Sender<S: SyncState> {
    /// Oldest first; the last entry is the current state.
    ring: VecDeque<S>,
    /// The highest state of ours the peer has acknowledged.
    peer_saw: u64,
    /// The last `ack_state` we actually put on the wire.
    ///
    /// Without this an acknowledgement can only travel piggybacked on data,
    /// so a side with nothing of its own to say can never acknowledge
    /// anything — and a user who is only WATCHING output never types, so
    /// never acks, so the sender stays pinned to an ancient base forever.
    last_ack_sent: u64,
}

impl<S: SyncState> Sender<S> {
    pub fn new(initial: S) -> Sender<S> {
        let mut ring = VecDeque::with_capacity(STATE_RING);
        ring.push_back(initial);
        Sender {
            ring,
            peer_saw: 0,
            last_ack_sent: 0,
        }
    }

    /// Replace the current state, assigning it the next sequence number.
    ///
    /// This is the **only** place sequence numbers are minted, which is why
    /// `InputState::append` and `consume` leave `seq` alone.
    ///
    /// States are replaced rather than queued. If output outruns the link the
    /// ring simply holds newer states; the next frame is current by
    /// construction, so a runaway `yes` produces one frame rather than a
    /// backlog.
    pub fn update(&mut self, mut next: S) {
        let seq = self.current().seq().saturating_add(1);
        next.set_seq(seq);
        self.ring.push_back(next);
        while self.ring.len() > STATE_RING {
            self.ring.pop_front();
        }
    }

    /// Record that the peer has applied everything up to `peer_saw`.
    ///
    /// Never goes backwards: acks can be reordered by the transport, and an
    /// ack that moved backwards would make the sender diff against a state the
    /// peer has already passed.
    pub fn on_ack(&mut self, peer_saw: u64) {
        self.peer_saw = self.peer_saw.max(peer_saw);
    }

    pub fn current(&self) -> &S {
        self.ring
            .back()
            .expect("the ring always holds at least one state")
    }

    /// The frame to send, or `None` when the peer is already up to date.
    ///
    /// `ack_state` is what *we* are telling the peer about *its* states — it
    /// comes from our own [`Receiver::ack`], and passes straight through.
    ///
    /// The payload is compressed only when compression actually shrinks it.
    /// That is measured every time rather than assumed from a size threshold:
    /// a small diff of high-entropy bytes grows under zstd, and sending it
    /// grown would be a silent regression on the keystroke path.
    pub fn make_frame(&mut self, ack_state: u64) -> Result<Option<Frame>, ApplyError> {
        let current_seq = self.current().seq();
        // Two reasons to send. The obvious one is that our own state moved.
        // The other is that we owe the peer an acknowledgement it has not
        // heard yet: acks travel only on frames, so if we never send one
        // because nothing of ours changed, the peer's sender is stranded on
        // whatever base it last heard about. The resulting frame carries an
        // empty diff and exists purely to move the ack.
        let state_moved = self.peer_saw != current_seq;
        let ack_owed = ack_state > self.last_ack_sent;
        if !state_moved && !ack_owed {
            return Ok(None);
        }
        let current = self.current();

        // Diff against the peer's acknowledged state if we still hold it.
        let base = self.ring.iter().find(|s| s.seq() == self.peer_saw);
        let (diff, from_state) = match base {
            Some(base) => (current.diff_from(base), base.seq()),
            // Nothing to diff against: the ack has fallen out of the ring, or
            // the peer has acknowledged nothing at all. `from_state == 0` is
            // the full-state sentinel.
            None => (current.full_diff(), 0),
        };

        let raw = postcard::to_stdvec(&diff)
            .map_err(|e| ApplyError::Decode(format!("encoding diff: {e}")))?;

        let (payload, flags) = match zstd::stream::encode_all(raw.as_slice(), ZSTD_LEVEL) {
            Ok(z) if z.len() < raw.len() => (z, FLAG_ZSTD),
            // Either it did not shrink, or zstd refused. Sending it plain is
            // always correct.
            _ => (raw, 0),
        };

        self.last_ack_sent = ack_state;
        Ok(Some(Frame {
            my_state: current_seq,
            from_state,
            ack_state,
            flags,
            payload,
        }))
    }
}

/// Applies incoming frames to a replicated state.
///
/// # It keeps a ring, for the same reason the [`Sender`] does
///
/// The sender diffs against the newest state the peer ACKNOWLEDGED, and an
/// acknowledgement takes a round trip to come back. Every frame put on the
/// wire inside that window therefore names a base the receiver has already
/// left behind. A receiver that held only its current state could apply none
/// of them — and each one carries a screen strictly NEWER than the one it is
/// showing. Under a flood that is not an edge case, it is the steady state:
/// measured, half of every frame sent was dropped this way at one round trip
/// of ack latency, seven in eight at eight.
///
/// The base is never unknown to the receiver, only forgotten: it stood on that
/// exact state a moment ago. So it keeps what it stood on, and a diff applies
/// to whichever of those states it names.
///
/// The ring is self-pruning rather than merely capped. Every frame's
/// `from_state` reveals which base the sender is still working from, and
/// nothing older can ever be named again, so it goes. In steady state that
/// leaves two entries, not [`STATE_RING`].
pub struct Receiver<S: SyncState> {
    /// Oldest first; the last entry is the state actually held.
    ///
    /// Every entry after the peer's first frame was authored by the PEER. The
    /// locally invented initial state is evicted the moment that frame lands,
    /// because its sequence number names a screen the peer never had — the
    /// attach collision, which must not come back through the ring.
    ring: VecDeque<S>,
    peer_ack: u64,
    /// Whether anything from the peer has been applied yet.
    ///
    /// Narrows the initial-collision exception in `on_frame` to the one frame
    /// it exists for.
    applied_any: bool,
    /// Applied frames that carried a real diff (`from_state != 0`).
    diffs_applied: u64,
    /// Applied frames that carried a whole screen.
    ///
    /// Counted because a full state is the protocol's RESCUE, and a rescue that
    /// is silently doing all the work looks exactly like health. When the
    /// sender's ring cannot reach the peer's acknowledged base it falls back to
    /// a full state, which applies unconditionally — so a session in which base
    /// drift is completely broken still converges, rejects nothing, and passes
    /// any test that only counts rejections. Nothing counted this before, and
    /// that is why the defect this ring fixes went unnoticed until it was
    /// measured. A gate that cannot tell the two regimes apart is not a gate.
    full_states_applied: u64,
}

impl<S: SyncState> Receiver<S> {
    pub fn new(initial: S) -> Receiver<S> {
        let mut ring = VecDeque::with_capacity(STATE_RING);
        ring.push_back(initial);
        Receiver {
            ring,
            peer_ack: 0,
            applied_any: false,
            diffs_applied: 0,
            full_states_applied: 0,
        }
    }

    /// Applied frames that carried a diff, and applied frames that carried a
    /// whole screen, in that order.
    ///
    /// For diagnostics and for tests that must tell a healthy session from one
    /// converging entirely on the full-state rescue. The two look identical
    /// from the outside: both reject nothing and both paint the right screen.
    #[must_use]
    pub fn applied_kinds(&self) -> (u64, u64) {
        (self.diffs_applied, self.full_states_applied)
    }

    /// Apply one frame. `true` when the state advanced.
    ///
    /// A stale or duplicate frame returns `Ok(false)`: both are ordinary on an
    /// unreliable transport and neither is an error.
    ///
    /// **Nothing is committed unless everything succeeds.** The diff is
    /// applied to a clone, the result is validated, and only then does it
    /// replace the live state. So a rejected frame leaves both the state and
    /// [`Receiver::ack`] exactly as they were — which matters more than it
    /// looks: a receiver that rejected a frame but advanced its ack would tell
    /// the sender "I have state N" while holding N-1, and the sender would
    /// then diff against N forever while the receiver rejected every one.
    pub fn on_frame(&mut self, f: &Frame) -> Result<bool, ApplyError> {
        // The peer's acknowledgement is recorded FIRST, and from EVERY frame
        // including stale and duplicate ones. `ack_state` describes what the
        // PEER holds; whether this frame's payload is applicable to US is a
        // separate question.
        //
        // This is not an optimisation, it is half of a matched pair. A side
        // with nothing of its own to say sends an ACK-ONLY frame, which by
        // construction repeats its current state number and is therefore
        // always stale right here. Drop the ack and that frame accomplishes
        // nothing, and a peer that has stopped typing never acknowledges the
        // screen again — which strands the other end's sender on an ancient
        // base until ring eviction accidentally rescues it.
        //
        // Monotonic, because reordering can deliver an older acknowledgement
        // after a newer one, and believing it would un-retire states the
        // sender had already dropped.
        self.peer_ack = self.peer_ack.max(f.ack_state);

        // Staleness, with one deliberate exception.
        //
        // A DIFF must strictly advance the sequence number, or it is a
        // duplicate and applying it twice would be wrong.
        //
        // A FULL STATE (`from_state == 0`) may apply at the number we already
        // hold, and must. Both ends independently construct an initial state
        // numbered 1 — the host from the live emulator, the client from a
        // blank screen — holding completely different content, and a sequence
        // number says which generation, never which content. Rejecting the
        // attach's first full state because its number merely EQUALS ours
        // leaves the client on its own invented screen, and every later diff
        // then arrives with a base the client never reached.
        //
        // Still bounded: a full state older than what we hold is dropped, so a
        // late one cannot claw the screen backwards.
        // The exception is exactly as wide as the collision and no wider: it
        // applies only before anything from the peer has been applied. A
        // duplicate full state arriving later is a duplicate like any other,
        // and re-applying it would repaint the screen for nothing.
        let current_seq = self.state().seq();
        let stale = if f.from_state == 0 && !self.applied_any {
            f.my_state < current_seq
        } else {
            f.my_state <= current_seq
        };
        if stale {
            return Ok(false);
        }

        let payload = if f.flags & FLAG_ZSTD != 0 {
            decompress(&f.payload)?
        } else {
            f.payload.clone()
        };

        let diff: S::Diff = postcard::from_bytes(&payload)
            .map_err(|e| ApplyError::Decode(format!("decoding diff: {e}")))?;

        // The diff builds on the state the SENDER named, which is whichever of
        // ours it last heard about — not necessarily the one we hold now.
        // A base we no longer have falls through to the current state, so
        // `apply` reports the mismatch: the rule about what is a legal base
        // stays in one place, and a genuinely unusable base is still refused
        // rather than guessed at.
        let mut next = match self.ring.iter().find(|s| s.seq() == f.from_state) {
            Some(base) if f.from_state != 0 => base.clone(),
            _ => self.state().clone(),
        };
        if f.from_state == 0 {
            self.full_states_applied = self.full_states_applied.saturating_add(1);
        } else {
            self.diffs_applied = self.diffs_applied.saturating_add(1);
        }
        next.apply(f.from_state, f.my_state, &diff)?;
        // AFTER apply, never before: the question is whether the RESULT is a
        // legal state, and the state we already hold is legal by induction.
        //
        // `validate_transition`, not `validate`: the state we are replacing is
        // the only thing that can show the invariants a single value cannot —
        // I5, the bell is a monotonic counter, and I6, scrollback never
        // shrinks. Both were enforced NOWHERE until this call existed, while
        // `oxutrm-proto`'s tests called the checker directly and reported
        // green. The default implementation of `validate_transition` is
        // `validate`, so nothing is lost for a state with no transition rules.
        //
        // This is where the "a rejected frame never disconnects the session"
        // rule is paid for: the check runs against a CLONE, so a failure
        // leaves `self.state` and `ack()` exactly as they were, and the
        // session loop logs and carries on.
        // `self.state()` is the ring's newest entry, which is what the single
        // `state` field was before the receiver gained a ring. It is the right
        // `previous` even when the diff was applied to an OLDER base out of the
        // ring: I5 and I6 are monotonic over the host's whole sequence, so the
        // newest state we have seen is the floor they must not fall below.
        // Checking against the base instead would let a bell counter go
        // backwards relative to a state we already hold.
        next.validate_transition(self.state())?;

        // Everything we held before the peer's first frame we invented
        // ourselves. Those sequence numbers name our screens, never the
        // peer's, so they are not diff bases and must not survive as any.
        if !self.applied_any {
            self.ring.clear();
        }
        // The sender is diffing from `from_state`, and `Sender::on_ack` never
        // walks backwards, so nothing older can ever be named again.
        while self.ring.front().is_some_and(|s| s.seq() < f.from_state) {
            self.ring.pop_front();
        }
        self.ring.push_back(next);
        // A cap as well as the pruning: a peer whose acks are all being lost
        // keeps naming one ancient base, so the pruning alone never fires.
        //
        // **The cap and `STATE_RING` are coupled, and the margin is one entry.**
        // `Sender::update` pushes exactly ONE state per call and caps at
        // `STATE_RING`, so a sender naming base `B` still holds it, which means
        // its current sequence is at most `B + STATE_RING - 1`. We therefore
        // hold `B` plus at most `STATE_RING - 1` newer entries — exactly
        // `STATE_RING` — and popping the front, which is `B` itself, would
        // evict the very base the sender is working from.
        //
        // The `+ 1` is that margin, and it is deliberate rather than a
        // fencepost. Without it the arithmetic is exact and any change to
        // either side — a sender that pushed two states in a turn, a sender
        // ring made deeper than this cap — evicts the base and reinstates the
        // base-drift defect INVISIBLY, because in that regime the sender's ring
        // is also near exhaustion, every frame degrades to a full state, and a
        // full state applies unconditionally. The bug would be masked by its
        // own consequence.
        while self.ring.len() > STATE_RING + 1 {
            self.ring.pop_front();
        }
        self.applied_any = true;
        // `peer_ack` is already advanced at the top of `on_frame`, before any
        // early return, and `max` is idempotent — so this second assignment is
        // dead. Do not "restore" it: the one at the top carries the reasoning
        // about reordered acknowledgements and is the load-bearing copy.
        Ok(true)
    }

    pub fn state(&self) -> &S {
        self.ring
            .back()
            .expect("the ring always holds at least one state")
    }

    /// The sequence number to put in our outgoing `ack_state`.
    ///
    /// **Zero until the peer's first frame has been applied**, and that is the
    /// whole point rather than a detail. An `ack_state` is a promise: "I hold
    /// YOUR state N, diff against it." The state a receiver starts from was
    /// invented locally — the client's blank screen, the host's empty input
    /// queue — and both ends invent theirs numbered 1. Acknowledging that 1
    /// tells the peer we hold a screen we have never seen, and the peer then
    /// diffs against its own state 1, which we cannot apply and never will be
    /// able to. It is the attach collision wearing a different hat: a sequence
    /// number says which generation, never which content.
    ///
    /// Zero means "I have nothing of yours", which is true, and it is the one
    /// value `Sender::make_frame` cannot find in its ring — so it sends a full
    /// state, which is exactly the first frame an attach is supposed to carry.
    pub fn ack(&self) -> u64 {
        if self.applied_any {
            self.state().seq()
        } else {
            0
        }
    }

    /// The peer's `ack_state` from the last frame we accepted.
    pub fn peer_ack(&self) -> u64 {
        self.peer_ack
    }
}

fn decompress(payload: &[u8]) -> Result<Vec<u8>, ApplyError> {
    use std::io::Read as _;

    let mut decoder = zstd::stream::read::Decoder::new(payload)
        .map_err(|e| ApplyError::Decode(format!("zstd: {e}")))?;
    let mut out = Vec::new();
    // `take` caps the work regardless of what the header claims.
    let read = (&mut decoder)
        .take(MAX_DECOMPRESSED as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| ApplyError::Decode(format!("zstd: {e}")))?;
    if read > MAX_DECOMPRESSED {
        return Err(ApplyError::Decode(format!(
            "compressed payload expands beyond {MAX_DECOMPRESSED} bytes"
        )));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::ScreenState;

    fn screen() -> ScreenState {
        ScreenState::blank(4, 8).expect("blank")
    }

    #[test]
    fn update_mints_consecutive_sequence_numbers_starting_after_the_initial() {
        let mut tx = Sender::new(screen());
        assert_eq!(tx.current().seq, 1);
        for want in 2..=5u64 {
            tx.update(screen());
            assert_eq!(tx.current().seq, want);
        }
    }

    #[test]
    fn update_overwrites_the_sequence_number_the_caller_supplied() {
        // A caller that hands over a state with a stale or invented seq must
        // not be able to corrupt the numbering.
        let mut tx = Sender::new(screen());
        let mut confused = screen();
        confused.seq = 9_999;
        tx.update(confused);
        assert_eq!(tx.current().seq, 2);
    }

    #[test]
    fn the_ring_holds_exactly_state_ring_entries() {
        let mut tx = Sender::new(screen());
        for _ in 0..(STATE_RING * 2) {
            tx.update(screen());
        }
        assert_eq!(tx.ring.len(), STATE_RING);
    }

    #[test]
    fn an_ack_never_moves_backwards() {
        // Acks arrive on an unreliable transport and can be reordered. An ack
        // that went backwards would make the sender diff against a base the
        // peer has already moved past.
        let mut tx = Sender::new(screen());
        tx.on_ack(5);
        tx.on_ack(2);
        assert_eq!(tx.peer_saw, 5);
    }

    #[test]
    fn nothing_is_sent_when_the_peer_is_already_current() {
        let mut tx = Sender::new(screen());
        tx.on_ack(1);
        assert!(tx.make_frame(0).expect("make_frame").is_none());
    }

    #[test]
    fn an_unacknowledged_start_produces_a_full_state() {
        let mut tx = Sender::new(screen());
        tx.update(screen());
        // peer_saw is 0: the peer has acknowledged nothing.
        let f = tx.make_frame(0).expect("make_frame").expect("frame");
        assert_eq!(
            f.from_state, 0,
            "nothing to diff against means a full state"
        );
    }

    #[test]
    fn the_ack_we_are_told_to_send_passes_straight_through() {
        let mut tx = Sender::new(screen());
        tx.update(screen());
        let f = tx.make_frame(42).expect("make_frame").expect("frame");
        assert_eq!(f.ack_state, 42);
    }

    #[test]
    fn a_compressible_payload_is_compressed_and_an_incompressible_one_is_not() {
        // "Only when it actually shrinks" is measured, not assumed.
        let mut tx = Sender::new(screen());
        let mut big = ScreenState::blank(60, 200).expect("blank");
        for c in big.cells.iter_mut() {
            c.text = "-".into();
        }
        big.seq = 1;
        tx.update(big);
        let f = tx.make_frame(0).expect("make_frame").expect("frame");
        assert_eq!(
            f.flags & FLAG_ZSTD,
            FLAG_ZSTD,
            "a screen of dashes must compress"
        );

        // A tiny diff cannot beat a zstd frame header.
        let mut small = Sender::new(screen());
        let mut one = screen();
        one.bell = 1;
        small.on_ack(1);
        small.update(one);
        let f = small.make_frame(0).expect("make_frame").expect("frame");
        assert_eq!(
            f.flags & FLAG_ZSTD,
            0,
            "a 3-byte diff must travel uncompressed"
        );
    }

    #[test]
    fn a_compressed_frame_round_trips() {
        let mut tx = Sender::new(ScreenState::blank(60, 200).expect("blank"));
        let mut big = ScreenState::blank(60, 200).expect("blank");
        for c in big.cells.iter_mut() {
            c.text = "=".into();
        }
        tx.update(big);
        tx.on_ack(1);

        let f = tx.make_frame(0).expect("make_frame").expect("frame");
        assert_eq!(f.flags & FLAG_ZSTD, FLAG_ZSTD);

        let mut rx = Receiver::new(ScreenState::blank(60, 200).expect("blank"));
        assert_eq!(rx.on_frame(&f), Ok(true));
        assert_eq!(rx.state(), tx.current());
    }

    #[test]
    fn the_receiver_reports_the_peers_ack_from_the_last_accepted_frame() {
        let mut tx = Sender::new(screen());
        tx.update(screen());
        let f = tx.make_frame(77).expect("make_frame").expect("frame");

        let mut rx = Receiver::new(screen());
        assert_eq!(rx.peer_ack(), 0);
        rx.on_frame(&f).expect("apply");
        assert_eq!(rx.peer_ack(), 77);
    }

    /// A stale or duplicate frame is not applicable to us, but its
    /// `ack_state` is still a true statement about what the PEER holds — and
    /// that statement is what lets our sender retire states from its ring and
    /// diff from a newer base.
    ///
    /// Discarding it costs real bandwidth forever: the sender keeps diffing
    /// against an older base than it needs to, and the ring never drains.
    /// Nothing on the happy path shows this, because stale frames appear only
    /// under reordering and loss.
    /// The sequence-number collision, and the rule that closes it.
    ///
    /// Both ends independently construct an initial state numbered 1 — the
    /// host from the live emulator, the client from a blank screen — holding
    /// COMPLETELY DIFFERENT content. A sequence number says which generation,
    /// never which content, so nothing in the protocol notices.
    ///
    /// The host's first frame of an attach is a full state (`from_state == 0`)
    /// precisely so the client's invented state cannot matter. It must
    /// therefore be applied even though its `my_state` does not EXCEED what the
    /// receiver holds, because the number it does not exceed was invented
    /// locally and names a different screen.
    #[test]
    fn the_first_full_state_applies_even_though_its_seq_does_not_advance() {
        let mut host_screen = screen();
        host_screen.cells[0] = oxutrm_proto::Cell {
            text: oxutrm_proto::CellText::from("H"),
            ..oxutrm_proto::Cell::blank()
        };
        assert_eq!(host_screen.seq, 1, "the host starts at 1");

        let tx: Sender<ScreenState> = Sender::new(host_screen.clone());
        // A fresh sender owes a full state: it has heard no ack, so there is
        // nothing in its ring to diff against.
        let mut tx = tx;
        let f = tx
            .make_frame(0)
            .expect("make_frame")
            .expect("a fresh sender owes the peer everything");
        assert_eq!(
            f.from_state, 0,
            "the first frame of an attach must be a full state"
        );
        assert_eq!(f.my_state, 1);

        // The client's own blank, also numbered 1, and a different screen.
        let mut rx: Receiver<ScreenState> = Receiver::new(screen());
        assert_eq!(rx.state().seq, 1);
        assert_ne!(rx.state().cells, host_screen.cells);

        assert!(
            rx.on_frame(&f)
                .expect("a full state must never be an error"),
            "the first full state was dropped as stale, so the client kept its \
             own invented screen and every later diff will mismatch its base"
        );
        assert_eq!(rx.state().cells, host_screen.cells);
    }

    /// The same collision, seen from the ACKNOWLEDGING side.
    ///
    /// The rule above lets the peer's first full state land. This one stops
    /// the receiver from asking for a diff it could never apply in the first
    /// place. A receiver's initial state was invented locally and is numbered
    /// 1 like everyone else's; acknowledging that 1 tells the sender "I hold
    /// YOUR state 1", so the sender diffs against a screen the receiver has
    /// never seen. Every such frame is unapplicable on arrival and on every
    /// retry, until the sender's ring evicts the base and a full state goes by
    /// accident — the rescue that hides the breakage.
    #[test]
    fn a_receiver_does_not_acknowledge_the_state_it_invented() {
        let mut host_screen = screen();
        host_screen.cells[0] = oxutrm_proto::Cell {
            text: oxutrm_proto::CellText::from("H"),
            ..oxutrm_proto::Cell::blank()
        };
        let mut tx: Sender<ScreenState> = Sender::new(host_screen);
        let mut rx: Receiver<ScreenState> = Receiver::new(screen());
        assert_eq!(
            rx.state().seq,
            1,
            "the receiver invented a state numbered 1"
        );

        // The sender moves on, and hears the receiver's acknowledgement.
        tx.update(screen());
        tx.on_ack(rx.ack());
        let f = tx.make_frame(rx.ack()).expect("make_frame").expect("frame");
        assert_eq!(
            f.from_state, 0,
            "the sender was told the receiver holds state 1 and diffed against \
             its own state 1, which the receiver has never seen"
        );
        assert!(rx.on_frame(&f).expect("apply"));
        assert_eq!(rx.state(), tx.current());
        assert_eq!(rx.ack(), 2, "now the ack names a state the SENDER authored");
    }

    /// The rule is narrow on purpose: only a FULL state may apply without
    /// advancing the sequence number. A diff at the same number is a genuine
    /// duplicate and must still be ignored.
    #[test]
    fn a_diff_at_the_same_sequence_number_is_still_stale() {
        let mut tx: Sender<ScreenState> = Sender::new(screen());
        tx.update(screen());
        let f = tx.make_frame(0).expect("make_frame").expect("frame");

        let mut rx: Receiver<ScreenState> = Receiver::new(screen());
        assert!(rx.on_frame(&f).expect("apply"));
        let seq = rx.state().seq;

        // The same frame again, but rewritten to look like a diff.
        let mut dup = f.clone();
        dup.from_state = 1;
        assert!(
            !rx.on_frame(&dup).expect("a duplicate is never an error"),
            "a duplicate diff was applied a second time"
        );
        assert_eq!(rx.state().seq, seq);
    }

    /// A diff whose base the receiver has already moved past must still apply.
    ///
    /// The sender diffs against the newest state the peer ACKNOWLEDGED, and an
    /// acknowledgement takes a round trip to come back. Every frame the sender
    /// puts on the wire inside that window therefore names a base the receiver
    /// has already left. Requiring the base to EQUAL the state currently held
    /// throws all of them away — and what is thrown away is strictly NEWER
    /// than the screen the receiver is showing, sitting in its own hand.
    ///
    /// The base is not unknown to the receiver. It is FORGOTTEN: the receiver
    /// stood on that exact state one frame ago and then overwrote it. Keeping
    /// a ring of what it held — the same ring the sender already keeps, for
    /// the same reason — makes the diff applicable and costs one clone.
    #[test]
    fn a_diff_from_a_base_the_receiver_has_left_behind_still_applies() {
        fn marked(c: &str) -> ScreenState {
            let mut s = screen();
            s.cells[0] = oxutrm_proto::Cell {
                text: oxutrm_proto::CellText::from(c),
                ..oxutrm_proto::Cell::blank()
            };
            s
        }

        let mut tx: Sender<ScreenState> = Sender::new(screen());
        let mut rx: Receiver<ScreenState> = Receiver::new(screen());

        // Get both ends onto the same state 1, so what follows are genuine
        // diffs rather than full states.
        let hello = tx.make_frame(rx.ack()).expect("mf").expect("full state");
        assert_eq!(hello.from_state, 0);
        rx.on_frame(&hello).expect("apply");
        tx.on_ack(rx.ack());

        // The sender's state moves twice with no ack coming back, which is
        // exactly what one round trip of ack latency looks like. Both frames
        // are therefore diffed from state 1.
        tx.update(marked("A"));
        let first = tx.make_frame(rx.ack()).expect("mf").expect("frame");
        tx.update(marked("B"));
        let second = tx.make_frame(rx.ack()).expect("mf").expect("frame");
        assert_eq!(first.from_state, 1, "diffed against the acknowledged base");
        assert_eq!(second.from_state, 1, "the ack has not come back yet");
        assert_eq!(second.my_state, 3);

        assert!(
            rx.on_frame(&first).expect("apply"),
            "the first diff applies"
        );
        assert_eq!(rx.state().seq, 2, "the receiver has now left state 1");

        assert!(
            rx.on_frame(&second)
                .expect("a strictly newer state must never be an error"),
            "the receiver dropped a screen NEWER than the one it is showing, \
             because it had forgotten the base it stood on one frame ago"
        );
        assert_eq!(rx.state(), tx.current(), "the ends did not converge");
    }

    /// The ring is bounded, so a base older than it is still unusable — and
    /// must still be refused rather than guessed at.
    #[test]
    fn a_base_older_than_the_receivers_ring_is_still_a_base_mismatch() {
        let mut tx: Sender<ScreenState> = Sender::new(screen());
        let mut rx: Receiver<ScreenState> = Receiver::new(screen());
        let hello = tx.make_frame(rx.ack()).expect("mf").expect("full state");
        rx.on_frame(&hello).expect("apply");
        tx.on_ack(rx.ack());

        // Walk the receiver past a whole ring's worth of states.
        for _ in 0..(STATE_RING as u64 + 2) {
            tx.update(screen());
            tx.on_ack(rx.ack());
            let f = tx.make_frame(rx.ack()).expect("mf").expect("frame");
            rx.on_frame(&f).expect("apply");
        }

        // A frame that names state 1 — long gone from both rings.
        let stale_base = Frame {
            my_state: rx.state().seq + 1,
            from_state: 1,
            ack_state: 0,
            flags: 0,
            payload: postcard::to_stdvec(&screen().diff_from(&screen())).expect("encode"),
        };
        assert!(matches!(
            rx.on_frame(&stale_base),
            Err(ApplyError::BaseMismatch { base: 1, .. })
        ));
    }

    #[test]
    fn a_stale_frame_still_advances_the_peers_ack() {
        let mut tx = Sender::new(screen());
        let mut rx = Receiver::new(screen());

        // Sync first, so what follows is a genuine DIFF rather than a full
        // state — a full state at the same number legitimately applies.
        let hello = tx.make_frame(1).expect("make_frame").expect("full state");
        assert_eq!(hello.from_state, 0);
        rx.on_frame(&hello).expect("apply");
        tx.on_ack(rx.ack());

        tx.update(screen());
        let diff = tx.make_frame(10).expect("make_frame").expect("a diff");
        assert_ne!(diff.from_state, 0, "this must be a diff to test staleness");
        assert!(rx.on_frame(&diff).expect("apply"), "the diff applies once");
        assert_eq!(rx.peer_ack(), 10);

        // The very same diff again — a duplicate, which the network produces
        // on its own — but the peer has since acknowledged more of our state.
        let mut duplicate = diff.clone();
        duplicate.ack_state = 42;
        assert!(
            !rx.on_frame(&duplicate)
                .expect("a duplicate is never an error"),
            "a duplicate diff must not advance the state"
        );
        assert_eq!(
            rx.peer_ack(),
            42,
            "the acknowledgement carried by a stale frame was thrown away"
        );
    }

    /// Reordering can deliver an OLDER acknowledgement after a newer one.
    /// Taking it at face value would walk the sender's view of the peer
    /// backwards and un-retire states it had already dropped.
    #[test]
    fn the_peers_ack_never_moves_backwards() {
        let mut tx = Sender::new(screen());
        tx.update(screen());
        let f = tx.make_frame(50).expect("make_frame").expect("frame");

        let mut rx = Receiver::new(screen());
        rx.on_frame(&f).expect("apply");
        assert_eq!(rx.peer_ack(), 50);

        let mut older = f.clone();
        older.ack_state = 7;
        rx.on_frame(&older).expect("stale is not an error");
        assert_eq!(rx.peer_ack(), 50, "the peer's ack went backwards");
    }

    #[test]
    fn ack_is_the_sequence_number_of_the_state_actually_held() {
        let mut tx = Sender::new(screen());
        let mut rx = Receiver::new(screen());
        assert_eq!(
            rx.ack(),
            0,
            "a receiver that has applied nothing holds nothing OF THE PEER'S, \
             whatever number its own invented state carries"
        );

        tx.update(screen());
        tx.update(screen());
        tx.on_ack(rx.ack());
        let f = tx.make_frame(rx.ack()).expect("make_frame").expect("frame");
        rx.on_frame(&f).expect("apply");
        assert_eq!(
            rx.ack(),
            3,
            "the ack is what we hold, not what we were sent"
        );
    }

    #[test]
    fn a_compression_bomb_is_refused_rather_than_allocated() {
        // 100 MiB of zeros compresses to a few hundred bytes. The peer is
        // authenticated, but that is exactly the assumption that fails first.
        let bomb =
            zstd::stream::encode_all(vec![0u8; 100 * 1024 * 1024].as_slice(), 1).expect("compress");
        assert!(bomb.len() < 100_000, "the fixture must actually be a bomb");

        let f = Frame {
            my_state: 2,
            from_state: 1,
            ack_state: 0,
            flags: FLAG_ZSTD,
            payload: bomb,
        };
        let mut rx = Receiver::new(screen());
        let before = rx.ack();
        assert!(matches!(rx.on_frame(&f), Err(ApplyError::Decode(_))));
        // The point is that the rejection changed nothing: a receiver that
        // advanced its ack after refusing a frame would tell the sender it
        // holds a state it does not.
        assert_eq!(rx.ack(), before);
    }
}

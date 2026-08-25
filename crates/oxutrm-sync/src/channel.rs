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
}

impl<S: SyncState> Sender<S> {
    pub fn new(initial: S) -> Sender<S> {
        let mut ring = VecDeque::with_capacity(STATE_RING);
        ring.push_back(initial);
        Sender { ring, peer_saw: 0 }
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
    pub fn make_frame(&self, ack_state: u64) -> Result<Option<Frame>, ApplyError> {
        let current = self.current();
        if self.peer_saw == current.seq() {
            return Ok(None);
        }

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

        Ok(Some(Frame {
            my_state: current.seq(),
            from_state,
            ack_state,
            flags,
            payload,
        }))
    }
}

/// Applies incoming frames to a replicated state.
pub struct Receiver<S: SyncState> {
    state: S,
    peer_ack: u64,
}

impl<S: SyncState> Receiver<S> {
    pub fn new(initial: S) -> Receiver<S> {
        Receiver {
            state: initial,
            peer_ack: 0,
        }
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
        // The peer's acknowledgement is recorded FIRST, and from every frame
        // including stale and duplicate ones. `ack_state` is a statement about
        // what the PEER holds; whether this frame's payload happens to be
        // applicable to US is a separate question. Throwing it away would
        // leave our sender diffing from an older base than it needs to, and
        // never retiring states from its ring — a permanent bandwidth cost
        // that only reordering and loss ever trigger, so no happy-path test
        // would show it.
        //
        // Monotonic, because reordering can deliver an older acknowledgement
        // after a newer one and taking that at face value would un-retire
        // states the sender had already dropped.
        self.peer_ack = self.peer_ack.max(f.ack_state);

        if f.my_state <= self.state.seq() {
            return Ok(false);
        }

        let payload = if f.flags & FLAG_ZSTD != 0 {
            decompress(&f.payload)?
        } else {
            f.payload.clone()
        };

        let diff: S::Diff = postcard::from_bytes(&payload)
            .map_err(|e| ApplyError::Decode(format!("decoding diff: {e}")))?;

        let mut next = self.state.clone();
        next.apply(f.from_state, f.my_state, &diff)?;
        // AFTER apply, never before: the question is whether the RESULT is a
        // legal state, and the state we already hold is legal by induction.
        next.validate()?;

        self.state = next;
        Ok(true)
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    /// The sequence number to put in our outgoing `ack_state`.
    pub fn ack(&self) -> u64 {
        self.state.seq()
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
    #[test]
    fn a_stale_frame_still_advances_the_peers_ack() {
        let mut tx = Sender::new(screen());
        tx.update(screen());
        let fresh = tx.make_frame(10).expect("make_frame").expect("frame");

        let mut rx = Receiver::new(screen());
        assert!(
            rx.on_frame(&fresh).expect("apply"),
            "the first frame applies"
        );
        assert_eq!(rx.peer_ack(), 10);

        // The very same frame again — a duplicate, which the network produces
        // on its own — but the peer has since acknowledged more of our state.
        let mut duplicate = fresh.clone();
        duplicate.ack_state = 42;
        assert!(
            !rx.on_frame(&duplicate)
                .expect("a duplicate is never an error"),
            "a duplicate must not advance the state"
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
        assert_eq!(rx.ack(), 1);

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
        assert!(matches!(rx.on_frame(&f), Err(ApplyError::Decode(_))));
        assert_eq!(rx.ack(), 1);
    }
}

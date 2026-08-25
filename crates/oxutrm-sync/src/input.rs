//! The same machinery, inverted: client to host, a byte stream rather than a
//! screen.

use serde::{Deserialize, Serialize};

use oxutrm_proto::{ApplyError, TermSize};

use crate::SyncState;

/// User input the host has not yet acknowledged, plus the size the client
/// wants.
///
/// Unacknowledged input is retransmitted automatically, without a
/// retransmission mechanism: it simply stays in `pending` until the host says
/// it consumed it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct InputState {
    /// Starts at 1. Zero is the full-state sentinel.
    pub seq: u64,
    /// In order, oldest first.
    pub pending: Vec<u8>,
    pub size: TermSize,
}

impl InputState {
    /// A new state with `bytes` appended and `size` recorded.
    ///
    /// The sequence number is left alone: [`Sender::update`](crate::Sender::update)
    /// assigns it, so that there is one place where sequence numbers are
    /// minted.
    pub fn append(&self, bytes: &[u8], size: TermSize) -> InputState {
        let mut pending = self.pending.clone();
        pending.extend_from_slice(bytes);
        InputState {
            seq: self.seq,
            pending,
            size,
        }
    }

    /// A new state with the first `n` bytes dropped, because the host consumed
    /// them.
    ///
    /// Saturates: consuming more than is pending leaves nothing pending, which
    /// is the only sane reading of "the host consumed everything".
    pub fn consume(&self, n: usize) -> InputState {
        let n = n.min(self.pending.len());
        InputState {
            seq: self.seq,
            pending: self.pending[n..].to_vec(),
            size: self.size,
        }
    }
}

/// What changed between two input states.
///
/// **The order of operations is not negotiable: drop `consumed` bytes from the
/// FRONT, then append.** Doing it the other way round writes input the host
/// already executed to the PTY a second time — a duplicated `rm`, a duplicated
/// `y`. There is a test for exactly this, and it is not decorative.
///
/// `base` and `target` are not here: they live in the
/// [`Frame`](oxutrm_proto::Frame) that carries this.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputDiff {
    /// Bytes the host has consumed, dropped from the front of `pending`.
    /// Saturating: [`u64::MAX`] means "everything", which is what a full state
    /// uses.
    pub consumed: u64,
    pub appended: Vec<u8>,
    pub size: Option<TermSize>,
}

impl SyncState for InputState {
    type Diff = InputDiff;

    fn seq(&self) -> u64 {
        self.seq
    }

    fn set_seq(&mut self, seq: u64) {
        self.seq = seq;
    }

    fn validate(&self) -> Result<(), ApplyError> {
        if self.seq == 0 {
            return Err(ApplyError::SeqZero);
        }
        Ok(())
    }

    fn diff_from(&self, base: &Self) -> InputDiff {
        // `self.pending` is `base.pending` with a prefix dropped and a suffix
        // appended. Find the largest overlap - the longest tail of the base
        // that is still the head of ours - so `appended` carries as little as
        // possible.
        //
        // Any overlap that satisfies the equation produces a CORRECT diff;
        // taking the largest is purely about size. That matters, because with
        // repeating bytes ("yyyy") several values satisfy it.
        let max = base.pending.len().min(self.pending.len());
        let mut overlap = 0usize;
        for k in (0..=max).rev() {
            if base.pending[base.pending.len() - k..] == self.pending[..k] {
                overlap = k;
                break;
            }
        }

        InputDiff {
            consumed: (base.pending.len() - overlap) as u64,
            appended: self.pending[overlap..].to_vec(),
            size: (self.size != base.size).then_some(self.size),
        }
    }

    fn full_diff(&self) -> InputDiff {
        // Consume everything the receiver holds, whatever that is, then append
        // the whole of ours. `apply` saturates, so u64::MAX means "clear".
        InputDiff {
            consumed: u64::MAX,
            appended: self.pending.clone(),
            size: Some(self.size),
        }
    }

    fn apply(&mut self, base: u64, target: u64, d: &InputDiff) -> Result<(), ApplyError> {
        if base != 0 && base != self.seq {
            return Err(ApplyError::BaseMismatch {
                base,
                current: self.seq,
            });
        }

        // DROP FIRST, THEN APPEND. The other order re-executes input the host
        // already ran.
        let drop = usize::try_from(d.consumed)
            .unwrap_or(usize::MAX)
            .min(self.pending.len());
        self.pending.drain(..drop);
        self.pending.extend_from_slice(&d.appended);

        if let Some(size) = d.size {
            self.size = size;
        }
        self.seq = target;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> TermSize {
        TermSize { cols: 80, rows: 24 }
    }

    fn state(pending: &[u8]) -> InputState {
        InputState {
            seq: 1,
            pending: pending.to_vec(),
            size: size(),
        }
    }

    #[test]
    fn apply_drops_from_the_front_before_it_appends() {
        // The order test. `pending` is "abc"; the host consumed "ab" and the
        // user then typed "d". The answer is "cd".
        //
        // Appending first would give "abcd", then dropping two would give
        // "cd" as well - so a naive round trip does NOT distinguish the two
        // orders. The distinguishing case is consuming MORE than the base
        // held, which is what the next test does.
        let mut s = state(b"abc");
        let d = InputDiff {
            consumed: 2,
            appended: b"d".to_vec(),
            size: None,
        };
        s.apply(1, 2, &d).expect("apply");
        assert_eq!(s.pending, b"cd");
    }

    #[test]
    fn consuming_everything_then_appending_is_not_the_same_as_the_reverse() {
        // Here the two orders genuinely differ. Base holds "ab", the host
        // consumed both, and the user typed "xy".
        //
        //   drop-then-append: "ab" -> "" -> "xy"        <- correct
        //   append-then-drop: "ab" -> "abxy" -> "xy"    <- same by luck
        //
        // So push it further: consume 4 from a 2-byte base.
        //   drop-then-append: "ab" -> "" -> "xy"        <- correct
        //   append-then-drop: "ab" -> "abxy" -> "y"     <- WRONG, and silently
        let mut s = state(b"ab");
        let d = InputDiff {
            consumed: 4,
            appended: b"xy".to_vec(),
            size: None,
        };
        s.apply(1, 2, &d).expect("apply");
        assert_eq!(
            s.pending, b"xy",
            "appending before dropping would have eaten a byte of new input"
        );
    }

    #[test]
    fn a_full_diff_replaces_whatever_the_receiver_held() {
        let target = state(b"fresh");
        let d = target.full_diff();
        assert_eq!(d.consumed, u64::MAX, "clear everything");

        let mut got = state(b"stale rubbish that must not survive");
        got.apply(0, 5, &d).expect("apply");
        assert_eq!(got.pending, b"fresh");
        assert_eq!(got.seq, 5);
    }

    #[test]
    fn append_and_consume_leave_the_sequence_number_alone() {
        // Sequence numbers are minted in exactly one place: Sender::update.
        let s = state(b"ab");
        assert_eq!(s.append(b"c", size()).seq, 1);
        assert_eq!(s.consume(1).seq, 1);
    }

    #[test]
    fn consume_saturates_rather_than_panicking() {
        assert_eq!(state(b"ab").consume(99).pending, b"");
        assert_eq!(state(b"").consume(1).pending, b"");
    }

    #[test]
    fn a_diff_round_trips_for_every_shape_of_change() {
        struct Case {
            name: &'static str,
            base: &'static [u8],
            target: &'static [u8],
        }
        let cases = [
            Case {
                name: "pure append",
                base: b"ab",
                target: b"abcd",
            },
            Case {
                name: "pure consume",
                base: b"abcd",
                target: b"cd",
            },
            Case {
                name: "both",
                base: b"abcd",
                target: b"cdef",
            },
            Case {
                name: "everything consumed",
                base: b"abcd",
                target: b"",
            },
            Case {
                name: "from empty",
                base: b"",
                target: b"xyz",
            },
            Case {
                name: "unchanged",
                base: b"abc",
                target: b"abc",
            },
            Case {
                name: "repeating bytes",
                base: b"yyyy",
                target: b"yyyyy",
            },
            Case {
                name: "all repeats consumed",
                base: b"yyyy",
                target: b"yy",
            },
        ];

        for c in cases {
            let base = state(c.base);
            let mut target = state(c.target);
            target.seq = 2;

            let d = target.diff_from(&base);
            let mut got = base.clone();
            got.apply(1, 2, &d).expect("apply");
            assert_eq!(got, target, "case: {}", c.name);
        }
    }

    #[test]
    fn a_resize_travels_only_when_it_changed() {
        let base = state(b"");
        let mut same = state(b"");
        same.seq = 2;
        assert!(same.diff_from(&base).size.is_none());

        let mut bigger = state(b"");
        bigger.seq = 2;
        bigger.size = TermSize {
            cols: 120,
            rows: 40,
        };
        assert_eq!(
            bigger.diff_from(&base).size,
            Some(TermSize {
                cols: 120,
                rows: 40
            })
        );
    }

    #[test]
    fn sequence_zero_is_rejected_here_too() {
        let mut s = state(b"");
        s.seq = 0;
        assert_eq!(SyncState::validate(&s), Err(ApplyError::SeqZero));
    }

    #[test]
    fn unacknowledged_input_is_retransmitted_without_a_retransmission_mechanism() {
        // The host never acknowledges, so the client's state keeps carrying
        // the same bytes and every diff against the host's stale base repeats
        // them. Nothing anywhere implements retransmission.
        let base = state(b"");
        let typed = base.append(b"ls -l\n", size());
        let d1 = typed.diff_from(&base);
        assert_eq!(d1.appended, b"ls -l\n");

        let typed_more = typed.append(b"pwd\n", size());
        let d2 = typed_more.diff_from(&base);
        assert_eq!(
            d2.appended, b"ls -l\npwd\n",
            "diffing against the same unacknowledged base carries both"
        );
        assert_eq!(d2.consumed, 0);
    }
}

#![forbid(unsafe_code)]

//! Replicated state and the diffs between versions of it.
//!
//! Do not send what happened; send the difference between what the peer is
//! known to have and what is true now. A lost datagram therefore costs
//! nothing, because the next diff is computed against the same acknowledged
//! base and contains whatever the lost one carried.
//!
//! This crate performs **no I/O whatsoever** — no sockets, no files, no
//! clocks. That is not incidental tidiness: it is what makes the riskiest part
//! of the protocol testable without a network.
//!
//! # The boundary, and why it is worth defending
//!
//! This crate depends on `oxutrm-proto` and nothing else that touches the
//! world. `tests/no_io.rs` enforces that against an **allowlist** rather than
//! a denylist, so an unrecognised dependency fails rather than an
//! unanticipated one slipping through.
//!
//! It is also **transport-agnostic**. It produces a [`Frame`](oxutrm_proto::Frame)
//! and has no opinion about how the frame travels; datagram versus stream is
//! the transport's decision, taken on size.

mod channel;
mod input;
mod screen;

pub use channel::{Receiver, Sender};
pub use input::{InputDiff, InputState};
pub use screen::{RowPatch, Run, ScreenDiff};

use oxutrm_proto::ApplyError;

/// The depth of BOTH rings, and they are coupled.
///
/// The sender keeps this many recent states so it can diff against whatever
/// the peer last acknowledged. Once the peer's ack falls out of that window
/// there is nothing to diff against and a full state goes instead — correct,
/// just larger.
///
/// The RECEIVER caps its ring at the same number, and that is not a
/// coincidence to be tidied away: the sender can only name a base within its
/// last `STATE_RING` updates, so a receiver cap of at least `STATE_RING` is
/// what guarantees the named base is still held. Lowering the receiver's cap
/// alone silently reinstates the base-drift defect (R4) — and HIDES it,
/// because in that regime ring exhaustion degrades every frame to a full
/// state, and a full state applies unconditionally (R3). The rescue masks the
/// bug, which is why the defect went unnoticed until it was measured.
pub const STATE_RING: usize = 32;

/// A replicated value.
///
/// No I/O, no clocks, no allocation assumptions. Everything here is a pure
/// function of the values involved, which is what makes the convergence
/// property checkable without a network.
pub trait SyncState: Clone {
    type Diff: serde::Serialize + serde::de::DeserializeOwned;

    fn seq(&self) -> u64;
    fn set_seq(&mut self, seq: u64);

    /// Check this value's own invariants.
    ///
    /// Called **after** [`SyncState::apply`], never before: the question is
    /// whether the *result* is a legal state, and the state already held is
    /// legal by induction.
    fn validate(&self) -> Result<(), ApplyError>;

    /// Check the invariants that exist only **between** two states — the ones
    /// a single value cannot show, because one state in isolation carries no
    /// history.
    ///
    /// This is what [`Receiver::on_frame`] calls after [`SyncState::apply`],
    /// with the pre-application state as `previous`. It replaces the bare
    /// [`SyncState::validate`] call rather than joining it: the default
    /// implementation here *is* `validate`, so a state whose invariants are
    /// all properties of one value gets exactly the old behaviour and an
    /// implementor cannot forget to chain.
    ///
    /// A failure here is a **rejection with a reason**, never a fatal error:
    /// `on_frame` applies to a clone, so the live state and the ack are both
    /// untouched, and the session carries on.
    fn validate_transition(&self, _previous: &Self) -> Result<(), ApplyError> {
        self.validate()
    }

    /// The diff that turns `base` into `self`.
    fn diff_from(&self, base: &Self) -> Self::Diff;

    /// Apply a diff.
    ///
    /// `base` and `target` come from the [`Frame`](oxutrm_proto::Frame) that
    /// carried the diff, because the diff itself does not repeat them — two
    /// copies of one fact can disagree. `base == 0` is the full-state
    /// sentinel: the diff builds on nothing, so whatever is currently held is
    /// irrelevant.
    ///
    /// On error, the caller discards this value rather than using it: nothing
    /// here promises to roll back a partial application, and
    /// [`Receiver::on_frame`] applies to a clone precisely so it never has to.
    fn apply(&mut self, base: u64, target: u64, d: &Self::Diff) -> Result<(), ApplyError>;

    /// A diff from nothing, for when the peer's ack has left the ring.
    fn full_diff(&self) -> Self::Diff;
}

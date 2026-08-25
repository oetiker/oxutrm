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

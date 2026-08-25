#![forbid(unsafe_code)]

//! The local half: it paints the authoritative screen into the terminal the
//! user is actually sitting in front of, and sends their keystrokes back.
//!
//! Every capability adaptation happens here — colours the local terminal
//! cannot show are folded down at render time — so the host's state keeps full
//! fidelity for a better terminal tomorrow. This crate also owns the promise
//! that the user's terminal is left exactly as it was found, on every exit
//! path including a panic.

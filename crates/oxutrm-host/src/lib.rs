#![forbid(unsafe_code)]

//! The remote half of a session: it owns the PTY and the authoritative screen,
//! survives the client going away, and can be found again on reattach.
//!
//! Detached is a normal state, not an error. A session that nobody is watching
//! keeps draining its PTY, transmits nothing, and costs no bandwidth for as
//! long as it is left alone.

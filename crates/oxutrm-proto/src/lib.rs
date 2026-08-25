#![forbid(unsafe_code)]

//! The wire. Every type that crosses between the two ends of a session is
//! defined here and nowhere else: the SSH signalling messages, the datagram
//! `Frame`, the stream messages, and the protocol version that is checked at
//! handshake time.
//!
//! This crate is the single normative source for the wire format. When the
//! design spec and this crate appear to disagree, this crate is right.

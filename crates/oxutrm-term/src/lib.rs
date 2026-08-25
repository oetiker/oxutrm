#![forbid(unsafe_code)]

//! The terminal itself: a PTY, an `alacritty_terminal` emulator driving it,
//! and the `ScreenState` snapshot that everything downstream replicates.
//!
//! The same emulator runs on both ends, which is what lets the client render
//! from authoritative state rather than approximating it. This crate also
//! answers what the local terminal can display, and what `TERM` an emulated
//! child should be given.

#![forbid(unsafe_code)]

//! The terminal itself: a PTY, an `alacritty_terminal` emulator driving it,
//! and the conversion from what the emulator holds into an
//! [`oxutrm_proto::ScreenState`].
//!
//! The same emulator runs on both ends, which is what lets the client render
//! from authoritative state rather than approximating it. This crate also
//! answers what the local terminal can display, and what `TERM` an emulated
//! child should be given.
//!
//! # What is NOT here
//!
//! The screen model — `ScreenState`, `Cell`, `Color`, `Attrs`, `Cursor`,
//! `Modes` — lives in `oxutrm-proto`, because it is a wire type and because
//! `alacritty_terminal` drags in a PTY, `polling` and `signal-hook` with no
//! feature flag to exclude them. Anything that reached for the screen model
//! here would inherit all three, `oxutrm-sync` included — and `oxutrm-sync`
//! having no I/O in its dependency tree is what makes the convergence property
//! testable without a socket.
//!
//! This crate is therefore where the I/O lives, deliberately and on its own.

// Two exemptions, each one call site, each documented where it sits.
// `pty.rs`: making the child a session leader with the PTY as its controlling
// terminal needs `CommandExt::pre_exec`, which std exposes only as an
// `unsafe fn`. Without it a shell has no job control - no ^C, no ^Z, no
// SIGWINCH - which is most of what a terminal is for.
// `exit_wake.rs`, macOS only: `kevent` is an `unsafe fn` because the
// descriptors an `Event` names must outlive the kqueue; that call names a PID
// and no descriptor. `deny` rather than `forbid` so both can be allowed and
// documented.
#![deny(unsafe_code)]

//! The terminal itself: a PTY, an `alacritty_terminal` emulator driving it,
//! and the conversion from what the emulator holds into an
//! [`oxutrm_proto::ScreenState`].
//!
//! The same emulator runs on both ends, which is what lets the client render
//! from authoritative state rather than approximating it.
//!
//! # What is NOT here
//!
//! The screen model — `ScreenState`, `Cell`, `Color`, `Attrs`, `Cursor`,
//! `Modes` — lives in `oxutrm-proto`, because it is a wire type and because
//! `alacritty_terminal` drags in a PTY, `polling` and `signal-hook` with no
//! feature flag to exclude them. Anything reaching for the screen model here
//! would inherit all three, `oxutrm-sync` included — and `oxutrm-sync` having
//! no I/O in its dependency tree is what makes the convergence property
//! testable without a socket. This crate is where the I/O lives, on its own.
//!
//! # Four things `alacritty_terminal` does not do for you
//!
//! Each is silent when you get it wrong, which is why each has a module.
//!
//! 1. **`Index<Point>` has only a `debug_assert`.** Out of range it panics in
//!    debug and reads whatever is next in memory in release. [`grid::cell_at`]
//!    is the single checked accessor and the only place in oxutrm that indexes
//!    the grid.
//! 2. **There is no default palette.** `Term::colors()` is an OSC 4/10/11
//!    *override* table, all-`None` until an application sets something.
//!    [`palette`] supplies the 269 entries the crate does not.
//! 3. **There is no scrolled-off counter.** `history_size()` saturates at
//!    capacity, so [`HostTerm::poll`] accumulates its growth on every pass to
//!    synthesize `scrollback_len`.
//! 4. **Blink is parsed and then dropped.** `vte` turns SGR 5, 6 and 25 into
//!    `Attr::BlinkSlow`/`BlinkFast`/`CancelBlink`, and
//!    `Term::terminal_attribute` discards all three. [`blink`] recovers them.

mod blink;
mod caps;
mod exit_wake;
mod grid;
mod host;
mod listener;
mod palette;
mod pty;

#[cfg(test)]
mod golden;
#[cfg(test)]
mod testing;

pub use caps::{detect_caps, negotiate_term};
pub use exit_wake::ExitWake;
pub use grid::GridSize;
pub use host::HostTerm;
pub use listener::{EventSink, Signals};
pub use palette::{PALETTE_LEN, palette};

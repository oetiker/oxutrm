// `deny` rather than `forbid`, for exactly one exception: installing the signal
// handlers that restore the user's terminal when the client is killed. That
// needs `sigaction`, which has no safe binding anywhere in this dependency
// tree. Every `#[allow(unsafe_code)]` in this crate is in `guard.rs` and is
// about that one thing; anything else is a bug.
#![deny(unsafe_code)]

//! The local half: it paints the authoritative screen into the terminal the
//! user is actually sitting in front of, and sends their keystrokes back.
//!
//! # Two things happen here and nowhere else
//!
//! **The second diff.** The sync engine diffs host state against host state.
//! [`Renderer`] diffs the desired screen against a model of what is *currently
//! painted on the physical terminal*, and emits the minimal ANSI to reconcile
//! them. That model is what will later make local scrollback and a locally
//! drawn status pane possible without repainting the world.
//!
//! **Capability adaptation.** Colours this terminal cannot show are folded down
//! at render time, so the host's [`ScreenState`](oxutrm_proto::ScreenState)
//! keeps full fidelity for a better terminal tomorrow. The host never learns
//! what this client can display, and deliberately so: the child's `TERM` cannot
//! change under a shell that has been running for a week.
//!
//! This crate does **not** emulate a terminal. It renders a `ScreenState`;
//! `alacritty_terminal` is `oxutrm-term`'s business. Phase C's speculative echo
//! will revisit that, by holding a second emulator — but not yet.

pub mod color;
pub mod guard;
pub mod notice;
pub mod overlay;
pub mod renderer;
pub mod status;

pub use color::down_convert;
pub use guard::{RawGuard, TERMINAL_RESTORE};
pub use notice::{Notice, layout_notice};
pub use overlay::{Overlay, overlay_from_buffer};
pub use renderer::Renderer;
pub use status::{rung_label, status_line};

use anyhow::Context;
use oxutrm_proto::TermSize;

/// The size of the terminal the user is sitting in front of.
///
/// Read from the terminal rather than from `$LINES`/`$COLUMNS`, which go stale
/// the moment the window is resized.
///
/// Asked of **standard input**, not standard output, and the difference is a
/// live bug rather than a preference. Nothing in oxutrm requires fd 1 to be a
/// terminal: [`RawGuard::enter`] asserts `isatty(0)` and the keyboard is read
/// from the controlling terminal by name. So `oxutrm … > transcript.txt`, run
/// by a person sitting in a real terminal, is an ordinary thing to do and has
/// no window size to report on fd 1 at all — `tcgetwinsize` answers `ENOTTY`.
///
/// A caller with a better descriptor to hand — the one it already opened on
/// `/dev/tty`, say — should use [`terminal_size_of`] and not rely on any
/// standard descriptor being the terminal.
pub fn terminal_size() -> anyhow::Result<TermSize> {
    terminal_size_of(rustix::stdio::stdin())
}

/// [`terminal_size`], asked of a specific descriptor.
///
/// Fails for a descriptor that is not a terminal, and for a window reported as
/// zero in either dimension — emulators emit `0x0` while tearing down and some
/// multiplexers emit it transiently on detach, and a zero-sized screen is not
/// a state the rest of this program is prepared to hold.
///
/// **Both failures are things a caller has to survive.** A window that cannot
/// be measured says nothing about whether the session should continue; the
/// last size that *was* measured is still the best answer available, and the
/// next resize will correct it.
pub fn terminal_size_of<F: std::os::fd::AsFd>(fd: F) -> anyhow::Result<TermSize> {
    let ws = rustix::termios::tcgetwinsize(fd).context("read the terminal size")?;
    anyhow::ensure!(
        ws.ws_col > 0 && ws.ws_row > 0,
        "the terminal reported a zero-sized window ({}x{})",
        ws.ws_col,
        ws.ws_row
    );
    Ok(TermSize {
        cols: ws.ws_col,
        rows: ws.ws_row,
    })
}

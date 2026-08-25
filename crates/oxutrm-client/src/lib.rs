#![forbid(unsafe_code)]

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
pub mod renderer;
pub mod status;

pub use color::down_convert;
pub use guard::RawGuard;
pub use renderer::Renderer;
pub use status::{rung_label, status_line};

use anyhow::Context;
use oxutrm_proto::TermSize;

/// The size of the terminal the user is sitting in front of.
///
/// Read from the controlling terminal rather than from `$LINES`/`$COLUMNS`,
/// which go stale the moment the window is resized.
pub fn terminal_size() -> anyhow::Result<TermSize> {
    let ws =
        rustix::termios::tcgetwinsize(rustix::stdio::stdout()).context("read the terminal size")?;
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

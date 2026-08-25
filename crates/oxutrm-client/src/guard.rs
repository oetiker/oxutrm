//! Leaving the user's terminal exactly as it was found, on every exit path.
//!
//! This is the safety-critical part of the client. A process that dies with the
//! terminal still in raw mode leaves the user with a shell that does not echo,
//! where Enter does nothing and Ctrl-C does not interrupt — and no indication of
//! why. They will most likely close the window and lose whatever else was in it.
//!
//! There are three ways out of a session and all three must restore:
//!
//! | Exit | Restored by |
//! |---|---|
//! | normal return | [`RawGuard`]'s `Drop` |
//! | panic (unwinding) | `Drop`, which runs during the unwind |
//! | panic (`panic = "abort"`) or a fatal signal | the panic hook and [`RawGuard::restore_now`] |
//!
//! `Drop` alone would be enough for the first two. The hook exists because the
//! third case skips destructors entirely, and "we do not build with
//! `panic = abort` today" is not a property worth betting a user's terminal on.

use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Context;
use rustix::termios::{OptionalActions, Termios};

/// The settings to put back, and the descriptor to put them back on.
///
/// Process-wide rather than held in the guard because a panic hook cannot
/// borrow the guard, and because the restore must be reachable from a context
/// that has no `self`.
static ORIGINAL: OnceLock<(OwnedFd, Termios)> = OnceLock::new();
/// Starts `true`: with no guard installed there is nothing to undo, so
/// `restore_now` from an unrelated panic is correctly a no-op.
static RESTORED: AtomicBool = AtomicBool::new(true);
static HOOK_INSTALLED: OnceLock<()> = OnceLock::new();

/// Raw mode on entry, restored on `Drop` and on panic.
pub struct RawGuard {
    /// Set when this guard owns the process-wide restore, i.e. it was built by
    /// [`RawGuard::enter`]. A guard on a caller-supplied descriptor restores
    /// itself and touches no global state.
    global: bool,
    fd: OwnedFd,
    original: Termios,
    restored: bool,
}

fn make_raw(fd: BorrowedFd<'_>) -> anyhow::Result<Termios> {
    let original = rustix::termios::tcgetattr(fd).context("read terminal settings")?;
    let mut raw = original.clone();
    raw.make_raw();
    rustix::termios::tcsetattr(fd, OptionalActions::Flush, &raw).context("enter raw mode")?;
    Ok(original)
}

impl RawGuard {
    /// Put the controlling terminal into raw mode.
    ///
    /// Fails when stdin is not a terminal, which is the correct outcome: a
    /// client with no terminal has nothing to render into.
    pub fn enter() -> anyhow::Result<RawGuard> {
        let stdin = rustix::stdio::stdin();
        anyhow::ensure!(
            rustix::termios::isatty(stdin),
            "standard input is not a terminal"
        );
        // A private duplicate, so the restore does not depend on fd 0 still
        // meaning what it meant at startup.
        let fd = stdin.try_clone_to_owned().context("duplicate stdin")?;
        let original = make_raw(fd.as_fd())?;

        // Ignore the error when a guard already registered: the first one's
        // settings are the ones that predate every guard, so they are the right
        // ones to restore.
        let _ = ORIGINAL.set((fd.try_clone().context("duplicate stdin")?, original.clone()));
        RESTORED.store(false, Ordering::SeqCst);

        HOOK_INSTALLED.get_or_init(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                // Restore first, so the panic message lands on a terminal the
                // user can actually read.
                RawGuard::restore_now();
                previous(info);
            }));
        });

        Ok(RawGuard {
            global: true,
            fd,
            original,
            restored: false,
        })
    }

    /// Put a caller-supplied descriptor into raw mode.
    ///
    /// This exists so the restore can be tested against a real terminal: a test
    /// binary has no controlling terminal, so [`RawGuard::enter`] cannot run
    /// under `cargo test` at all, and a restore path that is never executed is
    /// a restore path that does not work.
    pub fn enter_on(fd: OwnedFd) -> anyhow::Result<RawGuard> {
        let original = make_raw(fd.as_fd())?;
        Ok(RawGuard {
            global: false,
            fd,
            original,
            restored: false,
        })
    }

    /// Undo the process-wide raw mode. Idempotent, and a no-op when no guard
    /// was ever installed.
    pub fn restore_now() {
        if RESTORED.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some((fd, original)) = ORIGINAL.get() {
            let _ = rustix::termios::tcsetattr(fd.as_fd(), OptionalActions::Flush, original);
        }
    }

    /// True once the process-wide raw mode has been undone.
    pub fn is_restored() -> bool {
        RESTORED.load(Ordering::SeqCst)
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        if self.restored {
            return;
        }
        self.restored = true;
        // Best effort by necessity: `Drop` may be running during an unwind, and
        // panicking inside a panic aborts the process — which would leave the
        // terminal in exactly the state this type exists to prevent.
        let _ = rustix::termios::tcsetattr(self.fd.as_fd(), OptionalActions::Flush, &self.original);
        if self.global {
            RESTORED.store(true, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::termios::LocalModes;

    /// A real pty, so the restore is exercised against a real line discipline
    /// rather than a mock that cannot disagree with the kernel.
    fn open_pty() -> OwnedFd {
        use rustix::pty::OpenptFlags;
        let master = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY)
            .expect("open a pty master");
        rustix::pty::grantpt(&master).expect("grantpt");
        rustix::pty::unlockpt(&master).expect("unlockpt");
        master
    }

    fn is_cooked(fd: BorrowedFd<'_>) -> bool {
        let t = rustix::termios::tcgetattr(fd).expect("tcgetattr");
        t.local_modes.contains(LocalModes::ECHO) && t.local_modes.contains(LocalModes::ICANON)
    }

    #[test]
    fn entering_raw_mode_turns_echo_and_canonical_input_off() {
        let pty = open_pty();
        let borrowed = pty.try_clone().expect("dup for observation");
        assert!(is_cooked(borrowed.as_fd()), "a fresh pty should be cooked");

        let guard = RawGuard::enter_on(pty).expect("enter raw mode");
        assert!(!is_cooked(borrowed.as_fd()), "raw mode did not take effect");
        drop(guard);
    }

    #[test]
    fn a_normal_drop_restores_the_terminal() {
        let pty = open_pty();
        let observer = pty.try_clone().expect("dup for observation");
        {
            let _guard = RawGuard::enter_on(pty).expect("enter raw mode");
            assert!(!is_cooked(observer.as_fd()));
        }
        assert!(
            is_cooked(observer.as_fd()),
            "the terminal was left in raw mode after a normal exit"
        );
    }

    /// The case that matters: the process is unwinding, and the user's terminal
    /// must still come back.
    #[test]
    fn a_panic_inside_the_guards_scope_restores_the_terminal() {
        let pty = open_pty();
        let observer = pty.try_clone().expect("dup for observation");

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = RawGuard::enter_on(pty).expect("enter raw mode");
            assert!(!is_cooked(observer.as_fd()), "raw mode did not take effect");
            panic!("deliberate panic with the terminal in raw mode");
        }));

        assert!(result.is_err(), "the test's own panic did not happen");
        assert!(
            is_cooked(observer.as_fd()),
            "a panic left the user's terminal in raw mode"
        );
    }

    #[test]
    fn restoring_twice_is_harmless() {
        let pty = open_pty();
        let observer = pty.try_clone().expect("dup for observation");
        let guard = RawGuard::enter_on(pty).expect("enter raw mode");
        drop(guard);
        RawGuard::restore_now();
        RawGuard::restore_now();
        assert!(is_cooked(observer.as_fd()));
    }

    #[test]
    fn restore_now_without_a_guard_does_nothing_and_does_not_panic() {
        // No `enter()` has run in this test binary, so there is nothing to undo.
        RawGuard::restore_now();
        assert!(RawGuard::is_restored());
    }
}

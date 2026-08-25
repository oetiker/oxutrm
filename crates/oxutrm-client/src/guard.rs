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
//! | panic (`panic = "abort"`) | the panic hook, via [`RawGuard::restore_now`] |
//! | SIGTERM/INT/HUP/QUIT | the signal handler, via the same |
//!
//! `Drop` alone would be enough for the first two. The hook exists because the
//! third case skips destructors entirely, and "we do not build with
//! `panic = abort` today" is not a property worth betting a user's terminal on.
//! The handler exists because `kill <pid>` is a thing people do, and its
//! default action terminates the process without running a single destructor.
//!
//! `SIGKILL` and `SIGSTOP` cannot be caught by anyone, so `kill -9` still
//! leaves the terminal raw. That is a property of the kernel, not a gap here.

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
static SIGNALS_INSTALLED: OnceLock<()> = OnceLock::new();

/// The termination signals that can be caught at all.
///
/// `SIGKILL` and `SIGSTOP` are absent because the kernel does not allow them to
/// be caught, not because they were overlooked.
const FATAL_SIGNALS: [libc::c_int; 4] = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];

/// Raw mode on entry, restored on `Drop`, on panic, and on a catchable
/// termination signal.
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
        install_signal_handlers();

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

/// Restore the terminal, then die of the signal that arrived.
///
/// Everything this does is on POSIX's list of async-signal-safe operations:
/// [`RawGuard::restore_now`] is an atomic swap, a `OnceLock` read and
/// `tcsetattr`; `signal` and `raise` are listed too. There is no allocation, no
/// lock, no formatting and no I/O beyond the one `ioctl` — which is what makes
/// this safe to run at an arbitrary point in the program.
///
/// Re-raising with the default disposition, rather than calling `exit`, is what
/// lets a waiting parent see that the client was killed and by what. An
/// invented exit code would be a lie about how the process died.
extern "C" fn restore_and_reraise(sig: libc::c_int) {
    RawGuard::restore_now();
    #[allow(unsafe_code)] // `signal` and `raise` have no safe binding here
    // SAFETY: both are async-signal-safe, take a plain signal number, and this
    // is the handler for `sig` itself, so resetting its own disposition races
    // with nothing that matters.
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
}

/// Claim every termination signal that can be caught, once per process.
///
/// Any handler already installed for these is replaced. Nothing in oxutrm
/// installs one, and a client that has put the terminal into raw mode has the
/// strongest claim on them there is.
fn install_signal_handlers() {
    SIGNALS_INSTALLED.get_or_init(|| {
        for sig in FATAL_SIGNALS {
            #[allow(unsafe_code)] // `sigaction` is the whole point of this crate's one exception
            // SAFETY: `act` is fully initialised before use and lives for the
            // whole call; the handler is a real `extern "C"` function that does
            // only async-signal-safe work.
            unsafe {
                let mut act: libc::sigaction = std::mem::zeroed();
                act.sa_sigaction = restore_and_reraise as *const () as usize;
                libc::sigemptyset(&mut act.sa_mask);
                // Nothing here resumes: the handler re-raises and the process
                // dies. SA_RESTART is set anyway so that a signal arriving
                // while a handler is already running cannot turn an unrelated
                // syscall into a spurious EINTR on the way out.
                act.sa_flags = libc::SA_RESTART;
                libc::sigaction(sig, &act, std::ptr::null_mut());
            }
        }
    });
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
    use rustix::process::Signal;
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

    // ---- the paths that need a whole process -------------------------------
    //
    // `enter()` is the only entry point that installs the process-wide restore,
    // the panic hook and the signal handlers, and it can only run where stdin is
    // a terminal — which a test binary's stdin is not. Worse, the two things
    // worth proving here are *the process dying*: a signal whose default action
    // terminates, and a panic whose `Drop` never runs. Neither can be observed
    // from inside the process it happens to.
    //
    // So the parent opens a pty, hands it to a child as stdin, and watches the
    // line discipline from its own descriptor. The child is this same test
    // binary re-invoked on the ignored helper below, told by an environment
    // variable which way to die.

    const HELPER_VAR: &str = "OXUTRM_GUARD_HELPER";
    const HELPER_TEST: &str = "guard::tests::the_helper_child";

    /// Spawn the helper with `pty` as its stdin. Returns the child and a
    /// descriptor onto the same pty for the parent to watch.
    fn spawn_helper(mode: &str) -> (std::process::Child, OwnedFd) {
        let pty = open_pty();
        let observer = pty.try_clone().expect("dup for observation");
        let exe = std::env::current_exe().expect("the test binary's own path");
        let child = std::process::Command::new(exe)
            .args([HELPER_TEST, "--exact", "--ignored", "--test-threads", "1"])
            .env(HELPER_VAR, mode)
            .stdin(std::process::Stdio::from(pty))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the helper child");
        (child, observer)
    }

    /// Wait up to five seconds for the pty to reach `want`.
    fn wait_for_cooked(fd: BorrowedFd<'_>, want: bool) -> bool {
        for _ in 0..500 {
            if is_cooked(fd) == want {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    /// Is a signal's disposition something other than "do the default"?
    #[allow(unsafe_code)] // reading a disposition needs `sigaction`, which is `unsafe`
    fn has_handler(sig: libc::c_int) -> bool {
        // SAFETY: a query-only `sigaction` — the `act` argument is null, so
        // nothing is installed, and `old` points at a live, owned value.
        unsafe {
            let mut old: libc::sigaction = std::mem::zeroed();
            assert_eq!(
                libc::sigaction(sig, std::ptr::null(), &mut old),
                0,
                "sigaction query failed for signal {sig}"
            );
            old.sa_sigaction != libc::SIG_DFL && old.sa_sigaction != libc::SIG_IGN
        }
    }

    /// `kill <pid>` on a running client used to leave the user with a terminal
    /// that does not echo and does not line-edit, and no clue why. The default
    /// action for SIGTERM terminates the process outright, so `Drop` never runs
    /// and only a handler can put the terminal back.
    #[test]
    fn a_terminating_signal_restores_the_terminal_before_the_process_dies() {
        for sig in [Signal::TERM, Signal::INT, Signal::HUP] {
            let (mut child, observer) = spawn_helper("wait");
            assert!(
                wait_for_cooked(observer.as_fd(), false),
                "the helper never entered raw mode"
            );

            let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("a live pid");
            rustix::process::kill_process(pid, sig).expect("signal the helper");
            let status = child.wait().expect("wait for the helper");

            assert!(
                is_cooked(observer.as_fd()),
                "signal {sig:?} left the user's terminal in raw mode"
            );
            // And it died *of* that signal, rather than of an exit code we
            // invented: whatever is waiting on the client sees the truth.
            use std::os::unix::process::ExitStatusExt;
            assert_eq!(
                status.signal(),
                Some(sig.as_raw()),
                "signal {sig:?}: the process did not die of the signal it was sent"
            );
        }
    }

    /// SIGQUIT is not sent above, because its default action dumps core and a
    /// test suite has no business writing core files. What can be checked
    /// without raising it is that `enter()` did claim it — the check runs in the
    /// child, because that is where `enter()` can run at all.
    #[test]
    fn every_catchable_termination_signal_is_claimed_including_sigquit() {
        let (mut child, observer) = spawn_helper("dispositions");
        let status = child.wait().expect("wait for the helper");
        drop(observer);
        assert_eq!(
            status.code(),
            Some(0),
            "a termination signal was left on its default action (see the helper)"
        );
    }

    /// The hook exists for the exit that skips destructors. Every other test in
    /// this file uses `enter_on`, which installs no hook, so without this one
    /// the hook is never executed at all — and a restore path that never runs
    /// is a restore path that does not work.
    #[test]
    fn the_panic_hook_restores_when_drop_never_runs() {
        let (mut child, observer) = spawn_helper("panic");
        assert!(
            wait_for_cooked(observer.as_fd(), false),
            "the helper never entered raw mode"
        );
        child.wait().expect("wait for the helper");
        assert!(
            is_cooked(observer.as_fd()),
            "the panic hook did not restore the terminal"
        );
    }

    /// Not a test. This is the body of the child process the three tests above
    /// spawn; it does nothing unless it was spawned by one of them.
    #[test]
    #[ignore = "the child half of the guard's process-level tests; run by them, not on its own"]
    fn the_helper_child() {
        let Ok(mode) = std::env::var(HELPER_VAR) else {
            return;
        };
        // stdin is the pty the parent is watching, so this is the real
        // `enter()` — the panic hook and the signal handlers included.
        let guard = RawGuard::enter().expect("the helper's stdin is a pty");

        match mode.as_str() {
            "dispositions" => {
                let all = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT]
                    .into_iter()
                    .all(has_handler);
                drop(guard);
                std::process::exit(if all { 0 } else { 3 });
            }
            "panic" => {
                // Leaked on purpose: `Drop` must not be the thing that restores,
                // or this proves nothing about the hook.
                std::mem::forget(guard);
                std::thread::sleep(std::time::Duration::from_millis(200));
                panic!("deliberate panic with the terminal in raw mode");
            }
            // Stay alive, in raw mode, until the parent signals. The sleep is a
            // backstop against a parent that died without signalling.
            _ => {
                std::mem::forget(guard);
                std::thread::sleep(std::time::Duration::from_secs(30));
                std::process::exit(4);
            }
        }
    }
}

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
//! Every one of those paths restores TWO things, and the second is the one that
//! was missing. `tcsetattr` puts the line discipline back, but the alternate
//! screen, mouse reporting and a hidden cursor live inside the terminal
//! emulator, where no `ioctl` reaches them. Undoing those needs an escape
//! sequence — see [`TERMINAL_RESTORE`], which is why it is a `const` slice
//! written with one `write(2)`.
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
/// Where [`TERMINAL_RESTORE`] is written on the process-wide restore.
///
/// Standard OUTPUT, not the terminal descriptor in `ORIGINAL`. Whatever
/// received the alternate-screen switch must receive the undo, and the renderer
/// paints to stdout — so if stdout is a file or a pipe, the terminal was never
/// touched and neither is anything else. Kept separately because a signal
/// handler cannot borrow a guard.
static RESTORE_OUT: OnceLock<OwnedFd> = OnceLock::new();
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

/// Everything a session may have switched on, switched off again — I8's
/// exit-time restoration, in one `const` byte string.
///
/// A host is not trusted. `alt_screen`, mouse reporting and a hidden cursor are
/// all legitimate mid-session, so a hostile host simply turns them on and drops
/// the connection: `RawGuard` used to restore termios and emit nothing at all,
/// and the user was returned to a shell on the alternate buffer, with no
/// cursor, emitting SGR mouse reports at every twitch of the mouse. Restoring
/// the line discipline does not undo any of that — those are modes inside the
/// terminal emulator, and only an escape sequence turns them off.
///
/// It is one `const` slice because this must be emitable from a signal handler.
/// That rules out allocation, formatting and locking, and leaves `write(2)` of
/// bytes that already exist — the same discipline `restore_and_reraise`
/// already follows.
///
/// In order, and the order is deliberate:
///
/// | Bytes | Undoes |
/// |---|---|
/// | `\x1b[?1049l` | the alternate screen buffer — FIRST, so everything after it lands on the screen the user keeps |
/// | `\x1b[?1003l` | any-motion mouse tracking |
/// | `\x1b[?1002l` | button-motion mouse tracking |
/// | `\x1b[?1000l` | press/release mouse tracking |
/// | `\x1b[?1006l` | SGR mouse encoding |
/// | `\x1b[?2004l` | bracketed paste |
/// | `\x1b[?25h`   | a hidden cursor |
/// | `\x1b[0m`     | colours and attributes — LAST, so the prompt that follows is plain |
///
/// The three mouse tracking modes are separate switches rather than one, so all
/// three are cleared; `Renderer::write_modes` turns them on the same way.
pub const TERMINAL_RESTORE: &[u8] = b"\x1b[?1049l\
                                      \x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1006l\
                                      \x1b[?2004l\
                                      \x1b[?25h\
                                      \x1b[0m";

/// Raw mode on entry, restored on `Drop`, on panic, and on a catchable
/// termination signal.
pub struct RawGuard {
    /// Set when this guard owns the process-wide restore, i.e. it was built by
    /// [`RawGuard::enter`]. A guard on a caller-supplied descriptor restores
    /// itself and touches no global state.
    global: bool,
    fd: OwnedFd,
    /// Where this guard writes [`TERMINAL_RESTORE`].
    out: OwnedFd,
    original: Termios,
    restored: bool,
}

/// Put [`TERMINAL_RESTORE`] on the wire. Callable from a signal handler.
///
/// Nothing here allocates, formats, locks or takes a reference to anything that
/// could be mid-mutation: the bytes are `const` and the only syscall is
/// `write`, which POSIX lists as async-signal-safe.
///
/// The loop is for a SHORT write, which a terminal under flow control can
/// produce and which would otherwise leave the sequence half-emitted — half of
/// it being no better than none, since the modes it did not reach stay on. It
/// is still only `write(2)` calls, and it cannot spin: every branch that is not
/// forward progress breaks.
fn emit_restore(fd: BorrowedFd<'_>) {
    let mut rest = TERMINAL_RESTORE;
    while !rest.is_empty() {
        match rustix::io::write(fd, rest) {
            Ok(0) | Err(_) => break,
            Ok(n) => rest = &rest[n..],
        }
    }
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
        // The escape sequence goes where the PAINTING went, which is stdout.
        let out = rustix::stdio::stdout()
            .try_clone_to_owned()
            .context("duplicate stdout")?;

        // Ignore the error when a guard already registered: the first one's
        // settings are the ones that predate every guard, so they are the right
        // ones to restore.
        let _ = ORIGINAL.set((fd.try_clone().context("duplicate stdin")?, original.clone()));
        let _ = RESTORE_OUT.set(out.try_clone().context("duplicate stdout")?);
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
            out,
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
        // No separate stdout here: the caller supplied the terminal, so the
        // terminal is also where the escape sequence belongs.
        let out = fd.try_clone().context("duplicate the terminal")?;
        Ok(RawGuard {
            global: false,
            fd,
            out,
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
        // Termios FIRST, and the order is load-bearing: `tcsetattr` cannot
        // block, while a write to a terminal stopped by Ctrl-S can. Emitting
        // first would risk never reaching the line discipline at all, which is
        // the worse half to lose.
        if let Some((fd, original)) = ORIGINAL.get() {
            let _ = rustix::termios::tcsetattr(fd.as_fd(), OptionalActions::Flush, original);
        }
        if let Some(out) = RESTORE_OUT.get() {
            emit_restore(out.as_fd());
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
        // Every sequence in `TERMINAL_RESTORE` is idempotent, so a guard
        // dropping after `restore_now` already ran emits it a second time and
        // nothing happens twice.
        emit_restore(self.out.as_fd());
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

    /// A pty master, plus the path its slave answers to.
    ///
    /// The escape sequence is emitted towards the terminal, so proving it was
    /// emitted means reading the OTHER end: the guard writes to a slave and the
    /// test reads the master.
    fn open_pty_with_name() -> (OwnedFd, std::ffi::CString) {
        let master = open_pty();
        let name = rustix::pty::ptsname(&master, Vec::new()).expect("ptsname");
        (master, name)
    }

    fn open_slave(name: &std::ffi::CStr) -> OwnedFd {
        rustix::fs::open(
            name,
            rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
            rustix::fs::Mode::empty(),
        )
        .expect("open the pty slave")
    }

    /// Read once from `fd`, from a thread, so that a restore path which emits
    /// NOTHING fails this test rather than hanging it forever.
    fn read_with_timeout(fd: OwnedFd, what: &str) -> Vec<u8> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            let n = rustix::io::read(&fd, &mut buf[..]).unwrap_or(0);
            let _ = tx.send(buf[..n].to_vec());
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .unwrap_or_else(|_| panic!("{what}: nothing was emitted towards the terminal"))
    }

    fn show(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).replace('\x1b', "\\x1b")
    }

    /// Stream `fd` from a thread, so the reader can never block the test.
    fn read_chunks(fd: OwnedFd) -> std::sync::mpsc::Receiver<Vec<u8>> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            loop {
                match rustix::io::read(&fd, &mut buf[..]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        rx
    }

    /// Collect until the stream ENDS WITH `needle`.
    ///
    /// The helper child shares this pty with libtest, which announces itself on
    /// stdout before the test body runs, so what arrives is that chatter and
    /// then the emission. The tail is the claim being made here; the exact
    /// bytes are pinned by the normal-exit test, on a pty nothing else writes.
    fn collect_until_ends_with(
        rx: &std::sync::mpsc::Receiver<Vec<u8>>,
        needle: &[u8],
        what: &str,
    ) -> Vec<u8> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut got = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Ok(chunk) = rx.recv_timeout(std::time::Duration::from_millis(200)) {
                got.extend_from_slice(&chunk);
                if got.ends_with(needle) {
                    return got;
                }
            }
        }
        panic!(
            "{what}: the restore sequence never arrived.\nsaw:    {}\nwanted: ...{}",
            show(&got),
            show(needle)
        );
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

    /// Spawn the helper with the pty master as its stdin and a slave of the
    /// same pty as its stdout. Returns the child, a descriptor onto the pty for
    /// the parent to watch, and a second slave held open by the parent.
    ///
    /// stdout is the pty rather than `/dev/null` because the restore SEQUENCE
    /// travels that way — the renderer paints to stdout, so whatever received
    /// the alternate-screen switch is what must receive the undo. The parent
    /// keeps its own slave open so that reading the master still yields the
    /// child's last bytes after the child, and its own slave, are gone.
    fn spawn_helper(mode: &str) -> (std::process::Child, OwnedFd, OwnedFd) {
        let (master, name) = open_pty_with_name();
        let observer = master.try_clone().expect("dup for observation");
        let keepalive = open_slave(&name);
        let exe = std::env::current_exe().expect("the test binary's own path");
        let child = std::process::Command::new(exe)
            .args([HELPER_TEST, "--exact", "--ignored", "--test-threads", "1"])
            .env(HELPER_VAR, mode)
            .stdin(std::process::Stdio::from(master))
            .stdout(std::process::Stdio::from(open_slave(&name)))
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the helper child");
        (child, observer, keepalive)
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
            let (mut child, observer, _keepalive) = spawn_helper("wait");
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
        let (mut child, observer, _keepalive) = spawn_helper("dispositions");
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
        let (mut child, observer, _keepalive) = spawn_helper("panic");
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

    /// Restoring termios is not restoring the TERMINAL.
    ///
    /// A host that set the alternate screen, mouse reporting and a hidden
    /// cursor leaves all three switched on inside the emulator, where
    /// `tcsetattr` cannot reach. Only an escape sequence turns them off, and
    /// until this test existed none was ever emitted.
    #[test]
    fn a_normal_exit_emits_the_sequence_that_undoes_what_a_host_turned_on() {
        let (master, name) = open_pty_with_name();
        // Held by the test, so reading the master still yields the guard's
        // bytes after the guard has closed its own slave.
        let _keepalive = open_slave(&name);

        let guard = RawGuard::enter_on(open_slave(&name)).expect("enter raw mode");
        drop(guard);

        let got = read_with_timeout(master, "a normal exit");
        assert_eq!(
            got,
            TERMINAL_RESTORE,
            "\nemitted: {}\nwanted:  {}",
            show(&got),
            show(TERMINAL_RESTORE)
        );
    }

    /// The same bytes, from a real signal handler in a real process that then
    /// really dies — which is the case the whole constant is shaped around.
    ///
    /// `kill <pid>` runs no destructor, so `Drop` proves nothing here. Only the
    /// handler can emit, and it may use nothing but `write(2)` of bytes that
    /// already exist.
    #[test]
    fn a_terminating_signal_emits_the_restore_sequence_before_the_process_dies() {
        let (mut child, observer, keepalive) = spawn_helper("wait");
        assert!(
            wait_for_cooked(observer.as_fd(), false),
            "the helper never entered raw mode"
        );

        let stream = read_chunks(observer);
        let pid = rustix::process::Pid::from_raw(child.id() as i32).expect("a live pid");
        rustix::process::kill_process(pid, Signal::TERM).expect("signal the helper");
        child.wait().expect("wait for the helper");

        let got = collect_until_ends_with(&stream, TERMINAL_RESTORE, "a SIGTERM");
        drop(keepalive);
        assert!(got.ends_with(TERMINAL_RESTORE), "emitted: {}", show(&got));
    }

    /// The bytes themselves, checked against I8's list rather than against
    /// whatever the constant happens to say.
    ///
    /// Each entry is one thing a hostile host can leave switched on. A
    /// regression that drops one of them would still pass the two tests above,
    /// because they compare the emission with the constant and would simply
    /// agree with the smaller one.
    #[test]
    fn the_restore_sequence_covers_every_mode_i8_names() {
        for (undoes, bytes) in [
            ("the alternate screen buffer", &b"\x1b[?1049l"[..]),
            ("any-motion mouse tracking", b"\x1b[?1003l"),
            ("button-motion mouse tracking", b"\x1b[?1002l"),
            ("press/release mouse tracking", b"\x1b[?1000l"),
            ("SGR mouse encoding", b"\x1b[?1006l"),
            ("bracketed paste", b"\x1b[?2004l"),
            ("a hidden cursor", b"\x1b[?25h"),
            ("colours and attributes", b"\x1b[0m"),
        ] {
            assert!(
                TERMINAL_RESTORE.windows(bytes.len()).any(|w| w == bytes),
                "nothing in the restore sequence undoes {undoes}"
            );
        }
        // Leaving the alternate buffer must come first and the attribute reset
        // last, or both land on a screen the user is about to stop looking at.
        assert!(
            TERMINAL_RESTORE.starts_with(b"\x1b[?1049l"),
            "the alternate screen is left after other modes were restored on it"
        );
        assert!(
            TERMINAL_RESTORE.ends_with(b"\x1b[0m"),
            "the attribute reset does not land on the screen the user keeps"
        );
    }

    /// Not a test. This is the body of the child process the tests above
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

//! The pseudoterminal, and the child on the far end of it.
//!
//! # The one `unsafe` in oxutrm, and why it is here
//!
//! Making the child a session leader with the PTY as its controlling terminal
//! requires running code between `fork` and `exec`, which `std` exposes only
//! through `CommandExt::pre_exec` — an `unsafe fn`. There is no safe
//! alternative in `std`, and skipping it would give a shell with no job
//! control: no `^C`, no `^Z`, no `SIGWINCH`, which is most of what a terminal
//! is for.
//!
//! So this crate is `deny(unsafe_code)` rather than `forbid`, with exactly one
//! documented exemption below. Everything else here — turning descriptors into
//! `File`s and `Stdio`s — uses the safe `From<OwnedFd>` conversions.

use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt as _;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use rustix::termios::Winsize;

use oxutrm_proto::TermSize;

use crate::exit_wake::ExitWake;

/// How long `Pty::drop` will drain and re-check before giving up on reaping.
///
/// Generous for what it does - the child is reaped on the first turn in
/// practice - because the cost of being wrong is a zombie, while the cost of
/// having no bound at all is a host that never returns from a drop.
const REAP_BUDGET: Duration = Duration::from_secs(2);

/// A PTY with a child attached to its user side.
pub struct Pty {
    /// Our end: reads what the child wrote, writes what the user typed.
    controller: File,
    child: Child,
    exit_wake: ExitWake,
}

impl Pty {
    /// Open a PTY and start `shell` on the far side of it.
    pub fn spawn(
        shell: &str,
        args: &[String],
        env: &[(String, String)],
        size: TermSize,
    ) -> anyhow::Result<Pty> {
        let winsize = winsize_of(size);
        // `openpty` (rather than `openpty_nocloexec`) marks both descriptors
        // close-on-exec. That matters for the controller: if the child kept a
        // copy, our read side would never see EOF, because the child would be
        // holding open the very descriptor we wait on. The child's stdio is
        // unaffected, since `Stdio` dups and a dup does not inherit CLOEXEC.
        let pty =
            rustix_openpty::openpty(None, Some(&winsize)).context("opening a pseudoterminal")?;

        let user = pty.user;
        let for_child = user
            .try_clone()
            .context("duplicating the pty for the child")?;

        let mut command = Command::new(shell);
        command.args(args);
        for (k, v) in env {
            command.env(k, v);
        }
        command
            .stdin(Stdio::from(
                user.try_clone().context("duplicating the pty for stdin")?,
            ))
            .stdout(Stdio::from(
                user.try_clone().context("duplicating the pty for stdout")?,
            ))
            .stderr(Stdio::from(
                user.try_clone().context("duplicating the pty for stderr")?,
            ));

        // The single exemption. `login_tty` does setsid, TIOCSCTTY and the
        // dup onto 0/1/2 in one call; everything it touches belongs to this
        // process, and it is async-signal-safe, which is the actual
        // requirement between fork and exec.
        #[allow(unsafe_code)]
        unsafe {
            command.pre_exec(move || {
                let fd = for_child.try_clone()?;
                rustix_openpty::login_tty(fd).map_err(io::Error::other)
            });
        }

        let child = command
            .spawn()
            .with_context(|| format!("starting {shell}"))?;
        // The parent has no use for the user side, and holding it open would
        // stop our read from ever reaching EOF.
        drop(user);

        // Non-blocking, because `poll` must drain whatever is ready and
        // return - a detached session cannot afford to stall on a read.
        rustix::io::ioctl_fionbio(pty.controller.as_fd(), true)
            .context("making the pty non-blocking")?;

        let exit_wake = crate::exit_wake::watch(child.id())?;
        Ok(Pty {
            controller: File::from(pty.controller),
            child,
            exit_wake,
        })
    }

    /// Write user input to the child.
    pub fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<()> {
        self.controller
            .write_all(bytes)
            .context("writing to the pty")
    }

    /// Read whatever is ready. `Ok(0)` means nothing was.
    pub fn read_ready(&mut self, buf: &mut [u8]) -> anyhow::Result<usize> {
        match self.controller.read(buf) {
            Ok(n) => Ok(n),
            // Nothing to read right now: the ordinary case on a quiet terminal.
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(0),
            // EIO on a PTY controller means the child closed its side. That is
            // how a PTY reports EOF, not a failure.
            Err(e) if e.raw_os_error() == Some(libc_eio()) => Ok(0),
            Err(e) => Err(anyhow::Error::new(e).context("reading from the pty")),
        }
    }

    /// A descriptor that becomes readable when the child exits.
    pub fn exit_wake(&self) -> &ExitWake {
        &self.exit_wake
    }

    /// Tell the child its terminal changed size, so it redraws.
    pub fn resize(&mut self, size: TermSize) -> anyhow::Result<()> {
        rustix::termios::tcsetwinsize(self.controller.as_fd(), winsize_of(size))
            .context("resizing the pty")?;
        Ok(())
    }

    /// The child's exit code, or `None` while it is still running.
    ///
    /// Never blocks: a detached session polls this and must not stall.
    pub fn child_exited(&mut self) -> Option<i32> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(exit_code(status)),
            Ok(None) => None,
            // Gone and unwaitable. Reporting "exited" beats claiming a session
            // is alive forever when nothing is on the other end.
            Err(_) => Some(-1),
        }
    }

    /// The child's process id.
    #[cfg(test)]
    pub fn child_pid(&self) -> u32 {
        self.child.id()
    }

    /// The size the kernel currently believes the PTY is.
    ///
    /// Only the tests ask today; it is how they check that a resize actually
    /// reached the kernel rather than only the emulator.
    #[cfg(test)]
    pub fn winsize(&self) -> anyhow::Result<TermSize> {
        let ws = rustix::termios::tcgetwinsize(self.controller.as_fd())
            .context("reading the pty size")?;
        Ok(TermSize {
            cols: ws.ws_col,
            rows: ws.ws_row,
        })
    }
    /// Wait for the killed child, draining the PTY while we wait.
    ///
    /// `Child::wait` on its own **deadlocks on macOS**. A child killed while
    /// writing to a PTY that has already been read at least once stays in `E`
    /// (exiting) until its terminal output is consumed, so a parent blocked in
    /// `wait4` without reading is waiting for a process that is waiting for
    /// it. Measured with a 45-line C reproduction: draining, the child is
    /// reaped on the first turn; not draining, it is never reaped at all, at
    /// every flood duration tried. Linux does not care either way.
    ///
    /// Bounded, because dropping a session must not be able to hang the host.
    /// A child we fail to reap becomes a zombie until this process exits -
    /// bad, but finite, and SIGKILL has already been sent by the time we are
    /// here.
    fn reap(&mut self) {
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + REAP_BUDGET;
        loop {
            // The controller is non-blocking, so this stops at `WouldBlock`.
            while matches!(self.controller.read(&mut buf), Ok(n) if n > 0) {}
            match self.child.try_wait() {
                // Reaped, or unwaitable and so nobody's zombie.
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                return;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for Pty {
    /// Kill the child.
    ///
    /// `std::process::Child` deliberately does **not** do this: dropping it
    /// just stops waiting. For a shell on a PTY that is the wrong default -
    /// an abandoned session leaves a process holding a descriptor nobody
    /// reads, forever. A test that spawns `yes` and returns would leak a core
    /// until the machine is rebooted.
    ///
    /// SIGHUP first, because that is what a hangup on a real terminal sends
    /// and a shell knows how to clean up after it; SIGKILL only if it is still
    /// there. `try_wait` is then required, or the child becomes a zombie.
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_some() {
            return;
        }
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32);
        if let Some(pid) = pid {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::HUP);
            std::thread::sleep(std::time::Duration::from_millis(20));
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
        self.reap();
    }
}

fn libc_eio() -> i32 {
    rustix::io::Errno::IO.raw_os_error()
}

fn winsize_of(size: TermSize) -> Winsize {
    Winsize {
        ws_row: size.rows,
        ws_col: size.cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// A child's exit status as one number.
///
/// A shell killed by a signal has no exit code. 128+n is what every shell
/// reports, so the number matches what `echo $?` would have said.
pub(crate) fn exit_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn size() -> TermSize {
        TermSize { cols: 40, rows: 10 }
    }

    /// Read until `needle` appears, or give up. The child is a real process,
    /// so this waits on it rather than assuming it has already run.
    fn read_until(pty: &mut Pty, needle: &[u8], budget: Duration) -> Vec<u8> {
        let deadline = Instant::now() + budget;
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        while Instant::now() < deadline {
            let n = pty.read_ready(&mut buf).expect("read");
            if n == 0 {
                std::thread::sleep(Duration::from_millis(5));
                continue;
            }
            out.extend_from_slice(&buf[..n]);
            if out.windows(needle.len()).any(|w| w == needle) {
                break;
            }
        }
        out
    }

    /// Is `fd` readable within `budget`? The wake's whole contract is "this
    /// descriptor becomes readable", so the tests ask the kernel that
    /// question directly rather than through an async runtime.
    fn readable_within(wake: &ExitWake, budget: Duration) -> bool {
        let Some(fd) = wake.as_fd() else {
            return false;
        };
        let deadline = Instant::now() + budget;
        loop {
            let mut fds = [rustix::event::PollFd::new(
                &fd,
                rustix::event::PollFlags::IN,
            )];
            let timeout = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 20_000_000,
            };
            if rustix::event::poll(&mut fds, Some(&timeout)).expect("poll") > 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
        }
    }

    fn sh(script: &str) -> Pty {
        Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), script.to_owned()],
            &[],
            size(),
        )
        .expect("spawn")
    }

    #[test]
    fn a_running_child_offers_a_wake_that_stays_quiet() {
        let mut pty = sh("sleep 5");
        assert!(
            pty.exit_wake().as_fd().is_some(),
            "a live child must be watchable on both platforms"
        );
        assert!(
            !readable_within(pty.exit_wake(), Duration::from_millis(300)),
            "the wake fired while the child was still running"
        );
        assert_eq!(pty.child_exited(), None, "try_wait agrees it is alive");
    }

    #[test]
    fn the_wake_becomes_readable_when_the_child_exits() {
        let mut pty = sh("sleep 0.3; exit 7");
        assert!(
            readable_within(pty.exit_wake(), Duration::from_secs(10)),
            "the wake never fired for a child that exited"
        );
        // Readability is only the hint; `child_exited` is the authority.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(code) = pty.child_exited() {
                assert_eq!(code, 7);
                break;
            }
            assert!(Instant::now() < deadline, "woke but the child never reaped");
        }
    }

    /// The registration races the child: it can be gone before we watch it.
    /// Asserted on the OUTCOME, because the two platforms answer differently
    /// and neither answer is wrong.
    #[test]
    fn a_child_that_exits_before_it_is_watched_is_still_reported() {
        let mut pty = sh("exit 5");
        std::thread::sleep(Duration::from_millis(500));
        let wake = crate::exit_wake::watch(pty.child_pid()).expect("watching must not fail");
        assert!(
            wake.as_fd().is_none() || readable_within(&wake, Duration::from_secs(5)),
            "an armed wake for an already-exited child must be readable at once"
        );
        assert_eq!(
            pty.child_exited(),
            Some(5),
            "whatever the wake says, try_wait is the authority"
        );
    }

    /// The reason this exists at all rather than watching the PTY for EOF.
    /// A child that closes its terminal and keeps running gives EOF on the
    /// controller on Linux - permanently - while it is still very much alive.
    #[test]
    fn a_child_that_closes_its_terminal_is_not_reported_as_exited() {
        let mut pty = sh("exec 0<&- 1>&- 2>&-; sleep 3; exit 9");
        // Without this the test would pass against a wake that is never armed
        // at all, which is not what it claims to check.
        assert!(
            pty.exit_wake().as_fd().is_some(),
            "the child must be watched"
        );
        std::thread::sleep(Duration::from_millis(500));
        // Drain whatever the controller has to say; on Linux this is EOF.
        let mut buf = [0u8; 4096];
        while matches!(pty.read_ready(&mut buf), Ok(n) if n > 0) {}
        assert_eq!(pty.child_exited(), None, "the child is still running");
        assert!(
            !readable_within(pty.exit_wake(), Duration::from_millis(400)),
            "the wake fired for a child that had only closed its terminal"
        );
    }

    #[test]
    fn a_child_runs_and_its_output_comes_back() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "printf oxutrm-marker".to_owned()],
            &[],
            size(),
        )
        .expect("spawn");
        let out = read_until(&mut pty, b"oxutrm-marker", Duration::from_secs(10));
        assert!(
            out.windows(13).any(|w| w == b"oxutrm-marker"),
            "got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn the_pty_starts_at_the_size_it_was_given() {
        let pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "exit 0".to_owned()],
            &[],
            size(),
        )
        .expect("spawn");
        assert_eq!(pty.winsize().expect("winsize"), size());
    }

    #[test]
    fn a_resize_reaches_the_kernel() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "sleep 5".to_owned()],
            &[],
            size(),
        )
        .expect("spawn");
        let bigger = TermSize {
            cols: 132,
            rows: 43,
        };
        pty.resize(bigger).expect("resize");
        assert_eq!(pty.winsize().expect("winsize"), bigger);
    }

    #[test]
    fn the_child_sees_the_environment_it_was_given() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "printf %s \"$OXUTRM_TEST\"".to_owned()],
            &[("OXUTRM_TEST".to_owned(), "hello".to_owned())],
            size(),
        )
        .expect("spawn");
        let out = read_until(&mut pty, b"hello", Duration::from_secs(10));
        assert!(
            out.windows(5).any(|w| w == b"hello"),
            "got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn reading_a_quiet_pty_returns_zero_rather_than_blocking() {
        // The whole session model depends on this: a detached host polls and
        // must never stall, even when nothing has happened for a week.
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "sleep 5".to_owned()],
            &[],
            size(),
        )
        .expect("spawn");
        let started = Instant::now();
        let mut buf = [0u8; 64];
        let n = pty.read_ready(&mut buf).expect("read");
        assert_eq!(n, 0);
        assert!(started.elapsed() < Duration::from_secs(1), "read blocked");
    }

    #[test]
    fn an_exit_code_comes_back_once_the_child_is_done() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "exit 3".to_owned()],
            &[],
            size(),
        )
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(code) = pty.child_exited() {
                assert_eq!(code, 3);
                break;
            }
            assert!(Instant::now() < deadline, "the child never exited");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_running_child_reports_no_exit_code() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &["-c".to_owned(), "sleep 5".to_owned()],
            &[],
            size(),
        )
        .expect("spawn");
        assert_eq!(pty.child_exited(), None);
    }

    #[test]
    fn input_reaches_the_child() {
        let mut pty = Pty::spawn(
            "/bin/sh",
            &[
                "-c".to_owned(),
                "read line; printf 'got:%s' \"$line\"".to_owned(),
            ],
            &[],
            size(),
        )
        .expect("spawn");
        pty.write_input(b"ping\n").expect("write");
        let out = read_until(&mut pty, b"got:ping", Duration::from_secs(10));
        assert!(
            out.windows(8).any(|w| w == b"got:ping"),
            "got {:?}",
            String::from_utf8_lossy(&out)
        );
    }

    #[test]
    fn spawning_something_that_does_not_exist_is_an_error_not_a_panic() {
        let e = Pty::spawn("/nonexistent/oxutrm-test-shell", &[], &[], size());
        assert!(e.is_err());
    }
}

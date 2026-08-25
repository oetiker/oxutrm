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

use anyhow::Context as _;
use rustix::termios::Winsize;

use oxutrm_proto::TermSize;

/// A PTY with a child attached to its user side.
pub struct Pty {
    /// Our end: reads what the child wrote, writes what the user typed.
    controller: File,
    child: Child,
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

        Ok(Pty {
            controller: File::from(pty.controller),
            child,
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

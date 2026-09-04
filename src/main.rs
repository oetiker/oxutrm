#![forbid(unsafe_code)]

//! One binary, three roles — exactly one thing to install per machine.
//!
//! | Invocation                          | Runs where | Job                                        |
//! |-------------------------------------|------------|--------------------------------------------|
//! | `oxutrm <ssh-target>`               | local      | drives SSH, then becomes the client        |
//! | `oxutrm host --serve`               | remote     | owns the PTY and the authoritative screen  |
//! | `oxutrm host --list` / `--attach`   | remote     | session registry queries                   |
//! | `oxutrm loopback`                   | local      | both halves in one process, no network     |
//!
//! oxutrm never parses `~/.ssh/config`. It shells out to `ssh` and assumes the
//! user has already made `ssh <target>` work, by whatever means.

// `session` holds both loops. `HostSession` is live from here -- `host --serve`
// is wired -- but `ClientSession` and everything that feeds it has no caller
// until `run_connect` lands (L1-L14 in `docs/cli-wiring-design.md`), so the
// allow stays on this module and this module only. Everywhere else the linter
// is watching: `accept`, `ladder` and `link` carry item-level allows naming
// exactly which items belong to the client half, and when `run_connect` lands
// every one of them must come off. An allow that cannot be removed then is a
// piece of code with no caller on either side, which is worth knowing.
mod accept;
mod attach_exchange;
mod candidates;
mod connect;
mod ladder;
mod link;
mod linkstate;
mod listener;
mod loopback;
mod roam;
mod serve;
mod session;

use std::io::{Read as _, Write as _};
use std::os::fd::BorrowedFd;

use anyhow::{Context as _, Result};

use oxutrm_client::{RawGuard, terminal_size};
use oxutrm_term::detect_caps;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    dispatch(&args)
}

fn dispatch(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") | Some("help") => {
            print!("{USAGE}");
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("oxutrm {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("host") => run_host(&args[1..]),
        Some("loopback") => run_loopback(&args[1..]),
        Some(other) if other.starts_with('-') => {
            eprintln!("oxutrm: unknown option {other:?}\nTry `oxutrm --help`.");
            std::process::exit(2);
        }
        // Anything else is an ssh target: connect, or reattach.
        Some(_) => connect::run_connect(args),
    }
}

/// The remote half, spawned over SSH. Not normally typed by hand.
fn run_host(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            print!("{HOST_USAGE}");
            Ok(())
        }
        // Works today: it needs the registry and nothing else.
        Some("--list") => run_host_list(),
        Some("--serve") => serve::run_host_serve(),
        Some("--attach") => match args.get(1) {
            Some(id) => run_host_attach(id),
            None => Err(anyhow::anyhow!(
                "`oxutrm host --attach` needs a session id. \
                 `oxutrm host --list` shows them."
            )),
        },
        Some(other) => {
            eprintln!("oxutrm host: unknown option {other:?}\nTry `oxutrm host --help`.");
            std::process::exit(2);
        }
        None => {
            eprintln!(
                "oxutrm host: needs one of --serve, --list or --attach <id>.\nTry `oxutrm host --help`."
            );
            std::process::exit(2);
        }
    }
}

/// `oxutrm host --list`: what is running on this machine.
///
/// Prints the registry's own warning first when it had to fall back out of
/// `$XDG_RUNTIME_DIR`, because a user wondering why a session vanished at
/// logout needs that sentence more than they need the list.
fn run_host_list() -> Result<()> {
    let root = oxutrm_host::resolve_registry_root()
        .context("deciding where oxutrm records its sessions")?;
    if let Some(warning) = &root.warning {
        eprintln!("{warning}");
    }

    let sessions = oxutrm_host::Registry::list().context("reading the session registry")?;
    print!("{}", oxutrm_host::attach::format_session_list(&sessions));
    Ok(())
}

/// `oxutrm host --attach <id>`: relay a second client into a running session.
///
/// `connect_to_session` checks the registry before touching the socket, so
/// "no such session" and "the session is dead" are told apart; `relay_signals`
/// decodes and re-encodes rather than copying bytes, so garbage typed at this
/// terminal cannot be relayed into a running session. Both already existed and
/// were already tested — this only wires them to stdin and stdout.
fn run_host_attach(id: &str) -> Result<()> {
    let root = oxutrm_host::resolve_registry_root()
        .context("deciding where oxutrm records its sessions")?;
    // Printed here for the same reason `--serve` and `--list` print it: on a
    // fallback root, "no session <id> here" is the message the user gets, and
    // without this line there is nothing to explain why the id they were just
    // shown is not there.
    if let Some(warning) = &root.warning {
        eprintln!("{warning}");
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("building the runtime")?;
    let outcome = runtime.block_on(async {
        let stream =
            oxutrm_host::attach::connect_to_session(&oxutrm_host::Registry::dir_at(&root.base), id)
                .await?;
        let (sr, mut sw) = stream.into_split();
        let mut sr = tokio::io::BufReader::new(sr);
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
        let mut stdout = tokio::io::stdout();

        // Both directions, concurrently, until either side finishes.
        tokio::select! {
            r = oxutrm_host::attach::relay_signals(&mut stdin, &mut sw) => { r?; }
            r = oxutrm_host::attach::relay_signals(&mut sr, &mut stdout) => { r?; }
        }
        anyhow::Ok(())
    });

    // Do not wait for whichever `relay_signals` branch `tokio::select!` just
    // cancelled. The workspace's `io-std` feature backs BOTH `stdin()` and
    // `stdout()` with the blocking pool, so this is not only the
    // stdin-can't-be-cancelled case `serve.rs`'s R3 comment documents for
    // ssh's pipe: if the stdin-to-socket branch wins instead, the socket-to-
    // stdout branch can be cancelled mid-write, and unlike an implicit drop —
    // which would block until that write's blocking-pool task finishes —
    // `shutdown_background()` does not wait for it either. So what can be
    // lost here, unlike in `serve.rs`, is a write of content the user is
    // trying to see: the tail of the host's most recent screen update,
    // mid-flight to this terminal at the moment of detach. Two things keep
    // that cheap: design spec §8.5 guarantees the next attach's first
    // datagram is a full state, so this heals on reconnect rather than
    // corrupting anything lasting; and `read_signal_async` (blocking on new
    // data) dominates wall-clock time over `write_signal_async` (a fast local
    // write), so the loser of the select is usually parked in a read, not a
    // write, same as `serve.rs`. Shutting down in the background is still the
    // right call: the hang it avoids — the process never returning once the
    // session end hangs up, until the terminal produces a byte — is real and
    // certain, where the write it might drop is rare and self-healing.
    runtime.shutdown_background();
    outcome
}

/// Both halves in one process over a channel, with no network in between.
/// This is the milestone-1 deliverable and stays useful afterwards as the
/// fastest way to exercise the terminal core.
///
/// The frames are real: every screen and every keystroke is encoded to bytes,
/// decoded again, and applied through a `Receiver`. See `src/loopback.rs`.
fn run_loopback(args: &[String]) -> Result<()> {
    // Arguments first, and `--help` before anything that needs a terminal:
    // `oxutrm loopback --help | less` and a CI log must both work, and a help
    // text that only prints on a tty is a help text nobody can read when they
    // most need it.
    match args.first().map(String::as_str) {
        Some("-h") | Some("--help") => {
            print!("{LOOPBACK_USAGE}");
            return Ok(());
        }
        Some(other) if other.starts_with('-') => {
            eprintln!("oxutrm loopback: unknown option {other:?}\nTry `oxutrm loopback --help`.");
            std::process::exit(2);
        }
        _ => {}
    }

    let shell = match args.first() {
        Some(s) => s.clone(),
        None => std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
    };

    let size = terminal_size().context("oxutrm loopback needs a real terminal")?;
    let caps = detect_caps();

    // Raw mode for the whole session. The guard restores the terminal on every
    // exit path there is - normal return, `?`, panic and signal - because a
    // terminal left in raw mode with the alternate screen on is a terminal the
    // user has to blindly type `reset` into.
    let _raw = RawGuard::enter().context("putting the terminal into raw mode")?;

    let mut session = loopback::Loopback::new(&shell, &[], &[], size, 10_000, caps)?;

    let mut keyboard = Keyboard::open(rustix::stdio::stdin())?;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];

    let code = loop {
        // The window size is polled rather than driven by SIGWINCH. A handler
        // would need `unsafe` and an async-signal-safe body for something an
        // ioctl answers exactly, 125 times a second, for nothing.
        if let Ok(now) = terminal_size() {
            session.resize(now)?;
        }

        let n = keyboard.read(&mut buf).context("reading the keyboard")?;

        let tick = session.tick(&buf[..n], &mut stdout)?;
        if let Some(code) = tick.exited {
            break code;
        }
        std::thread::sleep(loopback::TICK);
    };

    // Drop the guard before writing anything a human should read.
    drop(_raw);
    let _ = writeln!(stdout, "\r\noxutrm: the shell exited ({code}).");
    std::process::exit(code);
}

/// The controlling terminal, reachable by name from any process that has one.
const CONTROLLING_TERMINAL: &str = "/dev/tty";

/// The keyboard, read without blocking and **without touching the descriptor
/// the user gave us**.
///
/// `O_NONBLOCK` is a property of the open FILE DESCRIPTION, not of the
/// descriptor that names it. The description behind fd 0 was created by
/// whoever started us — interactively, the user's shell — and `dup` shares it
/// rather than copying it. So the obvious implementation, `ioctl_fionbio(fd 0,
/// true)`, does not configure oxutrm's keyboard: it reconfigures the SHELL's
/// standard input, for the rest of that shell's life. The next program to read
/// a line gets a spurious `EAGAIN` and reports an error on an empty prompt.
///
/// Restoring it afterwards is not good enough and the difference is not
/// theoretical. The change would have to be undone on every exit path, and
/// `kill -9` is an exit path no guard can reach, so a `SIGKILL`ed session would
/// still leave the shell broken. The state is therefore never created.
///
/// Instead the terminal is opened **again**. `/dev/tty` is the same device and
/// the same input queue, but `open` mints a NEW file description, so the
/// `O_NONBLOCK` on it is ours alone and dies with the process. No keystroke is
/// lost, and there is nothing to restore anywhere.
struct Keyboard {
    file: std::fs::File,
    /// True when `file` is our own description and carries our own
    /// `O_NONBLOCK`. False on the fallback below, where it merely duplicates
    /// the caller's descriptor and therefore SHARES a description we must not
    /// modify — see [`Keyboard::read`].
    private: bool,
}

impl Keyboard {
    fn open(user: BorrowedFd<'_>) -> Result<Keyboard> {
        Keyboard::open_via(CONTROLLING_TERMINAL, user)
    }

    /// `tty` is a parameter only so the tests can name a pty of their own.
    /// `/dev/tty` inside a test binary means the developer's real terminal,
    /// which a test has no business opening, let alone reading.
    fn open_via<P: rustix::path::Arg>(tty: P, user: BorrowedFd<'_>) -> Result<Keyboard> {
        use rustix::fs::{Mode, OFlags};

        match rustix::fs::open(
            tty,
            OFlags::RDONLY | OFlags::NOCTTY | OFlags::NONBLOCK,
            Mode::empty(),
        ) {
            Ok(fd) => Ok(Keyboard {
                file: fd.into(),
                private: true,
            }),
            // `RawGuard::enter` has already established that fd 0 is a
            // terminal, so reaching here means there is no CONTROLLING
            // terminal to open by name — a session started by a supervisor
            // that handed us a pty without making it our own. Falling back to
            // a duplicate is correct; making that duplicate non-blocking would
            // not be, because it shares the description we are protecting.
            Err(_) => Ok(Keyboard {
                file: user
                    .try_clone_to_owned()
                    .context("duplicate the keyboard")?
                    .into(),
                private: false,
            }),
        }
    }

    /// Whatever has been typed, or nothing at all. Never blocks.
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // On the fallback the description belongs to the caller, so instead of
        // setting a flag on it we ask whether a read would block. `poll` with
        // a zero timeout answers that and changes no state anywhere — the
        // point of the whole exercise.
        if !self.private {
            let mut fds = [rustix::event::PollFd::new(
                &self.file,
                rustix::event::PollFlags::IN,
            )];
            let ready = rustix::event::poll(&mut fds, Some(&IMMEDIATELY))?;
            if ready == 0 {
                return Ok(0);
            }
        }
        match self.file.read(buf) {
            Ok(n) => Ok(n),
            // The private path's own `O_NONBLOCK`, which is the normal case on
            // a quiet terminal.
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(0),
            Err(e) => Err(e),
        }
    }
}

/// A zero `poll` timeout: answer now, wait for nothing.
const IMMEDIATELY: rustix::event::Timespec = rustix::event::Timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

const HOST_USAGE: &str = "\
oxutrm host — the remote half of a session. Normally spawned over ssh rather
than typed by hand.

USAGE
  oxutrm host --list          Sessions on this machine, oldest first.
  oxutrm host --serve         Create a session and hand it to a client.
  oxutrm host --attach <id>   Relay a new attach into a running session.

--serve creates the session and hands it to the first client, which connects,
paints and carries the shell's exit code back. --attach relays a second
client's signalling into that same running session.

A session that reached the far end over an ssh tunnel (rung 4) is listed as
NOT detachable: it carries its data inside that ssh connection, so it dies
with it and cannot be reattached.
";

const LOOPBACK_USAGE: &str = "\
oxutrm loopback — run both halves in one process, with no network in between.

Everything a real session does happens here: a shell on a pty, the terminal
emulator, the state diff, a Frame encoded to bytes and decoded again, and the
renderer. Only the transport is missing.

USAGE
  oxutrm loopback [shell]
      Run <shell>, or $SHELL, or /bin/sh.

OPTIONS
  -h, --help        Show this help.
";

const USAGE: &str = "\
oxutrm — a remote terminal that survives bad networks, changing IP addresses
and NAT on both ends.

USAGE
  oxutrm <ssh-target> [command ...]
      Start a session there and connect to it. Each connection starts a
      fresh session; reattaching to an existing one is not implemented.

  oxutrm host --serve
      Run the remote half. Spawned over SSH; not normally typed by hand.

  oxutrm host --list
      List sessions on this machine, pruning any whose process is gone.

  oxutrm host --attach <session-id>
      Relay a new client into a running session.

  oxutrm loopback
      Run both halves in one process, with no network in between.

OPTIONS
  -h, --help        Show this help.
      --version     Show the version.
";

#[cfg(test)]
mod tests {
    use super::*;

    /// The help text and the code must agree about what works.
    ///
    /// Both help strings spent this project's whole life so far saying the
    /// local half "is still being wired" — true when written, false since the
    /// first two-machine run, and the first thing a new user reads. This
    /// pins the pair together so a feature landing cannot quietly leave the
    /// documentation behind.
    ///
    /// `--attach` was the same story: both usage strings called it
    /// unimplemented, and the day it worked (this commit) that half of the
    /// test was deleted rather than left asserting a now-false claim — see
    /// `attach_without_an_id_says_where_to_find_one` for the no-id usage
    /// error, `crates/oxutrm-host/tests/attach.rs` for `connect_to_session`
    /// and `relay_signals` themselves, and `tests/host_attach.rs` for
    /// `run_host_attach`'s own wiring between the two.
    #[test]
    fn the_help_text_agrees_with_what_is_actually_implemented() {
        for (name, text) in [("USAGE", USAGE), ("HOST_USAGE", HOST_USAGE)] {
            assert!(
                !text.contains("still being wired"),
                "{name} still claims the local half is unwired; it connects, \
                 paints and carries an exit code"
            );
            assert!(
                !text.contains("nobody to paint for"),
                "{name} still claims a served session has nobody to paint for"
            );
            assert!(
                !text.contains("does not work yet") && !text.contains("NOT IMPLEMENTED"),
                "{name} still claims --attach is unimplemented, which is no \
                 longer true"
            );
            // Reattach via a bare `oxutrm <ssh-target>` is a *different*,
            // still-unimplemented thing from `host --attach <id>` — see the
            // top line of USAGE, which still says so on purpose.
            assert!(
                !text.contains("or reattach to a session already running"),
                "{name} promises reattach, which is the one thing that does \
                 not work; a second connection starts a fresh session"
            );
        }
    }

    #[test]
    fn help_is_the_default_and_names_every_subcommand() {
        for needle in [
            "oxutrm <ssh-target>",
            "oxutrm host --serve",
            "oxutrm host --list",
            "oxutrm host --attach",
            "oxutrm loopback",
        ] {
            assert!(USAGE.contains(needle), "usage is missing {needle:?}");
        }
    }

    #[test]
    fn no_arguments_and_help_take_the_same_branch() {
        assert!(dispatch(&[]).is_ok());
        assert!(dispatch(&["--help".to_string()]).is_ok());
        assert!(dispatch(&["-h".to_string()]).is_ok());
        assert!(dispatch(&["help".to_string()]).is_ok());
    }

    /// `--attach` with no id is a usage error that points at `--list`, not a
    /// panic on `args[1]`.
    ///
    /// Checking only for `"--list"` is not enough to prove this: the old
    /// blanket "not wired up yet" error already mentioned `--list` in passing
    /// (to say it *does* work), so that assertion alone passes whether or not
    /// the no-id case is actually handled. `"needs a session id"` is the part
    /// that only the real usage error says.
    #[test]
    fn attach_without_an_id_says_where_to_find_one() {
        let err = run_host(&["--attach".to_owned()]).expect_err("no id given");
        let text = err.to_string();
        assert!(
            text.contains("needs a session id"),
            "the error does not say an id is required: {text}"
        );
        assert!(
            text.contains("--list"),
            "the error does not tell the user how to find a session id: {text}"
        );
    }

    #[test]
    fn loopback_help_works_without_a_terminal() {
        // It used to ask the terminal for its size BEFORE parsing arguments,
        // so `oxutrm loopback --help` failed in a pipe, in CI, and anywhere
        // else someone would actually be reading it.
        assert!(LOOPBACK_USAGE.contains("oxutrm loopback [shell]"));
        assert!(run_loopback(&["--help".to_string()]).is_ok());
        assert!(run_loopback(&["-h".to_string()]).is_ok());
    }

    #[test]
    fn the_version_flag_is_accepted() {
        assert!(dispatch(&["--version".to_string()]).is_ok());
    }

    // ---- the keyboard, and the user's shell it must not damage --------------
    //
    // `O_NONBLOCK` is a property of the open FILE DESCRIPTION, not of the
    // descriptor. The description behind fd 0 belongs to whatever started us -
    // interactively, the user's shell - and `dup` does not copy it, it shares
    // it. So setting the flag on fd 0 is a change to the SHELL's stdin that
    // outlives oxutrm, survives every exit path including `kill -9`, and makes
    // the next program that reads a line see a spurious EAGAIN.
    //
    // These tests turn that sharing into the instrument: a `dup` of the
    // descriptor handed to `Keyboard::open` sees exactly what the user's shell
    // would see. A pty stands in for the terminal, and the path `open_via` is
    // told to open is a parameter, because `/dev/tty` in a test binary means
    // the developer's own terminal and nothing else.

    use std::ffi::CString;
    use std::os::fd::{AsFd as _, OwnedFd};

    /// A pty master, plus the path of its slave.
    fn open_pty() -> (OwnedFd, CString) {
        use rustix::pty::OpenptFlags;
        let master =
            rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).expect("open a pty");
        rustix::pty::grantpt(&master).expect("grantpt");
        rustix::pty::unlockpt(&master).expect("unlockpt");
        let name = rustix::pty::ptsname(&master, Vec::new()).expect("ptsname");
        (master, name)
    }

    fn flags(fd: BorrowedFd<'_>) -> rustix::fs::OFlags {
        rustix::fs::fcntl_getfl(fd).expect("fcntl F_GETFL")
    }

    /// The whole bug in one assertion, on the path that is meant to work.
    #[test]
    fn opening_the_keyboard_never_touches_the_users_file_description() {
        let (user, slave) = open_pty();
        // A duplicate SHARES the description, which is why the damage escaped
        // the process in the first place - and is what lets this observe it.
        let shell_would_see = user.try_clone().expect("dup for observation");
        let before = flags(shell_would_see.as_fd());

        let keyboard = Keyboard::open_via(slave.as_c_str(), user.as_fd())
            .expect("open the keyboard on the pty");
        assert_eq!(
            flags(shell_would_see.as_fd()),
            before,
            "opening the keyboard changed the user's own stdin"
        );
        assert!(
            keyboard.private,
            "a terminal that opened must give a PRIVATE description"
        );

        drop(keyboard);
        assert_eq!(
            flags(shell_would_see.as_fd()),
            before,
            "the user's stdin was left changed after oxutrm let go of it"
        );
    }

    /// The same promise on the fallback, where there is no private description
    /// to be had. It is the harder half: a `dup` shares the caller's
    /// description, so the fallback must reach non-blocking behaviour WITHOUT
    /// setting a flag - otherwise the fix holds only where `/dev/tty` opens,
    /// and the one place it does not is a supervised session, where a broken
    /// shell is hardest to notice.
    #[test]
    fn the_fallback_keyboard_does_not_touch_it_either() {
        let (user, _slave) = open_pty();
        let shell_would_see = user.try_clone().expect("dup for observation");
        let before = flags(shell_would_see.as_fd());

        let keyboard = Keyboard::open_via(c"/nonexistent/dev/tty", user.as_fd())
            .expect("the fallback must not fail");
        assert!(!keyboard.private, "this must be the fallback path");
        assert_eq!(
            flags(shell_would_see.as_fd()),
            before,
            "the fallback changed the user's own stdin"
        );

        drop(keyboard);
        assert_eq!(flags(shell_would_see.as_fd()), before);
    }

    /// Deleting the non-blocking read would pass both tests above and hang the
    /// session on the first quiet tick, so it is asserted separately: an idle
    /// keyboard reports nothing typed, promptly, on BOTH paths.
    #[test]
    fn an_idle_keyboard_reports_nothing_typed_rather_than_blocking() {
        for private in [true, false] {
            let (master, slave) = open_pty();
            // Keep the slave open, or reading the master is EIO rather than
            // "nothing to read".
            let _slave_open = rustix::fs::open(
                slave.as_c_str(),
                rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY,
                rustix::fs::Mode::empty(),
            )
            .expect("open the pty slave");

            let mut keyboard = if private {
                Keyboard::open_via(slave.as_c_str(), master.as_fd()).expect("open on the slave")
            } else {
                Keyboard::open_via(c"/nonexistent/dev/tty", master.as_fd()).expect("fallback")
            };
            assert_eq!(keyboard.private, private, "wrong path under test");

            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let mut buf = [0u8; 64];
                let _ = tx.send(keyboard.read(&mut buf));
            });
            let got = rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or_else(|_| {
                    panic!("private={private}: reading an idle keyboard blocked the session")
                });
            assert_eq!(
                got.expect("reading an idle keyboard is not an error"),
                0,
                "private={private}"
            );
        }
    }

    /// The call site, machine-checked.
    ///
    /// The tests above prove the mechanism; this one proves `run_loopback`
    /// still uses it. Nothing in this file may make standard input
    /// non-blocking again - the whole defect was one such call, and it read as
    /// obviously correct for as long as it shipped.
    #[test]
    fn nothing_in_this_binary_changes_the_flags_on_standard_input() {
        const SOURCE: &str = include_str!("main.rs");
        // Comments are excluded so that the prose above is free to name the
        // call it is warning about; the needles are spelled in halves so this
        // list is not itself a match.
        let code: String = SOURCE
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        for forbidden in [
            concat!("ioctl_", "fionbio"),
            concat!("fcntl_", "setfl"),
            concat!("set_", "nonblocking"),
        ] {
            assert!(
                !code.contains(forbidden),
                "`{forbidden}` is back in src/main.rs. Anything that sets a file \
                 status flag here lands on the description behind fd 0, which \
                 belongs to the user's shell and outlives this process."
            );
        }
        assert!(
            code.contains("Keyboard::open(rustix::stdio::stdin())"),
            "run_loopback no longer takes the keyboard through `Keyboard`"
        );
    }
}

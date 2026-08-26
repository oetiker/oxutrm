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

// M4's session loops and the QUIC framing under them. Nothing in `main`
// reaches them yet: `oxutrm host --serve` and the connect path are M3's, and
// they are what will call `HostSession` and `ClientSession`. The allow is
// temporary and should come off with that wiring - it is here rather than a
// fabricated call site because inventing a use to satisfy the linter hides
// exactly the fact worth knowing, which is that this code has no caller yet.
// `accept` carries the same caveat and one more: it is the host's whole accept
// path, built with its hardening rather than hardened later, because there is
// no accept path to add it to afterwards. Its caller is `run_host --serve`,
// which is the next piece of wiring and does not exist yet.
#[allow(dead_code)]
mod accept;
#[allow(dead_code)]
mod link;
mod loopback;
#[allow(dead_code)]
mod session;

use std::io::{Read as _, Write as _};

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
        Some(_) => run_connect(args),
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
        Some("--serve") => Err(anyhow::anyhow!(
            "`oxutrm host --serve` is not wired up yet. The pieces exist and are \
             tested -- the ssh handshake, the registry, daemonize, the ladder -- \
             but the session loop they feed is still being fixed, so serving \
             would start a session that never paints. Use `oxutrm loopback` to \
             exercise the terminal core in the meantime."
        )),
        Some("--attach") => Err(anyhow::anyhow!(
            "`oxutrm host --attach` is not wired up yet, for the same reason as \
             --serve: there is no live session to attach to until serving works."
        )),
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

    let mut stdin = std::io::stdin();
    set_nonblocking(rustix::stdio::stdin())?;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 8192];

    let code = loop {
        // The window size is polled rather than driven by SIGWINCH. A handler
        // would need `unsafe` and an async-signal-safe body for something an
        // ioctl answers exactly, 125 times a second, for nothing.
        if let Ok(now) = terminal_size() {
            session.resize(now)?;
        }

        let n = match stdin.read(&mut buf) {
            Ok(0) => 0,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(anyhow::Error::new(e).context("reading the keyboard")),
        };

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

fn set_nonblocking(fd: rustix::fd::BorrowedFd<'_>) -> Result<()> {
    rustix::io::ioctl_fionbio(fd, true).context("making the keyboard non-blocking")?;
    Ok(())
}

/// The default path: drive `ssh` to start or find a session on the far end,
/// exchange candidates over that channel, bring up QUIC, then become the
/// client. Connect and reattach are deliberately one code path.
fn run_connect(args: &[String]) -> Result<()> {
    let target = args.first().map_or("<ssh-target>", String::as_str);
    Err(anyhow::anyhow!(
        "connecting to {target} is not wired up yet. Every piece it needs \
         exists and is tested on its own -- driving ssh, the signalling \
         handshake and its failure messages, the connection ladder, \
         detachability settled from the nominated rung, the registry and \
         daemonize -- but the session loop that would carry the screen is \
         still being fixed, so connecting would attach you to a terminal that \
         never paints.\n\nRun `oxutrm loopback` to use the terminal core \
         today, or `oxutrm host --list` to see what this machine is tracking."
    ))
}

const HOST_USAGE: &str = "\
oxutrm host — the remote half of a session. Normally spawned over ssh rather
than typed by hand.

USAGE
  oxutrm host --list          Sessions on this machine, oldest first.
  oxutrm host --serve         Create a session and hand it to a client.
  oxutrm host --attach <id>   Relay a new attach into a running session.

Only --list works today; the others say why when you run them.

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
      Connect, or reattach to a session already running there.

  oxutrm host --serve
      Run the remote half. Spawned over SSH; not normally typed by hand.

  oxutrm host --list
      List sessions on this machine, pruning any whose process is gone.

  oxutrm host --attach <session-id>
      Reattach to a running session.

  oxutrm loopback
      Run both halves in one process, with no network in between.

OPTIONS
  -h, --help        Show this help.
      --version     Show the version.
";

#[cfg(test)]
mod tests {
    use super::*;

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
}

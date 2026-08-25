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

mod loopback;

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
fn run_host(_args: &[String]) -> Result<()> {
    unimplemented!("oxutrm host: implemented in M3")
}

/// Both halves in one process over a channel, with no network in between.
/// This is the milestone-1 deliverable and stays useful afterwards as the
/// fastest way to exercise the terminal core.
///
/// The frames are real: every screen and every keystroke is encoded to bytes,
/// decoded again, and applied through a `Receiver`. See `src/loopback.rs`.
fn run_loopback(args: &[String]) -> Result<()> {
    let shell = match args.first() {
        Some(s) if !s.starts_with('-') => s.clone(),
        _ => std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
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
fn run_connect(_args: &[String]) -> Result<()> {
    unimplemented!("oxutrm <ssh-target>: implemented in M3 and M4")
}

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
    fn the_version_flag_is_accepted() {
        assert!(dispatch(&["--version".to_string()]).is_ok());
    }
}

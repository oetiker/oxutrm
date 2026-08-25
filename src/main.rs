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

use anyhow::Result;

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
fn run_loopback(_args: &[String]) -> Result<()> {
    unimplemented!("oxutrm loopback: implemented in M1")
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

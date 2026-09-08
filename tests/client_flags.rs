//! `oxutrm [--attach <id>] [--new] <target>`: the flags, through the real
//! binary.
//!
//! `run_connect` (`src/connect.rs`) parses these before ever touching ssh, so
//! a usage mistake here must be reported and exited before any network
//! activity starts. That is exactly what makes it reachable without an ssh
//! target that actually works: none of these invocations get far enough to
//! need one. This has to run the compiled binary rather than call
//! `run_connect` in-process, because `dispatch`'s own routing -- whether
//! `--attach`/`--new` reach `run_connect` at all, rather than being caught by
//! its "unknown option" catch-all -- is part of what is under test.

use std::process::Command;

/// `--attach` and `--new` are mutually exclusive: nothing sensible follows
/// from asking for a specific session AND insisting on a fresh one.
#[test]
fn attach_and_new_together_are_refused_by_name() {
    let output = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .args(["--attach", "abcd1234", "--new", "example.com"])
        .output()
        .expect("running oxutrm --attach ... --new ...");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a usage mistake must exit 2, not run off and try to connect"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--attach") && stderr.contains("--new"),
        "the error must name both flags, so the user knows which pair is the \
         problem: {stderr}"
    );
}

/// `--attach` with nothing after it is a usage error, not a panic on a
/// missing argument.
#[test]
fn attach_without_an_id_exits_two() {
    let output = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .arg("--attach")
        .output()
        .expect("running oxutrm --attach");

    assert_eq!(
        output.status.code(),
        Some(2),
        "a missing --attach argument must exit 2"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("--attach"),
        "the error must name the flag that needed an argument: {stderr}"
    );
}

/// `--help` must mention `--attach`, or a user who wants it has nowhere to
/// read about it.
#[test]
fn help_mentions_attach() {
    let output = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .arg("--help")
        .output()
        .expect("running oxutrm --help");

    assert!(output.status.success(), "--help must exit successfully");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("--attach"),
        "the help text does not mention --attach: {stdout}"
    );
}

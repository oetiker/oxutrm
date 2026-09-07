//! `oxutrm host --connect` as a subprocess: it must offer before it serves.
//!
//! A subprocess and not an in-process call because `src/main.rs` is
//! `#![forbid(unsafe_code)]`, so a test there cannot set `OXUTRM_STATE_DIR`
//! for itself. Same pattern as `tests/serve_exits.rs`.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn connect_offers_an_empty_list_when_nothing_is_running() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .args(["host", "--connect"])
        .env("OXUTRM_STATE_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the remote half");

    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("reading the offer");

    let offer: serde_json::Value = serde_json::from_str(&line).expect("the offer is JSON");
    assert_eq!(
        offer["t"], "Sessions",
        "the FIRST line must be the offer: {line}"
    );
    assert_eq!(
        offer["sessions"]
            .as_array()
            .expect("a sessions array")
            .len(),
        0,
        "an empty registry offers nothing: {line}"
    );

    // Answering New must lead to the serve path, whose first act is its own
    // hello. Asserting on that and not merely on "the process is still alive":
    // a dispatcher that dropped the choice on the floor would also stay alive.
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"t": "Choose", "choice": {"c": "New"}})
    )
    .expect("sending the choice");
    stdin.flush().expect("flushing the choice");

    let mut hello = String::new();
    out.read_line(&mut hello).expect("reading the hello");
    let hello: serde_json::Value = serde_json::from_str(&hello).expect("the hello is JSON");
    assert_eq!(
        hello["t"], "HostHello",
        "New must reach the serve path: {hello}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn attaching_to_a_session_that_is_not_there_fails_definitively() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .args(["host", "--connect"])
        .env("OXUTRM_STATE_DIR", dir.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the remote half");

    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
    let mut line = String::new();
    out.read_line(&mut line).expect("reading the offer");

    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(
        stdin,
        "{}",
        serde_json::json!({"t": "Choose", "choice": {"c": "Attach", "id": "0".repeat(32)}})
    )
    .expect("sending the choice");
    stdin.flush().expect("flushing the choice");

    let mut reply = String::new();
    out.read_line(&mut reply).expect("reading the reply");
    let reply: serde_json::Value = serde_json::from_str(&reply).expect("the reply is JSON");
    assert_eq!(
        reply["t"], "Failed",
        "a missing session is a definite answer: {reply}"
    );
    let reason = reply["reason"].as_str().expect("a reason");
    assert!(
        reason.contains("no such session") || reason.contains("not"),
        "the reason must say the session is gone, not something generic: {reason}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

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
        offer["list"].as_array().expect("a list array").len(),
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
    // The exact phrase `run_host_connect` writes, and nothing looser: the
    // `|| reason.contains("not")` this used to carry was satisfied by
    // "cannot", "another", "nothing" and "note" -- which is to say by nearly
    // any English sentence, including every generic failure this assertion
    // exists to reject.
    assert!(
        reason.contains("no such session"),
        "the reason must say the session is gone, not something generic: {reason}"
    );

    let _ = child.kill();
    let _ = child.wait();
}

/// Spawn `oxutrm host --connect` on `dir`, read its offer, answer `choice`
/// and return the next line it writes.
fn answer_offer(dir: &std::path::Path, choice: serde_json::Value) -> serde_json::Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .args(["host", "--connect"])
        .env("OXUTRM_STATE_DIR", dir)
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
        serde_json::json!({"t": "Choose", "choice": choice})
    )
    .expect("sending the choice");
    stdin.flush().expect("flushing the choice");
    let mut next = String::new();
    out.read_line(&mut next).expect("reading the answer");
    let _ = child.kill();
    let _ = child.wait();
    serde_json::from_str(&next).expect("the answer is JSON")
}

#[test]
fn a_lobby_choice_reaches_the_serve_path() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    let hello = answer_offer(dir.path(), serde_json::json!({"c": "Lobby"}));
    assert_eq!(hello["t"], "HostHello", "{hello}");
}

#[test]
fn a_new_session_with_a_taken_name_is_refused_before_the_hello() {
    let dir = tempfile::tempdir().expect("a scratch directory");
    // A live session named "build": this test's own pid keeps it live.
    let id = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    let entry = dir.path().join("oxutrm").join(id);
    std::fs::create_dir_all(&entry).expect("an entry directory");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    std::fs::write(
        entry.join("meta.json"),
        serde_json::json!({
            "session_id": id, "attach_id": 1, "pid": std::process::id(),
            "created_unix": now, "shell": "/bin/bash",
            "size": {"cols": 120, "rows": 40}, "detachable": true, "name": "build"
        })
        .to_string(),
    )
    .expect("writing meta.json");

    let answer = answer_offer(dir.path(), serde_json::json!({"c": "New", "name": "build"}));
    assert_eq!(answer["t"], "Failed", "{answer}");
    assert!(
        answer["reason"].as_str().unwrap_or("").contains("taken"),
        "{answer}"
    );
}

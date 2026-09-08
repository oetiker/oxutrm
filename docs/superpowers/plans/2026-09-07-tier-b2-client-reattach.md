# Tier B2 — client-side reattach — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the client ask for a session instead of always starting one — so a fresh `oxutrm <target>` can resume a parked session, and a client whose link dies rebuilds it by itself.

**Architecture:** A session offer is added *before* the hellos. The remote command becomes `oxutrm host --connect`, which writes a `Sessions` signal, reads one `Choose`, and then dispatches to the `--serve` or `--attach` path it already has. The client's L4–L10 becomes `establish`, generic over an `AsyncBufRead`/`AsyncWrite` pair exactly as the host's `run_attach_exchange` already is — which is what finally lets the reattach composition be tested over `tokio::io::duplex`, with no ssh, no shim and no network. On top of that: a decision function and picker for a new connect, and a `Recovering` phase whose attempts run the same `establish` against a fresh ssh channel.

**Tech Stack:** Rust, edition 2024, MSRV 1.96. `tokio` (`io::duplex` for tests), `serde` for the signalling JSON, `quinn` for the transport, `anyhow` for errors.

**Spec:** `docs/superpowers/specs/2026-09-07-tier-b2-client-reattach-design.md` — read it before Task 1. The plan argues from the spec; where they disagree, the spec wins. Its parent is `docs/superpowers/specs/2026-08-29-session-recovery-design.md` §5.3, §6, §7.

**Baseline:** `make check` on `main` at `249f0de` exits 0. Any failure you see at Task 1 is yours.

## Global Constraints

Copied verbatim from the standing record. Every task's requirements include these.

- **Reattachment is not a second code path** (`crates/oxutrm-host/src/attach.rs:3`). Everything this plan adds happens BEFORE the R4–R10 exchange. After the choice is made, the exchange that runs must be the one `--serve` and `--attach` already run.
- **`oxutrm-host` MUST NOT depend on `oxutrm-net`.**
- **`src/main.rs` is `#![forbid(unsafe_code)]`** — `forbid` cannot be locally overridden, so a test there that needs an env var must spawn `env!("CARGO_BIN_EXE_oxutrm")`. Patterns: `tests/serve_exits.rs`, `tests/host_attach.rs`.
- **The host loop's arms borrow locals, never `self`** (constraint C1). The rebuild receiver on the client follows the same rule for the same reason.
- **A rejected frame must never disconnect, and a send failure must never end a session.**
- **Both ends reset sequence counters at every attach and the host's first datagram is a full state** (design spec §8.5). B2 changes nothing here and must not appear to.
- **Never a bare `cargo test`** — the repo root is both a package and a workspace, so it silently runs almost nothing. `make check` is the gate: `cargo fmt --all -- --check`, then `cargo clippy --workspace --all-targets -- -D warnings`, then the tests, parallelism capped at 4.
- **A changelog entry is part of the work** (`CHANGES.md`, under `## Unreleased`).
- **Do not add `#[allow(dead_code)]` for something not wired up yet.** If a helper is test-only, use `#[cfg(test)]`.
- **Tests use a STUN-free `NetConfig`** (`stun_servers: vec![]`, `enable_port_mapping: false`, `enable_birthday: false`). A test that reaches STUN makes every fixed `sleep()` in it non-deterministic.
- **Do not assert platform rules in tests.**
- English for all comments, identifiers and documentation.

## Test discipline — read before writing any test

This project's two signature defects, both of which have recurred:

1. **A guard that cannot fail.** **Every task below ends with an injection step: break the thing deliberately, watch the new test fail, put it back.** A test that has not been seen to fail is not a guard.
2. **A comment that outlives its truth.**

Three specific shapes that have bitten this codebase:

- **When a test asserts a return value, ask what else produces that value.** Assert the side effect.
- **When a test asserts on a string, check what the string said BEFORE the change.** Three tests on an earlier branch passed against the pre-existing message.
- **Prefer a completed observation over a duration.** Read a message off a channel; do not sleep and hope.

## File Structure

| File | Responsibility |
|---|---|
| `crates/oxutrm-proto/src/signal.rs` | Modified. `SessionSummary`, `Choice`, and the two new `Signal` variants. |
| `crates/oxutrm-host/src/attach.rs` | Modified. `SessionSummary::from(&SessionMeta)` and the numbered list rendering the picker prints. |
| `crates/oxutrm-host/src/ssh.rs` | Modified, one line: `REMOTE_SERVE` becomes `--connect`. |
| `src/main.rs` | Modified. `host --connect` dispatch; the client's `--attach`/`--new` flags; usage text. |
| `src/connect.rs` | Modified heavily. `establish` extracted; the offer, the decision and the picker added. |
| `src/choose.rs` | **New.** The pure decision function and the picker. Separate file because the decision is a table with nine rows and a picker loop, and `connect.rs` is already the longest client file. |
| `src/rebuild.rs` | **New.** One rebuild attempt and the backoff schedule. Separate from `session.rs`, which is already large and owns a different concern. |
| `src/linkstate.rs` | Modified. `Recovering`, `REBUILD_AFTER`. |
| `src/session.rs` | Modified. Session id held; the rebuild receiver as a local; the swap; `TAKEN_OVER` during a swap. |
| `crates/oxutrm-client/src/notice.rs` | Modified. The `Recovering` notice. |
| `src/connect.rs` `mod tests` | The duplex-paired client/host exchange — the centrepiece. It lives here and not under `tests/`: the root crate is a binary with no `[lib]` target, so an integration test cannot reach `pub(crate)` items. |
| `tests/client_flags.rs` | **New**, root crate. Subprocess tests of the new flags. |
| `CHANGES.md` | The user-facing entry. |

---

### Task 1: The offer messages

**Files:**
- Modify: `crates/oxutrm-proto/src/signal.rs`
- Modify: `crates/oxutrm-host/src/attach.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct SessionSummary { pub session_id: String, pub created_unix: u64, pub shell: String, pub size: TermSize, pub detachable: bool, pub attach_id: u64 }` — `Clone, Debug, Serialize, Deserialize, PartialEq, Eq`.
  - `pub enum Choice { Attach { id: String }, New }` — same derives, `#[serde(tag = "c")]`.
  - `Signal::Sessions { sessions: Vec<SessionSummary> }` and `Signal::Choose { choice: Choice }`.
  - `pub fn summarize(sessions: &[SessionMeta]) -> Vec<SessionSummary>` in `crates/oxutrm-host/src/attach.rs`.

- [ ] **Step 1: Write the failing tests**

Add to the existing `#[cfg(test)] mod tests` in `crates/oxutrm-proto/src/signal.rs`:

```rust
#[test]
fn an_offer_round_trips_through_json() {
    // The offer travels on the same newline-delimited JSON channel as the
    // hellos, so it must survive the same encode/decode. Asserting on the
    // decoded value and not on the string: the tag names are an internal
    // detail, the round trip is the contract.
    let offer = Signal::Sessions {
        sessions: vec![SessionSummary {
            session_id: "f00d".repeat(8),
            created_unix: 1_757_200_000,
            shell: "/bin/bash".to_owned(),
            size: TermSize { cols: 120, rows: 40 },
            detachable: true,
            attach_id: 3,
        }],
    };
    let line = serde_json::to_string(&offer).expect("an offer encodes");
    let back: Signal = serde_json::from_str(&line).expect("an offer decodes");
    match back {
        Signal::Sessions { sessions } => {
            assert_eq!(sessions.len(), 1);
            assert_eq!(sessions[0].session_id, "f00d".repeat(8));
            assert_eq!(sessions[0].size.cols, 120);
            assert!(sessions[0].detachable);
            assert_eq!(sessions[0].attach_id, 3);
        }
        other => panic!("expected Sessions, got {other:?}"),
    }
}

#[test]
fn an_empty_offer_is_a_legitimate_offer() {
    // The ordinary first connect. It must not be expressed as an absent
    // message: the client blocks on reading exactly one line here, and
    // "nothing to offer" has to be sayable.
    let line = serde_json::to_string(&Signal::Sessions { sessions: vec![] })
        .expect("an empty offer encodes");
    match serde_json::from_str::<Signal>(&line).expect("an empty offer decodes") {
        Signal::Sessions { sessions } => assert!(sessions.is_empty()),
        other => panic!("expected Sessions, got {other:?}"),
    }
}

#[test]
fn both_choices_round_trip() {
    for choice in [Choice::New, Choice::Attach { id: "abcd1234".to_owned() }] {
        let line = serde_json::to_string(&Signal::Choose { choice: choice.clone() })
            .expect("a choice encodes");
        match serde_json::from_str::<Signal>(&line).expect("a choice decodes") {
            Signal::Choose { choice: back } => assert_eq!(back, choice),
            other => panic!("expected Choose, got {other:?}"),
        }
    }
}

#[test]
fn an_offer_carries_no_process_identity() {
    // SessionMeta also holds `pid` and `boot`. Both are local bookkeeping and
    // mean nothing on the client's machine; a pid on the wire invites a
    // client to reason about a process it cannot see. Asserting on the
    // serialized text on purpose -- this one IS about what goes on the wire.
    let line = serde_json::to_string(&Signal::Sessions {
        sessions: vec![SessionSummary {
            session_id: "f00d".repeat(8),
            created_unix: 1,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: false,
            attach_id: 0,
        }],
    })
    .expect("an offer encodes");
    assert!(!line.contains("\"pid\""), "pid must not travel: {line}");
    assert!(!line.contains("\"boot\""), "boot must not travel: {line}");
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 -p oxutrm-proto --lib -- signal:: --test-threads 4`
Expected: FAIL to compile — `cannot find struct 'SessionSummary'`, `cannot find enum 'Choice'`, `no variant named 'Sessions'`.

- [ ] **Step 3: Add the types and the variants**

In `crates/oxutrm-proto/src/signal.rs`, above the `Signal` enum:

```rust
/// One live session, as offered to a client.
///
/// Deliberately NOT `SessionMeta`. That struct also carries `pid` and `boot`,
/// which are this host's own bookkeeping: a pid means nothing on the client's
/// machine, and a boot token even less. What a client needs is enough to
/// choose between sessions and to be told when a choice cannot work.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionSummary {
    /// 32 lowercase hex characters, the same id `--list` and `--attach` use.
    pub session_id: String,
    pub created_unix: u64,
    pub shell: String,
    /// The size the session was last driven at, which is what a returning
    /// client sees redrawn before its own size takes effect.
    pub size: TermSize,
    /// A rung-4 session tunnels QUIC through its ssh connection and dies with
    /// it. It is offered anyway, and refused with a reason: a list that
    /// silently omits a session the host's own `--list` shows is a list that
    /// makes a user doubt the tool.
    pub detachable: bool,
    pub attach_id: u64,
}

/// What the client wants done, in answer to [`Signal::Sessions`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "c")]
pub enum Choice {
    /// Relay me into this session.
    Attach { id: String },
    /// Start a fresh one.
    New,
}
```

Then, inside `enum Signal`, immediately after the `HostHello` variant's closing brace and before `ClientHello`:

```rust
    /// host -> client, and it comes BEFORE `HostHello`.
    ///
    /// The offer is what makes reattach reachable from an ordinary connect:
    /// the client learns what is already running and says which of it it
    /// wants. Empty is the ordinary first-connect case and is not an error.
    Sessions {
        sessions: Vec<SessionSummary>,
    },
    /// client -> host, the only answer to `Sessions`.
    ///
    /// Everything after this point is the exchange that `--serve` and
    /// `--attach` already ran; this message is the last one that can differ.
    Choose {
        choice: Choice,
    },
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 -p oxutrm-proto --lib -- signal:: --test-threads 4`
Expected: PASS, four new tests among the existing ones.

- [ ] **Step 5: Add `summarize` on the host side, with its test**

In `crates/oxutrm-host/src/attach.rs`, beside `format_session_list`:

```rust
/// The registry's entries, as a client is allowed to see them.
///
/// The filtering is the caller's: `Registry::list_in` has already dropped
/// stale entries, and a NOT-detachable session is offered rather than hidden
/// (see [`SessionSummary::detachable`]).
#[must_use]
pub fn summarize(sessions: &[SessionMeta]) -> Vec<SessionSummary> {
    sessions
        .iter()
        .map(|m| SessionSummary {
            session_id: m.session_id.clone(),
            created_unix: m.created_unix,
            shell: m.shell.clone(),
            size: m.size,
            detachable: m.detachable,
            attach_id: m.attach_id,
        })
        .collect()
}
```

Add `use oxutrm_proto::SessionSummary;` to that file's imports, and re-export `SessionSummary` and `Choice` from `crates/oxutrm-proto/src/lib.rs` alongside the existing signal exports.

Test, in `attach.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn a_summary_keeps_the_id_and_drops_the_pid() {
    let meta = SessionMeta {
        session_id: "a".repeat(32),
        attach_id: 2,
        pid: 4242,
        created_unix: 1_757_200_000,
        shell: "/bin/zsh".to_owned(),
        size: TermSize { cols: 100, rows: 30 },
        detachable: true,
        boot: Some("boot-token".to_owned()),
    };
    let summaries = summarize(std::slice::from_ref(&meta));
    assert_eq!(summaries.len(), 1);
    assert_eq!(summaries[0].session_id, meta.session_id);
    assert_eq!(summaries[0].attach_id, 2);
    assert_eq!(summaries[0].size.cols, 100);
    // The side effect that matters: nothing local survives the crossing.
    let encoded = serde_json::to_string(&summaries[0]).expect("a summary encodes");
    assert!(!encoded.contains("4242"), "the pid escaped: {encoded}");
    assert!(!encoded.contains("boot-token"), "the boot token escaped: {encoded}");
}
```

- [ ] **Step 6: Run the host tests**

Run: `cargo test --workspace --jobs 4 -p oxutrm-host --lib -- attach:: --test-threads 4`
Expected: PASS.

- [ ] **Step 7: Injection — prove the guards can fail**

Temporarily add `pid: u32` to `SessionSummary` and set it in `summarize`. Re-run both test commands.
Expected: `an_offer_carries_no_process_identity` and `a_summary_keeps_the_id_and_drops_the_pid` FAIL.
Then revert the injection and re-run: PASS.

- [ ] **Step 8: Full gate and commit**

Run: `make check`
Expected: exit 0, no warnings.

```bash
git add crates/oxutrm-proto/src/signal.rs crates/oxutrm-proto/src/lib.rs crates/oxutrm-host/src/attach.rs
git commit -m "feat(proto): a session offer, before the hellos

The client has never been able to ask for anything: the remote command
is a hardcoded --serve, so a parked session is unreachable from an
ordinary connect. Sessions/Choose is the one exchange that can differ;
everything after it is what --serve and --attach already ran.

SessionSummary rather than SessionMeta because pid and boot are this
host's bookkeeping and mean nothing on the client's machine. A test
asserts on the serialized text for exactly that reason."
```

---

### Task 2: `host --connect`, the dispatcher

**Files:**
- Modify: `src/main.rs`
- Test: `tests/host_connect.rs` (new, root crate)

**Interfaces:**
- Consumes: `oxutrm_host::attach::summarize`, `Signal::Sessions`, `Signal::Choose`, `Choice` (Task 1).
- Produces: `oxutrm host --connect` on the CLI. No new Rust API — `run_host_connect()` is private to `main.rs`.

- [ ] **Step 1: Write the failing test**

Create `tests/host_connect.rs`:

```rust
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
    assert_eq!(offer["t"], "Sessions", "the FIRST line must be the offer: {line}");
    assert_eq!(
        offer["sessions"].as_array().expect("a sessions array").len(),
        0,
        "an empty registry offers nothing: {line}"
    );

    // Answering New must lead to the serve path, whose first act is its own
    // hello. Asserting on that and not merely on "the process is still alive":
    // a dispatcher that dropped the choice on the floor would also stay alive.
    let mut stdin = child.stdin.take().expect("stdin");
    writeln!(stdin, "{}", serde_json::json!({"t": "Choose", "choice": {"c": "New"}}))
        .expect("sending the choice");
    stdin.flush().expect("flushing the choice");

    let mut hello = String::new();
    out.read_line(&mut hello).expect("reading the hello");
    let hello: serde_json::Value = serde_json::from_str(&hello).expect("the hello is JSON");
    assert_eq!(hello["t"], "HostHello", "New must reach the serve path: {hello}");

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
    assert_eq!(reply["t"], "Failed", "a missing session is a definite answer: {reply}");
    let reason = reply["reason"].as_str().expect("a reason");
    assert!(
        reason.contains("no such session") || reason.contains("not"),
        "the reason must say the session is gone, not something generic: {reason}"
    );

    let _ = child.kill();
    let _ = child.wait();
}
```

Add `serde_json` and `tempfile` to `[dev-dependencies]` in the root `Cargo.toml` if they are not already there.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --workspace --jobs 4 --test host_connect -- --test-threads 4`
Expected: FAIL — the child exits 2 with `oxutrm host: unknown option "--connect"`, so `read_line` returns 0 bytes and the JSON parse panics.

- [ ] **Step 3: Add the dispatcher**

In `src/main.rs`, add to `run_host`'s match, directly after the `--list` arm:

```rust
        Some("--connect") => run_host_connect(),
```

Update the `None` arm's message to name it:

```rust
        None => {
            eprintln!(
                "oxutrm host: needs one of --connect, --serve, --list or --attach <id>.\nTry `oxutrm host --help`."
            );
            std::process::exit(2);
        }
```

Then add the function beside `run_host_list`:

```rust
/// `oxutrm host --connect`: offer what is running, then do what is chosen.
///
/// This is the command the client spawns over ssh. It is a DISPATCHER and
/// nothing else: after the choice it calls the same `run_host_serve` or
/// `run_host_attach` that were already there, so no code path exists that
/// only a reattach exercises — `attach.rs`'s standing rule.
///
/// The offer goes out before `HostHello`, which is why it can be sent at all:
/// at this point neither side has committed to a session.
fn run_host_connect() -> Result<()> {
    let root = oxutrm_host::resolve_registry_root()
        .context("deciding where oxutrm records its sessions")?;
    let sessions = oxutrm_host::Registry::list_in(&oxutrm_host::Registry::dir_at(&root.base))
        .context("reading the session registry")?;

    let mut stdout = std::io::stdout();
    oxutrm_host::signalling::write_signal(
        &mut stdout,
        &Signal::Sessions {
            sessions: oxutrm_host::attach::summarize(&sessions),
        },
    )
    .context("offering the live sessions")?;

    let mut stdin = std::io::BufReader::new(std::io::stdin());
    let choice = match oxutrm_host::signalling::read_signal(&mut stdin)
        .context("reading the client's choice")?
    {
        Signal::Choose { choice } => choice,
        other => {
            return Err(anyhow::anyhow!(
                "the client answered the offer with {other:?} instead of a choice"
            ));
        }
    };

    match choice {
        Choice::New => serve::run_host_serve(),
        Choice::Attach { id } => {
            // Refused HERE rather than inside the relay, because the client is
            // still listening on this channel: a `Failed` it can read is worth
            // more than an exit code it has to guess at.
            if !sessions.iter().any(|m| m.session_id == id) {
                let reason = format!("no such session on this host: {id}");
                let _ = oxutrm_host::signalling::write_signal(
                    &mut std::io::stdout(),
                    &Signal::Failed {
                        reason: reason.clone(),
                    },
                );
                return Err(anyhow::anyhow!(reason));
            }
            run_host_attach(&id)
        }
    }
}
```

Add the imports `use oxutrm_proto::{Choice, Signal};` to `main.rs` if not present, and confirm `oxutrm_host::signalling::{read_signal, write_signal}` are public and blocking; if only the async pair exists, use a current-thread runtime here exactly as `run_host_attach` does.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --workspace --jobs 4 --test host_connect -- --test-threads 4`
Expected: PASS, both tests.

- [ ] **Step 5: Injection**

Reorder `run_host_connect` so the offer is written *after* reading the choice. Re-run.
Expected: `connect_offers_an_empty_list_when_nothing_is_running` hangs then fails on the read — the ordering is the contract. Restore, re-run: PASS.

Second injection: make the missing-session arm call `run_host_attach(&id)` unconditionally. Re-run.
Expected: `attaching_to_a_session_that_is_not_there_fails_definitively` FAILS, because no `Failed` line arrives. Restore.

- [ ] **Step 6: Update the help text**

In `src/main.rs`, `HOST_USAGE` gains a line for `--connect` describing it as the command the client spawns. Leave the client-facing `USAGE` alone — Task 5 rewrites it once the client can actually reattach.

- [ ] **Step 7: Gate and commit**

Run: `make check`
Expected: exit 0.

```bash
git add src/main.rs tests/host_connect.rs Cargo.toml
git commit -m "feat(host): --connect offers the live sessions, then dispatches

A dispatcher and nothing more: after the choice it calls the serve or
attach path that already existed, so nothing runs on reattach that an
ordinary connect does not exercise.

A chosen session that is not in the registry is refused with a Failed
the client can read, not an exit code it has to interpret -- the
channel is still open at that point, so a sentence is affordable."
```

---

### Task 3: `establish` — the client exchange, generic over its stream

**Files:**
- Modify: `src/connect.rs:53-161`
- Modify: `crates/oxutrm-host/src/ssh.rs:43`
- Test: `src/connect.rs`, in its existing `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces:
  - `pub(crate) struct Established { pub link: Link, pub path: PathDescription, pub session_id: String, pub attach_id: u64 }`
  - `pub(crate) async fn establish<R, W>(reader: R, writer: W, size: TermSize, cfg: &NetConfig) -> anyhow::Result<Established> where R: tokio::io::AsyncBufRead + Unpin + Send + 'static, W: tokio::io::AsyncWrite + Unpin + Send + 'static`
  - Task 5's picker and Task 6's rebuild both call `establish`.

- [ ] **Step 1: Write the failing test — the centrepiece**

Add to `src/connect.rs`'s existing `#[cfg(test)] mod tests`. **Not** under `tests/`: the root crate is a binary with no `[lib]` target (checked — there is no `src/lib.rs` and no `[lib]` section in `Cargo.toml`), so an integration test cannot see `pub(crate)` items. A unit test in the binary crate can, and `cargo test --bin oxutrm` runs it.

The test pairs the client's exchange with the host's over a pipe. This is the coverage the reattach path has never had: both functions are generic over a reader and a writer, so they join through `tokio::io::duplex` with no ssh, no shim, no Unix socket and no network, and the composition B1 could only verify by hand becomes an ordinary test. Both halves run on one runtime with a real UDP loopback path between them, because what is under test is the handshake, not the transport.

```rust
/// STUN-free, like every other test in this repo: a test that reaches STUN
/// makes every timing in it non-deterministic, and an injected bug once
/// passed because a real gather ate the ordering.
fn test_config() -> oxutrm_net::NetConfig {
    oxutrm_net::NetConfig {
        stun_servers: vec![],
        enable_port_mapping: false,
        enable_birthday: false,
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_and_a_host_complete_an_exchange_over_a_pipe() {
    use oxutrm_proto::TermSize;

    let (host_side, client_side) = tokio::io::duplex(64 * 1024);
    let (host_read, host_write) = tokio::io::split(host_side);
    let (client_read, client_write) = tokio::io::split(client_side);

    let mut meta = oxutrm_host::SessionMeta {
        session_id: "b".repeat(32),
        attach_id: 0,
        pid: std::process::id(),
        created_unix: 0,
        shell: "/bin/sh".to_owned(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
        boot: oxutrm_host::boot_token(),
    };

    let cfg = test_config();
    let host = tokio::spawn({
        let cfg = cfg.clone();
        async move {
            crate::attach_exchange::run_attach_exchange(
                tokio::io::BufReader::new(host_read),
                host_write,
                &mut meta,
                &cfg,
            )
            .await
        }
    });

    let client = establish(
        tokio::io::BufReader::new(client_read),
        client_write,
        TermSize { cols: 120, rows: 40 },
        &cfg,
    )
    .await
    .expect("the client exchange completes");

    let attached = host.await.expect("the host task joins").expect("the host exchange completes");

    // The side effects, not just "it returned Ok":
    assert_eq!(
        client.session_id,
        "b".repeat(32),
        "the client must keep the session id -- the rebuild loop cannot exist without it"
    );
    assert_eq!(
        client.attach_id, 1,
        "begin_attach bumps the generation, and the client must see the bumped one"
    );
    assert_eq!(
        attached.client_size,
        TermSize { cols: 120, rows: 40 },
        "the host must adopt the CLIENT's size, not the size the session had"
    );
}
```

`run_attach_exchange` is already `pub(crate)`, and `establish` will be, so both are reachable from this module with no re-exports and no visibility widening.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- connect::tests::a_client_and_a_host --test-threads 4`
Expected: FAIL to compile — `establish` does not exist.

- [ ] **Step 3: Extract `establish`**

In `src/connect.rs`, move the body from `// L4.` through the `// L10.` line into:

```rust
/// L4 to L10: one socket, the hello exchange, the ICE ladder and the QUIC
/// handshake, ending the moment the host declares the path up.
///
/// Generic over its signalling stream for the same reason the host's
/// `run_attach_exchange` is: the ssh channel is one caller, and a `duplex`
/// pair in a test is another. Until this was extracted, the only way to
/// exercise a reattach was to shadow the remote binary with a shim.
///
/// `Send + 'static` because the candidate pumps are spawned.
pub(crate) async fn establish<R, W>(
    reader: R,
    writer: W,
    size: TermSize,
    cfg: &NetConfig,
) -> Result<Established>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // ... the existing L4-L10 body, with `channel.recv()` replaced by
    // `read_signal_async(&mut reader)`, `channel.send(..)` by
    // `write_signal_async(&mut writer, ..)`, and `channel.halves()` by the
    // reader and writer this function was handed.
}
```

and return:

```rust
/// One completed client-side attach, and the two identities the rebuild loop
/// needs afterwards.
pub(crate) struct Established {
    pub link: Link,
    pub path: PathDescription,
    /// Kept, not discarded: a rebuild attaches to THIS session by name, and
    /// before this existed `host_facts` threw the id away with `..`.
    pub session_id: String,
    pub attach_id: u64,
}
```

`HostFacts` gains `session_id: String` and `attach_id: u64`, and `host_facts` stops discarding them:

```rust
        Signal::HostHello {
            session_id,
            attach_id,
            psk,
            cert_spki_sha256,
            candidates,
            nat_type,
            ..
        } => Ok(HostFacts {
            session_id,
            attach_id,
            psk,
            host_spki: cert_spki_sha256,
            candidates,
            nat: nat_type,
        }),
```

Leave `HostFacts`'s hand-written `Debug` naming only the fields it names today, plus the two new ones — never `psk`.

`connect()` keeps L3, L11, L12, L13 and now calls `establish` for the middle.

- [ ] **Step 4: Point the remote command at `--connect`**

`crates/oxutrm-host/src/ssh.rs:43`:

```rust
/// What the client asks ssh to run on the far end.
///
/// `--connect` and not `--serve`: the far end offers its live sessions first
/// and the client chooses. A host binary too old to know the flag exits with
/// ssh's own error, which `diagnose` already reports as a remote binary that
/// needs upgrading.
pub const REMOTE_SERVE: [&str; 3] = ["oxutrm", "host", "--connect"];
```

Rename the const to `REMOTE_CONNECT` throughout if the compiler shows fewer than five call sites; otherwise leave the name and let the doc comment carry the meaning. Do not do both.

In `connect()`, between L3 and `establish`, read the offer and answer it. For this task answer `Choice::New` unconditionally with a `TODO`-free comment saying Task 5 replaces it:

```rust
    // The offer. Task 5 turns this into a real decision; until it does, a
    // fresh connect asks for a fresh session, which is what it did before.
    let _offer = read_offer(&mut channel).await?;
    channel.send(&Signal::Choose { choice: Choice::New }).await?;
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- connect::tests::a_client_and_a_host --test-threads 4`
Expected: PASS.

- [ ] **Step 6: Injection — the regression this test exists for**

In `crates/oxutrm-host/src/session.rs`, find `adopt` and change its `self.resize(size)?` back to `self.size = size;`. Re-run the test.
Expected: the `client_size` assertion FAILS. This is the B1 defect that only a reading review caught; it is a test now. Restore and re-run: PASS.

- [ ] **Step 7: Gate and commit**

Run: `make check`
Expected: exit 0. The count must exceed the baseline by every test added so far.

```bash
git add src/connect.rs crates/oxutrm-host/src/ssh.rs
git commit -m "refactor(client): extract establish, generic over its stream

L4-L10 was welded to SshChannel, so the only way to exercise a reattach
was a shim on the remote host that rewrote the command line. Generic
over a reader and writer -- the same bounds run_attach_exchange already
has -- it pairs with the host's exchange over tokio::io::duplex.

The composition B1 shipped without now has a test, and the adopt-must-
resize regression fails it. host_facts also stops discarding the
session id: a rebuild attaches to a session by name."
```

---

### Task 4: `Recovering`, and the backoff

**Files:**
- Modify: `src/linkstate.rs`
- Modify: `crates/oxutrm-client/src/notice.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `Phase::Recovering { attempt: u32, next_try: Instant }`
  - `pub const REBUILD_AFTER: Duration = Duration::from_secs(20);`
  - `pub fn backoff(attempt: u32) -> Duration`
  - `LinkState::begin_attempt(&mut self, now: Instant)` and `LinkState::attempt_failed(&mut self, now: Instant)`
  - Task 6 drives all of these.

- [ ] **Step 1: Write the failing tests**

Add to `src/linkstate.rs`'s `#[cfg(test)] mod tests`:

```rust
#[test]
fn silence_becomes_recovering_only_after_rebuild_after() {
    let t0 = Instant::now();
    let mut state = LinkState::new(t0);
    assert_eq!(state.evaluate(t0, true), Phase::Live);
    // Silent first, and it must STAY Silent across the whole grace period:
    // a client that starts spawning ssh after two seconds spawns one on
    // every blip.
    let silent = t0 + SILENT_AFTER;
    assert!(matches!(state.evaluate(silent, true), Phase::Silent { .. }));
    let nearly = t0 + REBUILD_AFTER - Duration::from_millis(1);
    assert!(
        matches!(state.evaluate(nearly, true), Phase::Silent { .. }),
        "one millisecond early is still Silent"
    );
    match state.evaluate(t0 + REBUILD_AFTER, true) {
        Phase::Recovering { attempt, .. } => assert_eq!(attempt, 0, "the first attempt is numbered 0"),
        other => panic!("expected Recovering, got {other:?}"),
    }
}

#[test]
fn a_frame_returns_to_live_from_recovering() {
    // The whole reason the old link is held: whichever path revives first
    // wins. A client that could not leave Recovering would keep rebuilding
    // over a link that had already come back.
    let t0 = Instant::now();
    let mut state = LinkState::new(t0);
    let _ = state.evaluate(t0 + REBUILD_AFTER, true);
    state.heard(t0 + REBUILD_AFTER + Duration::from_millis(1));
    assert_eq!(
        state.evaluate(t0 + REBUILD_AFTER + Duration::from_millis(2), false),
        Phase::Live
    );
}

#[test]
fn the_backoff_doubles_then_holds_at_eight_seconds() {
    // Spec: 1, 2, 4, 8, then every 8 s indefinitely. The cap is the point --
    // an unbounded doubling means a client that reconnects hours after the
    // network came back.
    assert_eq!(backoff(0), Duration::from_secs(1));
    assert_eq!(backoff(1), Duration::from_secs(2));
    assert_eq!(backoff(2), Duration::from_secs(4));
    assert_eq!(backoff(3), Duration::from_secs(8));
    assert_eq!(backoff(4), Duration::from_secs(8));
    assert_eq!(backoff(1000), Duration::from_secs(8));
    // And it never overflows, which a shift-based implementation would.
    assert_eq!(backoff(u32::MAX), Duration::from_secs(8));
}

#[test]
fn a_failed_attempt_schedules_the_next_one_further_out() {
    let t0 = Instant::now();
    let mut state = LinkState::new(t0);
    let _ = state.evaluate(t0 + REBUILD_AFTER, true);
    let first = match state.phase_now() {
        Phase::Recovering { next_try, .. } => next_try,
        other => panic!("expected Recovering, got {other:?}"),
    };
    state.attempt_failed(t0 + REBUILD_AFTER + Duration::from_secs(1));
    match state.phase_now() {
        Phase::Recovering { attempt, next_try } => {
            assert_eq!(attempt, 1);
            assert!(next_try > first, "the second attempt must be scheduled later than the first");
        }
        other => panic!("expected Recovering, got {other:?}"),
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- linkstate:: --test-threads 4`
Expected: FAIL to compile — no `Recovering`, no `REBUILD_AFTER`, no `backoff`, no `attempt_failed`.

- [ ] **Step 3: Implement**

In `src/linkstate.rs`, beside `SILENT_AFTER`:

```rust
/// How long silence lasts before the client starts rebuilding the link on its
/// own.
///
/// **Twenty seconds**, and it is a guess the spec is honest about: long
/// enough that a rebuild is not raced against an outage about to end by
/// itself, short enough not to feel abandoned. Revisit it against a real bad
/// network, not by reasoning about it.
pub const REBUILD_AFTER: Duration = Duration::from_secs(20);

/// How long to wait before attempt `attempt` (zero-based).
///
/// 1, 2, 4, 8, then every 8 s for ever. Saturating rather than shifting: an
/// attempt counter that runs for a week must not wrap into an instant retry.
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(3))
}
```

`Phase` gains the variant:

```rust
pub enum Phase {
    Live,
    Silent { since: Instant },
    /// Silent long enough that the client is rebuilding the link itself.
    ///
    /// `attempt` is zero-based and feeds [`backoff`]; `next_try` is when the
    /// next one is due. The client never leaves this state of its own accord
    /// — only a frame arriving does that, or the user quitting.
    Recovering { attempt: u32, next_try: Instant },
    Confirming,
}
```

`evaluate`'s early return changes: `Silent` must now be allowed to escalate, so replace

```rust
        if let Phase::Silent { .. } = self.phase {
            return self.phase;
        }
```

with

```rust
        // Recovering is terminal until something arrives: `heard` is the only
        // way out, and re-deciding here would reset the attempt counter every
        // lap.
        if let Phase::Recovering { .. } = self.phase {
            return self.phase;
        }
        if let Phase::Silent { since } = self.phase {
            // The one escalation. The clock still runs from `last_heard`, so
            // the displayed silence stays continuous across the boundary.
            if now.duration_since(since) >= REBUILD_AFTER {
                self.phase = Phase::Recovering {
                    attempt: 0,
                    next_try: now,
                };
            }
            return self.phase;
        }
```

Add the two transitions:

```rust
    /// The client is about to run an attempt.
    pub fn begin_attempt(&mut self, now: Instant) {
        if let Phase::Recovering { attempt, .. } = self.phase {
            self.phase = Phase::Recovering {
                attempt,
                next_try: now + backoff(attempt),
            };
        }
    }

    /// An attempt failed for a reason worth retrying.
    pub fn attempt_failed(&mut self, now: Instant) {
        if let Phase::Recovering { attempt, .. } = self.phase {
            let next = attempt.saturating_add(1);
            self.phase = Phase::Recovering {
                attempt: next,
                next_try: now + backoff(next),
            };
        }
    }
```

Confirm `heard` clears `Recovering` the same way it clears `Silent`; if it matches on `Phase::Silent` explicitly, add the new variant to that arm.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- linkstate:: --test-threads 4`
Expected: PASS.

- [ ] **Step 5: The notice**

In `crates/oxutrm-client/src/notice.rs`, add the `Recovering` rendering next to the `Silent` one:

- headline: `waiting for the network`
- body: how long the host has been quiet, the attempt number as `reconnect attempt N`, and the countdown to `next_try` in whole seconds
- keys: the existing `Ctrl-\ q` line, unchanged

Test it the way the existing notice tests do — assert on the rendered lines, and **check what the `Silent` notice said before**, so the new assertion cannot pass against the old text.

- [ ] **Step 6: Injection**

Change `REBUILD_AFTER` to `SILENT_AFTER` in `evaluate`'s comparison. Re-run.
Expected: `silence_becomes_recovering_only_after_rebuild_after` FAILS on the "one millisecond early" assertion. Restore.

Change `backoff` to `1u64 << attempt` without the `min`. Re-run.
Expected: `the_backoff_doubles_then_holds_at_eight_seconds` FAILS (panics on overflow). Restore.

- [ ] **Step 7: Gate and commit**

Run: `make check`
Expected: exit 0.

```bash
git add src/linkstate.rs crates/oxutrm-client/src/notice.rs
git commit -m "feat(client): a Recovering phase, and the backoff it paces

REBUILD_AFTER has been in the spec since the notice landed and in no
.rs file. Twenty seconds of silence now escalates Silent to Recovering,
which is terminal until a frame arrives -- whichever path revives first
wins, which is why the old link is never torn down for a rebuild.

backoff saturates at 8 s rather than shifting: an attempt counter that
runs for a week must not wrap into an instant retry."
```

---

### Task 5: Choosing, and the picker

**Files:**
- Create: `src/choose.rs`
- Modify: `src/connect.rs`, `src/main.rs`
- Test: `tests/client_flags.rs` (new, root crate)

**Interfaces:**
- Consumes: `SessionSummary`, `Choice` (Task 1); `establish` (Task 3).
- Produces:
  - `pub(crate) enum Decision { Chosen(Choice), Ask, Refused(String) }`
  - `pub(crate) fn decide(offered: &[SessionSummary], attach: Option<&str>, new: bool) -> Decision`
  - `pub(crate) fn pick(offered: &[SessionSummary], input: &mut impl std::io::BufRead, out: &mut impl std::io::Write) -> anyhow::Result<Option<Choice>>` — `None` means the user quit.

- [ ] **Step 1: Write the failing tests**

Create `src/choose.rs` with its test module first:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::TermSize;

    fn summary(id: &str, detachable: bool) -> SessionSummary {
        SessionSummary {
            session_id: id.to_owned(),
            created_unix: 1_757_200_000,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable,
            attach_id: 1,
        }
    }

    #[test]
    fn nothing_offered_means_a_new_session() {
        assert_eq!(decide(&[], None, false), Decision::Chosen(Choice::New));
    }

    #[test]
    fn exactly_one_detachable_session_is_resumed_without_asking() {
        // The case that sent a user to a new shell while their old one sat
        // parked. Asking here would be worse than deciding: there is only
        // one answer.
        let offered = [summary(&"a".repeat(32), true)];
        assert_eq!(
            decide(&offered, None, false),
            Decision::Chosen(Choice::Attach { id: "a".repeat(32) })
        );
    }

    #[test]
    fn exactly_one_session_that_cannot_be_attached_starts_a_new_one() {
        // A rung-4 session dies with its ssh. Resuming it is not possible, so
        // the useful thing is a new session, not an error.
        let offered = [summary(&"a".repeat(32), false)];
        assert_eq!(decide(&offered, None, false), Decision::Chosen(Choice::New));
    }

    #[test]
    fn several_sessions_ask() {
        let offered = [summary(&"a".repeat(32), true), summary(&"b".repeat(32), true)];
        assert_eq!(decide(&offered, None, false), Decision::Ask);
    }

    #[test]
    fn new_wins_over_everything() {
        let offered = [summary(&"a".repeat(32), true)];
        assert_eq!(decide(&offered, None, true), Decision::Chosen(Choice::New));
    }

    #[test]
    fn a_prefix_selects_one_session() {
        let offered = [summary(&"abcd".repeat(8), true), summary(&"ef01".repeat(8), true)];
        assert_eq!(
            decide(&offered, Some("abcd"), false),
            Decision::Chosen(Choice::Attach { id: "abcd".repeat(8) })
        );
    }

    #[test]
    fn an_ambiguous_prefix_is_refused_by_name() {
        let offered = [summary("abcd1111", true), summary("abcd2222", true)];
        match decide(&offered, Some("abcd"), false) {
            Decision::Refused(why) => {
                assert!(why.contains("abcd1111"), "the message must list the candidates: {why}");
                assert!(why.contains("abcd2222"), "the message must list the candidates: {why}");
            }
            other => panic!("an ambiguous prefix must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_prefix_that_matches_nothing_says_what_is_live() {
        match decide(&[summary("abcd1111", true)], Some("9999"), false) {
            Decision::Refused(why) => {
                assert!(why.contains("9999"), "the message must name what was asked for: {why}");
                assert!(why.contains("abcd1111"), "and what is actually live: {why}");
            }
            other => panic!("a missing id must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_named_session_that_cannot_be_attached_is_refused_with_the_reason() {
        match decide(&[summary("abcd1111", false)], Some("abcd1111"), false) {
            Decision::Refused(why) => assert!(
                why.contains("ssh"),
                "the reason must say it dies with its ssh, not merely 'no': {why}"
            ),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_prefix_shorter_than_four_characters_is_refused() {
        match decide(&[summary("abcd1111", true)], Some("ab"), false) {
            Decision::Refused(why) => assert!(why.contains("four"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn the_picker_takes_a_number() {
        let offered = [summary(&"a".repeat(32), true), summary(&"b".repeat(32), true)];
        let mut input = std::io::Cursor::new(b"2\n".to_vec());
        let mut out = Vec::new();
        let chosen = pick(&offered, &mut input, &mut out).expect("the picker runs");
        assert_eq!(chosen, Some(Choice::Attach { id: "b".repeat(32) }));
        let shown = String::from_utf8(out).expect("the prompt is text");
        assert!(shown.contains(&"a".repeat(32)), "the list must show the ids: {shown}");
    }

    #[test]
    fn the_picker_takes_n_for_new_and_q_for_quit() {
        let offered = [summary(&"a".repeat(32), true)];
        let mut new_input = std::io::Cursor::new(b"n\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut new_input, &mut out).expect("the picker runs"),
            Some(Choice::New)
        );
        let mut quit_input = std::io::Cursor::new(b"q\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(pick(&offered, &mut quit_input, &mut out).expect("the picker runs"), None);
    }

    #[test]
    fn the_picker_re_asks_after_nonsense_and_quits_at_eof() {
        let offered = [summary(&"a".repeat(32), true)];
        let mut input = std::io::Cursor::new(b"banana\n7\n1\n".to_vec());
        let mut out = Vec::new();
        assert_eq!(
            pick(&offered, &mut input, &mut out).expect("the picker runs"),
            Some(Choice::Attach { id: "a".repeat(32) })
        );
        // EOF is a quit, not a hang and not a panic: a client whose stdin is
        // closed must exit, and it must not pick something on the user's
        // behalf.
        let mut empty = std::io::Cursor::new(Vec::new());
        let mut out = Vec::new();
        assert_eq!(pick(&offered, &mut empty, &mut out).expect("the picker runs"), None);
    }
}
```

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- choose:: --test-threads 4`
Expected: FAIL to compile — `decide` and `pick` do not exist.

- [ ] **Step 3: Implement `decide` and `pick`**

Write `src/choose.rs`'s implementation above its test module. `Decision` derives `Debug, PartialEq, Eq`. Rules, in the order the spec's table gives them: `--new` first; then `--attach`, with a minimum prefix of four characters, an ambiguity error listing every candidate id, a not-found error listing what is live, and a not-detachable refusal naming ssh; then no flags, where empty offers `New`, exactly one detachable offers `Attach`, exactly one non-detachable offers `New`, and anything else is `Ask`.

`pick` prints a numbered list built from the same fields `format_session_list` uses, then `[1-N] resume, n new session, q quit`, reads one line, and loops on anything it does not understand. `Ok(None)` for `q` and for EOF.

Add `mod choose;` to `src/main.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- choose:: --test-threads 4`
Expected: PASS, twelve tests.

- [ ] **Step 5: Wire it into `connect()` and add the flags**

`run_connect` parses `--attach <id>` and `--new` before the target, rejecting both together with a message naming both flags. `connect(target, attach, new)` replaces the Task 3 placeholder: read the offer, call `decide`, and on `Ask` leave the terminal alone and call `pick` on `stdin`/`stdout`. `Decision::Refused` prints the reason and exits 2 **before raw mode**. `pick` returning `None` exits 0 without connecting.

`USAGE` in `src/main.rs` changes:

```
  oxutrm [--attach <session-id>] [--new] <ssh-target>
      Connect to that host. If a session of yours is already running
      there it is resumed; with several, oxutrm asks which. --attach
      names one directly, --new always starts a fresh session.
```

Update the help-text test that pins the old promise (`src/main.rs:445`): it asserted that no usage string promises reattach via a bare target. Reattach is implemented now, so the assertion inverts — and **read what that test asserted before changing it**, so the new one cannot pass against the old text.

- [ ] **Step 6: Subprocess test of the flags**

Create `tests/client_flags.rs`: spawning `env!("CARGO_BIN_EXE_oxutrm")` with `--attach` and `--new` together must exit 2 and name both flags on stderr; `--attach` with no id must exit 2; `--help` must mention `--attach`.

- [ ] **Step 7: Injection**

Make `decide` return `Ask` for the one-detachable-session case. Re-run.
Expected: `exactly_one_detachable_session_is_resumed_without_asking` FAILS. Restore.

Make `pick` return `Some(Choice::New)` at EOF. Re-run.
Expected: `the_picker_re_asks_after_nonsense_and_quits_at_eof` FAILS. Restore.

- [ ] **Step 8: Gate and commit**

Run: `make check`
Expected: exit 0.

```bash
git add src/choose.rs src/connect.rs src/main.rs tests/client_flags.rs
git commit -m "feat(client): resume a session instead of always starting one

A bare target now resumes when exactly one session is live, asks when
several are, and starts a new one when none is -- which is what a user
who killed their client and reconnected expected all along.

The decision is a pure function over the offer plus the flags, so every
row of the spec's table is a test. The picker is line I/O before raw
mode, like every other thing that may need the terminal."
```

---

### Task 6: The rebuild loop

**Files:**
- Create: `src/rebuild.rs`
- Modify: `src/session.rs`, `src/connect.rs`
- Modify: `CHANGES.md`

**Interfaces:**
- Consumes: `establish` (Task 3), `Phase::Recovering`/`backoff`/`begin_attempt`/`attempt_failed` (Task 4), `Choice` (Task 1).
- Produces:
  - `pub(crate) enum AttemptOutcome { Landed(Established), Retry(String), Definite(String) }`
  - `pub(crate) async fn attempt(target: &str, session_id: &str, size: TermSize, cfg: &NetConfig) -> AttemptOutcome`

- [ ] **Step 1: Write the failing tests**

In `src/rebuild.rs`'s test module, drive `attempt` against a fake launcher — a small script the test writes to a temp dir that speaks the protocol on stdio, the same shape `crates/oxutrm-host/tests/ssh_bootstrap.rs` already uses for its fakes. Cover:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_that_says_the_session_is_gone_is_definite() {
    // The distinction the whole loop rests on: a transport failure is worth
    // retrying for ever, and an answer from the far end is not. Retrying a
    // Failed would spawn ssh every eight seconds until the user noticed.
    let outcome = attempt_against(fake_host_replying_failed("no such session on this host"), "…").await;
    match outcome {
        AttemptOutcome::Definite(reason) => assert!(reason.contains("no such session"), "{reason}"),
        other => panic!("expected Definite, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ssh_that_dies_is_retried() {
    match attempt_against(fake_host_that_exits_immediately(), "…").await {
        AttemptOutcome::Retry(_) => {}
        other => panic!("expected Retry, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attempt_asks_for_the_session_it_came_from() {
    // The side effect, not the return value: a rebuild that answered `New`
    // would silently start a second shell and look like it had worked.
    let recorded = attempt_against_recording(fake_host_recording_the_choice(), "abc123").await;
    assert_eq!(recorded, serde_json::json!({"c": "Attach", "id": "abc123"}));
}
```

Write the three fakes as helper functions in the same module; do not describe them as "similar to" anything.

- [ ] **Step 2: Run them to verify they fail**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- rebuild:: --test-threads 4`
Expected: FAIL to compile.

- [ ] **Step 3: Implement `attempt`**

One fresh `SshChannel` with `BatchMode=yes` added to the launcher's args, then read the offer, send `Choose::Attach { id }`, then `establish`. Map the results: `Signal::Failed` and an unknown-option exit to `Definite`, everything else that goes wrong to `Retry`, success to `Landed`.

```rust
/// A rebuild's ssh must never stop to ask a question.
///
/// Raw mode is held and the screen belongs to the renderer, so a passphrase
/// prompt would fight it for the terminal. `BatchMode=yes` turns that into a
/// clean failure the notice can explain instead. The real answer is B3's
/// askpass; until then this is the deliberate, legible degradation the spec
/// asks for.
const BATCH_MODE: [&str; 2] = ["-o", "BatchMode=yes"];
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --workspace --jobs 4 --bin oxutrm -- rebuild:: --test-threads 4`
Expected: PASS.

- [ ] **Step 5: Wire the loop into `ClientSession`**

`ClientSession` gains `session_id: String` and `target: String`, set from `Established` and the CLI target. `run_on` gains a **local** `tokio::sync::mpsc::Receiver<AttemptOutcome>` selected on alongside the existing arms — a local, never a field, exactly as `HostSession::run_with_attaches` holds its receiver, and for the same borrow reason.

Each lap: when the phase is `Recovering` and `next_try` has passed and no attempt is in flight, call `begin_attempt` and spawn one. On `Landed`, close the old link with a local reason, swap in the new one, reset sync state the way a first attach does, and let the first arriving frame return the phase to `Live` or `Confirming`. On `Retry`, call `attempt_failed` and keep the reason for the notice. On `Definite`, leave raw mode, print the reason, exit non-zero.

The `Wake::Closed(reason)` arm changes: when `TAKEN_OVER` arrives while an attempt is in flight or has just landed, it is expected and ignored rather than reported. Everything else keeps today's behaviour.

Test this at the session level with a test that asserts a frame on the old link during a `Recovering` phase returns to `Live` and cancels the attempt — the "whichever revives first wins" rule, which is a side effect, not a return value.

- [ ] **Step 6: Injection**

Make `attempt` send `Choice::New`. Re-run.
Expected: `an_attempt_asks_for_the_session_it_came_from` FAILS. Restore.

Make the `Definite` arm return `Retry`. Re-run.
Expected: `a_host_that_says_the_session_is_gone_is_definite` FAILS. Restore.

- [ ] **Step 7: The changelog**

In `CHANGES.md`, under `## Unreleased` → `### New`, a prose entry in the house style saying that a session is now resumed rather than replaced, that oxutrm asks when there is more than one, and that a client whose network dies reconnects by itself. Under a `### Compatibility` subhead, say plainly that the client now runs `oxutrm host --connect` on the far end, so both ends must be upgraded together.

- [ ] **Step 8: Gate and commit**

Run: `make check`
Expected: exit 0.

```bash
git add src/rebuild.rs src/session.rs src/connect.rs CHANGES.md
git commit -m "feat(client): rebuild the link without being asked

Twenty seconds of silence starts attempts against the same session id,
paced 1, 2, 4, 8 s and then every 8 s for ever. The old link is held
throughout: a frame on it cancels the attempt, and an attempt landing
first swaps the transport under a session that never noticed.

A definite answer from the far end ends the loop; a transport failure
never does. Rebuild ssh runs BatchMode=yes -- raw mode is held, so a
passphrase prompt would fight the renderer. That is B3's askpass."
```

---

## Self-Review

**Spec coverage:** §2.1 remote command → Task 3 Step 4. §2.2 messages → Task 1. §2.3 compatibility → Task 3 Step 4 doc comment plus Task 6 Step 7 changelog. §3 `establish` → Task 3. §4.1 CLI → Task 5 Step 5. §4.2 decision table → Task 5 Steps 1 and 3, one test per row. §4.3 picker → Task 5. §5.1 `Recovering` and `REBUILD_AFTER` → Task 4. §5.2 attempt → Task 6. §5.3 swap and `TAKEN_OVER` → Task 6 Step 5. §5.4 failure split → Task 6 Steps 1 and 3. §6 tests → distributed, with the duplex pairing in Task 3. §7 file table → the File Structure above.

**Resolved while writing:** the root crate has no `src/lib.rs` and no `[lib]` section in `Cargo.toml`, so the centrepiece test is a unit test in `src/connect.rs` rather than an integration test. Task 3 says so and names the command that runs it.

**Signalling helpers checked:** `oxutrm_proto::read_signal` and `oxutrm_proto::write_signal` are the blocking pair Task 2 uses; `oxutrm_host::signalling::{read_signal_async, write_signal_async}` are the async pair Task 3 uses. Both exist today.

---

## Correction, 2026-09-08 (recorded during Task 6)

Task 3's justification for the duplex-paired composition test repeats the
design spec's claim that it catches the Tier B1 "`adopt` must call `resize`"
regression. It does not and cannot: the value it asserts on,
`attached.client_size`, is produced by `run_attach_exchange` when it parses
`ClientHello`, which is upstream of `adopt`, so breaking `adopt` cannot move
it. The regression is caught by the pre-existing
`session::tests::adopting_a_link_resizes_the_shell_to_the_newcomers_terminal`,
which reads the screen the host ships. No coverage is missing; the rationale
was overstated, and it is recorded here rather than only in a task report so
the correction outlives this branch's scratch workspace. See the matching note
appended to the design spec.

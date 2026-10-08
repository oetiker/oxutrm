# Session Switcher (D1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One session selector -- at connect time and as `s` in the status popup -- that lists a host's sessions and switches between them over the live link (never ssh), starts, renames and kills them; a session killed by its own client stays as a lobby until the user picks another.

**Architecture:** Every door of a session process -- its client's QUIC control stream and its Unix socket -- reads an `Open` line first and hands it to one dispatcher (`src/door.rs`), one task per connection; only `Attach` reaches the serial attach loop (`src/listener.rs`). A session process is a `HostSession` whose shell is optional: without one it is a **lobby** (blank screen, no registry entry) that becomes the session on `New`; `Kill` of its own session turns it back into one. A switch is the door relaying the client's stream to the target's socket as `Attach { Primary }` -- the target's ordinary attach exchange -- and a `New` in a session spawns a sibling as the ordinary `oxutrm host --serve` on a socket pair. The client sends each request on a fresh control stream from a task of its loop (`src/switcher.rs`), swaps the new link in only once it is `Established` (make-before-break, close reason `MOVED_AWAY`), and draws the selector (`src/selector.rs` state, `view::sessions`, `oxutrm_client::layout_sessions`) as the popup's `Mode::Sessions`.

**Tech Stack:** Rust 2024 (rust-version 1.96), tokio, quinn (vendored `quinn-proto`), serde/serde_json JSON lines, rustix (`flock`, `kill_process_group`, `tcgetpgrp`), ratatui through the existing popup code, `unicode-width`, `jiff` (start times), `insta` snapshots, `tempfile` in tests.

**Spec:** `docs/superpowers/specs/2026-10-07-session-switcher-design.md` (read it with this plan; § numbers below are its sections).

**How this plan was made.** Every task below was built, in this order, in a scratch worktree, and its code is that worktree's commit, cut into the test part and the implementation part. Diffs are `git diff -U4` against the state the previous tasks leave; apply them by hand where a context line has moved. Where the plan makes a call the spec does not, it says so in the task and in "Deviations from the spec" at the end. Flag anything that is verbatim here but wrong rather than copying it.

## Global Constraints

- The root crate is `#![forbid(unsafe_code)]` and a binary crate: an unused `pub(crate)` item is dead code. Items a later task uses carry `#[cfg_attr(not(test), expect(dead_code, reason = "... Task N ..."))]` until that task, which removes the attribute; the plan says where.
- Every module that runs while a client session owns the screen carries `#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]`: new modules here (`door`, `host_session`, `selector`, `switcher`) all do. The selector is drawn through the renderer's overlay, like the popup and the config screen; nothing is printed.
- `oxutrm-sync` has no I/O dependencies; `oxutrm-host` must not depend on `oxutrm-net`. `vendor/quinn-proto/` stays as it is: `max_idle_timeout` is `None`, so **every wait on a stream answer has its own bound** (`OPEN_TIMEOUT` 10 s, `SIBLING_TIMEOUT` 2 s, `FORWARD_TIMEOUT` 10 s, `switcher::ANSWER_WITHIN` 15 s, `control::SEARCH_DEADLINE` 90 s, `listener::ATTACH_TIMEOUT` 90 s).
- Dropping a `JoinHandle` detaches the task: tasks the loop must stop are held as `AbortOnDrop`; tasks the door owns are aborted and awaited by `Door::close`.
- `tokio::select!` arms borrow locals, never `self` (C1).
- A send failure never ends a session.
- ssh only initiates a connection: nothing in D1 uses ssh after `Established`. No russh; `~/.ssh/config` is never parsed.
- `run_host_connect` (src/main.rs) stays runtime-free before its fork: the name check it adds is a blocking `flock` and a directory read.
- Reattachment and new sessions are never a second code path: a switch is the target's ordinary attach exchange; a sibling is the ordinary `oxutrm host --serve`; a lobby becomes a session through the same `Door::register` a first connect uses.
- No wire compatibility: `PROTO_VERSION` becomes 3, no feature flags, no fallbacks. On disk, `meta.json`'s new `name` is `serde(default)`: a running host keeps its old binary, and its entries must keep parsing.
- Path-reading code takes its directory as a parameter; tests run on temp directories. A test registry whose socket path would pass macOS's 104 bytes lives under `/tmp`.
- UI fixtures use realistic values: 32-hex ids such as `3ff1218f5e0c4b7d9a1c2e3f40516273`, names such as `build` and `logs`, shells such as `/bin/bash`, sizes such as 120×40.
- English for code, comments and docs. Every commit ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- `make` caps parallelism at 4. The gate after every task: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?` -- expected `0`. Piping `make check` loses its exit code. Load-sensitive and pre-existing, re-run alone before concluding anything: `session::tests::a_moved_route_swaps_the_socket_and_the_session_survives_it`, the five STUN-punch loopback tests, `a_standby_is_found_over_the_first_links_control_stream`, `oxutrm-term host::tests::a_terminal_says_whether_output_is_still_waiting`. The loopback attach exchanges this plan adds (`listener`, `control`, `serve` and `session` tests that run a real exchange) are as load-sensitive as those: on a loaded or dual-homed machine they can miss their three-second ladder; they pass when re-run alone or with `--test-threads 1`.
- Tests that start a real sibling need the built binary: `cargo build --bin oxutrm` before `cargo test --bin oxutrm`. `make test` builds it, because the integration tests need it.

## Review Focus

The inputs the spec implies but does not spell out that are most likely to bite a user, most likely first. Each is pinned by a test in the task that owns the code.

1. **A session named in wide characters** (`構築ログ`): the selector's columns stay aligned, because a column is measured in cells, not characters. Pinned by `a_name_of_wide_characters_keeps_the_columns_aligned` (Task 11).
2. **The terminal resized while the client is in a lobby**: the lobby's blank screen follows the new size, so no cell of the old one is left to paint. Pinned by `a_lobbys_blank_screen_follows_its_clients_size` (Task 6).
3. **The binary a sibling would run is gone** (removed by an upgrade, a path that no longer resolves): `n` is refused with the reason, and the session it was asked of carries on. Pinned by `new_with_no_binary_to_run_is_refused_and_changes_nothing` (Task 8).
4. **The popup key typed while a name is being edited**: it neither closes the selector under the field nor becomes part of the name. Pinned by `the_popup_key_while_renaming_is_neither_a_close_nor_a_character` (Task 10).
5. **The shell exits on its own while the selector is open**: the client ends with the shell's status, exactly as without the selector -- only a kill from the selector leads to a lobby. Pinned by `a_shell_that_exits_while_the_selector_is_open_ends_the_client` (Task 12).

## File Structure

| File | Responsibility | Tasks |
|---|---|---|
| `src/host_session.rs` (new) | `HostSession`, split out of `session.rs`; the lobby (`term: Option<HostTerm>`), `Presence`, `LoopCmd`, `hang_up_shell`, `run_with_doors` | 1, 4, 5, 6 |
| `crates/oxutrm-proto/src/name.rs` (new) | `Name` and its rules | 2 |
| `crates/oxutrm-proto/src/open.rs` (new) | `Open`, `Request`, `Reply`, `Role`, `Attached`, `SessionEntry`, `OfferEntry`, `Answer`, line codecs | 2 |
| `crates/oxutrm-proto/src/ids.rs`, `signal.rs`, `lib.rs` | `SessionId` by parsing only; `Signal` purely the attach exchange; `Choice` with `Lobby`; `PROTO_VERSION` 3 | 2, 5, 7 |
| `crates/oxutrm-host/src/signalling.rs` | `read_line_async`, `write_line_async`, `read_answer_async` | 2 |
| `crates/oxutrm-host/src/registry.rs`, `attach.rs`, `lib.rs` | `name` in `SessionMeta`, `NamesLock`, `name_refusal`, atomic `meta.json`, `rename`, `offer()`; the `--list` name column | 3, 7 |
| `crates/oxutrm-term/src/pty.rs`, `host.rs`, `lib.rs` | `Start` (login shell, cwd), `hang_up`, `kill_group` | 4 |
| `src/door.rs` (new) | The dispatcher: Probe, Attach, Myself, Sessions, New, Kill, Rename, Switch; `Door::register`/`close` | 5, 6, 8 |
| `src/listener.rs` | `accept_doors` (task per connection) and the serial `serve_attaches` with primary pre-emption | 5 |
| `src/control.rs` | Control streams served through the door; the client's standby request and probe as `Open` lines | 5 |
| `src/serve.rs` | `Begin`, `Shell`, `run_process` (R13-R16 for every session process), `home_dir`, `own_binary`; test fixtures | 4, 5, 6, 7, 8 |
| `src/main.rs` | `host --serve [--name]`, `run_host_connect`'s choices, `--attach` relay's `Open`, usage | 5, 7 |
| `src/choose.rs` | The §3.1 table; the line picker is gone | 7 |
| `src/connect.rs` | `--name`; `establish_from`; lobby connect | 7, 8, 9 |
| `src/rebuild.rs` | `Aim` (session or lobby), `retarget` | 7, 9 |
| `src/switcher.rs` (new) | `Ask`, `Answered`, `ask` | 9 |
| `src/session.rs` | `MOVED_AWAY`, `QUIT`; the lobby flag; requests, answers, `moved`; the selector in layer 1 | 1, 9, 11, 12 |
| `src/activity.rs` | `Kind::Session` | 9 |
| `src/selector.rs` (new) | The selector's pure state and keys | 10 |
| `src/ui.rs` | `Field` (shared line editing), `Mode::Sessions`, `Command::Ask`, `s` | 10 |
| `src/view.rs` | `SessionsFacts` -> `SessionsView`; `s` enabled on a live link | 11 |
| `crates/oxutrm-client/src/popup.rs`, `lib.rs` | `SessionRow`, `SessionsView`, `Popup::Sessions`, `layout_sessions` | 11 |
| `CHANGES.md`, `README.md`, `docs/sessions.md` (new) | The changelog entries and the user docs | 13 |

Build order: **Part 1** -- Tasks 1-4, the ground (the split, the wire types, the registry, shells and their end). **Part 2** -- Tasks 5-8, the host (doors, the lobby, the connect choice, switch and sibling). **Part 3** -- Tasks 9-12, the client (requests, the selector's state, its view, the wiring). **Part 4** -- Task 13, docs and the hand test.

---

## Part 1 -- the ground

### Task 1: Split `HostSession` out of `src/session.rs`

`src/session.rs` is about 10.3k lines; D1 grows the host's loop (spec §5), so the host half moves out first, verbatim. Nothing behaves differently after this task.

**Files:**
- Create: `src/host_session.rs`
- Modify: `src/session.rs`, `src/main.rs`, `src/serve.rs`, `src/accept.rs` (tests), `src/attach_exchange.rs` (a doc link)

**Interfaces:**
- Consumes: nothing new.
- Produces: `crate::host_session::{HostSession, DETACH_AFTER}` (moved, unchanged), `HostSession`'s fields `input_rx`, `link`, `size` as `pub(crate)` for the session tests, `#[cfg(test)] HostSession::term_mut(&mut self) -> &mut HostTerm`; `crate::session::IDLE_POLL` becomes `pub(crate)`. `Turn`, `SHELL_EXITED`, `TAKEN_OVER`, `SWITCHED`, `SUPERSEDED`, `REBUILT` stay in `session.rs`.

- [ ] **Step 1: Create `src/host_session.rs` with this header**

````rust
//! The remote half's loop: the shell's pty, the authoritative screen, and the
//! link to whichever client is attached.
//!
//! Split out of `session.rs`, which keeps the client's loop and what the two
//! halves share (`Turn`, the close reasons). The session switcher grows this
//! side -- a lobby, a kill -- and does it here rather than in a module that
//! was already ten thousand lines long.

// The host daemon's stderr is not a terminal anybody is looking at, but the
// rule is the same as the client's so nothing here can start printing by
// accident. The one deliberate exception is `#[expect]`-ed at its call site.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};

use oxutrm_proto::{Frame, ScreenState, TermSize};
use oxutrm_sync::{InputState, Receiver};
use oxutrm_term::HostTerm;

use crate::link::{Link, SendOutcome};
use crate::session::{IDLE_POLL, SHELL_EXITED, SUPERSEDED, SWITCHED, TAKEN_OVER, Turn};
````

- [ ] **Step 2: Move the host's items, verbatim and in this order, from `src/session.rs` to after that header**

1. The doc comment and `pub const DETACH_AFTER: Duration = Duration::from_secs(30);` (it begins `/// How long the host keeps building frames for a client it has not heard from.`).
2. The doc comment `/// The remote half: owns the PTY and the authoritative screen.`, `pub struct HostSession { ... }` and the whole `impl HostSession { ... }`.
3. `fn close_as_exited(conn: &quinn::Connection, code: i32)` with its doc comment.
4. `enum HostWake { ... }` with its doc comment (`/// The host's half of the same idea. ...`).

Then, in the moved code, make three fields of the struct `pub(crate)` and add the test accessor before `screen()`:

````diff
 pub struct HostSession {
     term: HostTerm,
     screen_tx: oxutrm_sync::Sender<ScreenState>,
-    input_rx: Receiver<InputState>,
-    link: Link,
-    size: TermSize,
+    /// `pub(crate)` for the session tests, which drive both halves.
+    pub(crate) input_rx: Receiver<InputState>,
+    /// `pub(crate)` for the session tests, which drive both halves.
+    pub(crate) link: Link,
+    /// `pub(crate)` for the session tests, which drive both halves.
+    pub(crate) size: TermSize,
@@
+    /// The shell's terminal, for the session tests, which type into it and
+    /// read it directly.
+    #[cfg(test)]
+    pub(crate) fn term_mut(&mut self) -> &mut HostTerm {
+        &mut self.term
+    }
+
     /// The authoritative screen, for tests. Nothing in the session loop reads
````

- [ ] **Step 3: What `src/session.rs` keeps**

````diff
-//! The two loops that make a remote terminal.
+//! The two loops that make a remote terminal. The host's lives in
+//! `host_session.rs`; this module keeps the client's and what both share.
@@
 use oxutrm_sync::{InputState, Receiver, Sender, SyncState as _};
-use oxutrm_term::HostTerm;
@@
-const IDLE_POLL: Duration = Duration::from_millis(4);
+pub(crate) const IDLE_POLL: Duration = Duration::from_millis(4);
@@ mod tests {
     use super::*;
     // `use super::*` reaches the session module's own imports, not `crate`'s
     // other modules, so the route pace has to be named explicitly.
+    use crate::host_session::{DETACH_AFTER, HostSession};
     use crate::roam::ROUTE_PROBE_EVERY;
````

The tests reach the host's terminal through the accessor now. Every `host.term` in `src/session.rs`'s tests becomes `host.term_mut()` (eleven places; `host2.link` and `host.link` stay, the field is `pub(crate)`):

````bash
perl -pi -e 's/\bhost\.term\b(?!_)/host.term_mut()/g' src/session.rs
````

- [ ] **Step 4: The other modules**

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@
 mod egress;
+mod host_session;
 mod ladder;
--- a/src/serve.rs
+++ b/src/serve.rs
@@
-use crate::session::HostSession;
+use crate::host_session::HostSession;
--- a/src/accept.rs
+++ b/src/accept.rs
@@ mod tests {
+    use crate::host_session::HostSession;
     use crate::link::Link;
-    use crate::session::HostSession;
--- a/src/attach_exchange.rs
+++ b/src/attach_exchange.rs
@@
-    /// that builds one on a REATTACH is [`crate::session::HostSession::adopt`]
+    /// that builds one on a REATTACH is [`crate::host_session::HostSession::adopt`]
````

- [ ] **Step 5: Build, then the gate**

Run: `cargo build --all-targets --jobs 4` -- expected: no error and no warning (an unused import left in either file is a warning; remove it).

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?` -- expected `0`, with the test count unchanged from before the task (1241 on the branch this plan was made on).

- [ ] **Step 6: Commit**

````bash
git add src/host_session.rs src/session.rs src/main.rs src/serve.rs src/accept.rs src/attach_exchange.rs
git commit -m "refactor(host): split HostSession out of session.rs

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 2: The wire types: `Open`, `Request`, `Reply`, `SessionEntry`, `Name`; `SessionId` by parsing only

Spec §4.1 and §2.3, additively: the types exist and round-trip, nothing uses them yet. `SessionId` loses its `pub` field (nothing outside `ids.rs` built one) and gains serde as its hex string. The door's lines are JSON like the signalling channel's, told apart by their tags: a `Signal` has `"t"`, a `Reply` has `"r"`. Readers for door lines are strict -- a door's peer speaks the protocol from its first byte, so there is no preamble to skip -- and bounded by `MAX_SIGNAL_LINE` like a signal.

Fixed on the way: `SessionId::from_str` sliced the string by bytes and panicked on a multi-byte character at a pair boundary; it uses `str::get` now (`a_multibyte_character_at_a_pair_boundary_is_an_error_not_a_panic`).

**Files:**
- Create: `crates/oxutrm-proto/src/name.rs`, `crates/oxutrm-proto/src/open.rs`
- Modify: `crates/oxutrm-proto/src/ids.rs`, `crates/oxutrm-proto/src/lib.rs`, `crates/oxutrm-proto/src/signal.rs` (`check_version` becomes `pub(crate)`), `crates/oxutrm-host/src/signalling.rs`

**Interfaces:**
- Consumes: `oxutrm_proto::{Signal, ProtoError, TermSize, PROTO_VERSION, MAX_SIGNAL_LINE}` (existing).
- Produces:
  - `oxutrm_proto::SessionId` -- private field; `FromStr`/`Display` as 32 lowercase hex; serde as that string (`try_from = "String", into = "String"`); `short(&self) -> String` (first 8); `starts_with(&self, prefix: &str) -> bool` (case-sensitive: ids are lowercase hex, as on the wire).
  - `oxutrm_proto::Name` -- `parse(&str) -> Result<Name, String>` (1-24 characters, no control character, no space at either end, at least one character outside `[0-9a-f]`), `as_str()`, `Display`, serde through `parse`; `MAX_NAME: usize = 24`.
  - `oxutrm_proto::{Open { proto: u32, req: Request }, Open::new(Request) -> Open}`; `Request` (tag `"q"`): `Attach { role: Role }`, `Probe { nonce: u64 }`, `Sessions`, `Myself`, `Switch { to: SessionId }`, `New { name: Option<Name> }`, `Kill { id: SessionId }`, `Rename { id: SessionId, name: Option<Name> }`.
  - `Reply` (tag `"r"`, content `"v"`): `Sessions(Vec<SessionEntry>)`, `Entry(SessionEntry)`, `Done`, `Refused(String)`, `ProbeAck { nonce: u64 }`.
  - `Role { Primary, Standby }`; `Attached { No, Here, Elsewhere, Unknown, OtherVersion }`; `SessionEntry { id, name: Option<Name>, shell: String, created_unix: u64, size: TermSize, detachable: bool, attached: Attached, this: bool }`; `OfferEntry { id, name, shell, created_unix, size, detachable }` with `entry(self, Attached, this: bool) -> SessionEntry`.
  - `Answer` (untagged): `Reply(Reply)` | `Signal(Signal)`; `encode_line<T: Serialize>(&T) -> Result<Vec<u8>, ProtoError>`, `parse_line<T: DeserializeOwned>(&[u8]) -> Result<T, ProtoError>`, `parse_answer(&[u8]) -> Result<Answer, ProtoError>` (a `Signal` in it is version-checked).
  - `oxutrm_host::signalling::{read_line_async<R, T>(&mut R) -> Result<T, ProtoError>, write_line_async<W, T>(&mut W, &T) -> Result<(), ProtoError>, read_answer_async<R>(&mut R) -> Result<Answer, ProtoError>}`.

- [ ] **Step 1: Write the failing tests**

`crates/oxutrm-host/src/signalling.rs`:

````diff
--- a/crates/oxutrm-host/src/signalling.rs
+++ b/crates/oxutrm-host/src/signalling.rs
@@ -100,0 +162,46 @@
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use oxutrm_proto::{Open, Reply, Request};
+
+    #[tokio::test]
+    async fn a_door_line_round_trips_and_a_banner_is_not_skipped() {
+        let mut buf = Vec::new();
+        write_line_async(&mut buf, &Open::new(Request::Sessions))
+            .await
+            .unwrap();
+        let mut r = tokio::io::BufReader::new(buf.as_slice());
+        let back: Open = read_line_async(&mut r).await.unwrap();
+        assert_eq!(back.req, Request::Sessions);
+
+        // A door has no preamble: a banner line is malformed, not noise.
+        let mut r = tokio::io::BufReader::new(&b"Welcome to Ubuntu\n"[..]);
+        assert!(matches!(
+            read_line_async::<_, Open>(&mut r).await,
+            Err(ProtoError::Malformed(_))
+        ));
+    }
+
+    #[tokio::test]
+    async fn an_answer_is_a_reply_or_a_signal() {
+        let mut buf = Vec::new();
+        write_line_async(&mut buf, &Reply::Refused("gone".into()))
+            .await
+            .unwrap();
+        let mut r = tokio::io::BufReader::new(buf.as_slice());
+        assert!(matches!(
+            read_answer_async(&mut r).await.unwrap(),
+            Answer::Reply(Reply::Refused(_))
+        ));
+    }
+
+    #[tokio::test]
+    async fn an_endless_door_line_is_refused_at_the_limit() {
+        let endless = vec![b'x'; MAX_SIGNAL_LINE + 10];
+        let mut r = tokio::io::BufReader::new(endless.as_slice());
+        assert!(matches!(
+            read_line_async::<_, Open>(&mut r).await,
+            Err(ProtoError::SignalLineTooLong { .. })
+        ));
+    }
+}
````

`crates/oxutrm-proto/src/ids.rs`:

````diff
--- a/crates/oxutrm-proto/src/ids.rs
+++ b/crates/oxutrm-proto/src/ids.rs
@@ -47,4 +87,4 @@
 #[cfg(test)]
 mod tests {
     use super::*;
     use std::str::FromStr;
@@ -112,5 +152,33 @@ mod tests {
         assert!(seen.insert(SessionId([1; 16])));
         assert!(!seen.insert(SessionId([1; 16])));
         assert!(seen.insert(SessionId([2; 16])));
     }
+
+    #[test]
+    fn on_the_wire_it_is_the_hex_string_and_a_bad_one_fails_the_message() {
+        let id: SessionId = "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().expect("parse");
+        let json = serde_json::to_string(&id).expect("encode");
+        assert_eq!(json, "\"3ff1218f5e0c4b7d9a1c2e3f40516273\"");
+        let back: SessionId = serde_json::from_str(&json).expect("decode");
+        assert_eq!(back, id);
+        assert!(serde_json::from_str::<SessionId>("\"3ff1218f\"").is_err());
+    }
+
+    #[test]
+    fn short_is_the_first_eight_and_a_prefix_matches_in_either_case() {
+        let id: SessionId = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60".parse().expect("parse");
+        assert_eq!(id.short(), "a3f9c01e");
+        assert!(id.starts_with("a3f9"));
+        assert!(id.starts_with("A3F9C0"));
+        assert!(!id.starts_with("a3f8"));
+    }
+
+    #[test]
+    fn a_multibyte_character_at_a_pair_boundary_is_an_error_not_a_panic() {
+        // 30 ASCII bytes and one two-byte character: 32 bytes, and the
+        // character straddles the last pair.
+        let s = format!("{}\u{e9}", "0".repeat(30));
+        assert_eq!(s.len(), 32);
+        assert!(SessionId::from_str(&s).is_err());
+    }
 }
````

`crates/oxutrm-proto/src/name.rs` (new): this test module is the end of the file; the code above it comes in the implementation step. Create the file with it now.

````rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_accepted() {
        for ok in ["build", "logs", "web-1", "Ölberg", "a b", "x", "deadbeeX"] {
            assert_eq!(Name::parse(ok).expect(ok).as_str(), ok);
        }
        // Exactly the limit.
        assert!(Name::parse(&"x".repeat(MAX_NAME)).is_ok());
    }

    #[test]
    fn a_name_that_could_be_an_id_prefix_is_refused() {
        for hex in ["a3f9", "deadbeef", "0", "cafe", "3ff1218f5e0c"] {
            let why = Name::parse(hex).expect_err(hex);
            assert!(why.contains("session id"), "{hex}: {why}");
        }
        // Uppercase hex is not what an id looks like, so it is a name.
        assert!(Name::parse("CAFE").is_ok());
    }

    #[test]
    fn empty_long_padded_and_control_names_are_refused() {
        assert!(Name::parse("").is_err());
        assert!(Name::parse(&"x".repeat(MAX_NAME + 1)).is_err());
        assert!(Name::parse(" build").is_err());
        assert!(Name::parse("build ").is_err());
        assert!(Name::parse("bu\tild").is_err());
        assert!(Name::parse("bu\u{1b}[2Jild").is_err());
    }

    #[test]
    fn the_limit_is_in_characters_not_bytes() {
        let wide = "\u{e9}".repeat(MAX_NAME);
        assert!(wide.len() > MAX_NAME);
        assert!(Name::parse(&wide).is_ok());
    }

    #[test]
    fn the_wire_goes_through_parse() {
        let n = Name::parse("build").unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"build\"");
        assert_eq!(serde_json::from_str::<Name>(&json).unwrap(), n);
        assert!(serde_json::from_str::<Name>("\"cafe\"").is_err());
    }
}
````

`crates/oxutrm-proto/src/open.rs` (new): this test module is the end of the file; the code above it comes in the implementation step. Create the file with it now.

````rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostSpki, NatType, Psk};

    fn id() -> SessionId {
        "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().unwrap()
    }

    fn entry() -> SessionEntry {
        SessionEntry {
            id: id(),
            name: Some(Name::parse("build").unwrap()),
            shell: "/bin/bash".to_string(),
            created_unix: 1_791_450_840,
            size: TermSize {
                cols: 120,
                rows: 40,
            },
            detachable: true,
            attached: Attached::Elsewhere,
            this: false,
        }
    }

    fn round<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) {
        let line = encode_line(v).unwrap();
        assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
        let back: T = parse_line(&line).unwrap();
        assert_eq!(&back, v);
    }

    #[test]
    fn every_request_round_trips() {
        for req in [
            Request::Attach {
                role: Role::Primary,
            },
            Request::Attach {
                role: Role::Standby,
            },
            Request::Probe { nonce: 7 },
            Request::Sessions,
            Request::Myself,
            Request::Switch { to: id() },
            Request::New { name: None },
            Request::New {
                name: Some(Name::parse("logs").unwrap()),
            },
            Request::Kill { id: id() },
            Request::Rename {
                id: id(),
                name: Some(Name::parse("build").unwrap()),
            },
            Request::Rename {
                id: id(),
                name: None,
            },
        ] {
            round(&Open::new(req));
        }
    }

    #[test]
    fn every_reply_round_trips() {
        for reply in [
            Reply::Sessions(vec![entry()]),
            Reply::Sessions(vec![]),
            Reply::Entry(entry()),
            Reply::Done,
            Reply::Refused("no session a3f9c01e here".to_string()),
            Reply::ProbeAck { nonce: 9 },
        ] {
            round(&reply);
        }
    }

    #[test]
    fn an_open_carries_this_binarys_version() {
        let line = encode_line(&Open::new(Request::Myself)).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v["proto"], PROTO_VERSION);
    }

    #[test]
    fn a_reply_and_a_hello_are_told_apart() {
        let refused = encode_line(&Reply::Refused("gone".to_string())).unwrap();
        assert!(matches!(
            parse_answer(&refused).unwrap(),
            Answer::Reply(Reply::Refused(r)) if r == "gone"
        ));
        let hello = Signal::HostHello {
            proto: PROTO_VERSION,
            session_id: id().to_string(),
            attach_id: 1,
            cert_spki_sha256: HostSpki::new([7; 32]),
            psk: Psk::new([9; 32]),
            candidates: vec![],
            nat_type: NatType::Unknown,
            bound_port: 443,
            detachable: true,
            features: vec![],
        };
        let line = crate::encode_line(&hello).unwrap();
        assert!(matches!(
            parse_answer(&line).unwrap(),
            Answer::Signal(Signal::HostHello { .. })
        ));
    }

    #[test]
    fn a_hello_of_another_version_in_an_answer_is_a_version_mismatch() {
        let hello = Signal::HostHello {
            proto: PROTO_VERSION + 1,
            session_id: id().to_string(),
            attach_id: 1,
            cert_spki_sha256: HostSpki::new([7; 32]),
            psk: Psk::new([9; 32]),
            candidates: vec![],
            nat_type: NatType::Unknown,
            bound_port: 443,
            detachable: true,
            features: vec![],
        };
        let line = encode_line(&hello).unwrap();
        assert!(matches!(
            parse_answer(&line),
            Err(ProtoError::VersionMismatch { .. })
        ));
    }

    #[test]
    fn an_entry_with_a_bad_id_or_name_fails_to_parse() {
        let mut v = serde_json::to_value(entry()).unwrap();
        v["name"] = serde_json::json!("cafe");
        assert!(serde_json::from_value::<SessionEntry>(v).is_err());
        let mut v = serde_json::to_value(entry()).unwrap();
        v["id"] = serde_json::json!("../../etc");
        assert!(serde_json::from_value::<SessionEntry>(v).is_err());
    }

    #[test]
    fn an_offer_entry_becomes_a_session_entry() {
        let e = entry();
        let offer = OfferEntry {
            id: e.id,
            name: e.name.clone(),
            shell: e.shell.clone(),
            created_unix: e.created_unix,
            size: e.size,
            detachable: e.detachable,
        };
        assert_eq!(offer.entry(Attached::Elsewhere, false), e);
    }
}
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p oxutrm-proto --jobs 4 && cargo test -p oxutrm-host --lib --jobs 4 signalling`

Expected: compile errors: `Name`, `Open`, `Request`, `Reply`, `encode_line`, `read_line_async` and the rest are not defined, and `SessionId::short` does not exist.

- [ ] **Step 3: Implement**

`crates/oxutrm-host/src/signalling.rs`:

````diff
--- a/crates/oxutrm-host/src/signalling.rs
+++ b/crates/oxutrm-host/src/signalling.rs
@@ -19,9 +19,11 @@
 //! cursor, reporting `UnexpectedEof`. So "this line was noise" and "keep
 //! reading" become the same branch, and every other error — malformed JSON,
 //! version skew — propagates exactly as `oxutrm-proto` decided it should.
 
-use oxutrm_proto::{MAX_SIGNAL_LINE, ProtoError, Signal};
+use oxutrm_proto::{Answer, MAX_SIGNAL_LINE, ProtoError, Signal};
+use serde::Serialize;
+use serde::de::DeserializeOwned;
 use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt};
 
 /// Read one `Signal`, discarding whatever the remote login printed first.
 ///
@@ -96,4 +98,64 @@ where
     w.write_all(&buf).await.map_err(ProtoError::Io)?;
     w.flush().await.map_err(ProtoError::Io)?;
     Ok(())
 }
+
+/// One line, up to and including its newline, never more than
+/// [`MAX_SIGNAL_LINE`] bytes of it. End of stream before any byte is
+/// `UnexpectedEof`, as for [`read_signal_async`].
+async fn read_bounded_line<R>(r: &mut R) -> Result<Vec<u8>, ProtoError>
+where
+    R: AsyncBufReadExt + Unpin,
+{
+    let mut raw = Vec::new();
+    let taken = AsyncReadExt::take(&mut *r, MAX_SIGNAL_LINE as u64)
+        .read_until(b'\n', &mut raw)
+        .await?;
+    if taken == 0 {
+        return Err(ProtoError::Io(std::io::Error::new(
+            std::io::ErrorKind::UnexpectedEof,
+            "the stream closed before a line arrived",
+        )));
+    }
+    if taken == MAX_SIGNAL_LINE && raw.last() != Some(&b'\n') {
+        return Err(ProtoError::SignalLineTooLong {
+            limit: MAX_SIGNAL_LINE,
+        });
+    }
+    Ok(raw)
+}
+
+/// Read one door line -- an `Open`, a `Reply` -- strictly: the peer speaks
+/// the protocol from its first byte, so nothing is skipped (switcher spec
+/// §4.1). Bounded like a signal.
+pub async fn read_line_async<R, T>(r: &mut R) -> Result<T, ProtoError>
+where
+    R: AsyncBufReadExt + Unpin,
+    T: DeserializeOwned,
+{
+    let raw = read_bounded_line(r).await?;
+    oxutrm_proto::parse_line(&raw)
+}
+
+/// Read what answers an `Open` that may run an attach exchange: its first
+/// `Signal`, or a `Reply`.
+pub async fn read_answer_async<R>(r: &mut R) -> Result<Answer, ProtoError>
+where
+    R: AsyncBufReadExt + Unpin,
+{
+    let raw = read_bounded_line(r).await?;
+    oxutrm_proto::parse_answer(&raw)
+}
+
+/// Write one door line and flush it.
+pub async fn write_line_async<W, T>(w: &mut W, value: &T) -> Result<(), ProtoError>
+where
+    W: AsyncWrite + Unpin,
+    T: Serialize,
+{
+    let line = oxutrm_proto::encode_line(value)?;
+    w.write_all(&line).await.map_err(ProtoError::Io)?;
+    w.flush().await.map_err(ProtoError::Io)?;
+    Ok(())
+}
+
````

`crates/oxutrm-proto/src/ids.rs`:

````diff
--- a/crates/oxutrm-proto/src/ids.rs
+++ b/crates/oxutrm-proto/src/ids.rs
@@ -1,16 +1,39 @@
 //! The session identifier.
 
+use serde::{Deserialize, Serialize};
+
 use crate::ProtoError;
 
 /// 128-bit session identifier.
 ///
 /// `Display` and `FromStr` are 32 lowercase hex characters. That form is what
 /// travels in `Signal::HostHello.session_id`, what names the registry
 /// directory, and what a user types after `--attach`, so it is deliberately
 /// one representation and not three.
-#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
-pub struct SessionId(pub [u8; 16]);
+///
+/// The field is private: parsing is the only way to make one, so a value of
+/// this type is always a well-formed id. On the wire it is that same string
+/// (`serde(try_from, into)`), and a malformed one fails the whole message.
+#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
+#[serde(try_from = "String", into = "String")]
+pub struct SessionId([u8; 16]);
+
+impl SessionId {
+    /// The first eight characters, which is how the selector and the popup's
+    /// title name a session that has no name.
+    #[must_use]
+    pub fn short(&self) -> String {
+        self.to_string().chars().take(8).collect()
+    }
+
+    /// Whether `prefix` begins this id, compared as `Display` spells it:
+    /// lowercase, so an uppercase prefix matches too.
+    #[must_use]
+    pub fn starts_with(&self, prefix: &str) -> bool {
+        self.to_string().starts_with(&prefix.to_ascii_lowercase())
+    }
+}
 
 impl std::fmt::Display for SessionId {
     fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
         for byte in self.0 {
@@ -32,9 +55,13 @@ impl std::str::FromStr for SessionId {
             )));
         }
         let mut bytes = [0u8; 16];
         for (i, byte) in bytes.iter_mut().enumerate() {
-            let pair = &s[i * 2..i * 2 + 2];
+            // `get`, not indexing: a multi-byte character can make the
+            // length 32 while a pair boundary falls inside it.
+            let pair = s.get(i * 2..i * 2 + 2).ok_or_else(|| {
+                ProtoError::Malformed("session id must be 32 hex characters".to_string())
+            })?;
             *byte = u8::from_str_radix(pair, 16).map_err(|_| {
                 ProtoError::Malformed(format!(
                     "session id must be 32 hex characters, {pair:?} is not hex"
                 ))
@@ -43,4 +70,17 @@ impl std::str::FromStr for SessionId {
         Ok(SessionId(bytes))
     }
 }
 
+impl TryFrom<String> for SessionId {
+    type Error = ProtoError;
+    fn try_from(s: String) -> Result<Self, Self::Error> {
+        s.parse()
+    }
+}
+
+impl From<SessionId> for String {
+    fn from(id: SessionId) -> String {
+        id.to_string()
+    }
+}
+
````

`crates/oxutrm-proto/src/lib.rs`:

````diff
--- a/crates/oxutrm-proto/src/lib.rs
+++ b/crates/oxutrm-proto/src/lib.rs
@@ -93,8 +93,10 @@ pub mod cell;
 pub mod error;
 pub mod frame;
 pub mod ids;
 pub mod keymat;
+pub mod name;
+pub mod open;
 pub mod screen;
 pub mod signal;
 pub mod stream;
 pub mod text;
@@ -104,8 +106,13 @@ pub use cell::{Attrs, Cell, CellText, Color};
 pub use error::ApplyError;
 pub use frame::{FLAG_ZSTD, Frame};
 pub use ids::SessionId;
 pub use keymat::{ClientSpki, HostSpki, Psk, SpkiSha256, WIRE_KEY_B64_LEN, WIRE_KEY_LEN};
+pub use name::{MAX_NAME, Name};
+pub use open::{
+    Answer, Attached, OfferEntry, Open, Reply, Request, Role, SessionEntry, encode_line,
+    parse_answer, parse_line,
+};
 pub use screen::{Cursor, CursorShape, Modes, MouseMode, ScreenState};
 pub use signal::{Choice, MAX_SIGNAL_LINE, SessionSummary, Signal, read_signal, write_signal};
 pub use stream::{ControlMsg, ScrollbackReq};
 pub use text::{check_cell_text, check_title, fit_cell_text, fit_title, is_control_scalar};
````

`crates/oxutrm-proto/src/name.rs` (new) (the test module from the tests step follows it):

````rust
//! A session's name: what `--new --name`, `--attach <name>` and the
//! selector's `r` deal in.

use serde::{Deserialize, Serialize};

/// The longest name, in characters.
pub const MAX_NAME: usize = 24;

/// A session's name.
///
/// 1 to [`MAX_NAME`] characters, none of them a control character, no
/// whitespace at either end, and **at least one character outside
/// `[0-9a-f]`**. That last rule is what lets `--attach x` take a name or an
/// id prefix with no precedence between them: an id is lowercase hex, so a
/// name can never be read as one (spec §2.3).
///
/// The field is private: [`Name::parse`] is the only way to make one, and
/// the wire goes through it too (`serde(try_from)`), so a name that broke a
/// rule fails the whole message rather than reaching a session.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    /// `text` as a name, or why it cannot be one. The reason is for the
    /// user: the selector shows it under the list, `connect` prints it.
    pub fn parse(text: &str) -> Result<Name, String> {
        let n = text.chars().count();
        if n == 0 {
            return Err("a name cannot be empty".to_string());
        }
        if n > MAX_NAME {
            return Err(format!("a name has at most {MAX_NAME} characters"));
        }
        if text.chars().any(char::is_control) {
            return Err("a name cannot contain control characters".to_string());
        }
        if text.trim() != text {
            return Err("a name cannot start or end with a space".to_string());
        }
        if text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return Err(
                "a name needs a character outside 0-9 and a-f, or it reads as a session id"
                    .to_string(),
            );
        }
        Ok(Name(text.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Name {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Name::parse(&s)
    }
}

impl From<Name> for String {
    fn from(n: Name) -> String {
        n.0
    }
}

````

`crates/oxutrm-proto/src/open.rs` (new) (the test module from the tests step follows it):

````rust
//! The first line of every door into a session process (switcher spec §4.1).
//!
//! A session process has two doors: the QUIC control stream from its own
//! client, and its Unix socket, from a sibling session or from
//! `oxutrm host --attach` over ssh. Both read one [`Open`] first and dispatch
//! it in one place. **Every request has exactly one reply before anything else
//! on the stream**: a [`Reply`] -- or, for a request that runs the attach
//! exchange (`Attach`, `Switch`, a sibling `New`), the exchange's own first
//! line, a `Signal::HostHello`.
//!
//! JSON lines, like the signalling channel, and told apart from it by their
//! tags: a [`Signal`] carries `"t"`, a [`Reply`] carries `"r"`, so a reader
//! that may get either ([`Answer`]) needs no guess.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{Name, PROTO_VERSION, ProtoError, SessionId, Signal, TermSize};

/// What an attach is for.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Role {
    /// Adopt now; newest attach wins (recovery spec §6).
    Primary,
    /// Park until the client's first sync frame on it (standby spec §3.5).
    Standby,
}

/// The first line on a door.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Open {
    /// [`PROTO_VERSION`] of the asker. A door refuses another version with a
    /// reason rather than misreading what follows.
    pub proto: u32,
    pub req: Request,
}

impl Open {
    /// `req`, at this binary's protocol version.
    #[must_use]
    pub fn new(req: Request) -> Open {
        Open {
            proto: PROTO_VERSION,
            req,
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "q")]
pub enum Request {
    /// Run the attach exchange over this stream; its `HostHello` is the reply.
    Attach { role: Role },
    /// Does this link answer? Replied to with [`Reply::ProbeAck`].
    Probe { nonce: u64 },
    /// The host's sessions, as [`Reply::Sessions`].
    Sessions,
    /// This session, as [`Reply::Entry`]. Asked of a sibling over its socket.
    Myself,
    /// Relayed to `to`'s socket as `Attach { Primary }`; the target's attach
    /// exchange follows.
    Switch { to: SessionId },
    /// From a lobby: it starts its shell and becomes the session, replying
    /// [`Reply::Entry`]. From a session: a sibling is started and its attach
    /// exchange follows.
    New { name: Option<Name> },
    /// Answered [`Reply::Done`] once the shell is reaped and the entry gone.
    Kill { id: SessionId },
    /// Answered [`Reply::Entry`]; `None` clears the name.
    Rename { id: SessionId, name: Option<Name> },
}

/// The one answer to a request that does not run an attach exchange.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(tag = "r", content = "v")]
pub enum Reply {
    Sessions(Vec<SessionEntry>),
    Entry(SessionEntry),
    Done,
    /// Any request; the reason is for the user.
    Refused(String),
    /// The answer to [`Request::Probe`].
    ProbeAck {
        nonce: u64,
    },
}

/// Whether a session has a client, as seen by whoever asked.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Attached {
    /// Detached.
    No,
    /// Attached to the asker's own client: the asker's own session.
    Here,
    /// Attached to some other client.
    Elsewhere,
    /// The sibling did not answer in time; listed from its `meta.json`.
    Unknown,
    /// The sibling runs a binary with another protocol version.
    OtherVersion,
}

/// One session, as the selector lists it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct SessionEntry {
    pub id: SessionId,
    pub name: Option<Name>,
    pub shell: String,
    pub created_unix: u64,
    pub size: TermSize,
    pub detachable: bool,
    pub attached: Attached,
    /// The session the asker is in.
    pub this: bool,
}

/// One session, as the ssh offer lists it: what `meta.json` says, and
/// nothing a sibling would have to be asked (spec §4.2). Built with blocking
/// I/O before `run_host_connect`'s fork, where no runtime may exist.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct OfferEntry {
    pub id: SessionId,
    pub name: Option<Name>,
    pub shell: String,
    pub created_unix: u64,
    pub size: TermSize,
    pub detachable: bool,
}

impl OfferEntry {
    /// The offer's facts, plus what only a live answer can say.
    #[must_use]
    pub fn entry(self, attached: Attached, this: bool) -> SessionEntry {
        SessionEntry {
            id: self.id,
            name: self.name,
            shell: self.shell,
            created_unix: self.created_unix,
            size: self.size,
            detachable: self.detachable,
            attached,
            this,
        }
    }
}

/// What may come back on a stream after an `Open` that can run an attach
/// exchange: the exchange's first line, or a refusal.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Answer {
    Reply(Reply),
    Signal(Signal),
}

/// One value as one JSON line, newline included.
pub fn encode_line<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtoError> {
    let mut line =
        serde_json::to_vec(value).map_err(|e| ProtoError::Malformed(format!("encoding: {e}")))?;
    line.push(b'\n');
    Ok(line)
}

/// One JSON line as a `T`, strictly: a door's first line is from a peer
/// that speaks the protocol from its first byte, so there is no preamble to
/// skip and anything else is malformed.
pub fn parse_line<T: DeserializeOwned>(raw: &[u8]) -> Result<T, ProtoError> {
    let text = std::str::from_utf8(raw)
        .map_err(|_| ProtoError::Malformed("a line that is not UTF-8".to_string()))?;
    serde_json::from_str(text.trim()).map_err(|e| ProtoError::Malformed(format!("json: {e}")))
}

/// One line as an [`Answer`]. A `Signal` in it is version-checked exactly as
/// `read_signal` checks one.
pub fn parse_answer(raw: &[u8]) -> Result<Answer, ProtoError> {
    let answer: Answer = parse_line(raw)?;
    if let Answer::Signal(s) = &answer {
        crate::signal::check_version(s)?;
    }
    Ok(answer)
}

````

`crates/oxutrm-proto/src/signal.rs`:

````diff
--- a/crates/oxutrm-proto/src/signal.rs
+++ b/crates/oxutrm-proto/src/signal.rs
@@ -202,9 +202,9 @@ fn looks_like_signal(line: &str) -> bool {
 }
 
 /// Hard version check (spec §4.2): a mismatch is a loud failure, never a
 /// downgrade and never a warning. Messages that carry no version pass.
-fn check_version(s: &Signal) -> Result<(), ProtoError> {
+pub(crate) fn check_version(s: &Signal) -> Result<(), ProtoError> {
     match s.proto() {
         Some(peer) if peer != PROTO_VERSION => Err(ProtoError::VersionMismatch {
             peer,
             ours: PROTO_VERSION,
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p oxutrm-proto --jobs 4 && cargo test -p oxutrm-host --lib --jobs 4 signalling`

Expected: all pass, the new `ids`, `name`, `open` and `signalling` tests among them.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add crates/oxutrm-host/src/signalling.rs crates/oxutrm-proto/src/ids.rs crates/oxutrm-proto/src/lib.rs crates/oxutrm-proto/src/name.rs crates/oxutrm-proto/src/open.rs crates/oxutrm-proto/src/signal.rs
git commit -m "feat(proto): Open, Request, Reply, SessionEntry, Name; SessionId by parsing only

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 3: The registry: names under `names.lock`, atomic `meta.json`, the `--list` name column

Spec §2.3 and §5. `SessionMeta` gains `name: Option<String>` with `serde(default)`. **A plain string on disk, checked as a `Name` only where it is shown or sent** (`SessionMeta::name`, `SessionMeta::offer`): an entry whose name some later rule refuses must still parse, or the session would vanish from `--list` while it runs (a judgement call; the spec says "stored as `name`"). A name is taken under an exclusive `flock` on `<registry>/names.lock`, against a fresh `Registry::list_in`, and `meta.json` is written while the lock is held -- by `RegistryGuard::register_in` (when the meta has a name) and by `RegistryGuard::rename`. `RegistryGuard::update` writes a temporary file and renames it, so a reader never sees a torn file.

Every `SessionMeta { .. }` literal in the workspace gains `name: None` -- the diffs below show each one; `crates/oxutrm-host/tests/no_keys_on_disk.rs` names `name` as the ninth field deliberately.

`NamesLock::take` is a blocking `flock`. It is held for one directory read and one small write, and is called from door tasks later: accepted as it is, rather than moved to `spawn_blocking`.

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`, `crates/oxutrm-host/src/lib.rs`, `crates/oxutrm-host/src/attach.rs`, `crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs`
- Modify (the `name: None` literals): `src/attach_exchange.rs`, `src/connect.rs`, `src/serve.rs`, `tests/host_attach.rs`, `crates/oxutrm-host/tests/{attach,detachable,ladder,no_keys_on_disk,registry,registry_roots}.rs`

**Interfaces:**
- Consumes: `oxutrm_proto::{Name, OfferEntry}` (Task 2).
- Produces:
  - `SessionMeta.name: Option<String>` (`#[serde(default)]`); `SessionMeta::name(&self) -> Option<Name>`; `SessionMeta::offer(&self) -> Option<OfferEntry>` (`None` for an id that does not parse).
  - `oxutrm_host::{NAMES_LOCK, NamesLock, name_refusal}`: `NamesLock::take(dir: &Path) -> anyhow::Result<NamesLock>`; `name_refusal(dir: &Path, name: &str, id: &str) -> anyhow::Result<Option<String>>` (the reason when a live session other than `id` has `name`).
  - `RegistryGuard::register_in` refuses a taken name; `RegistryGuard::update` is atomic; `RegistryGuard::rename(&self, meta: &mut SessionMeta, name: Option<String>) -> anyhow::Result<()>` (`meta` changes only on success).
  - `format_session_list` prints the name (or `-`) after the id, in a column as wide as the longest name.

- [ ] **Step 1: Write the failing tests**

`crates/oxutrm-host/src/attach.rs`:

````diff
--- a/crates/oxutrm-host/src/attach.rs
+++ b/crates/oxutrm-host/src/attach.rs
@@ -195,8 +204,9 @@ mod tests {
                 rows: 30,
             },
             detachable: true,
             boot: Some("boot-token".to_owned()),
+            name: None,
         };
         let summaries = summarize(std::slice::from_ref(&meta));
         assert_eq!(summaries.len(), 1);
         assert_eq!(summaries[0].session_id, meta.session_id);
````

`crates/oxutrm-host/src/registry.rs`:

````diff
--- a/crates/oxutrm-host/src/registry.rs
+++ b/crates/oxutrm-host/src/registry.rs
@@ -1076,8 +1189,9 @@ mod tests {
             shell: "/bin/sh".to_owned(),
             size: TermSize { cols: 80, rows: 24 },
             detachable: true,
             boot: boot.map(str::to_owned),
+            name: None,
         }
     }
 
     #[test]
````

`crates/oxutrm-host/tests/attach.rs`:

````diff
--- a/crates/oxutrm-host/tests/attach.rs
+++ b/crates/oxutrm-host/tests/attach.rs
@@ -37,8 +37,9 @@ fn meta(id: &str, pid: u32) -> SessionMeta {
         shell: "/bin/bash".to_string(),
         size: TermSize { cols: 80, rows: 24 },
         detachable: true,
         boot: None,
+        name: None,
     }
 }
 
 /// A `Psk` as it appears on the wire, without the JSON quotes.
@@ -382,4 +383,17 @@ fn the_listing_shows_detachability_rather_than_implying_it() {
 #[test]
 fn an_empty_listing_is_a_sentence_not_a_blank() {
     assert!(format_session_list(&[]).contains("no live oxutrm sessions"));
 }
+
+#[test]
+fn the_listing_has_a_name_column_that_keeps_the_rest_aligned() {
+    let mut build = meta("3ff1218f5e0c4b7d9a1c2e3f40516273", 4242);
+    build.name = Some("build".to_string());
+    let unnamed = meta("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60", 4243);
+    let text = format_session_list(&[build, unnamed]);
+    let lines: Vec<&str> = text.lines().collect();
+    assert!(lines[0].contains("  build  "), "{}", lines[0]);
+    assert!(lines[1].contains("  -      "), "{}", lines[1]);
+    // The pid column starts at the same place on both lines.
+    assert_eq!(lines[0].find("4242"), lines[1].find("4243"), "{text}");
+}
````

`crates/oxutrm-host/tests/detachable.rs`:

````diff
--- a/crates/oxutrm-host/tests/detachable.rs
+++ b/crates/oxutrm-host/tests/detachable.rs
@@ -23,8 +23,9 @@ fn meta(id: &str) -> SessionMeta {
         // The host's intent, before ICE has nominated anything. Deliberately
         // optimistic, which is why it must not be trusted.
         detachable: true,
         boot: None,
+        name: None,
     }
 }
 
 #[test]
````

`crates/oxutrm-host/tests/ladder.rs`:

````diff
--- a/crates/oxutrm-host/tests/ladder.rs
+++ b/crates/oxutrm-host/tests/ladder.rs
@@ -26,8 +26,9 @@ fn meta() -> SessionMeta {
         shell: "/bin/bash".to_string(),
         size: TermSize { cols: 80, rows: 24 },
         detachable: true,
         boot: None,
+        name: None,
     }
 }
 
 // ---------------------------------------------------------------------------
````

`crates/oxutrm-host/tests/no_keys_on_disk.rs`:

````diff
--- a/crates/oxutrm-host/tests/no_keys_on_disk.rs
+++ b/crates/oxutrm-host/tests/no_keys_on_disk.rs
@@ -72,8 +72,9 @@ fn nothing_a_session_writes_contains_its_key_material() {
             rows: 40,
         },
         detachable: true,
         boot: None,
+        name: None,
     };
     let guard = RegistryGuard::register_in(&root, &meta).expect("register");
 
     // Reattaching bumps the generation and rewrites meta.json. Do that too, so
@@ -128,9 +129,9 @@ fn nothing_a_session_writes_contains_its_key_material() {
 /// The forward-looking half. The scan above can only find secrets that already
 /// exist; this fails the moment somebody adds a field to `SessionMeta`,
 /// whatever it is called, so the addition gets looked at rather than shipped.
 #[test]
-fn meta_json_holds_exactly_the_eight_fields_it_is_allowed_to() {
+fn meta_json_holds_exactly_the_nine_fields_it_is_allowed_to() {
     let tmp = tempfile::tempdir().expect("tempdir");
     let root = Registry::dir_at(tmp.path());
     let meta = SessionMeta {
         session_id: "9999aaaabbbbccccddddeeeeffff0000".to_string(),
@@ -143,8 +144,11 @@ fn meta_json_holds_exactly_the_eight_fields_it_is_allowed_to() {
         // `boot` was looked at deliberately here: it is a boot/machine
         // identifier (a Linux boot UUID or a macOS boot timestamp), never key
         // material, so it belongs in this list rather than being excluded.
         boot: None,
+        // `name` was looked at deliberately too: the user's own label for
+        // the session (switcher spec §2.3), never key material.
+        name: None,
     };
     let guard = RegistryGuard::register_in(&root, &meta).expect("register");
 
     let text = std::fs::read_to_string(guard.meta_path()).expect("read meta");
@@ -159,8 +163,9 @@ fn meta_json_holds_exactly_the_eight_fields_it_is_allowed_to() {
             "attach_id",
             "boot",
             "created_unix",
             "detachable",
+            "name",
             "pid",
             "session_id",
             "shell",
             "size",
````

`crates/oxutrm-host/tests/registry.rs`:

````diff
--- a/crates/oxutrm-host/tests/registry.rs
+++ b/crates/oxutrm-host/tests/registry.rs
@@ -19,8 +19,9 @@ fn meta(id: &str, pid: u32) -> SessionMeta {
         shell: "/bin/bash".to_string(),
         size: TermSize { cols: 80, rows: 24 },
         detachable: true,
         boot: None,
+        name: None,
     }
 }
 
 fn mode_of(path: &Path) -> u32 {
@@ -59,8 +60,9 @@ fn plant(root: &Path, id: &str, pid: u32, created: u64) -> std::path::PathBuf {
         shell: "/bin/bash".to_string(),
         size: TermSize { cols: 80, rows: 24 },
         detachable: true,
         boot: None,
+        name: None,
     };
     std::fs::write(dir.join(META_FILE), serde_json::to_vec(&m).unwrap()).expect("write meta");
     std::fs::write(dir.join("sock"), b"").expect("write sock");
     dir
@@ -401,4 +403,100 @@ fn two_session_identifiers_are_not_the_same() {
     let a = oxutrm_host::registry::new_session_id().expect("the system CSPRNG");
     let b = oxutrm_host::registry::new_session_id().expect("the system CSPRNG");
     assert_ne!(a, b, "session identifiers are not being drawn at random");
 }
+
+// ---------------------------------------------------------------------------
+// Names (switcher spec §2.3)
+// ---------------------------------------------------------------------------
+
+fn named(id: &str, name: Option<&str>) -> SessionMeta {
+    let mut m = meta(id, std::process::id());
+    m.name = name.map(str::to_string);
+    m
+}
+
+#[test]
+fn a_name_is_registered_and_a_second_session_cannot_take_it() {
+    let dir = tempfile::tempdir().unwrap();
+    let _build = RegistryGuard::register_in(
+        dir.path(),
+        &named("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("build")),
+    )
+    .expect("the first build");
+    let err = RegistryGuard::register_in(
+        dir.path(),
+        &named("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60", Some("build")),
+    )
+    .err()
+    .expect("a second build is refused");
+    assert!(err.to_string().contains("taken"), "{err}");
+    // And it left no directory behind.
+    assert!(!dir.path().join("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60").exists());
+}
+
+#[test]
+fn a_rename_is_written_refused_when_taken_and_cleared_by_none() {
+    let dir = tempfile::tempdir().unwrap();
+    let _logs = RegistryGuard::register_in(
+        dir.path(),
+        &named("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("logs")),
+    )
+    .unwrap();
+    let mut me = named("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60", None);
+    let guard = RegistryGuard::register_in(dir.path(), &me).unwrap();
+
+    guard.rename(&mut me, Some("build".into())).expect("free");
+    assert_eq!(me.name.as_deref(), Some("build"));
+    let on_disk: SessionMeta =
+        serde_json::from_slice(&std::fs::read(guard.meta_path()).unwrap()).unwrap();
+    assert_eq!(on_disk.name.as_deref(), Some("build"));
+
+    // Its own name again is no conflict.
+    guard
+        .rename(&mut me, Some("build".into()))
+        .expect("its own");
+
+    let err = guard
+        .rename(&mut me, Some("logs".into()))
+        .expect_err("taken by the other");
+    assert!(err.to_string().contains("taken"), "{err}");
+    assert_eq!(
+        me.name.as_deref(),
+        Some("build"),
+        "a refusal changes nothing"
+    );
+
+    guard.rename(&mut me, None).expect("cleared");
+    let on_disk: SessionMeta =
+        serde_json::from_slice(&std::fs::read(guard.meta_path()).unwrap()).unwrap();
+    assert_eq!(on_disk.name, None);
+}
+
+#[test]
+fn an_update_leaves_no_temporary_file_and_the_lock_is_not_an_entry() {
+    let dir = tempfile::tempdir().unwrap();
+    let m = named("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("build"));
+    let guard = RegistryGuard::register_in(dir.path(), &m).unwrap();
+    guard.update(&m).unwrap();
+    let names: Vec<_> = std::fs::read_dir(guard.dir())
+        .unwrap()
+        .map(|e| e.unwrap().file_name().into_string().unwrap())
+        .collect();
+    assert_eq!(names, vec![META_FILE.to_string()], "{names:?}");
+    assert!(dir.path().join(oxutrm_host::NAMES_LOCK).exists());
+    assert_eq!(Registry::list_in(dir.path()).unwrap().len(), 1);
+}
+
+#[test]
+fn an_entry_written_without_a_name_parses_and_a_bad_name_is_not_shown() {
+    let text = r#"{"session_id":"3ff1218f5e0c4b7d9a1c2e3f40516273","attach_id":1,"pid":1,
+        "created_unix":1,"shell":"/bin/bash","size":{"cols":120,"rows":40},"detachable":true}"#;
+    let m: SessionMeta = serde_json::from_str(text).expect("an old entry parses");
+    assert_eq!(m.name, None);
+    let mut m = m;
+    m.name = Some("cafe".to_string());
+    assert_eq!(m.name(), None, "an all-hex name is not a name");
+    let offer = m.offer().expect("an offer");
+    assert_eq!(offer.name, None);
+    assert_eq!(offer.id.to_string(), m.session_id);
+}
````

`crates/oxutrm-host/tests/registry_roots.rs`:

````diff
--- a/crates/oxutrm-host/tests/registry_roots.rs
+++ b/crates/oxutrm-host/tests/registry_roots.rs
@@ -368,8 +368,9 @@ async fn a_session_stays_discoverable_after_the_runtime_directory_is_destroyed()
         shell: "/bin/bash".to_string(),
         size: TermSize { cols: 80, rows: 24 },
         detachable: true,
         boot: None,
+        name: None,
     };
     let guard = RegistryGuard::register_in(&root, &meta).expect("register");
     let sock = guard.socket_path();
     check_socket_path_length(&sock).expect("short enough");
````

`src/attach_exchange.rs`:

````diff
--- a/src/attach_exchange.rs
+++ b/src/attach_exchange.rs
@@ -399,8 +399,9 @@ pub(crate) mod fixtures {
             shell: "/bin/sh".to_owned(),
             size: TermSize { cols: 80, rows: 24 },
             detachable: false,
             boot: None,
+            name: None,
         }
     }
 
     /// A configuration that reaches no network at all.
@@ -707,8 +708,9 @@ mod tests {
             shell: "/bin/sh".to_owned(),
             size: TermSize { cols: 80, rows: 24 },
             detachable: false,
             boot: None,
+            name: None,
         };
         let attach = oxutrm_host::begin_attach(&mut meta, HostSpki::new([7u8; 32]))
             .expect("fresh key material");
         (meta, attach)
````

`src/connect.rs`:

````diff
--- a/src/connect.rs
+++ b/src/connect.rs
@@ -805,8 +805,9 @@ mod tests {
             shell: "/bin/sh".to_owned(),
             size: TermSize { cols: 80, rows: 24 },
             detachable: true,
             boot: oxutrm_host::boot_token(),
+            name: None,
         };
 
         let cfg = test_config();
         let host = tokio::spawn({
````

`tests/host_attach.rs`:

````diff
--- a/tests/host_attach.rs
+++ b/tests/host_attach.rs
@@ -59,8 +59,9 @@ fn a_session(id: &str) -> SessionMeta {
         shell: "/bin/bash".to_owned(),
         size: TermSize { cols: 80, rows: 24 },
         detachable: true,
         boot: None,
+        name: None,
     }
 }
 
 /// A mistyped id must reach `connect_to_session`'s error — the one that lists
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p oxutrm-host --jobs 4`

Expected: compile errors: `SessionMeta` has no field `name`, `NAMES_LOCK`, `RegistryGuard::rename` and `SessionMeta::offer` do not exist.

- [ ] **Step 3: Implement**

`crates/oxutrm-host/src/attach.rs`:

````diff
--- a/crates/oxutrm-host/src/attach.rs
+++ b/crates/oxutrm-host/src/attach.rs
@@ -136,13 +136,21 @@ where
 pub fn format_session_list(sessions: &[SessionMeta]) -> String {
     if sessions.is_empty() {
         return "no live oxutrm sessions on this host\n".to_string();
     }
+    // The name column is as wide as the longest name, so the ids and the
+    // rest stay aligned whatever the names are.
+    let width = sessions
+        .iter()
+        .map(|m| m.name.as_deref().map_or(1, |n| n.chars().count()))
+        .max()
+        .unwrap_or(1);
     let mut out = String::new();
     for m in sessions {
         out.push_str(&format!(
-            "{}  {:>7}  {:>3}x{:<3}  attach {}  {}  {}\n",
+            "{}  {:<width$}  {:>7}  {:>3}x{:<3}  attach {}  {}  {}\n",
             m.session_id,
+            m.name.as_deref().unwrap_or("-"),
             m.pid,
             m.size.cols,
             m.size.rows,
             m.attach_id,
@@ -151,8 +159,9 @@ pub fn format_session_list(sessions: &[SessionMeta]) -> String {
                 "detachable"
             } else {
                 "NOT detachable (dies with its ssh)"
             },
+            width = width,
         ));
     }
     out
 }
````

`crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs`:

````diff
--- a/crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs
+++ b/crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs
@@ -166,8 +166,9 @@ fn run_split(report: &str, held: &[(String, i32)], gate: bool, keep_open: bool)
         shell: "/bin/sh".to_string(),
         size: oxutrm_proto::TermSize { cols: 80, rows: 24 },
         detachable: false,
         boot: oxutrm_host::boot_token(),
+        name: None,
     };
     let permit = oxutrm_host::settle_detachability(&mut meta, oxutrm_proto::Rung::StunPunch)
         .expect("a nominated non-tunnel rung must yield a permit");
     oxutrm_host::sever_from_ssh(detached, permit).expect("sever_from_ssh");
````

`crates/oxutrm-host/src/lib.rs`:

````diff
--- a/crates/oxutrm-host/src/lib.rs
+++ b/crates/oxutrm-host/src/lib.rs
@@ -52,13 +52,14 @@ pub mod ssh;
 pub use daemon::{Detached, FD_DIRS, daemonize, daemonize_session, detach_process, sever_from_ssh};
 pub use keys::{Attach, AttachKeys, DetachPermit, PSK_LEN, begin_attach, settle_detachability};
 pub use ladder::LadderPlan;
 pub use registry::{
-    DirVerdict, META_FILE, PID_REUSE_SLACK_SECS, PrepareError, REGISTRY_SUBDIR, Registry,
-    RegistryGuard, RegistryRoot, RegistryRootKind, RootEnv, SOCK_FILE, SessionMeta, boot_token,
-    check_socket_path_length, detachable_for_rung, dir_verdict, entry_is_stale, linger_enabled,
-    new_session_id, now_unix, pid_alive, prepare_root, process_start_unix, read_root_env,
-    registry_root_candidates, resolve_registry_root, walk_candidates,
+    DirVerdict, META_FILE, NAMES_LOCK, NamesLock, PID_REUSE_SLACK_SECS, PrepareError,
+    REGISTRY_SUBDIR, Registry, RegistryGuard, RegistryRoot, RegistryRootKind, RootEnv, SOCK_FILE,
+    SessionMeta, boot_token, check_socket_path_length, detachable_for_rung, dir_verdict,
+    entry_is_stale, linger_enabled, name_refusal, new_session_id, now_unix, pid_alive,
+    prepare_root, process_start_unix, read_root_env, registry_root_candidates,
+    resolve_registry_root, walk_candidates,
 };
 
 // There is deliberately no `transport::Path` here any more. It existed to hold
 // one rule — that `max_datagram_size()` returning `None` means "the peer turned
````

`crates/oxutrm-host/src/registry.rs`:

````diff
--- a/crates/oxutrm-host/src/registry.rs
+++ b/crates/oxutrm-host/src/registry.rs
@@ -25,8 +25,11 @@ use serde::{Deserialize, Serialize};
 
 pub const REGISTRY_SUBDIR: &str = "oxutrm";
 pub const META_FILE: &str = "meta.json";
 pub const SOCK_FILE: &str = "sock";
+/// The lock every name change is made under (switcher spec §2.3). A plain
+/// file beside the session directories, so `list_in` passes it by.
+pub const NAMES_LOCK: &str = "names.lock";
 
 /// What `--list` shows, and what `--attach` needs to find a session again.
 ///
 /// Everything here is safe to write down. Nothing here is a secret.
@@ -57,8 +60,17 @@ pub struct SessionMeta {
     /// parsing. `serde(default)` is what makes the second true; without it an
     /// upgrade would strand every running session behind a parse error.
     #[serde(default)]
     pub boot: Option<String>,
+    /// The session's name, if it has one.
+    ///
+    /// A plain string on disk, and checked as an `oxutrm_proto::Name` only
+    /// where it is shown or sent ([`SessionMeta::name`]): an entry whose name
+    /// some later rule refuses must still parse, or the session would vanish
+    /// from `--list` while it runs. `serde(default)` because a running host
+    /// keeps its old binary, and its entries have no name at all.
+    #[serde(default)]
+    pub name: Option<String>,
 }
 
 impl SessionMeta {
     /// Settle detachability from the rung ICE actually nominated, and return
@@ -71,8 +83,69 @@ impl SessionMeta {
     pub fn set_detachable(&mut self, rung: Rung) -> bool {
         self.detachable = detachable_for_rung(rung);
         self.detachable
     }
+
+    /// The name, if it has one that is still a valid name.
+    #[must_use]
+    pub fn name(&self) -> Option<oxutrm_proto::Name> {
+        self.name
+            .as_deref()
+            .and_then(|n| oxutrm_proto::Name::parse(n).ok())
+    }
+
+    /// What the ssh offer says about this session; `None` for an entry
+    /// whose id is not an id, which no session of ours writes.
+    #[must_use]
+    pub fn offer(&self) -> Option<oxutrm_proto::OfferEntry> {
+        Some(oxutrm_proto::OfferEntry {
+            id: self.session_id.parse().ok()?,
+            name: self.name(),
+            shell: self.shell.clone(),
+            created_unix: self.created_unix,
+            size: self.size,
+            detachable: self.detachable,
+        })
+    }
+}
+
+/// An exclusive lock on the registry's [`NAMES_LOCK`], held until dropped.
+///
+/// Every name is taken under it: a fresh read of the registry, the check,
+/// and the `meta.json` that records the name, in that order, so two sessions
+/// cannot both take one. `flock`, so a process that dies holding it lets go.
+pub struct NamesLock {
+    _file: std::fs::File,
+}
+
+impl NamesLock {
+    /// Wait for the lock on `dir`'s names. Blocks: it is held only for one
+    /// registry read and one small write, by whichever session is naming.
+    pub fn take(dir: &Path) -> anyhow::Result<NamesLock> {
+        use std::os::unix::fs::OpenOptionsExt;
+        create_private_dir(dir)?;
+        let path = dir.join(NAMES_LOCK);
+        let file = std::fs::OpenOptions::new()
+            .read(true)
+            .write(true)
+            .create(true)
+            .truncate(false)
+            .mode(0o600)
+            .open(&path)
+            .with_context(|| format!("opening {}", path.display()))?;
+        rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
+            .with_context(|| format!("locking {}", path.display()))?;
+        Ok(NamesLock { _file: file })
+    }
+}
+
+/// Why `name` cannot be taken by session `id` in `dir`, if it cannot: a live
+/// session other than `id` has it. Call it holding the [`NamesLock`].
+pub fn name_refusal(dir: &Path, name: &str, id: &str) -> anyhow::Result<Option<String>> {
+    let taken = Registry::list_in(dir)?
+        .iter()
+        .any(|m| m.session_id != id && m.name.as_deref() == Some(name));
+    Ok(taken.then(|| format!("the name {name} is taken by another session")))
 }
 
 /// A fresh session identifier: 32 lowercase hex characters.
 ///
@@ -445,10 +518,22 @@ impl RegistryGuard {
     pub fn register(meta: &SessionMeta) -> anyhow::Result<RegistryGuard> {
         Self::register_in(&Registry::dir()?, meta)
     }
 
+    /// A meta with a name is registered under the [`NamesLock`], and refused
+    /// when a live session already has that name.
     pub fn register_in(root: &Path, meta: &SessionMeta) -> anyhow::Result<RegistryGuard> {
         create_private_dir(root)?;
+        let _lock = match &meta.name {
+            Some(name) => {
+                let lock = NamesLock::take(root)?;
+                if let Some(why) = name_refusal(root, name, &meta.session_id)? {
+                    anyhow::bail!("{why}");
+                }
+                Some(lock)
+            }
+            None => None,
+        };
         let dir = root.join(&meta.session_id);
         // `create_dir` and not `create_dir_all`: an existing directory means
         // another live session already owns this id, and taking it over would
         // delete that session's socket on drop.
@@ -477,11 +562,39 @@ impl RegistryGuard {
 
     /// Rewrite `meta.json`. Called after `daemonize()`, because forking twice
     /// changes the pid that `--list` prunes on, and after every attach, because
     /// `attach_id` moves.
+    ///
+    /// Atomically: a temporary file beside it, then a rename. A reader --
+    /// `list_in`, under a name check -- sees the old file or the new one,
+    /// never a torn one it would skip as unparsable (spec §2.3).
     pub fn update(&self, meta: &SessionMeta) -> anyhow::Result<()> {
         let text = serde_json::to_vec_pretty(meta).context("encoding meta.json")?;
-        write_private_file(&self.meta_path(), &text)
+        let tmp = self.dir.join(format!("{META_FILE}.tmp"));
+        write_private_file(&tmp, &text)?;
+        std::fs::rename(&tmp, self.meta_path())
+            .with_context(|| format!("replacing {}", self.meta_path().display()))
+    }
+
+    /// Give this session `name`, or none, under the [`NamesLock`]: refused
+    /// when another live session has it. `meta` is this session's record,
+    /// and is changed only when the name is taken.
+    pub fn rename(&self, meta: &mut SessionMeta, name: Option<String>) -> anyhow::Result<()> {
+        let root = self
+            .dir
+            .parent()
+            .context("a session directory has a registry around it")?;
+        let _lock = NamesLock::take(root)?;
+        if let Some(n) = &name
+            && let Some(why) = name_refusal(root, n, &meta.session_id)?
+        {
+            anyhow::bail!("{why}");
+        }
+        let mut renamed = meta.clone();
+        renamed.name = name;
+        self.update(&renamed)?;
+        *meta = renamed;
+        Ok(())
     }
 }
 
 impl Drop for RegistryGuard {
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -90,8 +90,9 @@ async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::
         // safe direction: a record that over-promises reattachment is worse
         // than one that under-promises it for a few hundred milliseconds.
         detachable: false,
         boot: oxutrm_host::boot_token(),
+        name: None,
     };
 
     let attached = crate::attach_exchange::run_attach_exchange(
         tokio::io::BufReader::new(tokio::io::stdin()),
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p oxutrm-host --jobs 4`

Expected: all pass, including `a_name_is_registered_and_a_second_session_cannot_take_it`, `a_rename_is_written_refused_when_taken_and_cleared_by_none`, `an_update_leaves_no_temporary_file_and_the_lock_is_not_an_entry`, `meta_json_holds_exactly_the_nine_fields_it_is_allowed_to` and `the_listing_has_a_name_column_that_keeps_the_rest_aligned`.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add crates/oxutrm-host/src/attach.rs crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs crates/oxutrm-host/src/lib.rs crates/oxutrm-host/src/registry.rs crates/oxutrm-host/tests/attach.rs crates/oxutrm-host/tests/detachable.rs crates/oxutrm-host/tests/ladder.rs crates/oxutrm-host/tests/no_keys_on_disk.rs crates/oxutrm-host/tests/registry.rs crates/oxutrm-host/tests/registry_roots.rs src/attach_exchange.rs src/connect.rs src/serve.rs tests/host_attach.rs
git commit -m "feat(registry): session names under names.lock, atomic meta.json, name column in --list

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 4: Login shells in `$HOME`; hanging a shell up, and killing it after a grace

Spec §3.4. `oxutrm-term` gets `Start { cwd, login }`: a login shell has `argv[0]` = `-<basename>` (`CommandExt::arg0`) and starts in `$HOME`. `serve.rs` reads `$HOME` (`home_dir`: only an absolute directory, else the inherited one) and passes it in; `HostSession::spawn` takes a `&Start`, the tests pass `Start::default()` (a plain `/bin/sh`).

`HostTerm::hang_up` sends SIGHUP to the terminal's foreground process group and to the shell; `HostTerm::kill_group` sends SIGKILL to the shell's process group (it is a session leader, `login_tty`). `HostSession::hang_up_shell(grace)` hangs up, drains the pty while it waits (macOS does not reap a killed child whose pty output is unread), kills after `grace`, and returns the status once reaped. **Deviation:** the spec says "then closing the pty master"; the master is closed when the terminal is dropped after the shell was reaped, because closing it earlier would lose the drain the reap needs on macOS.

`hang_up_shell` and `KILL_GRACE` have no caller until Task 6: they carry `#[cfg_attr(not(test), expect(dead_code, reason = "the door's Kill ... from Task 6 on"))]`, which Task 6 removes.

**Files:**
- Modify: `crates/oxutrm-term/src/pty.rs`, `crates/oxutrm-term/src/host.rs`, `crates/oxutrm-term/src/lib.rs`, `crates/oxutrm-term/Cargo.toml` (`tempfile = "3"` under `[dev-dependencies]`; `Cargo.lock` follows)
- Modify: `src/host_session.rs`, `src/serve.rs`, `src/session.rs` and `src/accept.rs` (tests pass `Start::default()`)

**Interfaces:**
- Consumes: `HostSession` (Task 1).
- Produces:
  - `oxutrm_term::Start { cwd: Option<PathBuf>, login: bool }` (`Default`), `Start::login_in(home: Option<PathBuf>) -> Start`; `HostTerm::spawn_with(shell, args, env, size, scrollback, &Start)`; `HostTerm::hang_up(&self)`; `HostTerm::kill_group(&self)`.
  - `HostSession::spawn(shell: &str, start: &oxutrm_term::Start, size, scrollback, link)`; `HostSession::hang_up_shell(&mut self, grace: Duration) -> i32` (`-1` if it cannot be reaped); `host_session::KILL_GRACE: Duration` = 3 s.
  - `serve::home_dir(home: Option<OsString>) -> Option<PathBuf>`.

- [ ] **Step 1: Write the failing tests**

The two `host_session` tests use `crate::link::fixtures::link_pair()` for a link and a script file as the "shell" that ignores SIGHUP; it creates a `ready` file once its trap is set, so the test never races the trap.

`crates/oxutrm-term/src/lib.rs`:

````diff
--- a/crates/oxutrm-term/src/lib.rs
+++ b/crates/oxutrm-term/src/lib.rs
@@ -63,4 +63,5 @@ pub use exit_wake::ExitWake;
 pub use grid::GridSize;
 pub use host::HostTerm;
 pub use listener::{EventSink, Signals};
 pub use palette::{PALETTE_LEN, palette};
+pub use pty::Start;
````

`crates/oxutrm-term/src/pty.rs`:

````diff
--- a/crates/oxutrm-term/src/pty.rs
+++ b/crates/oxutrm-term/src/pty.rs
@@ -547,5 +618,60 @@ mod tests {
     fn spawning_something_that_does_not_exist_is_an_error_not_a_panic() {
         let e = Pty::spawn("/nonexistent/oxutrm-test-shell", &[], &[], size());
         assert!(e.is_err());
     }
+
+    #[test]
+    fn a_login_shell_starts_in_its_directory_with_a_dash_name() {
+        let dir = tempfile::tempdir().expect("tempdir");
+        let home = dir.path().canonicalize().expect("canonical");
+        let mut pty = Pty::spawn_with(
+            "/bin/sh",
+            &[
+                "-c".to_owned(),
+                "printf '%s:%s\\n' \"$0\" \"$(pwd -P)\"".to_owned(),
+            ],
+            &[],
+            size(),
+            &Start::login_in(Some(home.clone())),
+        )
+        .expect("spawn");
+        let want = format!("-sh:{}", home.display());
+        let out = read_until(&mut pty, want.as_bytes(), Duration::from_secs(5));
+        assert!(
+            String::from_utf8_lossy(&out).contains(&want),
+            "{:?}",
+            String::from_utf8_lossy(&out)
+        );
+    }
+
+    fn wait_exit(pty: &mut Pty, budget: Duration) -> Option<i32> {
+        let deadline = Instant::now() + budget;
+        let mut buf = [0u8; 4096];
+        while Instant::now() < deadline {
+            let _ = pty.read_ready(&mut buf);
+            if let Some(code) = pty.child_exited() {
+                return Some(code);
+            }
+            std::thread::sleep(Duration::from_millis(5));
+        }
+        None
+    }
+
+    #[test]
+    fn a_hang_up_ends_a_shell_that_does_not_ignore_it() {
+        let mut pty = sh("echo up; sleep 30");
+        read_until(&mut pty, b"up", Duration::from_secs(5));
+        pty.hang_up();
+        assert_eq!(wait_exit(&mut pty, Duration::from_secs(5)), Some(128 + 1));
+    }
+
+    #[test]
+    fn a_shell_that_ignores_the_hang_up_needs_the_kill() {
+        let mut pty = sh("trap '' HUP; echo up; while :; do sleep 1; done");
+        read_until(&mut pty, b"up", Duration::from_secs(5));
+        pty.hang_up();
+        assert_eq!(wait_exit(&mut pty, Duration::from_millis(400)), None);
+        pty.kill_group();
+        assert_eq!(wait_exit(&mut pty, Duration::from_secs(5)), Some(128 + 9));
+    }
 }
````

`src/accept.rs`:

````diff
--- a/src/accept.rs
+++ b/src/accept.rs
@@ -357,8 +357,9 @@ mod tests {
                 match accept_one(permit, nominated).await {
                     Ok(conn) => {
                         let session = HostSession::spawn(
                             "/bin/sh",
+                            &oxutrm_term::Start::default(),
                             size(),
                             200,
                             Link::new(conn, endpoint.clone(), Arc::clone(&socket)),
                         )
@@ -679,10 +680,16 @@ mod tests {
         let host = tokio::spawn(async move {
             let conn = accept_one(permit, nominated)
                 .await
                 .expect("the first client");
-            let session =
-                HostSession::spawn("/bin/sh", size(), 200, Link::new(conn, ep, sock)).unwrap();
+            let session = HostSession::spawn(
+                "/bin/sh",
+                &oxutrm_term::Start::default(),
+                size(),
+                200,
+                Link::new(conn, ep, sock),
+            )
+            .unwrap();
             counter.0.fetch_add(1, Ordering::SeqCst);
             // Keep it alive so the test is not measuring a dropped session.
             std::future::pending::<()>().await;
             drop(session);
````

`src/host_session.rs`:

````diff
--- a/src/host_session.rs
+++ b/src/host_session.rs
@@ -693,0 +743,60 @@
+#[cfg(test)]
+mod tests {
+    use super::*;
+    use crate::link::fixtures::link_pair;
+
+    fn size() -> TermSize {
+        TermSize {
+            cols: 120,
+            rows: 40,
+        }
+    }
+
+    /// A "shell" that ignores SIGHUP, as a script file the session runs.
+    fn stubborn_shell(dir: &std::path::Path) -> String {
+        use std::os::unix::fs::PermissionsExt as _;
+        let path = dir.join("stubborn");
+        std::fs::write(&path, "#!/bin/sh\ntrap '' HUP\nwhile :; do sleep 1; done\n").unwrap();
+        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
+        path.to_str().unwrap().to_string()
+    }
+
+    #[tokio::test]
+    async fn a_hung_up_shell_ends_at_once() {
+        let (host_link, _client) = link_pair().await;
+        let mut host = HostSession::spawn(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size(),
+            200,
+            host_link,
+        )
+        .unwrap();
+        let begun = Instant::now();
+        let code = host.hang_up_shell(KILL_GRACE).await;
+        assert_eq!(code, 128 + 1, "ended by the SIGHUP");
+        assert!(begun.elapsed() < KILL_GRACE, "{:?}", begun.elapsed());
+    }
+
+    #[tokio::test]
+    async fn a_shell_that_ignores_the_hang_up_is_killed_after_the_grace() {
+        let dir = tempfile::tempdir().unwrap();
+        let shell = stubborn_shell(dir.path());
+        let (host_link, _client) = link_pair().await;
+        let mut host = HostSession::spawn(
+            &shell,
+            &oxutrm_term::Start::default(),
+            size(),
+            200,
+            host_link,
+        )
+        .unwrap();
+        // Long enough for the script to have set its trap.
+        tokio::time::sleep(Duration::from_millis(300)).await;
+        let grace = Duration::from_millis(400);
+        let begun = Instant::now();
+        let code = host.hang_up_shell(grace).await;
+        assert_eq!(code, 128 + 9, "ended by the SIGKILL");
+        assert!(begun.elapsed() >= grace, "{:?}", begun.elapsed());
+    }
+}
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -227,0 +244,19 @@
+#[cfg(test)]
+mod tests {
+    use super::*;
+
+    #[test]
+    fn home_is_used_only_when_it_is_an_absolute_directory() {
+        let dir = tempfile::tempdir().unwrap();
+        assert_eq!(
+            home_dir(Some(dir.path().as_os_str().to_owned())),
+            Some(dir.path().to_path_buf())
+        );
+        assert_eq!(home_dir(None), None);
+        assert_eq!(home_dir(Some("relative/home".into())), None);
+        assert_eq!(
+            home_dir(Some(dir.path().join("missing").into_os_string())),
+            None
+        );
+    }
+}
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -2504,9 +2504,16 @@ mod tests {
         let relay = relay_to(listening.addr).await;
         // The client's peer is the RELAY, which is the whole point.
         let (host_link, client_link) = listening.dial("127.0.0.1:0", relay.addr).await;
 
-        let mut host = HostSession::spawn("/bin/sh", size, 200, host_link).unwrap();
+        let mut host = HostSession::spawn(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size,
+            200,
+            host_link,
+        )
+        .unwrap();
         let client = ClientSession::new(size, caps(), client_link, None).unwrap();
         host.term_mut().write_input(shell.as_bytes()).unwrap();
         (host, client, relay)
     }
@@ -2539,9 +2546,16 @@ mod tests {
         let listening = crate::link::fixtures::listening().await;
         let addr = listening.addr;
         let (host_link, client_link) = listening.dial(client_bind, addr).await;
 
-        let host = HostSession::spawn("/bin/sh", size, 200, host_link).unwrap();
+        let host = HostSession::spawn(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size,
+            200,
+            host_link,
+        )
+        .unwrap();
         let client = ClientSession::new(size, caps(), client_link, rebuild).unwrap();
 
         // The caller decides what the shell runs; `spawn` above starts one, so
         // the script is fed as input instead, which is also how a real session
@@ -3206,9 +3220,16 @@ mod tests {
             rows: 60,
         };
         let (host_link, client_link) = crate::link::fixtures::link_pair().await;
 
-        let mut host = HostSession::spawn("/bin/sh", big, 200, host_link).unwrap();
+        let mut host = HostSession::spawn(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            big,
+            200,
+            host_link,
+        )
+        .unwrap();
         let mut client = ClientSession::new(big, caps(), client_link, None).unwrap();
 
         // Fill the screen with varied, poorly compressible content.
         host.term_mut()
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p oxutrm-term --lib --jobs 4 pty && cargo test --bin oxutrm --jobs 4 host_session serve::tests`

Expected: compile errors: `Start`, `spawn_with`, `hang_up`, `kill_group`, `hang_up_shell`, `KILL_GRACE` and `home_dir` are not defined.

- [ ] **Step 3: Implement**

`crates/oxutrm-term/Cargo.toml`:

````diff
--- a/crates/oxutrm-term/Cargo.toml
+++ b/crates/oxutrm-term/Cargo.toml
@@ -20,4 +20,6 @@ anyhow.workspace = true
 oxutrm-proto.workspace = true
 
 [dev-dependencies]
 insta.workspace = true
+# A directory for the login-shell test to start in.
+tempfile = "3"
````

`crates/oxutrm-term/src/host.rs`:

````diff
--- a/crates/oxutrm-term/src/host.rs
+++ b/crates/oxutrm-term/src/host.rs
@@ -69,14 +69,27 @@ impl HostTerm {
         args: &[String],
         env: &[(String, String)],
         size: TermSize,
         scrollback: usize,
+    ) -> anyhow::Result<HostTerm> {
+        HostTerm::spawn_with(shell, args, env, size, scrollback, &crate::Start::default())
+    }
+
+    /// [`HostTerm::spawn`], with the child started as `start` says: a
+    /// session's shell is a login shell in `$HOME` (switcher spec §3.4).
+    pub fn spawn_with(
+        shell: &str,
+        args: &[String],
+        env: &[(String, String)],
+        size: TermSize,
+        scrollback: usize,
+        start: &crate::Start,
     ) -> anyhow::Result<HostTerm> {
         // Before the PTY, not after: `size` came from the client and this is
         // the host. Spawning a shell and then discovering the geometry was
         // hostile means a process to clean up as well as an error to report.
         let dims = GridSize::new(size, scrollback)?;
-        let pty = Pty::spawn(shell, args, env, size)?;
+        let pty = Pty::spawn_with(shell, args, env, size, start)?;
         let events = EventSink::new();
         let config = Config {
             scrolling_history: scrollback,
             // Accept paste requests as well as copies; the default is
@@ -285,8 +298,20 @@ impl HostTerm {
         }
         self.exited
     }
 
+    /// SIGHUP to the shell and its terminal's foreground job: what a closed
+    /// terminal does. See `Pty::hang_up`.
+    pub fn hang_up(&self) {
+        self.pty.hang_up();
+    }
+
+    /// SIGKILL to the shell's process group, for a shell that outlived its
+    /// hang-up.
+    pub fn kill_group(&self) {
+        self.pty.kill_group();
+    }
+
     /// The size the emulator is currently at.
     pub fn size(&self) -> TermSize {
         self.size
     }
````

`crates/oxutrm-term/src/pty.rs`:

````diff
--- a/crates/oxutrm-term/src/pty.rs
+++ b/crates/oxutrm-term/src/pty.rs
@@ -33,8 +33,38 @@ use crate::exit_wake::ExitWake;
 /// practice - because the cost of being wrong is a zombie, while the cost of
 /// having no bound at all is a host that never returns from a drop.
 const REAP_BUDGET: Duration = Duration::from_secs(2);
 
+/// How the child is started, beyond its program and arguments.
+#[derive(Clone, Debug, Default, PartialEq, Eq)]
+pub struct Start {
+    /// The directory it starts in; `None` inherits ours.
+    pub cwd: Option<std::path::PathBuf>,
+    /// A login shell: `argv[0]` is `-<basename>`, the convention `login(1)`
+    /// and `sshd` use to tell a shell to read its profile.
+    pub login: bool,
+}
+
+impl Start {
+    /// A login shell in `home`, as ssh would give you (switcher spec §3.4);
+    /// in the inherited directory when there is no usable home.
+    #[must_use]
+    pub fn login_in(home: Option<std::path::PathBuf>) -> Start {
+        Start {
+            cwd: home,
+            login: true,
+        }
+    }
+}
+
+/// `-<basename of shell>`, the `argv[0]` of a login shell.
+fn login_name(shell: &str) -> String {
+    let base = std::path::Path::new(shell)
+        .file_name()
+        .map_or_else(|| shell.to_string(), |b| b.to_string_lossy().into_owned());
+    format!("-{base}")
+}
+
 /// A PTY with a child attached to its user side.
 pub struct Pty {
     /// Our end: reads what the child wrote, writes what the user typed.
     controller: File,
@@ -43,13 +73,25 @@ pub struct Pty {
 }
 
 impl Pty {
     /// Open a PTY and start `shell` on the far side of it.
+    #[cfg(test)]
     pub fn spawn(
         shell: &str,
         args: &[String],
         env: &[(String, String)],
         size: TermSize,
+    ) -> anyhow::Result<Pty> {
+        Pty::spawn_with(shell, args, env, size, &Start::default())
+    }
+
+    /// [`Pty::spawn`], started as `start` says.
+    pub fn spawn_with(
+        shell: &str,
+        args: &[String],
+        env: &[(String, String)],
+        size: TermSize,
+        start: &Start,
     ) -> anyhow::Result<Pty> {
         let winsize = winsize_of(size);
         // `openpty` (rather than `openpty_nocloexec`) marks both descriptors
         // close-on-exec. That matters for the controller: if the child kept a
@@ -65,8 +107,14 @@ impl Pty {
             .context("duplicating the pty for the child")?;
 
         let mut command = Command::new(shell);
         command.args(args);
+        if start.login {
+            command.arg0(login_name(shell));
+        }
+        if let Some(dir) = &start.cwd {
+            command.current_dir(dir);
+        }
         for (k, v) in env {
             command.env(k, v);
         }
         command
@@ -165,8 +213,31 @@ impl Pty {
             Err(_) => Some(-1),
         }
     }
 
+    /// Hang the child up the way the kernel does when a terminal goes away:
+    /// SIGHUP to the terminal's foreground process group and to the child.
+    /// Best effort -- either may already be gone.
+    pub fn hang_up(&self) {
+        use rustix::process::{Pid, Signal, kill_process, kill_process_group};
+        if let Ok(fg) = rustix::termios::tcgetpgrp(self.controller.as_fd()) {
+            let _ = kill_process_group(fg, Signal::HUP);
+        }
+        if let Some(pid) = Pid::from_raw(self.child.id() as i32) {
+            let _ = kill_process(pid, Signal::HUP);
+        }
+    }
+
+    /// SIGKILL to the child's process group. The child is a session leader
+    /// (`login_tty`), so its group is its own pid. Best effort.
+    pub fn kill_group(&self) {
+        use rustix::process::{Pid, Signal, kill_process, kill_process_group};
+        if let Some(pid) = Pid::from_raw(self.child.id() as i32) {
+            let _ = kill_process_group(pid, Signal::KILL);
+            let _ = kill_process(pid, Signal::KILL);
+        }
+    }
+
     /// The child's process id.
     #[cfg(test)]
     pub fn child_pid(&self) -> u32 {
         self.child.id()
````

`src/host_session.rs`:

````diff
--- a/src/host_session.rs
+++ b/src/host_session.rs
@@ -46,8 +46,20 @@ use crate::session::{IDLE_POLL, SHELL_EXITED, SUPERSEDED, SWITCHED, TAKEN_OVER,
 /// A peer that comes back is heard on its first frame and `screen_stale` forces
 /// the snapshot.
 pub const DETACH_AFTER: Duration = Duration::from_secs(30);
 
+/// How long a hung-up shell has to exit before its process group is killed
+/// (switcher spec §3.4).
+#[cfg_attr(
+    not(test),
+    expect(dead_code, reason = "the door's Kill passes it from Task 6 on")
+)]
+pub(crate) const KILL_GRACE: Duration = Duration::from_secs(3);
+
+/// How long a killed shell is waited for. A SIGKILLed process is reaped at
+/// once in practice; this bounds the wait for one that cannot be.
+const REAP_AFTER_KILL: Duration = Duration::from_secs(2);
+
 /// The remote half: owns the PTY and the authoritative screen.
 pub struct HostSession {
     term: HostTerm,
     screen_tx: oxutrm_sync::Sender<ScreenState>,
@@ -75,10 +87,14 @@ impl HostSession {
     /// Start a shell and serve it over `link`.
     ///
     /// `TERM` and `COLORTERM` come from [`oxutrm_term::negotiate_term`], which
     /// takes no arguments on purpose.
+    ///
+    /// `start` says where and how: a session's is a login shell in `$HOME`
+    /// (switcher spec §3.4); the tests' is a plain `/bin/sh`.
     pub fn spawn(
         shell: &str,
+        start: &oxutrm_term::Start,
         size: TermSize,
         scrollback: usize,
         link: Link,
     ) -> Result<HostSession> {
@@ -87,9 +103,9 @@ impl HostSession {
         if let Some(ct) = colorterm {
             env.push(("COLORTERM".to_owned(), ct));
         }
 
-        let term = HostTerm::spawn(shell, &[], &env, size, scrollback)
+        let term = HostTerm::spawn_with(shell, &[], &env, size, scrollback, start)
             .context("starting the shell on a pty")?;
         let blank = ScreenState::blank(size.rows, size.cols)?;
         let empty = InputState {
             seq: 1,
@@ -484,8 +500,41 @@ impl HostSession {
             }
         }
     }
 
+    /// End the shell on request: hang it up as a closing terminal would, and
+    /// SIGKILL its process group if it is still there after `grace`
+    /// ([`KILL_GRACE`] in the session). Returns its exit status once it is
+    /// reaped -- or `-1` for one that could not be reaped even after the
+    /// kill, which a session ends on all the same.
+    ///
+    /// Drains the pty while it waits: on macOS a child killed while writing
+    /// to a pty is not reaped until its output is read (`Pty::reap`).
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the door's Kill calls it from Task 6 on")
+    )]
+    pub(crate) async fn hang_up_shell(&mut self, grace: Duration) -> i32 {
+        self.term.hang_up();
+        let start = tokio::time::Instant::now();
+        let mut killed = false;
+        loop {
+            let _ = self.term.poll();
+            if let Some(code) = self.term.child_exited() {
+                return code;
+            }
+            let waited = start.elapsed();
+            if !killed && waited >= grace {
+                self.term.kill_group();
+                killed = true;
+            }
+            if killed && waited >= grace + REAP_AFTER_KILL {
+                return -1;
+            }
+            tokio::time::sleep(IDLE_POLL).await;
+        }
+    }
+
     /// The last screen, and only then the close. `ls; exit` lives or dies here.
     ///
     /// Three separate things were losing it, and all three had to go:
     ///
@@ -689,4 +738,5 @@ enum HostWake {
     StandbyFrame(Frame),
     /// The parked standby's connection is gone.
     StandbyGone,
 }
+
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -171,10 +171,19 @@ async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::
     // R14. `negotiate_term` takes no arguments: the child's TERM comes from
     // the emulator, never from the client, or a client narrower than the
     // emulator would bake degraded output into the authoritative screen for
     // the life of the session.
-    let mut session = HostSession::spawn(&shell, attached.client_size, SCROLLBACK, attached.link)
-        .context("starting the shell")?;
+    //
+    // A login shell in `$HOME`, as ssh would give you (switcher spec §3.4).
+    let start = oxutrm_term::Start::login_in(home_dir(std::env::var_os("HOME")));
+    let mut session = HostSession::spawn(
+        &shell,
+        &start,
+        attached.client_size,
+        SCROLLBACK,
+        attached.link,
+    )
+    .context("starting the shell")?;
 
     // R15, then R16: dropping the guard takes the session directory with it,
     // so a session that exits cleanly leaves nothing for `--list` to prune.
     let code = match listening {
@@ -189,8 +198,15 @@ async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::
     drop(guard);
     code.map(|_| ())
 }
 
+/// The directory a new shell starts in: `$HOME` when it names a directory,
+/// else none -- the inherited one -- rather than a shell that fails to start.
+pub(crate) fn home_dir(home: Option<std::ffi::OsString>) -> Option<std::path::PathBuf> {
+    home.map(std::path::PathBuf::from)
+        .filter(|p| p.is_absolute() && p.is_dir())
+}
+
 /// The two ways into a severed session after its first attach: the Unix
 /// socket, and the door that the first link's control stream knocks on.
 ///
 /// Returns the listener task and where completed attaches arrive. A function
@@ -223,4 +239,5 @@ pub(crate) fn open_doors(
         attach_tx,
     ));
     (task, attach_rx)
 }
+
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p oxutrm-term --lib --jobs 4 pty && cargo test --bin oxutrm --jobs 4 host_session serve::tests`

Expected: all pass: `a_login_shell_starts_in_its_directory_with_a_dash_name`, `a_hang_up_ends_a_shell_that_does_not_ignore_it`, `a_shell_that_ignores_the_hang_up_needs_the_kill`, `a_hung_up_shell_ends_at_once`, `a_shell_that_ignores_the_hang_up_is_killed_after_the_grace`, `home_is_used_only_when_it_is_an_absolute_directory`.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add Cargo.lock crates/oxutrm-term/Cargo.toml crates/oxutrm-term/src/host.rs crates/oxutrm-term/src/lib.rs crates/oxutrm-term/src/pty.rs src/accept.rs src/host_session.rs src/serve.rs src/session.rs
git commit -m "feat(host): login shells in $HOME; hang a shell up, kill it after a grace

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

## Part 2 -- the host

### Task 5: Both doors read an `Open` first: one dispatcher, a task per connection; `PROTO_VERSION` 3

Spec §2.2, §4.1. The control stream (`Via::Client`) and the Unix socket (`Via::Socket`) hand every connection to `door::serve` on a task of its own: it reads the `Open` under `OPEN_TIMEOUT`, refuses another protocol version with a reason, and dispatches. `Attach` goes to the serial attach loop through an `AttachQueue` -- two channels, so a primary can pre-empt a standby's exchange exactly as the socket attach did before. `Probe` is answered with `Reply::ProbeAck`, and further probes on the same stream keep being answered. `Myself` is this session's entry as a sibling sees it (`Elsewhere` while a client is attached, else `No`). `Sessions` reads the registry and asks every other live entry `Myself` over its socket, all at once, each bounded by `SIBLING_TIMEOUT`: no answer is `Unknown`, an answer that is not an `Entry` (a refusal, the old protocol's `HostHello`, an unparsable line) is `OtherVersion`. `Switch`, `New`, `Kill` and `Rename` are refused for now; Tasks 6 and 8 serve them.

`Myself` and `Sessions` answer from the door's own copy of the session's record and from `Presence` -- the loop's last-heard clock, shared -- never from the attach loop's working record, which is held across an exchange (spec §2.2). The attach loop copies what an attach moved (generation, size, detachability) into the door's record (`Door::record_attach`), which writes `meta.json`.

`Signal` loses `StandbyRequest`, `Probe` and `ProbeAck` and is purely the attach exchange; `PROTO_VERSION` is 3. `oxutrm host --attach <id>` writes `Open { Attach { Primary } }` into the socket before relaying.

**Deviation:** `Reply::ProbeAck { nonce }` is a fifth `Reply` variant; the spec lists four, and `Probe`'s "unchanged semantics" need an answer that is not a `Signal` any more.

`listener::serve_attaches` keeps its `eprintln!` for a timed-out attempt (the host daemon's stderr; the module has no print lint). `serve::open_doors` is the seam this task gives `serve()`; Task 6 replaces it with `Door::register`.

**Files:**
- Create: `src/door.rs`
- Modify: `src/listener.rs` (rewritten around the door), `src/control.rs` (rewritten client half and tests), `src/host_session.rs` (`Presence`), `src/serve.rs`, `src/main.rs`, `src/session.rs` (two tests use the new door), `crates/oxutrm-proto/src/signal.rs`, `crates/oxutrm-proto/src/lib.rs`

**Interfaces:**
- Consumes: Task 2's `Open`, `Request`, `Reply`, `Role`, `Attached`, `SessionEntry`, `read_line_async`, `write_line_async`, `read_answer_async`; Task 3's `SessionMeta::offer`, `RegistryGuard`.
- Produces:
  - `crate::door::{Via { Client, Socket }, AttachQueue { primary, standby }, AttachInbox { primary, standby }, attach_queue() -> (AttachQueue, AttachInbox), Door, serve(door: Arc<Door>, via: Via, reader, writer), OPEN_TIMEOUT, SIBLING_TIMEOUT}`; `Door::new(registry: PathBuf, meta: SessionMeta, guard: Option<Arc<RegistryGuard>>, presence: Presence, attaches: Option<AttachQueue>) -> Arc<Door>` (Task 6 replaces it), `Door::meta(&self) -> SessionMeta`, `Door::record_attach(&self, &SessionMeta)`; test fixtures `door::fixtures::{registered, meta, socket_door, ask}`.
  - `crate::host_session::Presence` (`Clone + Default`; `attached(&self, now: Instant) -> bool`); `HostSession::with_presence(self, Presence) -> HostSession`.
  - `control::serve_control(conn: quinn::Connection, door: Arc<Door>) -> JoinHandle<()>`; `pub(crate) use oxutrm_proto::Role` in `control`; `control::DoorRequest { reader, writer, role }` (unchanged shape).
  - `listener::accept_doors(listener: UnixListener, door: Arc<Door>)`; `listener::serve_attaches(door: Arc<Door>, cfg: NetConfig, attach_timeout: Duration, inbox: AttachInbox, tx: mpsc::Sender<Attached>)`.
  - `oxutrm_proto::PROTO_VERSION == 3`; `Signal` without `StandbyRequest`, `Probe`, `ProbeAck`.

- [ ] **Step 1: Write the failing tests**

`listener.rs`'s tests are rewritten around a `Doors` fixture (a registered session, its socket, the accept loop and the attach loop, as `serve.rs` opens them); `control.rs`'s around `door::fixtures::registered`. Two new ones pin spec §2.2: `myself_is_answered_while_an_attach_is_under_way` and `a_sessions_fetch_does_not_cancel_a_siblings_standby_search`. `a_standby_is_found_over_the_first_links_control_stream` in `session.rs` reads the generation from `meta.json` now, polling briefly: the host records it just after its `Established` went out.

`crates/oxutrm-proto/src/signal.rs`:

````diff
--- a/crates/oxutrm-proto/src/signal.rs
+++ b/crates/oxutrm-proto/src/signal.rs
@@ -1138,19 +1127,5 @@ mod tests {
             Signal::HostHello { features, .. } => assert!(features.is_empty()),
             other => panic!("parsed as {other:?}"),
         }
     }
-
-    #[test]
-    fn the_standby_signals_round_trip() {
-        for s in [
-            Signal::StandbyRequest,
-            Signal::Probe { nonce: 42 },
-            Signal::ProbeAck { nonce: 42 },
-        ] {
-            let mut line = Vec::new();
-            write_signal(&mut line, &s).unwrap();
-            let back = read_signal(&mut &line[..]).unwrap();
-            assert_eq!(format!("{back:?}"), format!("{s:?}"));
-        }
-    }
 }
````

`src/control.rs`:

````diff
--- a/src/control.rs
+++ b/src/control.rs
@@ -176,100 +126,41 @@
 #[cfg(test)]
 mod tests {
     use super::*;
+    use crate::door::fixtures::{meta, registered};
     use crate::link::fixtures::link_pair;
 
+    const SESSION: &str = "00112233445566778899aabbccddeeff";
+
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn a_probe_is_answered_with_its_own_nonce() {
+    async fn a_probe_is_answered_with_its_own_nonce_and_the_stream_keeps_answering() {
+        let dir = tempfile::tempdir().unwrap();
+        let (door, _inbox, _) = registered(dir.path(), meta(SESSION, None));
         let (host, client) = link_pair().await;
-        let (door_tx, _door_rx) = tokio::sync::mpsc::channel(1);
-        let _server = serve_control(host.sink.connection().clone(), door_tx);
+        let _server = serve_control(host.sink.connection().clone(), door);
 
         let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
-        write_signal_async(&mut send, &Signal::Probe { nonce: 7 })
-            .await
-            .unwrap();
         let mut recv = tokio::io::BufReader::new(recv);
-        let back = read_signal_async(&mut recv).await.unwrap();
-        assert!(matches!(back, Signal::ProbeAck { nonce: 7 }), "{back:?}");
-
-        // The same stream keeps answering, so one probe stream can serve an outage.
-        write_signal_async(&mut send, &Signal::Probe { nonce: 8 })
-            .await
-            .unwrap();
-        let back = read_signal_async(&mut recv).await.unwrap();
-        assert!(matches!(back, Signal::ProbeAck { nonce: 8 }), "{back:?}");
-    }
-
-    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn a_standby_request_is_handed_to_the_door_with_its_stream() {
-        let (host, client) = link_pair().await;
-        let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
-        let _server = serve_control(host.sink.connection().clone(), door_tx);
-
-        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
-        write_signal_async(&mut send, &Signal::StandbyRequest)
-            .await
-            .unwrap();
-        // Something after the request must reach whoever the door hands the
-        // stream to: the exchange reads the client's hello from it.
-        write_signal_async(&mut send, &Signal::Probe { nonce: 1 })
-            .await
-            .unwrap();
-
-        let mut req = door_rx.recv().await.expect("the door was knocked on");
-        assert_eq!(req.role, Role::Standby);
-        let next = read_signal_async(&mut req.reader).await.unwrap();
-        assert!(matches!(next, Signal::Probe { nonce: 1 }), "{next:?}");
-    }
-
-    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn anything_else_first_is_dropped_without_reaching_the_door() {
-        let (host, client) = link_pair().await;
-        let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
-        let _server = serve_control(host.sink.connection().clone(), door_tx);
-
-        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
-        write_signal_async(
-            &mut send,
-            &Signal::Choose {
-                choice: oxutrm_proto::Choice::New,
-            },
-        )
-        .await
-        .unwrap();
-        send.finish().unwrap();
-        let knocked =
-            tokio::time::timeout(std::time::Duration::from_millis(500), door_rx.recv()).await;
-        assert!(
-            knocked.is_err(),
-            "the door was knocked on: a stray line can start an attach"
-        );
-
-        // And the quiet meant something: the server was there, and a request
-        // on the next stream still gets through. Without this, a server that
-        // never accepted a stream at all would pass the assertion above.
-        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
-        write_signal_async(&mut send, &Signal::StandbyRequest)
-            .await
-            .unwrap();
-        let req = tokio::time::timeout(std::time::Duration::from_secs(5), door_rx.recv())
-            .await
-            .expect("the control server stopped serving after a stray stream")
-            .expect("the door's sender is gone");
-        assert_eq!(req.role, Role::Standby);
+        for nonce in [7, 8] {
+            write_line_async(&mut send, &Open::new(Request::Probe { nonce }))
+                .await
+                .unwrap();
+            let back: Reply = read_line_async(&mut recv).await.unwrap();
+            assert_eq!(back, Reply::ProbeAck { nonce });
+        }
     }
 
     /// A host side that serves control streams and runs every standby request
     /// through the real exchange, returning what it produced.
     fn host_door(
+        dir: &std::path::Path,
         host_conn: quinn::Connection,
     ) -> tokio::sync::oneshot::Receiver<crate::attach_exchange::Attached> {
-        let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
-        serve_control(host_conn, door_tx);
+        let (door, mut inbox, _) = registered(dir, meta(SESSION, None));
+        serve_control(host_conn, door);
         let (done_tx, done_rx) = tokio::sync::oneshot::channel();
         tokio::spawn(async move {
-            let req: DoorRequest = door_rx.recv().await.expect("a request");
+            let req: DoorRequest = inbox.standby.recv().await.expect("a request");
             let mut meta = crate::attach_exchange::fixtures::fresh_meta(SESSION);
             let cfg = crate::attach_exchange::fixtures::stun_free();
             if let Ok(mut a) =
                 crate::attach_exchange::run_attach_exchange(req.reader, req.writer, &mut meta, &cfg)
@@ -281,18 +172,17 @@ mod tests {
         });
         done_rx
     }
 
-    const SESSION: &str = "00112233445566778899aabbccddeeff";
-
     fn small() -> oxutrm_proto::TermSize {
         oxutrm_proto::TermSize { cols: 40, rows: 10 }
     }
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_standby_search_lands_a_second_connection_to_the_same_host() {
+        let dir = tempfile::tempdir().unwrap();
         let (host, client) = link_pair().await;
-        let done = host_door(host.sink.connection().clone());
+        let done = host_door(dir.path(), host.sink.connection().clone());
         let cfg = crate::attach_exchange::fixtures::stun_free();
         let primary = client.sink.connection().clone();
 
         let est = tokio::time::timeout(
@@ -321,10 +211,11 @@ mod tests {
     }
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_standby_search_honours_the_filter() {
+        let dir = tempfile::tempdir().unwrap();
         let (host, client) = link_pair().await;
-        let _done = host_door(host.sink.connection().clone());
+        let _done = host_door(dir.path(), host.sink.connection().clone());
         let mut cfg = crate::attach_exchange::fixtures::stun_free();
         cfg.gather_timeout = std::time::Duration::from_millis(800);
 
         let r = tokio::time::timeout(
@@ -347,17 +238,8 @@ mod tests {
             "the search failed, but not because the filter left it no path: {why}"
         );
     }
 
-    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn a_probe_of_a_served_link_is_answered() {
-        let (host, client) = link_pair().await;
-        let _server = serve_control_without_door(host.sink.connection().clone());
-        assert!(probe(client.sink.connection().clone(), 5).await);
-        // And again, on a fresh stream: one probe per conversation.
-        assert!(probe(client.sink.connection().clone(), 6).await);
-    }
-
     /// Nobody answers: the connection is up, but no control server reads the
     /// stream. The probe must come back `false` on its own clock rather than
     /// wait for a reply that is never written.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
@@ -374,37 +256,43 @@ mod tests {
         assert!(started.elapsed() >= crate::linkstate::PROBE_TIMEOUT);
     }
 
     /// Rung 4 advertises a control stream like every other link, so it has to
-    /// answer one: a standby request ends promptly instead of waiting on a
-    /// door nobody opens, and probes still work.
+    /// answer one: a standby request ends promptly instead of waiting on an
+    /// attach loop nobody runs, and probes still work.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn without_a_door_a_standby_request_ends_at_once_and_probes_are_answered() {
+    async fn without_an_attach_loop_a_standby_request_ends_at_once_and_probes_are_answered() {
+        let dir = tempfile::tempdir().unwrap();
+        let door = crate::door::Door::new(
+            dir.path().to_path_buf(),
+            meta(SESSION, None),
+            None,
+            crate::host_session::Presence::default(),
+            None,
+        );
         let (host, client) = link_pair().await;
-        let _server = serve_control_without_door(host.sink.connection().clone());
+        let _server = serve_control(host.sink.connection().clone(), door);
         let within = std::time::Duration::from_secs(5);
 
         let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
-        write_signal_async(&mut send, &Signal::StandbyRequest)
-            .await
-            .unwrap();
+        write_line_async(
+            &mut send,
+            &Open::new(Request::Attach {
+                role: Role::Standby,
+            }),
+        )
+        .await
+        .unwrap();
         let mut recv = tokio::io::BufReader::new(recv);
-        let ended = tokio::time::timeout(within, read_signal_async(&mut recv))
+        let ended = tokio::time::timeout(within, read_line_async::<_, Reply>(&mut recv))
             .await
-            .expect("a standby request with no door behind it was left waiting");
+            .expect("a standby request with no loop behind it was left waiting");
         assert!(
             ended.is_err(),
-            "a standby request with no door was answered: {ended:?}"
+            "a standby request with no loop was answered: {ended:?}"
         );
 
-        let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
-        write_signal_async(&mut send, &Signal::Probe { nonce: 3 })
-            .await
-            .unwrap();
-        let mut recv = tokio::io::BufReader::new(recv);
-        let back = tokio::time::timeout(within, read_signal_async(&mut recv))
-            .await
-            .expect("a probe on a doorless link went unanswered")
-            .unwrap();
-        assert!(matches!(back, Signal::ProbeAck { nonce: 3 }), "{back:?}");
+        assert!(probe(client.sink.connection().clone(), 3).await);
+        // And again, on a fresh stream: one probe per conversation.
+        assert!(probe(client.sink.connection().clone(), 4).await);
     }
 }
````

`src/door.rs` (new): this test module is the end of the file; the code above it comes in the implementation step. Create the file with it now.

````rust
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    /// A door for a session registered in `registry` as `meta`, with an
    /// attach queue whose inbox comes back for the test to read.
    pub(crate) fn registered(
        registry: &std::path::Path,
        meta: SessionMeta,
    ) -> (Arc<Door>, AttachInbox, Presence) {
        let guard = Arc::new(RegistryGuard::register_in(registry, &meta).expect("register"));
        let (queue, inbox) = attach_queue();
        let presence = Presence::default();
        let door = Door::new(
            registry.to_path_buf(),
            meta,
            Some(guard),
            presence.clone(),
            Some(queue),
        );
        (door, inbox, presence)
    }

    /// A session record as a real session writes it, with a realistic id,
    /// shell and size, and a live pid so `list_in` keeps it.
    pub(crate) fn meta(id: &str, name: Option<&str>) -> SessionMeta {
        SessionMeta {
            session_id: id.to_string(),
            attach_id: 1,
            pid: std::process::id(),
            created_unix: oxutrm_host::now_unix(),
            shell: "/bin/bash".to_string(),
            size: oxutrm_proto::TermSize {
                cols: 120,
                rows: 40,
            },
            detachable: true,
            boot: None,
            name: name.map(str::to_string),
        }
    }

    /// Accept on `listener` and serve every connection through `door`, as
    /// `listener::accept_doors` does.
    pub(crate) fn socket_door(
        door: Arc<Door>,
        listener: tokio::net::UnixListener,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(crate::listener::accept_doors(listener, door))
    }

    /// Send `req` through the socket at `path` and read the one answer.
    pub(crate) async fn ask(path: &std::path::Path, req: Request) -> Answer {
        let stream = tokio::net::UnixStream::connect(path)
            .await
            .expect("connect");
        let (r, mut w) = stream.into_split();
        write_line_async(&mut w, &Open::new(req))
            .await
            .expect("ask");
        let mut r = tokio::io::BufReader::new(r);
        tokio::time::timeout(Duration::from_secs(10), read_answer_async(&mut r))
            .await
            .expect("an answer in time")
            .expect("a readable answer")
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use crate::link::fixtures::link_pair;

    const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    const LOGS: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
    const GONE: &str = "5d2e8a7c1b9f40e3a6c8d1f2b3e4a5c6";
    const OLD: &str = "77c0ffee77c0ffee77c0ffee77c0ffee";

    /// A sibling registered in `registry`, serving its socket.
    fn sibling(registry: &std::path::Path, id: &str, name: Option<&str>) -> Arc<Door> {
        let (door, _inbox, _presence) = registered(registry, meta(id, name));
        let path = Registry::socket_path_in(registry, id);
        let listener = tokio::net::UnixListener::bind(&path).expect("bind");
        socket_door(Arc::clone(&door), listener);
        door
    }

    #[tokio::test]
    async fn myself_is_this_sessions_entry_as_a_sibling_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        sibling(dir.path(), BUILD, Some("build"));
        let path = Registry::socket_path_in(dir.path(), BUILD);
        match ask(&path, Request::Myself).await {
            Answer::Reply(Reply::Entry(e)) => {
                assert_eq!(e.id.to_string(), BUILD);
                assert_eq!(e.name.unwrap().as_str(), "build");
                assert_eq!(e.shell, "/bin/bash");
                assert_eq!(e.attached, Attached::No, "nobody is attached");
                assert!(!e.this);
            }
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn another_version_is_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        sibling(dir.path(), BUILD, None);
        let path = Registry::socket_path_in(dir.path(), BUILD);
        let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        let (r, mut w) = stream.into_split();
        write_line_async(
            &mut w,
            &Open {
                proto: PROTO_VERSION + 1,
                req: Request::Myself,
            },
        )
        .await
        .unwrap();
        let reply: Reply = read_line_async(&mut tokio::io::BufReader::new(r))
            .await
            .unwrap();
        assert!(
            matches!(&reply, Reply::Refused(why) if why.contains("version")),
            "{reply:?}"
        );
    }

    /// The whole listing: this session (from the client's door), a named
    /// sibling, a sibling that never answers, and one that speaks another
    /// version.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn sessions_lists_this_one_its_siblings_and_says_which_did_not_answer() {
        let dir = tempfile::tempdir().unwrap();
        let (me, _inbox, _presence) = registered(dir.path(), meta(BUILD, Some("build")));
        sibling(dir.path(), LOGS, Some("logs"));

        // Registered, its socket bound -- and nobody accepting: connecting
        // works off the backlog, and no answer ever comes.
        let (_gone, _gone_inbox, _) = registered(dir.path(), meta(GONE, None));
        let _deaf =
            tokio::net::UnixListener::bind(Registry::socket_path_in(dir.path(), GONE)).unwrap();

        // An older binary answers every connection with its hello, as the
        // previous protocol did.
        let (_old, _old_inbox, _) = registered(dir.path(), meta(OLD, None));
        let old =
            tokio::net::UnixListener::bind(Registry::socket_path_in(dir.path(), OLD)).unwrap();
        tokio::spawn(async move {
            while let Ok((s, _)) = old.accept().await {
                use tokio::io::AsyncWriteExt as _;
                let mut s = s;
                let hello = format!(
                    concat!(
                        r#"{{"t":"HostHello","proto":2,"session_id":"{}","attach_id":1,"#,
                        r#""cert_spki_sha256":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
                        r#""psk":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
                        r#""candidates":[],"nat_type":"Unknown","bound_port":443,"#,
                        r#""detachable":true}}"#,
                        "\n"
                    ),
                    OLD
                );
                let _ = s.write_all(hello.as_bytes()).await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });

        let begun = Instant::now();
        let list = sessions(&me, Via::Client).await;
        assert!(
            begun.elapsed() < SIBLING_TIMEOUT + Duration::from_secs(1),
            "the siblings were asked one after another: {:?}",
            begun.elapsed()
        );
        let by_id = |id: &str| {
            list.iter()
                .find(|e| e.id.to_string() == id)
                .unwrap_or_else(|| panic!("{id} is missing from {list:?}"))
        };
        assert_eq!(list.len(), 4, "{list:?}");
        assert!(by_id(BUILD).this);
        assert_eq!(by_id(BUILD).attached, Attached::Here);
        assert!(!by_id(LOGS).this);
        assert_eq!(by_id(LOGS).attached, Attached::No);
        assert_eq!(by_id(LOGS).name.as_ref().unwrap().as_str(), "logs");
        assert_eq!(by_id(GONE).attached, Attached::Unknown);
        assert_eq!(by_id(OLD).attached, Attached::OtherVersion);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_sibling_with_a_client_is_listed_as_in_use_elsewhere() {
        let dir = tempfile::tempdir().unwrap();
        let (me, _inbox, _) = registered(dir.path(), meta(BUILD, None));
        let (logs, _logs_inbox, presence) = registered(dir.path(), meta(LOGS, None));
        let listener =
            tokio::net::UnixListener::bind(Registry::socket_path_in(dir.path(), LOGS)).unwrap();
        socket_door(logs, listener);
        // The way the session's loop says it: a link, heard from now.
        let (host_link, _client) = link_pair().await;
        let _session = crate::host_session::HostSession::spawn(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            oxutrm_proto::TermSize {
                cols: 120,
                rows: 40,
            },
            200,
            host_link,
        )
        .unwrap()
        .with_presence(presence);

        let list = sessions(&me, Via::Client).await;
        let logs = list.iter().find(|e| e.id.to_string() == LOGS).unwrap();
        assert_eq!(logs.attached, Attached::Elsewhere);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_attach_goes_to_the_loop_with_its_stream_and_its_role() {
        let dir = tempfile::tempdir().unwrap();
        let (door, mut inbox, _) = registered(dir.path(), meta(BUILD, None));
        let (host, client) = link_pair().await;
        crate::control::serve_control(host.sink.connection().clone(), Arc::clone(&door));

        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        write_line_async(
            &mut send,
            &Open::new(Request::Attach {
                role: Role::Standby,
            }),
        )
        .await
        .unwrap();
        // What follows the request reaches whoever the stream is handed to:
        // the exchange reads the client's hello from it.
        write_line_async(&mut send, &Open::new(Request::Probe { nonce: 1 }))
            .await
            .unwrap();
        let mut req = tokio::time::timeout(Duration::from_secs(5), inbox.standby.recv())
            .await
            .expect("the loop was asked")
            .expect("the queue is open");
        assert_eq!(req.role, Role::Standby);
        let next: Open = read_line_async(&mut req.reader).await.unwrap();
        assert_eq!(next.req, Request::Probe { nonce: 1 });
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_first_line_that_is_no_open_reaches_nothing_and_the_door_stays_open() {
        let dir = tempfile::tempdir().unwrap();
        let (door, mut inbox, _) = registered(dir.path(), meta(BUILD, None));
        let (host, client) = link_pair().await;
        crate::control::serve_control(host.sink.connection().clone(), Arc::clone(&door));

        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        oxutrm_host::signalling::write_signal_async(
            &mut send,
            &oxutrm_proto::Signal::Failed {
                reason: "stray".into(),
            },
        )
        .await
        .unwrap();
        send.finish().unwrap();
        let knocked = tokio::time::timeout(Duration::from_millis(500), async {
            tokio::select! {
                r = inbox.primary.recv() => r,
                r = inbox.standby.recv() => r,
            }
        })
        .await;
        assert!(knocked.is_err(), "a stray line reached the attach loop");

        assert!(
            crate::control::probe(client.sink.connection().clone(), 9).await,
            "the door stopped serving after a stray stream"
        );
    }
}
````

`src/listener.rs`:

````diff
--- a/src/listener.rs
+++ b/src/listener.rs
@@ -203,8 +202,11 @@ pub(crate) async fn close_the_door(task: tokio::task::JoinHandle<()>) {
 #[cfg(test)]
 mod tests {
     use super::*;
     use crate::attach_exchange::fixtures::{fresh_meta, stun_free};
+    use crate::door::{AttachQueue, Door, attach_queue};
+    use oxutrm_host::signalling::write_line_async;
+    use oxutrm_proto::{Open, Request};
 
     /// Long enough that a live loop always answers, short enough that a dead
     /// one does not hold the suite up. Nothing real waits on it: with
     /// [`stun_free`] the exchange reaches R6 in microseconds.
@@ -212,296 +214,229 @@ mod tests {
 
     /// How long "nothing arrived" is observed for.
     const QUIET: Duration = Duration::from_millis(500);
 
-    /// Connect, and read what the accept loop says back.
+    /// A session's doors as `serve.rs` opens them: registered as `meta` in a
+    /// temp registry, its socket accepted on, its attach loop running.
+    struct Doors {
+        _dir: tempfile::TempDir,
+        registry: std::path::PathBuf,
+        sock: std::path::PathBuf,
+        door: Option<std::sync::Arc<Door>>,
+        queue: AttachQueue,
+        guard_dir: std::path::PathBuf,
+        rx: tokio::sync::mpsc::Receiver<Attached>,
+        accept: tokio::task::JoinHandle<()>,
+        attaches: tokio::task::JoinHandle<()>,
+    }
+
+    fn doors(meta: SessionMeta, attach_timeout: Duration) -> Doors {
+        let dir = tempfile::tempdir().expect("a temp dir");
+        let registry = dir.path().to_path_buf();
+        let guard = std::sync::Arc::new(
+            oxutrm_host::RegistryGuard::register_in(&registry, &meta).expect("register"),
+        );
+        let sock = guard.socket_path();
+        let guard_dir = guard.dir().to_path_buf();
+        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
+        let (queue, inbox) = attach_queue();
+        let door = Door::new(
+            registry.clone(),
+            meta,
+            Some(guard),
+            crate::host_session::Presence::default(),
+            Some(queue.clone()),
+        );
+        let (tx, rx) = tokio::sync::mpsc::channel(1);
+        let accept = tokio::spawn(accept_doors(listener, std::sync::Arc::clone(&door)));
+        let attaches = tokio::spawn(serve_attaches(
+            std::sync::Arc::clone(&door),
+            stun_free(),
+            attach_timeout,
+            inbox,
+            tx,
+        ));
+        Doors {
+            _dir: dir,
+            registry,
+            sock,
+            door: Some(door),
+            queue,
+            guard_dir,
+            rx,
+            accept,
+            attaches,
+        }
+    }
+
+    /// Connect, ask for a primary attach, and return the stream's halves.
+    async fn open_attach(
+        sock: &std::path::Path,
+    ) -> (
+        tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
+        tokio::net::unix::OwnedWriteHalf,
+    ) {
+        let stream = tokio::net::UnixStream::connect(sock)
+            .await
+            .expect("connecting to the session socket");
+        let (r, mut w) = stream.into_split();
+        write_line_async(
+            &mut w,
+            &Open::new(Request::Attach {
+                role: Role::Primary,
+            }),
+        )
+        .await
+        .expect("asking to attach");
+        (tokio::io::BufReader::new(r), w)
+    }
+
+    /// Ask for an attach, and read what the loop says back.
     ///
     /// **This, and not a bare `connect`, is how "the door is still open" is
     /// observed.** `UnixStream::connect` succeeds off the kernel's listen
     /// backlog whether or not anything is still calling `accept`, so it cannot
-    /// tell a live loop from a dead one — a socket whose owner has stopped
-    /// accepting takes connections just the same, until the backlog fills. A
-    /// `HostHello` cannot be produced that way: only a loop that went round,
-    /// accepted, and ran the exchange as far as R6 writes one.
+    /// tell a live loop from a dead one. A `HostHello` cannot be produced that
+    /// way: only a loop that went round and ran the exchange as far as R6
+    /// writes one.
     ///
     /// The write half is bound rather than dropped, so the connection is not
     /// shut down before the loop has answered on it.
     async fn hello_off(sock: &std::path::Path) -> oxutrm_proto::Signal {
-        let stream = tokio::net::UnixStream::connect(sock)
-            .await
-            .expect("connecting to the session socket");
-        let (r, _w) = stream.into_split();
-        let mut r = tokio::io::BufReader::new(r);
+        let (mut r, _w) = open_attach(sock).await;
         tokio::time::timeout(
             ANSWER_WITHIN,
             oxutrm_host::signalling::read_signal_async(&mut r),
         )
         .await
-        .expect("the accept loop never answered; it is not accepting any more")
-        .expect("the accept loop answered with something that is not a Signal")
+        .expect("the attach loop never answered; it is not accepting any more")
+        .expect("the attach loop answered with something that is not a Signal")
     }
 
-    /// A failed attach attempt must not disturb the running session.
-    ///
-    /// The arm only ever fires on a COMPLETED link, so a client that connects
-    /// and hangs up costs the session nothing — no message on the channel, and
-    /// the listener still accepting afterwards.
-    ///
-    /// Both halves of that used to be unobservable. `try_recv` ran microseconds
-    /// after the drop, before the loop could have finished handling it, so it
-    /// read "empty" whatever `serve_attaches` did; and the second `connect`
-    /// proved only that the kernel has a backlog. Both are completed
-    /// observations now: a wait that must elapse, and an answer that must
-    /// arrive.
+    fn on_disk(d: &Doors) -> SessionMeta {
+        serde_json::from_slice(
+            &std::fs::read(d.guard_dir.join(oxutrm_host::META_FILE)).expect("meta.json is there"),
+        )
+        .expect("meta.json parses")
+    }
+
+    /// A failed attach attempt must not disturb the running session: a
+    /// client that asks and hangs up costs nothing -- no message on the
+    /// channel, and the loop still attaching afterwards.
     #[tokio::test]
     async fn an_abandoned_attempt_sends_nothing_and_leaves_the_door_open() {
-        let dir = tempfile::tempdir().expect("a temp dir");
-        let sock = dir.path().join("sock");
-        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
-        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
-        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-        let start = fresh_meta("door");
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
-        );
-        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));
-
-        let task = tokio::spawn(serve_attaches(
-            listener,
-            guard,
-            std::sync::Arc::clone(&meta),
-            stun_free(),
+        let mut d = doors(
+            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
             ATTACH_TIMEOUT,
-            door_rx,
-            door_tx,
-            tx,
-        ));
-
-        // Connect and hang up without saying anything.
-        drop(
-            tokio::net::UnixStream::connect(&sock)
-                .await
-                .expect("connect"),
         );
 
-        // Nothing reaches the session, and the loop has had [`QUIET`] to be
-        // wrong about that. An elapsed wait is the passing case; the two ways
-        // of not elapsing are different defects and say so separately.
-        match tokio::time::timeout(QUIET, rx.recv()).await {
+        drop(open_attach(&d.sock).await);
+
+        match tokio::time::timeout(QUIET, d.rx.recv()).await {
             Err(_) => {}
             Ok(Some(_)) => {
                 panic!("an attempt that never completed handed a link to the session")
             }
             Ok(None) => panic!(
-                "the accept loop dropped its sender: it stopped serving on a \
+                "the attach loop dropped its sender: it stopped serving on a \
                  failed attempt instead of going round again"
             ),
         }
 
-        // And the door is still open. See [`hello_off`] for why a second
-        // `connect` is not that observation.
-        let hello = hello_off(&sock).await;
+        let hello = hello_off(&d.sock).await;
         assert!(
             matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
-            "the loop answered the second connection with {hello:?} instead of \
-             an offer"
+            "the loop answered the second attach with {hello:?} instead of an offer"
         );
-        task.abort();
     }
 
-    /// A peer that connects and then says nothing must not close the door for
-    /// the life of the session.
-    ///
-    /// The exchange runs inline in the accept loop, one at a time, by design.
-    /// Nothing inside it bounded the wait for a `ClientHello`, so a peer that
-    /// stayed alive and silent parked the loop for ever: the session went on
-    /// advertising a socket it would never answer on again, silently, and the
-    /// only remedy was killing the session.
-    ///
-    /// A short timeout is passed in rather than [`ATTACH_TIMEOUT`] so this
-    /// costs a fifth of a second instead of ninety. The code it exercises is
-    /// the same.
+    /// A peer that asks to attach and then says nothing must not close the
+    /// door for the life of the session. A short timeout is passed in rather
+    /// than [`ATTACH_TIMEOUT`] so this costs a fifth of a second instead of
+    /// ninety. The code it exercises is the same.
     #[tokio::test]
     async fn a_stalled_attach_gives_up_and_the_door_opens_again() {
-        let dir = tempfile::tempdir().expect("a temp dir");
-        let sock = dir.path().join("sock");
-        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
-        let (tx, _rx) = tokio::sync::mpsc::channel(1);
-        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-        let start = fresh_meta("stall");
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
-        );
-        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));
-
-        let task = tokio::spawn(serve_attaches(
-            listener,
-            std::sync::Arc::clone(&guard),
-            std::sync::Arc::clone(&meta),
-            stun_free(),
+        let d = doors(
+            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
             Duration::from_millis(200),
-            door_rx,
-            door_tx,
-            tx,
-        ));
-
-        // Alive and silent, which is the case that has no other rescue: the
-        // connection is HELD, so the exchange's read of the client's hello
-        // never returns and nothing about the peer being gone can free the
-        // loop.
-        let stalled = tokio::net::UnixStream::connect(&sock)
-            .await
-            .expect("connecting to the session socket");
+        );
+        // Alive and silent: the connection is HELD, so the exchange's read of
+        // the client's hello never returns.
+        let stalled = open_attach(&d.sock).await;
 
-        // The loop has to come back to `accept` on its own clock.
-        let hello = hello_off(&sock).await;
+        let hello = hello_off(&d.sock).await;
         assert!(
             matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
-            "the loop answered the second connection with {hello:?} instead of \
-             an offer"
+            "the loop answered the second attach with {hello:?} instead of an offer"
         );
-
-        // A timed-out attempt is a failed attempt, and nothing more.
-        let on_disk: SessionMeta =
-            serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json is there"))
-                .expect("meta.json parses");
         assert_eq!(
-            on_disk.attach_id, 0,
+            on_disk(&d).attach_id,
+            0,
             "an attempt that timed out still advertised its generation"
         );
-
         drop(stalled);
-        task.abort();
     }
 
     /// A failed attempt must not advertise a generation that never served.
-    ///
-    /// `begin_attach` bumps `attach_id` at R4, before the exchange can fail,
-    /// so the in-memory record moves even when nothing comes of it. The
-    /// registry is only written on success — otherwise `--list` and a
-    /// reconnecting client would name a generation that no link ever ran as.
+    /// `begin_attach` bumps `attach_id` at R4, before the exchange can fail;
+    /// the registry is only written on success.
     #[tokio::test]
     async fn a_failed_attempt_does_not_write_its_generation_to_the_registry() {
-        let dir = tempfile::tempdir().expect("a temp dir");
-        let sock = dir.path().join("sock");
-        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
-        let (tx, _rx) = tokio::sync::mpsc::channel(1);
-        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-        let start = fresh_meta("gen");
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
-        );
-        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));
-
-        let task = tokio::spawn(serve_attaches(
-            listener,
-            std::sync::Arc::clone(&guard),
-            std::sync::Arc::clone(&meta),
-            stun_free(),
+        let d = doors(
+            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
             ATTACH_TIMEOUT,
-            door_rx,
-            door_tx,
-            tx,
-        ));
-
-        // Connect and hang up: R4 runs, R7 fails.
-        drop(
-            tokio::net::UnixStream::connect(&sock)
-                .await
-                .expect("connect"),
         );
 
-        // Ordered by the loop itself, not by a sleep. The exchange is serial —
-        // one attach at a time is this loop's whole design — so a second
-        // connection being ANSWERED is proof that the first attempt is over
-        // and has written whatever it was going to write. The fixed 50 ms
-        // sleep this replaces was not proof of anything: with a three-second
-        // gather budget in front of it, it is how this guard once passed under
-        // an injected bug, reading `meta.json` while the exchange was still
-        // probing.
-        let hello = hello_off(&sock).await;
+        // Ask and hang up: R4 runs, R7 fails.
+        drop(open_attach(&d.sock).await);
+
+        // Ordered by the loop itself, not by a sleep: it is serial, so a
+        // second attach being ANSWERED is proof that the first attempt is
+        // over and has written whatever it was going to write.
+        let hello = hello_off(&d.sock).await;
         assert!(
             matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
             "the first attempt never finished, so nothing below is ordered \
              after it: {hello:?}"
         );
-
-        let on_disk: SessionMeta =
-            serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json is there"))
-                .expect("meta.json parses");
         assert_eq!(
-            on_disk.attach_id, 0,
-            "the registry advertises generation {} but no link ever ran as it; \
-             a client told to expect it would be waiting for a host that is \
-             not there",
-            on_disk.attach_id
+            on_disk(&d).attach_id,
+            0,
+            "the registry advertises a generation no link ever ran as"
         );
-        task.abort();
     }
 
-    /// A standby comes in through the door, runs the very same exchange, and
-    /// comes out marked as a standby — while an attach over the Unix socket
-    /// comes out as a primary.
-    ///
-    /// The role is what the session acts on: a standby is parked, a primary
-    /// takes over. A standby that came out as a primary would displace the
-    /// live link and close it as taken over, which ends the client. So both
-    /// exchanges are driven to completion by the real client half,
-    /// `connect::establish`, and the role is read off the `Attached` the
-    /// session would receive — the socket case first, so "every attach says
-    /// Standby" cannot pass either.
+    /// A standby comes in through the queue, runs the very same exchange,
+    /// and comes out marked as a standby -- while an attach over the socket
+    /// comes out as a primary. A standby that came out as a primary would
+    /// displace the live link and end the client.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn a_door_request_runs_the_same_exchange_and_carries_its_role() {
-        let dir = tempfile::tempdir().expect("a temp dir");
-        let sock = dir.path().join("sock");
-        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
-        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
-        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-        let start = fresh_meta("roles");
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
-        );
-        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));
-
-        let task = tokio::spawn(serve_attaches(
-            listener,
-            guard,
-            std::sync::Arc::clone(&meta),
-            stun_free(),
+    async fn a_queued_standby_runs_the_same_exchange_and_carries_its_role() {
+        let mut d = doors(
+            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
             ATTACH_TIMEOUT,
-            door_rx,
-            door_tx.clone(),
-            tx,
-        ));
+        );
         let size = oxutrm_proto::TermSize { cols: 80, rows: 24 };
         let within = Duration::from_secs(10);
 
-        // Over the Unix socket: a primary.
-        let stream = tokio::net::UnixStream::connect(&sock)
-            .await
-            .expect("connecting to the session socket");
-        let (cr, cw) = stream.into_split();
+        let (cr, cw) = open_attach(&d.sock).await;
         let primary = tokio::time::timeout(
             within,
-            crate::connect::establish(tokio::io::BufReader::new(cr), cw, size, &stun_free(), None),
+            crate::connect::establish(cr, cw, size, &stun_free(), None),
         )
         .await
         .expect("the client exchange over the socket must not hang")
         .expect("the client exchange over the socket completes");
-        let attached = tokio::time::timeout(within, rx.recv())
+        let attached = tokio::time::timeout(within, d.rx.recv())
             .await
             .expect("the socket attach never reached the session")
-            .expect("the listener dropped its sender");
-        assert_eq!(
-            attached.role,
-            Role::Primary,
-            "an ssh-relayed attach must take over, not park"
-        );
+            .expect("the loop dropped its sender");
+        assert_eq!(attached.role, Role::Primary);
 
-        // Through the door: the same exchange over a different pipe.
         let (client_side, host_side) = tokio::io::duplex(64 * 1024);
         let (hr, hw) = tokio::io::split(host_side);
-        door_tx
+        d.queue
+            .standby
             .send(DoorRequest {
                 reader: Box::new(tokio::io::BufReader::new(hr)),
                 writer: Box::new(hw),
                 role: Role::Standby,
@@ -513,81 +448,39 @@ mod tests {
             within,
             crate::connect::establish(tokio::io::BufReader::new(cr), cw, size, &stun_free(), None),
         )
         .await
-        .expect("the client exchange through the door must not hang")
-        .expect("the client exchange through the door completes");
-        let attached = tokio::time::timeout(within, rx.recv())
+        .expect("the standby exchange must not hang")
+        .expect("the standby exchange completes");
+        let attached = tokio::time::timeout(within, d.rx.recv())
             .await
-            .expect("the door attach never reached the session")
-            .expect("the listener dropped its sender");
-        assert_eq!(
-            attached.role,
-            Role::Standby,
-            "a standby came out of the door as a takeover: it would displace \
-             the live link and end the client"
-        );
+            .expect("the standby never reached the session")
+            .expect("the loop dropped its sender");
+        assert_eq!(attached.role, Role::Standby);
+        assert_eq!(on_disk(&d).attach_id, 2, "both generations were recorded");
 
-        // Every link handed over carries a control server of its own. On the
-        // standby it answers the probe failover waits on; on the primary it
-        // takes the next standby request. Nothing in this test starts one, so
-        // only the listener can be answering. And a link with no server is
-        // not answered, so the probe itself cannot pass on its own.
+        // Every link handed over carries a control server of its own.
         let (_unserved_host, unserved) = crate::link::fixtures::link_pair().await;
-        assert!(
-            !crate::control::probe(unserved.sink.connection().clone(), 1).await,
-            "a link with no control server answered a probe"
-        );
-        assert!(
-            crate::control::probe(standby.link.sink.connection().clone(), 2).await,
-            "the standby the listener handed over answers no probe: failover \
-             would never fire"
-        );
-        assert!(
-            crate::control::probe(primary.link.sink.connection().clone(), 3).await,
-            "the primary the listener handed over answers no probe, so it \
-             cannot carry the next standby request either"
-        );
-        task.abort();
+        assert!(!crate::control::probe(unserved.sink.connection().clone(), 1).await);
+        assert!(crate::control::probe(standby.link.sink.connection().clone(), 2).await);
+        assert!(crate::control::probe(primary.link.sink.connection().clone(), 3).await);
     }
 
-    /// A standby's exchange gives way to an attach over the socket.
-    ///
-    /// The door is serial, and a standby whose control stream went silent
-    /// never errors: without pre-emption it would hold the door for the whole
-    /// of [`ATTACH_TIMEOUT`], ninety seconds, while the client's ssh rebuild —
-    /// the attach that matters — queued behind it. The real timeout is passed
-    /// on purpose, so nothing but pre-emption can free the loop in time.
+    /// A standby's exchange gives way to a primary. The real timeout is
+    /// passed on purpose, so nothing but pre-emption can free the loop in
+    /// time.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
-    async fn a_socket_attach_preempts_a_stalled_standby() {
-        let dir = tempfile::tempdir().expect("a temp dir");
-        let sock = dir.path().join("sock");
-        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
-        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
-        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-        let start = fresh_meta("preempt");
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
-        );
-        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));
-
-        let task = tokio::spawn(serve_attaches(
-            listener,
-            guard,
-            std::sync::Arc::clone(&meta),
-            stun_free(),
+    async fn a_primary_preempts_a_stalled_standby() {
+        let mut d = doors(
+            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
             ATTACH_TIMEOUT,
-            door_rx,
-            door_tx.clone(),
-            tx,
-        ));
+        );
         let within = Duration::from_secs(10);
 
-        // A standby whose peer hears the offer and then says nothing, held
-        // open so nothing about the peer being gone can end the attempt.
         let (client_side, host_side) = tokio::io::duplex(64 * 1024);
         let (hr, hw) = tokio::io::split(host_side);
-        door_tx
+        d.queue
+            .standby
             .send(DoorRequest {
                 reader: Box::new(tokio::io::BufReader::new(hr)),
                 writer: Box::new(hw),
                 role: Role::Standby,
@@ -595,106 +488,150 @@ mod tests {
             .await
             .unwrap();
         let (cr, _cw) = tokio::io::split(client_side);
         let mut cr = tokio::io::BufReader::new(cr);
-        // Before: the standby's exchange is under way, holding the door.
         let offer =
             tokio::time::timeout(within, oxutrm_host::signalling::read_signal_async(&mut cr))
                 .await
                 .expect("the standby's exchange never started")
                 .expect("the standby's exchange sent something that is not a Signal");
-        assert!(
-            matches!(offer, oxutrm_proto::Signal::HostHello { .. }),
-            "{offer:?}"
-        );
+        assert!(matches!(offer, oxutrm_proto::Signal::HostHello { .. }));
 
-        // After: a socket attach gets through anyway, and as a primary.
-        let stream = tokio::net::UnixStream::connect(&sock)
-            .await
-            .expect("connecting to the session socket");
-        let (sr, sw) = stream.into_split();
+        let (sr, sw) = open_attach(&d.sock).await;
         tokio::time::timeout(
             within,
             crate::connect::establish(
-                tokio::io::BufReader::new(sr),
+                sr,
                 sw,
                 oxutrm_proto::TermSize { cols: 80, rows: 24 },
                 &stun_free(),
                 None,
             ),
         )
         .await
-        .expect(
-            "the socket attach waited behind a stalled standby: the rebuild \
-             it stands for would queue for the whole attach timeout",
-        )
-        .expect("the socket attach completes");
-        let attached = tokio::time::timeout(within, rx.recv())
+        .expect("the primary waited behind a stalled standby")
+        .expect("the primary completes");
+        let attached = tokio::time::timeout(within, d.rx.recv())
             .await
-            .expect("the socket attach never reached the session")
-            .expect("the listener dropped its sender");
+            .expect("the primary never reached the session")
+            .expect("the loop dropped its sender");
         assert_eq!(attached.role, Role::Primary);
-        task.abort();
     }
 
-    /// The listener's `Arc` clone of the guard has to be gone before the
-    /// session drops its own, or the session directory outlives the session.
-    ///
-    /// `serve()` (`src/serve.rs`) ends on [`close_the_door`], and so does
-    /// this test; the listener must also be gone at all, which the timeout
-    /// asserts. `abort()` only SCHEDULES cancellation: without the await, the spawned
-    /// future — and its clone of the guard — is still alive when `drop(guard)`
-    /// runs, `RegistryGuard::drop` decrements two to one instead of one to
-    /// zero, `remove_dir_all` never runs, and the session leaves an entry
-    /// behind for `--list` to prune. Worse, `run_host_serve`'s comment cites
-    /// that cleanup as the reason it is allowed to `shutdown_background()` and
-    /// walk away.
-    ///
-    /// Asserted on the directory — the side effect — rather than on a strong
-    /// count, which is the mechanism.
-    #[tokio::test]
-    async fn awaiting_the_aborted_listener_is_what_lets_the_guard_clean_up() {
-        let dir = tempfile::tempdir().expect("a temp dir");
-        let sock = dir.path().join("sock");
-        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
-        let (tx, _rx) = tokio::sync::mpsc::channel(1);
-        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-        let start = fresh_meta("clean");
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
+    /// `Myself` is answered while the session is in the middle of an attach
+    /// (switcher spec §2.2): it never waits on the serial loop.
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn myself_is_answered_while_an_attach_is_under_way() {
+        let d = doors(
+            crate::door::fixtures::meta("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("build")),
+            ATTACH_TIMEOUT,
         );
-        let session_dir = guard.dir().to_path_buf();
-        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));
+        // An attach that has its hello and will say nothing more: the loop
+        // is held for the whole attach timeout.
+        let (mut r, _w) = open_attach(&d.sock).await;
+        let _ = oxutrm_host::signalling::read_signal_async(&mut r).await;
 
-        let task = tokio::spawn(serve_attaches(
-            listener,
-            std::sync::Arc::clone(&guard),
-            meta,
-            stun_free(),
+        let begun = std::time::Instant::now();
+        let answer = crate::door::fixtures::ask(&d.sock, Request::Myself).await;
+        assert!(
+            matches!(&answer, oxutrm_proto::Answer::Reply(oxutrm_proto::Reply::Entry(e)) if e.name.as_ref().map(oxutrm_proto::Name::as_str) == Some("build")),
+            "{answer:?}"
+        );
+        assert!(
+            begun.elapsed() < Duration::from_secs(1),
+            "Myself waited on the attach: {:?}",
+            begun.elapsed()
+        );
+    }
+
+    /// A `Sessions` fetch asks each sibling `Myself`; it must not cancel a
+    /// sibling's standby exchange, which a fetch through the attach loop
+    /// would pre-empt (switcher spec §2.2).
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_sessions_fetch_does_not_cancel_a_siblings_standby_search() {
+        let mut sibling = doors(
+            crate::door::fixtures::meta("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60", Some("logs")),
             ATTACH_TIMEOUT,
-            door_rx,
-            door_tx,
-            tx,
-        ));
-        assert!(session_dir.exists(), "the fixture registered nothing");
-
-        // Exactly what `serve()` does when the shell exits -- the same call,
-        // not a copy of it. A copy is how `serve()` lost its `abort()` while
-        // this test, aborting on its own, stayed green: the listener never
-        // returns by itself, so the session's daemon outlived its shell,
-        // stayed on `--list`, and handed the next reattach a dead link.
-        tokio::time::timeout(Duration::from_secs(5), close_the_door(task))
+        );
+        // This session, in the same registry, asking.
+        let (me, _inbox, _) = crate::door::fixtures::registered(
+            &sibling.registry,
+            crate::door::fixtures::meta("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("build")),
+        );
+        let me_sock = oxutrm_host::Registry::socket_path_in(
+            &sibling.registry,
+            "3ff1218f5e0c4b7d9a1c2e3f40516273",
+        );
+        crate::door::fixtures::socket_door(
+            me,
+            tokio::net::UnixListener::bind(&me_sock).expect("bind"),
+        );
+
+        // The sibling's standby exchange, under way.
+        let (client_side, host_side) = tokio::io::duplex(64 * 1024);
+        let (hr, hw) = tokio::io::split(host_side);
+        sibling
+            .queue
+            .standby
+            .send(DoorRequest {
+                reader: Box::new(tokio::io::BufReader::new(hr)),
+                writer: Box::new(hw),
+                role: Role::Standby,
+            })
             .await
-            .expect(
-                "the listener was still running after the shell exited: the \
-                 daemon outlives its session and keeps accepting attaches",
-            );
-        drop(guard);
+            .unwrap();
+
+        let answer = crate::door::fixtures::ask(&me_sock, Request::Sessions).await;
+        let oxutrm_proto::Answer::Reply(oxutrm_proto::Reply::Sessions(list)) = answer else {
+            panic!("{answer:?}");
+        };
+        assert_eq!(list.len(), 2, "{list:?}");
+
+        // And the standby still completes, as a standby.
+        let (cr, cw) = tokio::io::split(client_side);
+        tokio::time::timeout(
+            Duration::from_secs(10),
+            crate::connect::establish(
+                tokio::io::BufReader::new(cr),
+                cw,
+                oxutrm_proto::TermSize { cols: 80, rows: 24 },
+                &stun_free(),
+                None,
+            ),
+        )
+        .await
+        .expect("the standby hung")
+        .expect("the standby was cancelled by the fetch");
+        let attached = sibling.rx.recv().await.expect("the standby landed");
+        assert_eq!(attached.role, Role::Standby);
+    }
+
+    /// The tasks' clones of the door -- and through it of the guard -- have
+    /// to be gone before the session drops its own, or the session
+    /// directory outlives the session. `abort()` only SCHEDULES
+    /// cancellation; [`close_the_door`] awaits it.
+    #[tokio::test]
+    async fn awaiting_the_aborted_tasks_is_what_lets_the_guard_clean_up() {
+        let mut d = doors(
+            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
+            ATTACH_TIMEOUT,
+        );
+        assert!(d.guard_dir.exists(), "the fixture registered nothing");
+
+        let accept = std::mem::replace(&mut d.accept, tokio::spawn(async {}));
+        let attaches = std::mem::replace(&mut d.attaches, tokio::spawn(async {}));
+        tokio::time::timeout(Duration::from_secs(5), async {
+            close_the_door(accept).await;
+            close_the_door(attaches).await;
+        })
+        .await
+        .expect("the door's tasks were still running after the shell exited");
+        drop(d.door.take());
 
         assert!(
-            !session_dir.exists(),
-            "the session ended and {} is still there: the listener task was \
-             still holding a clone of the guard, so its Drop never ran",
-            session_dir.display()
+            !d.guard_dir.exists(),
+            "the session ended and {} is still there: a task was still \
+             holding the door, so the guard's Drop never ran",
+            d.guard_dir.display()
         );
     }
 }
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -244,2 +262,2 @@
 #[cfg(test)]
 mod tests {
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -6604,12 +6604,16 @@ mod tests {
     async fn a_dead_primary_fails_over_onto_an_answering_standby() {
         let (mut host, mut client, relay) = pair_through_relay_sized("", BIG).await;
         let primary_id = client.link.sink.connection().stable_id();
         let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
-        // The probe is answered by the standby's own control server. Its door
-        // is never knocked on.
-        let (door_tx, _door_rx) = tokio::sync::mpsc::channel(1);
-        crate::control::serve_control(standby_host.sink.connection().clone(), door_tx);
+        // The probe is answered by the standby's own control server. Its
+        // attach loop is never asked for anything.
+        let door_dir = tempfile::tempdir().expect("a scratch directory");
+        let (door, _inbox, _) = crate::door::fixtures::registered(
+            door_dir.path(),
+            crate::door::fixtures::meta("00112233445566778899aabbccddeeff", None),
+        );
+        crate::control::serve_control(standby_host.sink.connection().clone(), door);
         let standby_id = standby_client.sink.connection().stable_id();
         assert_ne!(standby_id, primary_id);
 
         client.standby = Some(standby_holding(standby_client));
@@ -6682,20 +6686,20 @@ mod tests {
     async fn a_standby_is_found_over_the_first_links_control_stream() {
         let (mut host, mut client) = pair_sized("", BIG).await;
         let primary_id = client.link.sink.connection().stable_id();
         let dir = tempfile::tempdir().expect("a scratch directory");
-        let listener =
-            tokio::net::UnixListener::bind(dir.path().join("sock")).expect("binding the socket");
         let start =
             crate::attach_exchange::fixtures::fresh_meta("00112233445566778899aabbccddeeff");
         let guard = Arc::new(
             oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
         );
-        let meta = Arc::new(tokio::sync::Mutex::new(start));
-        let (door_task, mut attach_rx) = crate::serve::open_doors(
+        let listener = tokio::net::UnixListener::bind(guard.socket_path()).expect("binding");
+        let (door_tasks, mut attach_rx) = crate::serve::open_doors(
             listener,
-            guard,
-            Arc::clone(&meta),
+            dir.path().to_path_buf(),
+            start,
+            Arc::clone(&guard),
+            crate::host_session::Presence::default(),
             crate::attach_exchange::fixtures::stun_free(),
             host.link.sink.connection().clone(),
         );
         let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });
@@ -6726,13 +6730,24 @@ mod tests {
         // itself: open it by hand and watch its log.
         typing.write_all(&[CTRL_BACKSLASH]).expect("type");
         wait_for_screen(&out, BIG, "Esc close", Duration::from_secs(10)).await;
         wait_for_screen(&out, BIG, "found ", Duration::from_secs(20)).await;
-        assert_eq!(
-            meta.lock().await.attach_id,
-            1,
-            "the host did not run an exchange for the standby"
-        );
+        // The host records the generation just after its `Established`
+        // went out, so the client may see the standby a moment first.
+        let recorded = async {
+            loop {
+                let on_disk: oxutrm_host::SessionMeta =
+                    serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json"))
+                        .expect("meta.json parses");
+                if on_disk.attach_id == 1 {
+                    return;
+                }
+                tokio::time::sleep(Duration::from_millis(10)).await;
+            }
+        };
+        tokio::time::timeout(Duration::from_secs(5), recorded)
+            .await
+            .expect("the host did not run an exchange for the standby");
 
         // The open popup takes every key: close it, then talk to the shell.
         close_the_popup(&mut typing, &out, BIG).await;
         typing.write_all(b"exit 7\n").expect("type");
@@ -6753,9 +6768,11 @@ mod tests {
                 .any(|e| e.kind == Kind::Standby && e.text.starts_with("found ")),
             "the standby was shown but not recorded"
         );
         assert_eq!(host_loop.await.expect("host task").expect("host loop"), 7);
-        crate::listener::close_the_door(door_task).await;
+        for task in door_tasks {
+            crate::listener::close_the_door(task).await;
+        }
     }
 
     /// What the popup may not say while `Silent` or `Confirming`, wherever
     /// in it it says it.
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --bin oxutrm --jobs 4 door:: listener:: control::`

Expected: compile errors: `crate::door` does not exist, `serve_control` takes a sender, `accept_doors` and `Presence` are not defined.

- [ ] **Step 3: Implement**

`crates/oxutrm-proto/src/lib.rs`:

````diff
--- a/crates/oxutrm-proto/src/lib.rs
+++ b/crates/oxutrm-proto/src/lib.rs
@@ -129,9 +129,15 @@ pub use types::{
 /// endpoint without that fingerprint — so a version-1 client is not a client
 /// with one missing field, it is a client that can never be let in. A hard
 /// version failure with both numbers in it is the only useful outcome, and it
 /// is what `check_version` already does.
-pub const PROTO_VERSION: u32 = 2;
+///
+/// **3**: the session switcher. Every door of a session process -- the
+/// control stream and the Unix socket -- reads an [`Open`] first, the ssh
+/// offer lists [`OfferEntry`]s, and `Choice` gained `Lobby` and a name. No
+/// compatibility with 2: a host session started by an older binary must be
+/// ended before a new client can reach that host.
+pub const PROTO_VERSION: u32 = 3;
 
 /// The host serves a control stream on every link (spec §2).
 pub const FEATURE_CONTROL: &str = "control";
 /// The host parks a standby link on request (spec §3).
````

`crates/oxutrm-proto/src/signal.rs`:

````diff
--- a/crates/oxutrm-proto/src/signal.rs
+++ b/crates/oxutrm-proto/src/signal.rs
@@ -172,19 +172,8 @@ pub enum Signal {
     },
     Failed {
         reason: String,
     },
-    /// client -> host, first line on a control stream: run an attach exchange
-    /// over this stream and park the result as a standby (spec §3.2).
-    StandbyRequest,
-    /// client -> host on a standby's control stream. Answered without
-    /// adopting anything: a sync frame is what adopts (spec §3.4).
-    Probe {
-        nonce: u64,
-    },
-    ProbeAck {
-        nonce: u64,
-    },
 }
 
 impl Signal {
     /// The protocol version a message carries, where it carries one.
````

`src/control.rs`:

````diff
--- a/src/control.rs
+++ b/src/control.rs
@@ -1,72 +1,61 @@
-//! The control stream: a QUIC bidirectional stream on a live link, carrying
-//! ordinary `Signal` lines (spec §2).
+//! The control stream: a QUIC bidirectional stream on a live link (spec §2).
 //!
 //! After `Established`, ssh is gone (`serve.rs`'s sever), and this is the only
 //! channel between the two ends. It is at least as well authenticated as ssh:
 //! both ends of the connection it rides on are pinned by SPKI.
 //!
-//! Each stream is one conversation, and its first line says which:
-//! `StandbyRequest` hands the stream to the session's door, where the ordinary
-//! attach exchange runs over it; `Probe` is answered in place, for as long as
-//! the stream stays open. Anything else is dropped.
+//! Each stream is one conversation, and its first line is an
+//! [`oxutrm_proto::Open`] saying which. The host hands every stream to the
+//! session's door ([`crate::door`]), the same dispatcher its Unix socket
+//! uses: `Attach { Standby }` runs the ordinary attach exchange over the
+//! stream and parks the result; `Probe` is answered in place, for as long as
+//! the stream stays open; the switcher's requests are answered there too.
 //!
-//! The client's half is [`request_standby`] and [`probe`], one stream per
-//! conversation as well.
+//! The client's half is [`request_standby`] and [`probe`] here, one stream
+//! per conversation as well.
 
 // This runs while a client session owns the screen: nothing here may print,
 // or it lands raw on the painted raw-mode terminal.
 #![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]
 
-use oxutrm_host::signalling::{read_signal_async, write_signal_async};
-use oxutrm_proto::Signal;
+use std::sync::Arc;
+
+use oxutrm_host::signalling::{read_line_async, write_line_async};
+pub(crate) use oxutrm_proto::Role;
+use oxutrm_proto::{Open, Reply, Request};
 use tokio::io::{AsyncBufRead, AsyncWrite};
 
-/// What a completed attach is for.
-#[derive(Clone, Copy, Debug, PartialEq, Eq)]
-pub(crate) enum Role {
-    /// Adopt now; newest attach wins (recovery spec §6).
-    Primary,
-    /// Park until the client's first sync frame on it (spec §3.5).
-    Standby,
-}
+use crate::door::{Door, Via};
 
-/// A pair of pipes for the door, and what the attach that runs over them is for.
+/// A pair of pipes for the attach loop, and what the attach that runs over
+/// them is for.
 pub(crate) struct DoorRequest {
     pub reader: Box<dyn AsyncBufRead + Unpin + Send>,
     pub writer: Box<dyn AsyncWrite + Unpin + Send>,
     pub role: Role,
 }
 
-/// Serve control streams on `conn` until it closes.
+/// Serve control streams on `conn` through `door` until it closes.
 ///
 /// Ends by itself when the connection does, because `accept_bi` then fails.
 /// Nothing needs to stop it: every link the host drops is closed first.
 pub(crate) fn serve_control(
     conn: quinn::Connection,
-    door: tokio::sync::mpsc::Sender<DoorRequest>,
+    door: Arc<Door>,
 ) -> tokio::task::JoinHandle<()> {
     tokio::spawn(async move {
         while let Ok((send, recv)) = conn.accept_bi().await {
-            tokio::spawn(one_stream(send, recv, door.clone()));
+            tokio::spawn(crate::door::serve(
+                Arc::clone(&door),
+                Via::Client,
+                tokio::io::BufReader::new(recv),
+                send,
+            ));
         }
     })
 }
 
-/// Serve control streams on a link that has no door behind it.
-///
-/// A session without a Unix socket (rung 4, whose QUIC runs inside ssh) has
-/// no listener to run a standby's exchange, but its hello still advertised a
-/// control stream: the hello is written before the rung is known. So the
-/// link is served anyway, against a door that is already closed. Probes are
-/// answered; a standby request is refused at once, which the client sees as
-/// its stream ending rather than as silence it would wait on for ever.
-pub(crate) fn serve_control_without_door(conn: quinn::Connection) -> tokio::task::JoinHandle<()> {
-    let (door, closed) = tokio::sync::mpsc::channel(1);
-    drop(closed);
-    serve_control(conn, door)
-}
-
 /// Longest a whole standby search may take, from opening the stream to the
 /// host's verdict. Every step inside `establish` has its own budget; this is
 /// the outer wall, as `ATTACH_TIMEOUT` is for the host. It matters more here
 /// than it looks: the primary has no idle timeout, so a request sent into a
@@ -89,11 +78,16 @@ pub(crate) async fn request_standby(
         let (mut send, recv) = primary
             .open_bi()
             .await
             .context("opening a control stream")?;
-        write_signal_async(&mut send, &Signal::StandbyRequest)
-            .await
-            .context("asking for a standby")?;
+        write_line_async(
+            &mut send,
+            &Open::new(Request::Attach {
+                role: Role::Standby,
+            }),
+        )
+        .await
+        .context("asking for a standby")?;
         crate::connect::establish(
             tokio::io::BufReader::new(recv),
             send,
             size,
@@ -113,14 +107,14 @@ pub(crate) async fn request_standby(
 /// since stream ids are ours to allocate, and it is the timeout that decides.
 pub(crate) async fn probe(conn: quinn::Connection, nonce: u64) -> bool {
     let attempt = async {
         let (mut send, recv) = conn.open_bi().await.ok()?;
-        write_signal_async(&mut send, &Signal::Probe { nonce })
+        write_line_async(&mut send, &Open::new(Request::Probe { nonce }))
             .await
             .ok()?;
         let mut reader = tokio::io::BufReader::new(recv);
-        match read_signal_async(&mut reader).await.ok()? {
-            Signal::ProbeAck { nonce: n } if n == nonce => Some(()),
+        match read_line_async(&mut reader).await.ok()? {
+            Reply::ProbeAck { nonce: n } if n == nonce => Some(()),
             _ => None,
         }
     };
     matches!(
@@ -128,48 +122,4 @@ pub(crate) async fn probe(conn: quinn::Connection, nonce: u64) -> bool {
         Ok(Some(()))
     )
 }
 
-async fn one_stream(
-    mut send: quinn::SendStream,
-    recv: quinn::RecvStream,
-    door: tokio::sync::mpsc::Sender<DoorRequest>,
-) {
-    let mut reader = tokio::io::BufReader::new(recv);
-    let Ok(first) = read_signal_async(&mut reader).await else {
-        return;
-    };
-    match first {
-        Signal::StandbyRequest => {
-            // A busy door is waited for: the door is serial, and the attempt
-            // ahead is bounded by `ATTACH_TIMEOUT`. A closed door (a session
-            // with no listener, see `serve_control_without_door`, or one whose
-            // listener has gone with it) drops the stream at once. The
-            // client's search then fails and backs off, which is the whole of
-            // the damage.
-            let _ = door
-                .send(DoorRequest {
-                    reader: Box::new(reader),
-                    writer: Box::new(send),
-                    role: Role::Standby,
-                })
-                .await;
-        }
-        Signal::Probe { nonce } => {
-            let mut nonce = nonce;
-            loop {
-                if write_signal_async(&mut send, &Signal::ProbeAck { nonce })
-                    .await
-                    .is_err()
-                {
-                    return;
-                }
-                match read_signal_async(&mut reader).await {
-                    Ok(Signal::Probe { nonce: next }) => nonce = next,
-                    _ => return,
-                }
-            }
-        }
-        _ => {}
-    }
-}
-
````

`src/door.rs` (new) (the test module from the tests step follows it):

````rust
//! The one dispatcher behind both doors of a session process (switcher spec
//! §2.2, §4.1).
//!
//! A session process -- or a lobby -- has two doors: the QUIC control stream
//! from its own client ([`Via::Client`]) and its Unix socket, from a sibling
//! session or from `oxutrm host --attach` over ssh ([`Via::Socket`]). Both
//! hand every connection to [`serve`], on a task of its own, which reads the
//! first line -- an [`Open`] -- under [`OPEN_TIMEOUT`] and answers it here.
//!
//! Only `Attach` goes on to the session's serial attach loop
//! (`listener::serve_attaches`, one exchange at a time). Everything else is
//! answered from state the door holds itself -- the session's record and
//! whether a client is attached -- so `Myself` and `Sessions` answer within
//! milliseconds even while the session is mid-attach, and a `Sessions` fetch
//! never disturbs a sibling's standby search.
//!
//! A request is always served by the process that owns what it touches: a
//! session only ever writes its own `meta.json` and signals its own shell.

// This runs in the host daemon, whose stderr is nobody's screen, under the
// same rule as the client so nothing here starts printing by accident.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use oxutrm_host::registry::{Registry, RegistryGuard, SessionMeta};
use oxutrm_host::signalling::{read_answer_async, read_line_async, write_line_async};
use oxutrm_proto::{
    Answer, Attached, Open, PROTO_VERSION, ProtoError, Reply, Request, Role, SessionEntry,
};
use tokio::io::{AsyncBufRead, AsyncWrite};

use crate::control::DoorRequest;
use crate::host_session::Presence;

/// How long a connection may take to say what it wants. Generous for a
/// peer on the same machine or a live link, short enough that a silent one
/// costs a task for seconds, not for ever (the QUIC idle timeout is off).
pub(crate) const OPEN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a sibling has to answer `Myself` before it is listed from its
/// `meta.json` as `Unknown` (shown `?`).
pub(crate) const SIBLING_TIMEOUT: Duration = Duration::from_secs(2);

/// Which door a connection came through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Via {
    /// The control stream of a link: this session's own client.
    Client,
    /// The Unix socket: a sibling, or `oxutrm host --attach`.
    Socket,
}

/// Where `Attach` requests go: the serial attach loop's two inboxes. Two,
/// so the loop can let a primary pre-empt a standby's exchange (listener).
#[derive(Clone)]
pub(crate) struct AttachQueue {
    pub(crate) primary: tokio::sync::mpsc::Sender<DoorRequest>,
    pub(crate) standby: tokio::sync::mpsc::Sender<DoorRequest>,
}

/// The receiving half of an [`AttachQueue`], for the serial attach loop.
pub(crate) struct AttachInbox {
    pub(crate) primary: tokio::sync::mpsc::Receiver<DoorRequest>,
    pub(crate) standby: tokio::sync::mpsc::Receiver<DoorRequest>,
}

/// A queue and its inbox. One deep each: the loop is serial, and a request
/// that finds the queue full waits for the attempt ahead, which is bounded.
pub(crate) fn attach_queue() -> (AttachQueue, AttachInbox) {
    let (primary, primary_rx) = tokio::sync::mpsc::channel(1);
    let (standby, standby_rx) = tokio::sync::mpsc::channel(1);
    (
        AttachQueue { primary, standby },
        AttachInbox {
            primary: primary_rx,
            standby: standby_rx,
        },
    )
}

/// The session's own record, as the door answers from it.
struct Own {
    /// What `meta.json` says, kept current by the attach loop and by
    /// renames. Not the loop's working copy: that one is held across an
    /// exchange, and this must answer while one runs.
    meta: SessionMeta,
    /// The registry entry, while there is one: a lobby has none.
    guard: Option<Arc<RegistryGuard>>,
}

/// What one session process's doors share.
pub(crate) struct Door {
    /// The registry directory: where siblings are found.
    registry: PathBuf,
    own: std::sync::Mutex<Own>,
    presence: Presence,
    /// `None` where nothing can run an attach: rung 4, which has no socket.
    attaches: Option<AttachQueue>,
}

impl Door {
    pub(crate) fn new(
        registry: PathBuf,
        meta: SessionMeta,
        guard: Option<Arc<RegistryGuard>>,
        presence: Presence,
        attaches: Option<AttachQueue>,
    ) -> Arc<Door> {
        Arc::new(Door {
            registry,
            own: std::sync::Mutex::new(Own { meta, guard }),
            presence,
            attaches,
        })
    }

    fn own(&self) -> std::sync::MutexGuard<'_, Own> {
        // A poisoned lock means a panic elsewhere while holding a copy; the
        // record inside is still the last whole one written.
        self.own
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The session's record as it stands.
    pub(crate) fn meta(&self) -> SessionMeta {
        self.own().meta.clone()
    }

    /// An attach completed with `m` as its working record: what it moved --
    /// the generation, the size, detachability -- becomes the session's, and
    /// is written to `meta.json`. The name is the door's and stays.
    pub(crate) fn record_attach(&self, m: &SessionMeta) {
        let mut own = self.own();
        own.meta.attach_id = m.attach_id;
        own.meta.size = m.size;
        own.meta.detachable = m.detachable;
        if let Some(guard) = &own.guard {
            // `update`'s own doc: "after every attach, because attach_id
            // moves". A failed write costs `--list` a stale generation.
            let _ = guard.update(&own.meta);
        }
    }

    /// This session as an entry, seen through `via`: its own client sees
    /// itself as `this`, `Here`; anybody else sees whether it has a client.
    fn own_entry(&self, via: Via) -> Option<SessionEntry> {
        let offer = self.own().meta.offer()?;
        Some(match via {
            Via::Client => offer.entry(Attached::Here, true),
            Via::Socket => {
                let attached = if self.presence.attached(Instant::now()) {
                    Attached::Elsewhere
                } else {
                    Attached::No
                };
                offer.entry(attached, false)
            }
        })
    }
}

/// Serve one connection on one of the doors: read its `Open`, answer it.
pub(crate) async fn serve<R, W>(door: Arc<Door>, via: Via, reader: R, writer: W)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = reader;
    let mut writer = writer;
    let open: Open = match tokio::time::timeout(OPEN_TIMEOUT, read_line_async(&mut reader)).await {
        Ok(Ok(open)) => open,
        // Silence, a hang-up, or a line that is no `Open`: nothing was
        // asked, so nothing is answered.
        _ => return,
    };
    if open.proto != PROTO_VERSION {
        let _ = write_line_async(
            &mut writer,
            &Reply::Refused(format!(
                "this session runs protocol version {PROTO_VERSION}, the request was \
                 version {}",
                open.proto
            )),
        )
        .await;
        return;
    }
    match open.req {
        Request::Attach { role } => attach(&door, role, reader, writer).await,
        Request::Probe { nonce } => probes(nonce, reader, writer).await,
        Request::Myself => {
            let reply = match door.own_entry(Via::Socket) {
                Some(e) => Reply::Entry(e),
                None => Reply::Refused("this session has no entry".to_string()),
            };
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::Sessions => {
            let reply = Reply::Sessions(sessions(&door, via).await);
            let _ = write_line_async(&mut writer, &reply).await;
        }
        Request::Switch { .. }
        | Request::New { .. }
        | Request::Kill { .. }
        | Request::Rename { .. } => {
            let _ = write_line_async(
                &mut writer,
                &Reply::Refused("this host does not do that yet".to_string()),
            )
            .await;
        }
    }
}

/// Hand the stream to the serial attach loop, whose exchange's `HostHello`
/// is the reply. A door with no loop behind it, or a loop that has gone,
/// drops the stream: the asker sees it end rather than wait on silence.
async fn attach<R, W>(door: &Door, role: Role, reader: R, writer: W)
where
    R: AsyncBufRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let Some(queue) = &door.attaches else {
        return;
    };
    let to = match role {
        Role::Primary => &queue.primary,
        Role::Standby => &queue.standby,
    };
    // A busy loop is waited for: its attempt ahead is bounded by
    // `ATTACH_TIMEOUT`.
    let _ = to
        .send(DoorRequest {
            reader: Box::new(reader),
            writer: Box::new(writer),
            role,
        })
        .await;
}

/// Answer a probe, and every further probe on the same stream, until it
/// ends: one stream can serve a whole outage's probing.
async fn probes<R, W>(nonce: u64, mut reader: R, mut writer: W)
where
    R: AsyncBufRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut nonce = nonce;
    loop {
        if write_line_async(&mut writer, &Reply::ProbeAck { nonce })
            .await
            .is_err()
        {
            return;
        }
        match read_line_async::<_, Open>(&mut reader).await {
            Ok(Open {
                req: Request::Probe { nonce: next },
                ..
            }) => nonce = next,
            _ => return,
        }
    }
}

/// The host's sessions, oldest first: this one from the door's own record,
/// every other live one as it answers `Myself` over its socket -- all asked
/// at once, each within [`SIBLING_TIMEOUT`].
async fn sessions(door: &Door, via: Via) -> Vec<SessionEntry> {
    let own_id = door.own().meta.session_id.clone();
    let registry = door.registry.clone();
    // A blocking read of a directory of small files.
    let live = Registry::list_in(&registry).unwrap_or_default();
    let mut found: Vec<Option<SessionEntry>> = vec![None; live.len()];
    let mut asked = tokio::task::JoinSet::new();
    for (i, m) in live.into_iter().enumerate() {
        if m.session_id == own_id {
            found[i] = door.own_entry(via);
        } else {
            let registry = registry.clone();
            asked.spawn(async move { (i, ask_myself(&registry, &m).await) });
        }
    }
    while let Some(Ok((i, entry))) = asked.join_next().await {
        found[i] = entry;
    }
    found.into_iter().flatten().collect()
}

/// A sibling, as it answers `Myself`: `Unknown` from its `meta.json` when it
/// does not answer in time or cannot be reached; `OtherVersion` when what
/// comes back is not this version's answer -- a refusal, an attach
/// exchange's hello, a line of another protocol.
async fn ask_myself(registry: &std::path::Path, m: &SessionMeta) -> Option<SessionEntry> {
    let offer = m.offer()?;
    let path = Registry::socket_path_in(registry, &m.session_id);
    let asked = async {
        let stream = tokio::net::UnixStream::connect(&path).await?;
        let (r, mut w) = stream.into_split();
        write_line_async(&mut w, &Open::new(Request::Myself)).await?;
        let mut r = tokio::io::BufReader::new(r);
        read_answer_async(&mut r).await
    };
    let attached = match tokio::time::timeout(SIBLING_TIMEOUT, asked).await {
        Ok(Ok(Answer::Reply(Reply::Entry(e)))) if e.id == offer.id => {
            return Some(SessionEntry { this: false, ..e });
        }
        Ok(Ok(_)) => Attached::OtherVersion,
        Ok(Err(ProtoError::Malformed(_) | ProtoError::VersionMismatch { .. })) => {
            Attached::OtherVersion
        }
        Ok(Err(_)) | Err(_) => Attached::Unknown,
    };
    Some(offer.entry(attached, false))
}

````

`src/host_session.rs`:

````diff
--- a/src/host_session.rs
+++ b/src/host_session.rs
@@ -58,8 +58,36 @@ pub(crate) const KILL_GRACE: Duration = Duration::from_secs(3);
 /// How long a killed shell is waited for. A SIGKILLed process is reaped at
 /// once in practice; this bounds the wait for one that cannot be.
 const REAP_AFTER_KILL: Duration = Duration::from_secs(2);
 
+/// Whether a session has a client right now, for the door to answer
+/// `Myself` and `Sessions` without asking the loop (switcher spec §2.2).
+///
+/// The loop writes it -- the connection it serves and when it last heard a
+/// frame -- and the door reads it, under a lock held for a copy. The answer
+/// is the one [`HostSession::turn_at`] gives itself: open, and heard from
+/// within [`DETACH_AFTER`].
+#[derive(Clone, Default)]
+pub(crate) struct Presence(std::sync::Arc<std::sync::Mutex<Option<(quinn::Connection, Instant)>>>);
+
+impl Presence {
+    fn heard(&self, conn: &quinn::Connection, at: Instant) {
+        if let Ok(mut p) = self.0.lock() {
+            *p = Some((conn.clone(), at));
+        }
+    }
+
+    /// Whether a client is attached at `now`.
+    pub(crate) fn attached(&self, now: Instant) -> bool {
+        let Ok(p) = self.0.lock() else {
+            return false;
+        };
+        p.as_ref().is_some_and(|(conn, at)| {
+            conn.close_reason().is_none() && now.saturating_duration_since(*at) < DETACH_AFTER
+        })
+    }
+}
+
 /// The remote half: owns the PTY and the authoritative screen.
 pub struct HostSession {
     term: HostTerm,
     screen_tx: oxutrm_sync::Sender<ScreenState>,
@@ -80,8 +108,10 @@ pub struct HostSession {
     /// The last time anything arrived from the client. The host's own liveness
     /// clock, because `close_reason()` stopped being one when the transport's
     /// idle timeout went. See [`DETACH_AFTER`].
     last_heard: Instant,
+    /// `last_heard` and the link it was heard on, for the door.
+    presence: Presence,
 }
 
 impl HostSession {
     /// Start a shell and serve it over `link`.
@@ -124,11 +154,19 @@ impl HostSession {
             screen_stale: false,
             // An attach has just completed and R5 obliges the client to send
             // immediately, so "now" is true rather than optimistic.
             last_heard: Instant::now(),
+            presence: Presence::default(),
         })
     }
 
+    /// Share whether a client is attached with `presence`, the door's copy.
+    pub(crate) fn with_presence(mut self, presence: Presence) -> HostSession {
+        presence.heard(self.link.sink.connection(), self.last_heard);
+        self.presence = presence;
+        self
+    }
+
     /// One turn: apply whatever arrived, drain the PTY, offer a frame.
     pub fn turn(&mut self) -> Result<Turn> {
         self.turn_at(Instant::now(), None)
     }
@@ -159,8 +197,9 @@ impl HostSession {
             // Any frame at all is evidence of a peer, including one `on_frame`
             // rejects: a stale sequence number says the client is behind, not
             // that it is gone.
             self.last_heard = now;
+            self.presence.heard(self.link.sink.connection(), now);
             // A rejected frame is not a disconnection: the state and the ack
             // are both untouched, and the peer's next diff will apply.
             match self.input_rx.on_frame(&frame) {
                 Ok(true) => {
@@ -648,8 +687,10 @@ impl HostSession {
         self.screen_stale = true;
         // An attach has just completed and the client sends immediately, so
         // "now" is true rather than optimistic — the same reasoning as `spawn`.
         self.last_heard = Instant::now();
+        self.presence
+            .heard(self.link.sink.connection(), self.last_heard);
         Ok(())
     }
 
     /// One completed attach, by role. `standby` is the loop's local slot.
````

`src/listener.rs`:

````diff
--- a/src/listener.rs
+++ b/src/listener.rs
@@ -41,100 +41,119 @@ use crate::control::{DoorRequest, Role};
 /// sized to sit clear of their sum so that a real attach over a slow link is
 /// never the thing it cuts off.
 pub(crate) const ATTACH_TIMEOUT: Duration = Duration::from_secs(90);
 
-/// Serve second attaches for the life of the session.
+/// Accept on the session's Unix socket for the life of the session, and
+/// hand every connection to the door ([`crate::door::serve`]) on a task of
+/// its own. Never returns on its own: it is spawned, and stopped with
+/// [`close_the_door`].
+///
+/// A task per connection is the point (switcher spec §2.2): a connection
+/// that stalls before saying what it wants holds its own task, never the
+/// accept, and a `Myself` is answered while an attach is under way.
+pub(crate) async fn accept_doors(
+    listener: tokio::net::UnixListener,
+    door: std::sync::Arc<crate::door::Door>,
+) {
+    loop {
+        match listener.accept().await {
+            Ok((s, _)) => {
+                let (r, w) = s.into_split();
+                tokio::spawn(crate::door::serve(
+                    std::sync::Arc::clone(&door),
+                    crate::door::Via::Socket,
+                    tokio::io::BufReader::new(r),
+                    w,
+                ));
+            }
+            // A failed accept is not a reason to stop answering the door.
+            Err(_) => tokio::task::yield_now().await,
+        }
+    }
+}
+
+/// Run attaches for the life of the session, one at a time.
 ///
 /// Never returns on its own: it is spawned, and dropped when the session ends.
 ///
-/// `doors` brings standby requests from the control streams; `door_tx` is its
-/// sender, handed to the control server started on every link this hands the
-/// session, so that link can carry the next request.
+/// `inbox` brings what the doors were asked to attach: primaries -- an
+/// ssh-relayed `--attach`, a client switching here -- and standbys from a
+/// client's control stream. Each completed link goes to the session on `tx`,
+/// and gets a control server of its own through `door`, so it can carry the
+/// next request.
 ///
 /// `attach_timeout` is a parameter rather than a constant read here so the
 /// guard for it can be exercised in a second instead of in ninety. It is not a
 /// knob: the one production call site passes [`ATTACH_TIMEOUT`], and there is
 /// no flag, environment variable or configuration field behind it.
-#[expect(
-    clippy::too_many_arguments,
-    reason = "each is a distinct thing the loop is handed once; a struct \
-              around them would only be a second name for this signature"
-)]
 pub(crate) async fn serve_attaches(
-    listener: tokio::net::UnixListener,
-    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
-    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
+    door: std::sync::Arc<crate::door::Door>,
     cfg: NetConfig,
     attach_timeout: Duration,
-    mut doors: tokio::sync::mpsc::Receiver<DoorRequest>,
-    door_tx: tokio::sync::mpsc::Sender<DoorRequest>,
+    mut inbox: crate::door::AttachInbox,
     tx: tokio::sync::mpsc::Sender<Attached>,
 ) {
-    // A socket attach that pre-empted a standby's exchange, served next.
-    let mut preempting: Option<tokio::net::UnixStream> = None;
+    // The exchange's working record: `begin_attach` moves its generation at
+    // R4, before the exchange can fail. Seeded from the door's record, which
+    // is what the session's first attach already wrote.
+    let mut meta: SessionMeta = door.meta();
+    // A primary that pre-empted a standby's exchange, served next.
+    let mut preempting: Option<DoorRequest> = None;
     loop {
-        // Two ways in, one door. The Unix socket brings ssh-relayed attaches
-        // (primary: newest attach wins); a control stream brings a standby.
-        // Both are served here, serially, under the one meta lock, for the
-        // reason the comment below gives: two concurrent exchanges would both
-        // bump the generation and race to hand the session a link.
-        //
-        // `doors` never closes while this runs, because `door_tx` is held
-        // right here; the `Some` pattern is only there to say so.
-        let (reader, writer, role): Pipes = match preempting.take() {
-            Some(s) => primary_pipes(s),
+        // A primary first: newest attach wins among primaries by running in
+        // turn, and a standby is only an insurance policy.
+        let req = match preempting.take() {
+            Some(r) => r,
             None => tokio::select! {
-                r = listener.accept() => match r {
-                    Ok((s, _)) => primary_pipes(s),
-                    // A failed accept is not a reason to stop answering the door.
-                    Err(_) => continue,
-                },
-                Some(d) = doors.recv() => (d.reader, d.writer, d.role),
+                biased;
+                Some(r) = inbox.primary.recv() => r,
+                Some(r) = inbox.standby.recv() => r,
+                else => return,
             },
         };
+        let role = req.role;
 
         // One attach at a time, deliberately. Two concurrent exchanges would
         // both bump the generation and race to hand the session a link, and
         // the loser's shell would be a live connection nobody owns.
         //
         // Which is exactly why the attempt is bounded: serial and unbounded
         // means one stalled peer closes the door for the life of the session.
-        let mut m = meta.lock().await;
         let exchange = tokio::time::timeout(
             attach_timeout,
-            crate::attach_exchange::run_attach_exchange(reader, writer, &mut m, &cfg),
+            crate::attach_exchange::run_attach_exchange(req.reader, req.writer, &mut meta, &cfg),
         );
         // A standby gives way to a primary. The standby is an insurance
-        // policy on a link that still works; a socket attach is the client's
-        // ssh rebuild, or a user taking the session over, and either one is
-        // the thing that matters now. A standby whose control stream went
-        // silent would otherwise hold the door for the whole of
-        // `attach_timeout`, longer than a rebuild is prepared to wait. The
-        // dropped attempt is a failed attempt like any other: the registry is
-        // not written and the session never hears of it. A primary is never
-        // pre-empted: newest attach wins among primaries by running in turn.
+        // policy on a link that still works; a primary is the client's ssh
+        // rebuild, a user taking the session over, or a client switching
+        // here, and any of them is the thing that matters now. A standby
+        // whose control stream went silent would otherwise hold the loop for
+        // the whole of `attach_timeout`, longer than a rebuild is prepared to
+        // wait. The dropped attempt is a failed attempt like any other: the
+        // registry is not written and the session never hears of it. A
+        // primary is never pre-empted.
         let outcome = if role == Role::Standby {
             tokio::select! {
                 o = exchange => o,
-                Ok((s, _)) = listener.accept() => {
-                    preempting = Some(s);
+                Some(r) = inbox.primary.recv() => {
+                    preempting = Some(r);
                     continue;
                 }
             }
         } else {
             exchange.await
         };
-        let attached = match outcome {
+        let mut attached = match outcome {
             Ok(Ok(a)) => a,
             // The attempt failed. The running session is untouched: it never
             // heard about this, which is the whole point of the arm firing
             // only on a completed link.
             Ok(Err(_)) => continue,
             // A timed-out attempt is a failed attempt and nothing more: the
             // registry is not written, the session never hears about it, and
-            // the door is open again on the next lap. Said aloud on stderr,
-            // because "the socket stopped answering" is otherwise indis-
-            // tinguishable from a wedged session.
+            // the loop takes the next request. Said aloud on stderr, because
+            // "the socket stopped answering" is otherwise indistinguishable
+            // from a wedged session.
             Err(_) => {
                 eprintln!(
                     "oxutrm: an attach attempt got no further than {}s and was \
                      given up on; the session is still accepting",
@@ -142,46 +161,26 @@ pub(crate) async fn serve_attaches(
                 );
                 continue;
             }
         };
-        // `update`'s own doc: "after every attach, because attach_id moves".
-        let _ = guard.update(&m);
-        drop(m);
+        door.record_attach(&meta);
 
         // The exchange cannot know why it was run; the way in does.
-        let mut attached = attached;
         attached.role = role;
         let conn = attached.link.sink.connection().clone();
 
         if tx.send(attached).await.is_err() {
             // The session is gone; so is the reason to keep listening.
             return;
         }
-        // Every link the session is handed can carry the next standby
-        // request and answer probes. The server dies with the connection.
-        // Started only once the session has the link, so a link nobody took
-        // is not held open by a server of its own.
-        crate::control::serve_control(conn, door_tx.clone());
+        // Every link the session is handed can carry the next request and
+        // answer probes. The server dies with the connection. Started only
+        // once the session has the link, so a link nobody took is not held
+        // open by a server of its own.
+        crate::control::serve_control(conn, std::sync::Arc::clone(&door));
     }
 }
 
-/// An attach's two pipes, and what the attach is for.
-type Pipes = (
-    Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
-    Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
-    Role,
-);
-
-/// The pipes of an attach that came in over the Unix socket.
-fn primary_pipes(s: tokio::net::UnixStream) -> Pipes {
-    let (r, w) = s.into_split();
-    (
-        Box::new(tokio::io::BufReader::new(r)),
-        Box::new(w),
-        Role::Primary,
-    )
-}
-
 /// Stop answering the door once the shell has exited.
 ///
 /// Both halves are needed. [`serve_attaches`] never returns by itself, so
 /// without the `abort()` the await is for ever: the daemon outlives its shell,
````

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -27,8 +27,9 @@ mod candidates;
 mod choose;
 mod config;
 mod connect;
 mod control;
+mod door;
 mod egress;
 mod host_session;
 mod ladder;
 mod link;
@@ -255,8 +256,18 @@ fn run_host_attach(id: &str) -> Result<()> {
         let stream =
             oxutrm_host::attach::connect_to_session(&oxutrm_host::Registry::dir_at(&root.base), id)
                 .await?;
         let (sr, mut sw) = stream.into_split();
+        // The socket is a door (switcher spec §4.1): say what this is before
+        // relaying the client's side of the exchange into it.
+        oxutrm_host::signalling::write_line_async(
+            &mut sw,
+            &oxutrm_proto::Open::new(oxutrm_proto::Request::Attach {
+                role: oxutrm_proto::Role::Primary,
+            }),
+        )
+        .await
+        .context("asking the session for an attach")?;
         let mut sr = tokio::io::BufReader::new(sr);
         let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
         let mut stdout = tokio::io::stdout();
 
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -133,39 +133,50 @@ async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::
     let guard =
         oxutrm_host::RegistryGuard::register_in(&oxutrm_host::Registry::dir_at(&root.base), &meta)
             .context("recording the session in the registry")?;
 
-    // `RegistryGuard` becomes an `Arc` here because the listener task and this
+    // `RegistryGuard` becomes an `Arc` here because the door and this
     // function both need it, and its `Drop` removes the session directory: the
     // directory must outlive both, which is exactly what an `Arc` says.
     let guard = std::sync::Arc::new(guard);
     let shell = meta.shell.clone();
-    let meta = std::sync::Arc::new(tokio::sync::Mutex::new(meta));
+    let presence = crate::host_session::Presence::default();
 
     // The socket path has always been computed and registered. Nothing has
     // ever bound it until now -- and only when this session severed from ssh:
     // rung 4 keeps `sock` false, because its QUIC traffic runs inside the ssh
     // connection and a socket bound anyway would offer an attach that cannot
     // outlive it.
+    let registry = oxutrm_host::Registry::dir_at(&root.base);
     let listening = if sock {
         let path = guard.socket_path();
         oxutrm_host::check_socket_path_length(&path)?;
         let listener = tokio::net::UnixListener::bind(&path)
             .with_context(|| format!("binding the session socket at {}", path.display()))?;
         Some(open_doors(
             listener,
+            registry,
+            meta,
             std::sync::Arc::clone(&guard),
-            std::sync::Arc::clone(&meta),
+            presence.clone(),
             cfg,
             attached.link.sink.connection().clone(),
         ))
     } else {
-        // No socket, so no listener to run a standby's exchange. Such a
+        // No socket, so no attach loop to run a standby's exchange. Such a
         // session (rung 4, whose QUIC runs inside ssh) has nothing a standby
         // could outlive it through, but its hello advertised a control stream
-        // all the same, so the link still serves one: probes are answered and
-        // a standby request is refused at once rather than left waiting.
-        crate::control::serve_control_without_door(attached.link.sink.connection().clone());
+        // all the same, so the link is served through a door with no attach
+        // loop: probes are answered and a standby request ends at once
+        // rather than being left waiting.
+        let door = crate::door::Door::new(
+            registry,
+            meta,
+            Some(std::sync::Arc::clone(&guard)),
+            presence.clone(),
+            None,
+        );
+        crate::control::serve_control(attached.link.sink.connection().clone(), door);
         None
     };
 
     // R14. `negotiate_term` takes no arguments: the child's TERM comes from
@@ -181,17 +192,20 @@ async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::
         attached.client_size,
         SCROLLBACK,
         attached.link,
     )
-    .context("starting the shell")?;
+    .context("starting the shell")?
+    .with_presence(presence);
 
     // R15, then R16: dropping the guard takes the session directory with it,
     // so a session that exits cleanly leaves nothing for `--list` to prune.
     let code = match listening {
-        Some((task, mut attach_rx)) => {
+        Some((tasks, mut attach_rx)) => {
             let code = session.run_with_attaches(&mut attach_rx).await;
             // Abort AND await, and why both, is `close_the_door`'s own note.
-            crate::listener::close_the_door(task).await;
+            for task in tasks {
+                crate::listener::close_the_door(task).await;
+            }
             code
         }
         None => session.run().await,
     };
@@ -206,38 +220,42 @@ pub(crate) fn home_dir(home: Option<std::ffi::OsString>) -> Option<std::path::Pa
         .filter(|p| p.is_absolute() && p.is_dir())
 }
 
 /// The two ways into a severed session after its first attach: the Unix
-/// socket, and the door that the first link's control stream knocks on.
+/// socket, and the control stream of every link, both through one door
+/// ([`crate::door`]).
 ///
-/// Returns the listener task and where completed attaches arrive. A function
-/// of its own so the first link's control server, which nothing else starts,
-/// can be reached from a test: every later link's is started by the listener
-/// as it hands the link over.
+/// Returns the door's two tasks -- the socket's accept loop and the serial
+/// attach loop -- and where completed attaches arrive. A function of its own
+/// so the first link's control server, which nothing else starts, can be
+/// reached from a test: every later link's is started by the attach loop as
+/// it hands the link over.
 pub(crate) fn open_doors(
     listener: tokio::net::UnixListener,
+    registry: std::path::PathBuf,
+    meta: SessionMeta,
     guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
-    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
+    presence: crate::host_session::Presence,
     cfg: NetConfig,
     first: quinn::Connection,
 ) -> (
-    tokio::task::JoinHandle<()>,
+    [tokio::task::JoinHandle<()>; 2],
     tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
 ) {
     let (attach_tx, attach_rx) = tokio::sync::mpsc::channel(1);
-    // The door: standby requests from the control streams, served by the
-    // same loop as the socket.
-    let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
-    crate::control::serve_control(first, door_tx.clone());
-    let task = tokio::spawn(crate::listener::serve_attaches(
+    let (queue, inbox) = crate::door::attach_queue();
+    let door = crate::door::Door::new(registry, meta, Some(guard), presence, Some(queue));
+    crate::control::serve_control(first, std::sync::Arc::clone(&door));
+    let accept = tokio::spawn(crate::listener::accept_doors(
         listener,
-        guard,
-        meta,
+        std::sync::Arc::clone(&door),
+    ));
+    let attaches = tokio::spawn(crate::listener::serve_attaches(
+        door,
         cfg,
         crate::listener::ATTACH_TIMEOUT,
-        door_rx,
-        door_tx,
+        inbox,
         attach_tx,
     ));
-    (task, attach_rx)
+    ([accept, attaches], attach_rx)
 }
 
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --bin oxutrm --jobs 4 door:: listener:: control:: && cargo test -p oxutrm-proto --jobs 4 && cargo test --test host_attach --jobs 4`

Expected: all pass, among them `sessions_lists_this_one_its_siblings_and_says_which_did_not_answer` (a named sibling, a deaf one as `Unknown`, an old one as `OtherVersion`, within `SIBLING_TIMEOUT` plus a second), `a_sibling_with_a_client_is_listed_as_in_use_elsewhere`, `a_primary_preempts_a_stalled_standby` and `awaiting_the_aborted_tasks_is_what_lets_the_guard_clean_up`.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add crates/oxutrm-proto/src/lib.rs crates/oxutrm-proto/src/signal.rs src/control.rs src/door.rs src/host_session.rs src/listener.rs src/main.rs src/serve.rs src/session.rs
git commit -m "feat(door): both doors read an Open first; one dispatcher, a task per connection; PROTO_VERSION 3

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 6: The lobby; `New` in a lobby, `Kill` and `Rename` through the door

Spec §2.1, §3.4. `HostSession.term` becomes `Option<HostTerm>`: `None` is a **lobby**, which answers its client's frames with a blank screen (resized with the client), drops what is typed, and ends when its link closes or its client has been silent for `DETACH_AFTER` (`run_with_doors` returns `Ok(0)`). `HostSession::lobby` builds one; `start_shell` makes it a session; `spawn` is now `lobby` + `start_shell` and only the tests use it.

What only the loop can do, the door asks with a `LoopCmd`: `StartShell`, and `Kill { end, reply, written }`. The door owns registration now: `Door::register` -- the registry entry under the session's name, and, where the rung severed, the socket with its accept loop and attach loop -- is the one way any session process becomes a registered session (a first connect and a lobby's `New` alike). `Door::close` stops those loops, awaits them and removes the entry; `run_process` (`serve.rs`) is R13-R16 for every session process, testable in-process.

- `New` in a lobby: the door puts the name in its record, registers (a taken name is refused, the lobby stays a lobby), asks the loop to start the shell, and replies `Entry` -- the same id.
- `Kill` of this session from its own client: the loop hangs the shell up (`KILL_GRACE`), closes a parked standby, becomes a lobby and replies the status; the door then unregisters and only then writes `Done`. From a sibling (`Via::Socket`): the same, but the loop then waits for the door's `Done` to be written (`DONE_WRITTEN_WITHIN`) and ends as a shell that exited -- its own client sees `SHELL_EXITED`.
- `Kill` or `Rename` of another session: forwarded to its socket (`FORWARD_TIMEOUT`), its one reply relayed; a session that is not live is refused with its short id.
- `Rename` of this session: `RegistryGuard::rename` under the name lock; a lobby has no entry to name.

A lobby drops `Attach` (it has no attach loop): the client never searches for a standby in a lobby (Task 9).

Review focus 2 is pinned here: `a_lobbys_blank_screen_follows_its_clients_size`.

**Files:**
- Modify: `src/host_session.rs`, `src/door.rs`, `src/serve.rs`, `src/listener.rs` (its fixture builds the door with `Door::assembled`), `src/control.rs` (same), `src/main.rs` (`run_host_serve(Begin::Session)`), `src/session.rs` (the standby test builds its door with `Door::process` + `register`)

**Interfaces:**
- Consumes: Task 4's `hang_up_shell`, `KILL_GRACE`, `Start`, `home_dir`; Task 5's door, `Presence`, `serve_attaches`, `accept_doors`.
- Produces:
  - `crate::host_session::{LoopCmd { StartShell { reply: oneshot::Sender<Result<(), String>> }, Kill { end: bool, reply: oneshot::Sender<i32>, written: oneshot::Receiver<()> } }}`; `HostSession::lobby(shell, &Start, size, scrollback, link) -> Result<HostSession>`, `start_shell(&mut self) -> Result<()>`, `is_lobby(&self) -> bool`, `run_with_doors(&mut self, attaches: &mut Receiver<Attached>, cmds: &mut Receiver<LoopCmd>) -> Result<i32>`; `#[cfg(test)] with_detach_after(self, Duration)`; `spawn`, `run`, `run_with_attaches` become `#[cfg(test)]`.
  - `Door::process(registry: PathBuf, meta: SessionMeta, presence: Presence, cfg: NetConfig, to_loop: LoopLink) -> Arc<Door>` (Task 8 adds `exe`); `#[cfg(test)] Door::assembled(registry, meta, guard: Option<RegistryGuard>, presence, attaches: Option<AttachQueue>) -> Arc<Door>`; `Door::register(self: &Arc<Self>) -> anyhow::Result<()>`; `Door::close(&self)` (async); `door::LoopLink { cmds: mpsc::Sender<LoopCmd>, attached: mpsc::Sender<Attached> }`; `door::FORWARD_TIMEOUT` = 10 s.
  - `serve::{Begin { Session, Lobby }, Shell { program: String, start: Start }, run_process(link: Link, client_size: TermSize, meta: SessionMeta, registry: PathBuf, cfg: NetConfig, begin: Begin, shell: Shell) -> anyhow::Result<i32>}`; `serve::run_host_serve(begin: Begin)`. (Task 7 gives `Begin::Session` a name, Task 8 gives `Shell` a `sibling`.)
  - Test fixtures `serve::fixtures::{SIZE, Process { client, task }, process(registry, id, name, begin, shell), sh(), ask_over(conn, Request) -> Answer, registry_holds(registry, &[ids])}`.

- [ ] **Step 1: Write the failing tests**

`src/control.rs`:

````diff
--- a/src/control.rs
+++ b/src/control.rs
@@ -261,9 +261,9 @@ mod tests {
     /// attach loop nobody runs, and probes still work.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn without_an_attach_loop_a_standby_request_ends_at_once_and_probes_are_answered() {
         let dir = tempfile::tempdir().unwrap();
-        let door = crate::door::Door::new(
+        let door = crate::door::Door::assembled(
             dir.path().to_path_buf(),
             meta(SESSION, None),
             None,
             crate::host_session::Presence::default(),
````

`src/door.rs`:

````diff
--- a/src/door.rs
+++ b/src/door.rs
@@ -327,12 +600,12 @@ pub(crate) mod fixtures {
     pub(crate) fn registered(
         registry: &std::path::Path,
         meta: SessionMeta,
     ) -> (Arc<Door>, AttachInbox, Presence) {
-        let guard = Arc::new(RegistryGuard::register_in(registry, &meta).expect("register"));
+        let guard = RegistryGuard::register_in(registry, &meta).expect("register");
         let (queue, inbox) = attach_queue();
         let presence = Presence::default();
-        let door = Door::new(
+        let door = Door::assembled(
             registry.to_path_buf(),
             meta,
             Some(guard),
             presence.clone(),
````

`src/host_session.rs`:

````diff
--- a/src/host_session.rs
+++ b/src/host_session.rs
@@ -784,2 +1002,2 @@
 #[cfg(test)]
 mod tests {
@@ -792,13 +1010,21 @@ mod tests {
             rows: 40,
         }
     }
 
-    /// A "shell" that ignores SIGHUP, as a script file the session runs.
+    /// A "shell" that ignores SIGHUP, as a script file the session runs. It
+    /// creates `ready` in `dir` once its trap is set.
     fn stubborn_shell(dir: &std::path::Path) -> String {
         use std::os::unix::fs::PermissionsExt as _;
         let path = dir.join("stubborn");
-        std::fs::write(&path, "#!/bin/sh\ntrap '' HUP\nwhile :; do sleep 1; done\n").unwrap();
+        std::fs::write(
+            &path,
+            format!(
+                "#!/bin/sh\ntrap '' HUP\n: > '{}'\nwhile :; do sleep 1; done\n",
+                dir.join("ready").display()
+            ),
+        )
+        .unwrap();
         std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
         path.to_str().unwrap().to_string()
     }
 
@@ -831,13 +1057,131 @@ mod tests {
             200,
             host_link,
         )
         .unwrap();
-        // Long enough for the script to have set its trap.
-        tokio::time::sleep(Duration::from_millis(300)).await;
+        let deadline = Instant::now() + Duration::from_secs(10);
+        while !dir.path().join("ready").exists() {
+            assert!(Instant::now() < deadline, "the shell never set its trap");
+            tokio::time::sleep(Duration::from_millis(20)).await;
+        }
         let grace = Duration::from_millis(400);
         let begun = Instant::now();
         let code = host.hang_up_shell(grace).await;
         assert_eq!(code, 128 + 9, "ended by the SIGKILL");
         assert!(begun.elapsed() >= grace, "{:?}", begun.elapsed());
     }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_lobby_whose_link_closes_ends() {
+        let (host_link, client) = link_pair().await;
+        let mut lobby = HostSession::lobby(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size(),
+            200,
+            host_link,
+        )
+        .unwrap();
+        assert!(lobby.is_lobby());
+        let task = tokio::spawn(async move { lobby.run_with_attaches(&mut closed()).await });
+        client
+            .sink
+            .connection()
+            .close(quinn::VarInt::from_u32(0), b"the client quit");
+        let code = tokio::time::timeout(Duration::from_secs(5), task)
+            .await
+            .expect("the lobby outlived its link")
+            .unwrap()
+            .unwrap();
+        assert_eq!(code, 0);
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_lobby_whose_client_vanished_ends_after_the_silence() {
+        let (host_link, _client) = link_pair().await;
+        let silence = Duration::from_millis(300);
+        let mut lobby = HostSession::lobby(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size(),
+            200,
+            host_link,
+        )
+        .unwrap()
+        .with_detach_after(silence);
+        let begun = Instant::now();
+        let task = tokio::spawn(async move { lobby.run_with_attaches(&mut closed()).await });
+        let code = tokio::time::timeout(Duration::from_secs(5), task)
+            .await
+            .expect("a silent lobby never ended")
+            .unwrap()
+            .unwrap();
+        assert_eq!(code, 0);
+        assert!(begun.elapsed() >= silence, "{:?}", begun.elapsed());
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_session_whose_client_vanished_does_not_end() {
+        let (host_link, _client) = link_pair().await;
+        let mut host = HostSession::spawn(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size(),
+            200,
+            host_link,
+        )
+        .unwrap()
+        .with_detach_after(Duration::from_millis(100));
+        let task = tokio::spawn(async move { host.run_with_attaches(&mut closed()).await });
+        tokio::time::sleep(Duration::from_millis(600)).await;
+        assert!(
+            !task.is_finished(),
+            "only a lobby ends on its client's silence"
+        );
+        task.abort();
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_lobby_answers_its_client_with_a_blank_screen() {
+        let (host_link, client_link) = link_pair().await;
+        let mut lobby = HostSession::lobby(
+            "/bin/sh",
+            &oxutrm_term::Start::default(),
+            size(),
+            200,
+            host_link,
+        )
+        .unwrap();
+        let mut client = crate::session::ClientSession::new(
+            size(),
+            oxutrm_proto::TerminalCaps {
+                truecolor: true,
+                colors: 16_777_216,
+                bracketed_paste: true,
+                mouse_sgr: true,
+                osc52: true,
+                term_name: "xterm-256color".to_owned(),
+            },
+            client_link,
+            None,
+        )
+        .unwrap();
+        let mut out = Vec::new();
+        let deadline = Instant::now() + Duration::from_secs(5);
+        while client.applied_kinds() == (0, 0) {
+            assert!(Instant::now() < deadline, "the lobby never answered");
+            client.turn(b"ls\n", &mut out).unwrap();
+            lobby.turn().unwrap();
+            tokio::time::sleep(Duration::from_millis(10)).await;
+        }
+        assert!(
+            client.screen().cells.iter().all(|c| c.text.as_str() == " "),
+            "a lobby's screen is blank"
+        );
+    }
+
+    /// An attach channel whose sender is gone, as `run` uses.
+    fn closed() -> tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached> {
+        let (_, rx) = tokio::sync::mpsc::channel(1);
+        rx
+    }
 }
````

`src/listener.rs`:

````diff
--- a/src/listener.rs
+++ b/src/listener.rs
@@ -231,16 +231,14 @@ mod tests {
 
     fn doors(meta: SessionMeta, attach_timeout: Duration) -> Doors {
         let dir = tempfile::tempdir().expect("a temp dir");
         let registry = dir.path().to_path_buf();
-        let guard = std::sync::Arc::new(
-            oxutrm_host::RegistryGuard::register_in(&registry, &meta).expect("register"),
-        );
+        let guard = oxutrm_host::RegistryGuard::register_in(&registry, &meta).expect("register");
         let sock = guard.socket_path();
         let guard_dir = guard.dir().to_path_buf();
         let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
         let (queue, inbox) = attach_queue();
-        let door = Door::new(
+        let door = Door::assembled(
             registry.clone(),
             meta,
             Some(guard),
             crate::host_session::Presence::default(),
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -260,4 +239,93 @@
+#[cfg(test)]
+pub(crate) mod fixtures {
+    use super::*;
+    use crate::link::Link;
+    use oxutrm_host::signalling::{read_answer_async, write_line_async};
+    use oxutrm_proto::{Answer, Open, Request};
+
+    /// The size every test client has.
+    pub(crate) const SIZE: TermSize = TermSize {
+        cols: 120,
+        rows: 40,
+    };
+
+    /// A session process run in-process, as `serve` runs one after its
+    /// sever: its client's end of the first link, and the process's task.
+    pub(crate) struct Process {
+        pub(crate) client: Link,
+        pub(crate) task: tokio::task::JoinHandle<anyhow::Result<i32>>,
+    }
+
+    /// Run a session process with id `id` in `registry`: `Begin::Session`
+    /// registers it (as `name`) and starts `program` (a plain `/bin/sh`
+    /// unless a test needs a particular shell).
+    pub(crate) async fn process(
+        registry: &std::path::Path,
+        id: &str,
+        name: Option<&str>,
+        begin: Begin,
+        shell: Shell,
+    ) -> Process {
+        let (host, client) = crate::link::fixtures::link_pair().await;
+        let mut meta = crate::door::fixtures::meta(id, name);
+        meta.size = SIZE;
+        let task = tokio::spawn(run_process(
+            host,
+            SIZE,
+            meta,
+            registry.to_path_buf(),
+            crate::attach_exchange::fixtures::stun_free(),
+            begin,
+            shell,
+        ));
+        Process { client, task }
+    }
+
+    pub(crate) fn sh() -> Shell {
+        Shell {
+            program: "/bin/sh".to_string(),
+            start: oxutrm_term::Start::default(),
+        }
+    }
+
+    /// Ask `req` over a fresh control stream of `conn`, as the client does,
+    /// and read the one answer.
+    pub(crate) async fn ask_over(conn: &quinn::Connection, req: Request) -> Answer {
+        let (mut send, recv) = conn.open_bi().await.expect("a control stream");
+        write_line_async(&mut send, &Open::new(req))
+            .await
+            .expect("asking");
+        let mut recv = tokio::io::BufReader::new(recv);
+        tokio::time::timeout(
+            std::time::Duration::from_secs(15),
+            read_answer_async(&mut recv),
+        )
+        .await
+        .expect("an answer in time")
+        .expect("a readable answer")
+    }
+
+    /// Wait until `registry` lists exactly `ids`, in any order.
+    pub(crate) async fn registry_holds(registry: &std::path::Path, ids: &[&str]) {
+        let want: std::collections::BTreeSet<String> = ids.iter().map(|s| s.to_string()).collect();
+        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
+        loop {
+            let live: std::collections::BTreeSet<String> = oxutrm_host::Registry::list_in(registry)
+                .unwrap()
+                .into_iter()
+                .map(|m| m.session_id)
+                .collect();
+            if live == want {
+                return;
+            }
+            assert!(
+                tokio::time::Instant::now() < deadline,
+                "the registry holds {live:?}, not {want:?}"
+            );
+            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
+        }
+    }
 }
 
 #[cfg(test)]
 mod tests {
@@ -276,5 +344,356 @@ mod tests {
             home_dir(Some(dir.path().join("missing").into_os_string())),
             None
         );
     }
+
+    use super::fixtures::*;
+    use oxutrm_proto::{Answer, Attached, Name, Reply, Request, SessionId};
+
+    const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
+    const LOGS: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
+
+    fn id(s: &str) -> SessionId {
+        s.parse().unwrap()
+    }
+
+    fn name(s: &str) -> Option<Name> {
+        Some(Name::parse(s).unwrap())
+    }
+
+    fn entry(a: Answer) -> oxutrm_proto::SessionEntry {
+        match a {
+            Answer::Reply(Reply::Entry(e)) => e,
+            other => panic!("not an entry: {other:?}"),
+        }
+    }
+
+    fn refused(a: Answer) -> String {
+        match a {
+            Answer::Reply(Reply::Refused(why)) => why,
+            other => panic!("not a refusal: {other:?}"),
+        }
+    }
+
+    fn done(a: Answer) {
+        assert!(matches!(a, Answer::Reply(Reply::Done)), "{a:?}");
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn new_in_a_lobby_registers_it_by_name_and_it_becomes_the_session() {
+        let dir = tempfile::tempdir().unwrap();
+        let p = process(dir.path(), BUILD, None, Begin::Lobby, sh()).await;
+        registry_holds(dir.path(), &[]).await;
+
+        let e = entry(
+            ask_over(
+                p.client.sink.connection(),
+                Request::New {
+                    name: name("build"),
+                },
+            )
+            .await,
+        );
+        assert_eq!(e.id, id(BUILD), "the lobby keeps the id it already had");
+        assert_eq!(e.name, name("build"));
+        assert!(e.this);
+        assert_eq!(e.attached, Attached::Here);
+        registry_holds(dir.path(), &[BUILD]).await;
+
+        // A session now: its socket answers, and it has a shell to kill.
+        let sock = oxutrm_host::Registry::socket_path_in(dir.path(), BUILD);
+        let mine = entry(crate::door::fixtures::ask(&sock, Request::Myself).await);
+        assert_eq!(mine.name, name("build"));
+        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn new_in_a_lobby_with_a_taken_name_is_refused_and_it_stays_a_lobby() {
+        let dir = tempfile::tempdir().unwrap();
+        let _logs = oxutrm_host::RegistryGuard::register_in(
+            dir.path(),
+            &crate::door::fixtures::meta(LOGS, Some("build")),
+        )
+        .unwrap();
+        let p = process(dir.path(), BUILD, None, Begin::Lobby, sh()).await;
+        let why = refused(
+            ask_over(
+                p.client.sink.connection(),
+                Request::New {
+                    name: name("build"),
+                },
+            )
+            .await,
+        );
+        assert!(why.contains("taken"), "{why}");
+        registry_holds(dir.path(), &[LOGS]).await;
+        // Still a lobby, and still answering: an unnamed New works.
+        let e = entry(ask_over(p.client.sink.connection(), Request::New { name: None }).await);
+        assert_eq!(e.id, id(BUILD));
+        assert_eq!(e.name, None);
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn killing_this_session_leaves_a_lobby_that_can_start_again() {
+        let dir = tempfile::tempdir().unwrap();
+        let p = process(dir.path(), BUILD, Some("build"), Begin::Session, sh()).await;
+        registry_holds(dir.path(), &[BUILD]).await;
+
+        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
+        // Done only after the entry went: no waiting here.
+        assert!(
+            oxutrm_host::Registry::list_in(dir.path())
+                .unwrap()
+                .is_empty(),
+            "Done came before the registry entry was removed"
+        );
+        assert!(
+            !p.task.is_finished(),
+            "a kill from its own client ended the process"
+        );
+        assert!(p.client.sink.connection().close_reason().is_none());
+
+        // The last session is gone, and the lobby lists nothing.
+        match ask_over(p.client.sink.connection(), Request::Sessions).await {
+            Answer::Reply(Reply::Sessions(list)) => assert!(list.is_empty(), "{list:?}"),
+            other => panic!("{other:?}"),
+        }
+        let why =
+            refused(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
+        assert!(why.contains("no shell"), "{why}");
+
+        // And it starts again, as the same id.
+        let e = entry(
+            ask_over(
+                p.client.sink.connection(),
+                Request::New {
+                    name: name("again"),
+                },
+            )
+            .await,
+        );
+        assert_eq!(e.id, id(BUILD));
+        registry_holds(dir.path(), &[BUILD]).await;
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn killing_a_sibling_ends_it_as_a_shell_that_exited() {
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(dir.path(), BUILD, Some("build"), Begin::Session, sh()).await;
+        let sibling = process(dir.path(), LOGS, Some("logs"), Begin::Session, sh()).await;
+        registry_holds(dir.path(), &[BUILD, LOGS]).await;
+
+        done(ask_over(me.client.sink.connection(), Request::Kill { id: id(LOGS) }).await);
+        let live = oxutrm_host::Registry::list_in(dir.path()).unwrap();
+        assert_eq!(
+            live.len(),
+            1,
+            "Done came before the sibling's entry went: {live:?}"
+        );
+
+        // The sibling's own client sees its shell exit, by SIGHUP.
+        let code = tokio::time::timeout(std::time::Duration::from_secs(10), sibling.task)
+            .await
+            .expect("the killed sibling's process did not end")
+            .expect("its task")
+            .expect("its loop");
+        assert_eq!(code, 128 + 1);
+        let reason = sibling.client.sink.connection().closed().await;
+        assert!(
+            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
+                if c.reason.as_ref() == crate::session::SHELL_EXITED),
+            "{reason:?}"
+        );
+        assert!(!me.task.is_finished(), "the asker was killed too");
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_shell_that_ignores_the_hang_up_is_killed_and_then_done() {
+        use std::os::unix::fs::PermissionsExt as _;
+        let dir = tempfile::tempdir().unwrap();
+        let ready = dir.path().join("ready");
+        let script = dir.path().join("stubborn");
+        std::fs::write(
+            &script,
+            format!(
+                "#!/bin/sh\ntrap '' HUP\n: > '{}'\nwhile :; do sleep 1; done\n",
+                ready.display()
+            ),
+        )
+        .unwrap();
+        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
+        // Its own directory: a socket path under the script's would be too
+        // long for macOS's 104 bytes.
+        let registry_dir = tempfile::tempdir().unwrap();
+        let registry = registry_dir.path().to_path_buf();
+        let p = process(
+            &registry,
+            BUILD,
+            None,
+            Begin::Session,
+            Shell {
+                program: script.to_str().unwrap().to_string(),
+                start: oxutrm_term::Start::default(),
+            },
+        )
+        .await;
+        registry_holds(&registry, &[BUILD]).await;
+        // The trap is set once the script says so.
+        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
+        while !ready.exists() {
+            assert!(std::time::Instant::now() < deadline, "the shell never ran");
+            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
+        }
+
+        let begun = std::time::Instant::now();
+        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
+        assert!(
+            begun.elapsed() >= crate::host_session::KILL_GRACE,
+            "Done before the grace was out: {:?}",
+            begun.elapsed()
+        );
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn rename_here_and_on_a_sibling_and_a_taken_name_is_refused() {
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(dir.path(), BUILD, None, Begin::Session, sh()).await;
+        let _sibling = process(dir.path(), LOGS, Some("logs"), Begin::Session, sh()).await;
+        registry_holds(dir.path(), &[BUILD, LOGS]).await;
+        let conn = me.client.sink.connection();
+
+        let e = entry(
+            ask_over(
+                conn,
+                Request::Rename {
+                    id: id(BUILD),
+                    name: name("build"),
+                },
+            )
+            .await,
+        );
+        assert_eq!(e.name, name("build"));
+        assert!(e.this);
+
+        let why = refused(
+            ask_over(
+                conn,
+                Request::Rename {
+                    id: id(BUILD),
+                    name: name("logs"),
+                },
+            )
+            .await,
+        );
+        assert!(why.contains("taken"), "{why}");
+
+        let e = entry(
+            ask_over(
+                conn,
+                Request::Rename {
+                    id: id(LOGS),
+                    name: name("tail"),
+                },
+            )
+            .await,
+        );
+        assert_eq!(e.name, name("tail"));
+        assert!(!e.this, "the sibling's entry is the sibling's");
+
+        let e = entry(
+            ask_over(
+                conn,
+                Request::Rename {
+                    id: id(BUILD),
+                    name: None,
+                },
+            )
+            .await,
+        );
+        assert_eq!(e.name, None);
+        let on_disk = oxutrm_host::Registry::list_in(dir.path()).unwrap();
+        let names: Vec<_> = on_disk.iter().map(|m| m.name.clone()).collect();
+        assert!(names.contains(&Some("tail".to_string())), "{names:?}");
+        assert!(names.contains(&None), "{names:?}");
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_request_for_a_session_that_is_not_there_is_refused_with_its_short_id() {
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(dir.path(), BUILD, None, Begin::Session, sh()).await;
+        registry_holds(dir.path(), &[BUILD]).await;
+        let why =
+            refused(ask_over(me.client.sink.connection(), Request::Kill { id: id(LOGS) }).await);
+        assert!(why.contains("a3f9c01e"), "{why}");
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_name_that_is_all_hex_never_reaches_the_session() {
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(dir.path(), BUILD, None, Begin::Session, sh()).await;
+        registry_holds(dir.path(), &[BUILD]).await;
+        // Written by hand: `Name` cannot hold it, so the line is malformed and
+        // the door answers nothing.
+        let (mut send, recv) = me.client.sink.connection().open_bi().await.unwrap();
+        let line = format!(
+            "{{\"proto\":{},\"req\":{{\"q\":\"Rename\",\"id\":\"{BUILD}\",\"name\":\"cafe\"}}}}\n",
+            oxutrm_proto::PROTO_VERSION
+        );
+        send.write_all(line.as_bytes()).await.unwrap();
+        let mut recv = tokio::io::BufReader::new(recv);
+        let got = tokio::time::timeout(
+            std::time::Duration::from_secs(5),
+            oxutrm_host::signalling::read_answer_async(&mut recv),
+        )
+        .await
+        .expect("the stream must end, not hang");
+        assert!(got.is_err(), "an all-hex name was answered: {got:?}");
+        let on_disk = oxutrm_host::Registry::list_in(dir.path()).unwrap();
+        assert_eq!(on_disk[0].name, None);
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_new_sessions_shell_starts_in_home() {
+        use std::os::unix::fs::PermissionsExt as _;
+        let dir = tempfile::tempdir().unwrap();
+        let home = dir.path().canonicalize().unwrap().join("home");
+        std::fs::create_dir(&home).unwrap();
+        let out = dir.path().join("out");
+        let script = dir.path().join("myshell");
+        std::fs::write(
+            &script,
+            format!(
+                "#!/bin/sh\nprintf '%s:%s' \"$0\" \"$(pwd -P)\" > '{}'\nexec sleep 30\n",
+                out.display()
+            ),
+        )
+        .unwrap();
+        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
+        let registry_dir = tempfile::tempdir().unwrap();
+        let p = process(
+            registry_dir.path(),
+            BUILD,
+            None,
+            Begin::Session,
+            Shell {
+                program: script.to_str().unwrap().to_string(),
+                start: oxutrm_term::Start::login_in(home_dir(Some(home.clone().into_os_string()))),
+            },
+        )
+        .await;
+        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
+        let text = loop {
+            if let Ok(t) = std::fs::read_to_string(&out)
+                && !t.is_empty()
+            {
+                break t;
+            }
+            assert!(std::time::Instant::now() < deadline, "the shell never ran");
+            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
+        };
+        // The directory. The dash in `argv[0]` is proven on the pty itself
+        // (`oxutrm-term`'s `a_login_shell_starts_in_its_directory_with_a_dash_name`):
+        // a `#!` script's `$0` is its path, whatever `argv[0]` was.
+        assert!(text.ends_with(&format!(":{}", home.display())), "{text}");
+        done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
+    }
 }
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -6686,24 +6686,32 @@ mod tests {
     async fn a_standby_is_found_over_the_first_links_control_stream() {
         let (mut host, mut client) = pair_sized("", BIG).await;
         let primary_id = client.link.sink.connection().stable_id();
         let dir = tempfile::tempdir().expect("a scratch directory");
-        let start =
+        let mut start =
             crate::attach_exchange::fixtures::fresh_meta("00112233445566778899aabbccddeeff");
-        let guard = Arc::new(
-            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
-        );
-        let listener = tokio::net::UnixListener::bind(guard.socket_path()).expect("binding");
-        let (door_tasks, mut attach_rx) = crate::serve::open_doors(
-            listener,
+        // Severed, so the door binds a socket and runs an attach loop.
+        start.detachable = true;
+        let meta_path = dir
+            .path()
+            .join("00112233445566778899aabbccddeeff")
+            .join(oxutrm_host::META_FILE);
+        let (cmds_tx, mut cmds) = tokio::sync::mpsc::channel(1);
+        let (attached_tx, mut attach_rx) = tokio::sync::mpsc::channel(1);
+        let door = crate::door::Door::process(
             dir.path().to_path_buf(),
             start,
-            Arc::clone(&guard),
             crate::host_session::Presence::default(),
             crate::attach_exchange::fixtures::stun_free(),
-            host.link.sink.connection().clone(),
+            crate::door::LoopLink {
+                cmds: cmds_tx,
+                attached: attached_tx,
+            },
         );
-        let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });
+        door.register().expect("register");
+        crate::control::serve_control(host.link.sink.connection().clone(), Arc::clone(&door));
+        let host_loop =
+            tokio::spawn(async move { host.run_with_doors(&mut attach_rx, &mut cmds).await });
 
         // Due at once rather than after the settling delay. Loopback is the
         // primary's own path, so the real filter would rightly refuse every
         // candidate this test has: this one admits them.
@@ -6735,9 +6743,9 @@ mod tests {
         // went out, so the client may see the standby a moment first.
         let recorded = async {
             loop {
                 let on_disk: oxutrm_host::SessionMeta =
-                    serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json"))
+                    serde_json::from_slice(&std::fs::read(&meta_path).expect("meta.json"))
                         .expect("meta.json parses");
                 if on_disk.attach_id == 1 {
                     return;
                 }
@@ -6768,11 +6776,9 @@ mod tests {
                 .any(|e| e.kind == Kind::Standby && e.text.starts_with("found ")),
             "the standby was shown but not recorded"
         );
         assert_eq!(host_loop.await.expect("host task").expect("host loop"), 7);
-        for task in door_tasks {
-            crate::listener::close_the_door(task).await;
-        }
+        door.close().await;
     }
 
     /// What the popup may not say while `Silent` or `Confirming`, wherever
     /// in it it says it.
````

`src/host_session.rs` -- append inside `mod tests`, at its end (review focus 2):

````rust
    /// A lobby's client resizing its terminal: the blank screen follows, so
    /// no cell of the old size is left for the client to paint.
    #[tokio::test]
    async fn a_lobbys_blank_screen_follows_its_clients_size() {
        let (host_link, _client) = link_pair().await;
        let mut lobby = HostSession::lobby(
            "/bin/sh",
            &oxutrm_term::Start::default(),
            size(),
            200,
            host_link,
        )
        .unwrap();
        let bigger = TermSize {
            cols: 160,
            rows: 50,
        };
        lobby.resize(bigger).unwrap();
        let screen = lobby.screen();
        assert_eq!((screen.cols, screen.rows), (160, 50));
        assert!(screen.cells.iter().all(|c| c.text.as_str() == " "));
    }
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --bin oxutrm --jobs 4 serve:: host_session::`

Expected: compile errors: `HostSession::lobby`, `LoopCmd`, `Door::process`, `run_process`, `Begin` and `serve::fixtures` are not defined.

- [ ] **Step 3: Implement**

`src/door.rs`:

````diff
--- a/src/door.rs
+++ b/src/door.rs
@@ -26,15 +26,17 @@ use std::sync::Arc;
 use std::time::{Duration, Instant};
 
 use oxutrm_host::registry::{Registry, RegistryGuard, SessionMeta};
 use oxutrm_host::signalling::{read_answer_async, read_line_async, write_line_async};
+use oxutrm_net::NetConfig;
 use oxutrm_proto::{
-    Answer, Attached, Open, PROTO_VERSION, ProtoError, Reply, Request, Role, SessionEntry,
+    Answer, Attached, Name, Open, PROTO_VERSION, ProtoError, Reply, Request, Role, SessionEntry,
+    SessionId,
 };
 use tokio::io::{AsyncBufRead, AsyncWrite};
 
 use crate::control::DoorRequest;
-use crate::host_session::Presence;
+use crate::host_session::{LoopCmd, Presence};
 
 /// How long a connection may take to say what it wants. Generous for a
 /// peer on the same machine or a live link, short enough that a silent one
 /// costs a task for seconds, not for ever (the QUIC idle timeout is off).
@@ -86,35 +88,86 @@ struct Own {
     /// What `meta.json` says, kept current by the attach loop and by
     /// renames. Not the loop's working copy: that one is held across an
     /// exchange, and this must answer while one runs.
     meta: SessionMeta,
-    /// The registry entry, while there is one: a lobby has none.
-    guard: Option<Arc<RegistryGuard>>,
+    /// The registry entry, while there is one: a lobby has none. Dropping it
+    /// removes the session's directory, its socket with it.
+    guard: Option<RegistryGuard>,
+    /// Where `Attach` goes, while the session is registered and severed:
+    /// `None` in a lobby, and on rung 4, which has no socket.
+    attaches: Option<AttachQueue>,
+    /// The socket's accept loop and the serial attach loop, while they run.
+    tasks: Vec<tokio::task::JoinHandle<()>>,
+    /// No shell: a connect-time lobby, or a session killed by its client.
+    lobby: bool,
+}
+
+/// How the door reaches the session's loop: what only the loop can do, and
+/// where completed attaches go.
+pub(crate) struct LoopLink {
+    pub(crate) cmds: tokio::sync::mpsc::Sender<LoopCmd>,
+    pub(crate) attached: tokio::sync::mpsc::Sender<crate::attach_exchange::Attached>,
 }
 
 /// What one session process's doors share.
 pub(crate) struct Door {
-    /// The registry directory: where siblings are found.
+    /// The registry directory: where siblings are found and this session
+    /// registers.
     registry: PathBuf,
+    /// What the attach loop runs exchanges with.
+    cfg: NetConfig,
     own: std::sync::Mutex<Own>,
     presence: Presence,
-    /// `None` where nothing can run an attach: rung 4, which has no socket.
-    attaches: Option<AttachQueue>,
+    /// `None` for a door with no loop behind it: the tests' doors.
+    to_loop: Option<LoopLink>,
 }
 
 impl Door {
-    pub(crate) fn new(
+    /// The door of a session process: a lobby until [`Door::register`].
+    pub(crate) fn process(
+        registry: PathBuf,
+        meta: SessionMeta,
+        presence: Presence,
+        cfg: NetConfig,
+        to_loop: LoopLink,
+    ) -> Arc<Door> {
+        Arc::new(Door {
+            registry,
+            cfg,
+            own: std::sync::Mutex::new(Own {
+                meta,
+                guard: None,
+                attaches: None,
+                tasks: Vec::new(),
+                lobby: true,
+            }),
+            presence,
+            to_loop: Some(to_loop),
+        })
+    }
+
+    /// A door assembled from its parts, with no loop behind it: a session
+    /// registered as `guard`, whose `Attach` requests go to `attaches`.
+    #[cfg(test)]
+    pub(crate) fn assembled(
         registry: PathBuf,
         meta: SessionMeta,
-        guard: Option<Arc<RegistryGuard>>,
+        guard: Option<RegistryGuard>,
         presence: Presence,
         attaches: Option<AttachQueue>,
     ) -> Arc<Door> {
         Arc::new(Door {
             registry,
-            own: std::sync::Mutex::new(Own { meta, guard }),
+            cfg: NetConfig::default(),
+            own: std::sync::Mutex::new(Own {
+                meta,
+                guard,
+                attaches,
+                tasks: Vec::new(),
+                lobby: false,
+            }),
             presence,
-            attaches,
+            to_loop: None,
         })
     }
 
     fn own(&self) -> std::sync::MutexGuard<'_, Own> {
@@ -129,8 +182,92 @@ impl Door {
     pub(crate) fn meta(&self) -> SessionMeta {
         self.own().meta.clone()
     }
 
+    /// The session's id.
+    fn id(&self) -> String {
+        self.own().meta.session_id.clone()
+    }
+
+    /// R13 and the doors after it: record the session in the registry --
+    /// under its name, refused if a live session has it -- and, where it
+    /// severed from ssh, bind its socket and start the accept and attach
+    /// loops. The one way a session process becomes a registered session,
+    /// on a first connect and when a lobby is asked for `New` alike.
+    pub(crate) fn register(self: &Arc<Self>) -> anyhow::Result<()> {
+        use anyhow::Context as _;
+        let mut own = self.own();
+        let guard = RegistryGuard::register_in(&self.registry, &own.meta)?;
+        own.lobby = false;
+        if !own.meta.detachable {
+            // Rung 4: its QUIC runs inside ssh, so a socket bound anyway
+            // would offer an attach that cannot outlive it.
+            own.guard = Some(guard);
+            return Ok(());
+        }
+        let path = guard.socket_path();
+        oxutrm_host::check_socket_path_length(&path)?;
+        let listener = tokio::net::UnixListener::bind(&path)
+            .with_context(|| format!("binding the session socket at {}", path.display()))?;
+        own.guard = Some(guard);
+        if let Some(to_loop) = &self.to_loop {
+            let (queue, inbox) = attach_queue();
+            own.attaches = Some(queue);
+            own.tasks.push(tokio::spawn(crate::listener::serve_attaches(
+                Arc::clone(self),
+                self.cfg.clone(),
+                crate::listener::ATTACH_TIMEOUT,
+                inbox,
+                to_loop.attached.clone(),
+            )));
+        }
+        own.tasks.push(tokio::spawn(crate::listener::accept_doors(
+            listener,
+            Arc::clone(self),
+        )));
+        Ok(())
+    }
+
+    /// The other way round: the registry entry and the socket go, and the
+    /// loops behind them stop. A lobby from here.
+    ///
+    /// Not awaited: this runs on a door task, and the loops it stops hold
+    /// clones of the door. [`Door::close`] is the awaited end of a process.
+    fn unregister(&self) {
+        let (guard, tasks) = {
+            let mut own = self.own();
+            own.lobby = true;
+            own.attaches = None;
+            (own.guard.take(), std::mem::take(&mut own.tasks))
+        };
+        for t in &tasks {
+            t.abort();
+        }
+        drop(guard);
+    }
+
+    /// The end of the process: stop the loops, wait until they are gone --
+    /// they hold clones of this door, and through it nothing else -- and
+    /// remove the registry entry.
+    ///
+    /// Abort AND await, and why both, is `listener::close_the_door`'s own
+    /// note. The guard is dropped here explicitly, not by the last clone of
+    /// the door, because a control server can hold one for as long as its
+    /// link stays open.
+    pub(crate) async fn close(&self) {
+        let tasks = std::mem::take(&mut self.own().tasks);
+        for t in tasks {
+            crate::listener::close_the_door(t).await;
+        }
+        let guard = {
+            let mut own = self.own();
+            own.lobby = true;
+            own.attaches = None;
+            own.guard.take()
+        };
+        drop(guard);
+    }
+
     /// An attach completed with `m` as its working record: what it moved --
     /// the generation, the size, detachability -- becomes the session's, and
     /// is written to `meta.json`. The name is the door's and stays.
     pub(crate) fn record_attach(&self, m: &SessionMeta) {
@@ -160,8 +297,128 @@ impl Door {
                 offer.entry(attached, false)
             }
         })
     }
+
+    /// `New` in a lobby: register under `name`, then have the loop start the
+    /// shell. The lobby becomes the session, with the id it already has.
+    async fn start(self: &Arc<Self>, name: Option<Name>) -> Reply {
+        let Some(to_loop) = &self.to_loop else {
+            return Reply::Refused("this session cannot start a shell".to_string());
+        };
+        self.own().meta.name = name.map(String::from);
+        if let Err(e) = self.register() {
+            self.own().meta.name = None;
+            return Reply::Refused(format!("{e:#}"));
+        }
+        let (reply, started) = tokio::sync::oneshot::channel();
+        let asked = to_loop.cmds.send(LoopCmd::StartShell { reply }).await;
+        match (asked, started.await) {
+            (Ok(()), Ok(Ok(()))) => match self.own_entry(Via::Client) {
+                Some(e) => Reply::Entry(e),
+                None => Reply::Refused("the new session has no entry".to_string()),
+            },
+            (_, Ok(Err(why))) => {
+                self.unregister();
+                Reply::Refused(why)
+            }
+            _ => {
+                self.unregister();
+                Reply::Refused("the session ended".to_string())
+            }
+        }
+    }
+
+    /// `Kill` of this session: the loop hangs the shell up, then the entry
+    /// goes, then -- and only then -- `Done` (switcher spec §3.4). From its
+    /// own client the session stays as a lobby; from a sibling it ends.
+    async fn kill_self<W: AsyncWrite + Unpin>(&self, via: Via, writer: &mut W) {
+        let refused = |why: &str| Reply::Refused(why.to_string());
+        let reply = if self.own().lobby {
+            refused("this session has no shell left to kill")
+        } else if let Some(to_loop) = &self.to_loop {
+            let (reply, code) = tokio::sync::oneshot::channel();
+            let (written, written_rx) = tokio::sync::oneshot::channel();
+            let asked = to_loop
+                .cmds
+                .send(LoopCmd::Kill {
+                    end: via == Via::Socket,
+                    reply,
+                    written: written_rx,
+                })
+                .await;
+            match (asked, code.await) {
+                (Ok(()), Ok(_code)) => {
+                    self.unregister();
+                    let _ = write_line_async(writer, &Reply::Done).await;
+                    let _ = written.send(());
+                    return;
+                }
+                _ => refused("the session ended before its shell could be killed"),
+            }
+        } else {
+            refused("this session cannot kill its shell")
+        };
+        let _ = write_line_async(writer, &reply).await;
+    }
+
+    /// `Rename` of this session, under the registry's name lock.
+    fn rename_self(&self, via: Via, name: Option<Name>) -> Reply {
+        let renamed = {
+            let mut own = self.own();
+            let own = &mut *own;
+            match &own.guard {
+                None => Err("this session has no entry to name".to_string()),
+                Some(guard) => guard
+                    .rename(&mut own.meta, name.map(String::from))
+                    .map_err(|e| format!("{e:#}")),
+            }
+        };
+        match renamed.and_then(|()| {
+            self.own_entry(via)
+                .ok_or_else(|| "this session has no entry".to_string())
+        }) {
+            Ok(e) => Reply::Entry(e),
+            Err(why) => Reply::Refused(why),
+        }
+    }
+}
+
+/// How long a request forwarded to a sibling may take: a kill waits out
+/// the shell's grace and its reaping.
+pub(crate) const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);
+
+/// Ask sibling `id` to do `req` -- a `Kill` or a `Rename` -- and bring back
+/// its one reply. Every failure is a refusal with a reason for the user.
+async fn forward(registry: &std::path::Path, id: SessionId, req: Request) -> Reply {
+    let live = Registry::list_in(registry).unwrap_or_default();
+    let Some(m) = live.iter().find(|m| m.session_id == id.to_string()) else {
+        return Reply::Refused(format!("no session {} on this host", id.short()));
+    };
+    if !m.detachable {
+        return Reply::Refused(format!(
+            "session {} has no socket to ask: it dies with its ssh",
+            id.short()
+        ));
+    }
+    let path = Registry::socket_path_in(registry, &m.session_id);
+    let asked = async {
+        let stream = tokio::net::UnixStream::connect(&path).await?;
+        let (r, mut w) = stream.into_split();
+        write_line_async(&mut w, &Open::new(req)).await?;
+        read_line_async::<_, Reply>(&mut tokio::io::BufReader::new(r)).await
+    };
+    match tokio::time::timeout(FORWARD_TIMEOUT, asked).await {
+        Ok(Ok(reply)) => reply,
+        Ok(Err(ProtoError::Malformed(_) | ProtoError::VersionMismatch { .. })) => {
+            Reply::Refused(format!(
+                "session {} runs another version of oxutrm; end its shell to end it",
+                id.short()
+            ))
+        }
+        Ok(Err(e)) => Reply::Refused(format!("session {} did not answer: {e}", id.short())),
+        Err(_) => Reply::Refused(format!("session {} did not answer in time", id.short())),
+    }
 }
 
 /// Serve one connection on one of the doors: read its `Open`, answer it.
 pub(crate) async fn serve<R, W>(door: Arc<Door>, via: Via, reader: R, writer: W)
@@ -202,12 +459,28 @@ where
         Request::Sessions => {
             let reply = Reply::Sessions(sessions(&door, via).await);
             let _ = write_line_async(&mut writer, &reply).await;
         }
-        Request::Switch { .. }
-        | Request::New { .. }
-        | Request::Kill { .. }
-        | Request::Rename { .. } => {
+        Request::New { name } if door.own().lobby => {
+            let reply = door.start(name).await;
+            let _ = write_line_async(&mut writer, &reply).await;
+        }
+        Request::Kill { id } if id.to_string() == door.id() => {
+            door.kill_self(via, &mut writer).await;
+        }
+        Request::Kill { id } => {
+            let reply = forward(&door.registry, id, Request::Kill { id }).await;
+            let _ = write_line_async(&mut writer, &reply).await;
+        }
+        Request::Rename { id, name } if id.to_string() == door.id() => {
+            let reply = door.rename_self(via, name);
+            let _ = write_line_async(&mut writer, &reply).await;
+        }
+        Request::Rename { id, name } => {
+            let reply = forward(&door.registry, id, Request::Rename { id, name }).await;
+            let _ = write_line_async(&mut writer, &reply).await;
+        }
+        Request::Switch { .. } | Request::New { .. } => {
             let _ = write_line_async(
                 &mut writer,
                 &Reply::Refused("this host does not do that yet".to_string()),
             )
@@ -223,9 +496,9 @@ async fn attach<R, W>(door: &Door, role: Role, reader: R, writer: W)
 where
     R: AsyncBufRead + Unpin + Send + 'static,
     W: AsyncWrite + Unpin + Send + 'static,
 {
-    let Some(queue) = &door.attaches else {
+    let Some(queue) = door.own().attaches.clone() else {
         return;
     };
     let to = match role {
         Role::Primary => &queue.primary,
````

`src/host_session.rs`:

````diff
--- a/src/host_session.rs
+++ b/src/host_session.rs
@@ -48,12 +48,8 @@ use crate::session::{IDLE_POLL, SHELL_EXITED, SUPERSEDED, SWITCHED, TAKEN_OVER,
 pub const DETACH_AFTER: Duration = Duration::from_secs(30);
 
 /// How long a hung-up shell has to exit before its process group is killed
 /// (switcher spec §3.4).
-#[cfg_attr(
-    not(test),
-    expect(dead_code, reason = "the door's Kill passes it from Task 6 on")
-)]
 pub(crate) const KILL_GRACE: Duration = Duration::from_secs(3);
 
 /// How long a killed shell is waited for. A SIGKILLed process is reaped at
 /// once in practice; this bounds the wait for one that cannot be.
@@ -86,11 +82,46 @@ impl Presence {
         })
     }
 }
 
+/// What the door asks of the loop: the two things only the owner of the
+/// shell can do (switcher spec §2.2, §3.4).
+pub(crate) enum LoopCmd {
+    /// A lobby was asked for `New` and is registered: start the shell. The
+    /// reply says whether it started.
+    StartShell {
+        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
+    },
+    /// Kill the shell ([`HostSession::hang_up_shell`]) and reply with its
+    /// status. Then, for its own client's request, become a lobby; for a
+    /// sibling's (`end`), wait for `written` -- the door's `Done`, written
+    /// after it removed the registry entry -- and end as a shell that exited.
+    Kill {
+        end: bool,
+        reply: tokio::sync::oneshot::Sender<i32>,
+        written: tokio::sync::oneshot::Receiver<()>,
+    },
+}
+
+/// How long a session killed by a sibling waits for its door to have
+/// answered the sibling before it closes its own client's link.
+const DONE_WRITTEN_WITHIN: Duration = Duration::from_secs(2);
+
 /// The remote half: owns the PTY and the authoritative screen.
+///
+/// Or, as a **lobby**, no PTY at all (switcher spec §2.1): the same link,
+/// the same sync loop answering the client's frames with a blank screen, and
+/// no shell. A connect-time lobby starts that way ([`HostSession::lobby`]);
+/// a session whose shell was killed on its client's request becomes one. A
+/// lobby ends when its link closes or its client has been silent for
+/// [`DETACH_AFTER`]; asked for `New`, it starts its shell and is a session.
 pub struct HostSession {
-    term: HostTerm,
+    /// `None` is a lobby.
+    term: Option<HostTerm>,
+    /// The shell a lobby starts, and how.
+    shell: String,
+    start: oxutrm_term::Start,
+    scrollback: usize,
     screen_tx: oxutrm_sync::Sender<ScreenState>,
     /// `pub(crate)` for the session tests, which drive both halves.
     pub(crate) input_rx: Receiver<InputState>,
     /// `pub(crate)` for the session tests, which drive both halves.
@@ -110,8 +141,12 @@ pub struct HostSession {
     /// idle timeout went. See [`DETACH_AFTER`].
     last_heard: Instant,
     /// `last_heard` and the link it was heard on, for the door.
     presence: Presence,
+    /// How long a client may be silent before the session counts as
+    /// detached -- and a lobby ends. [`DETACH_AFTER`]; a field so a test can
+    /// wait for it in a second.
+    detach_after: Duration,
 }
 
 impl HostSession {
     /// Start a shell and serve it over `link`.
@@ -120,32 +155,42 @@ impl HostSession {
     /// takes no arguments on purpose.
     ///
     /// `start` says where and how: a session's is a login shell in `$HOME`
     /// (switcher spec §3.4); the tests' is a plain `/bin/sh`.
+    #[cfg(test)]
     pub fn spawn(
         shell: &str,
         start: &oxutrm_term::Start,
         size: TermSize,
         scrollback: usize,
         link: Link,
     ) -> Result<HostSession> {
-        let (term_name, colorterm) = oxutrm_term::negotiate_term();
-        let mut env = vec![("TERM".to_owned(), term_name)];
-        if let Some(ct) = colorterm {
-            env.push(("COLORTERM".to_owned(), ct));
-        }
+        let mut session = HostSession::lobby(shell, start, size, scrollback, link)?;
+        session.start_shell()?;
+        Ok(session)
+    }
 
-        let term = HostTerm::spawn_with(shell, &[], &env, size, scrollback, start)
-            .context("starting the shell on a pty")?;
+    /// A lobby over `link`: everything a session has but the shell, which
+    /// [`HostSession::start_shell`] starts as `shell` and `start` say.
+    pub(crate) fn lobby(
+        shell: &str,
+        start: &oxutrm_term::Start,
+        size: TermSize,
+        scrollback: usize,
+        link: Link,
+    ) -> Result<HostSession> {
         let blank = ScreenState::blank(size.rows, size.cols)?;
         let empty = InputState {
             seq: 1,
             pending: Vec::new(),
             size,
         };
 
         Ok(HostSession {
-            term,
+            term: None,
+            shell: shell.to_owned(),
+            start: start.clone(),
+            scrollback,
             screen_tx: oxutrm_sync::Sender::new(blank),
             input_rx: Receiver::new(empty),
             link,
             size,
@@ -155,11 +200,57 @@ impl HostSession {
             // An attach has just completed and R5 obliges the client to send
             // immediately, so "now" is true rather than optimistic.
             last_heard: Instant::now(),
             presence: Presence::default(),
+            detach_after: DETACH_AFTER,
         })
     }
 
+    /// Start the shell, at the session's current size: a lobby becomes a
+    /// session. Its first screen goes out on the next turn.
+    pub(crate) fn start_shell(&mut self) -> Result<()> {
+        let (term_name, colorterm) = oxutrm_term::negotiate_term();
+        let mut env = vec![("TERM".to_owned(), term_name)];
+        if let Some(ct) = colorterm {
+            env.push(("COLORTERM".to_owned(), ct));
+        }
+        let term = HostTerm::spawn_with(
+            &self.shell,
+            &[],
+            &env,
+            self.size,
+            self.scrollback,
+            &self.start,
+        )
+        .context("starting the shell on a pty")?;
+        self.term = Some(term);
+        self.written = self.input_rx.state().pending.len();
+        self.screen_stale = true;
+        Ok(())
+    }
+
+    /// Whether this is a lobby: no shell.
+    pub(crate) fn is_lobby(&self) -> bool {
+        self.term.is_none()
+    }
+
+    /// The shell is gone on request: the session is a lobby from here, and
+    /// its client sees a blank screen.
+    fn become_lobby(&mut self) -> Result<()> {
+        self.term = None;
+        self.screen_tx
+            .update(ScreenState::blank(self.size.rows, self.size.cols)?);
+        self.screen_stale = false;
+        Ok(())
+    }
+
+    /// Wait for a silent client for `d` instead of [`DETACH_AFTER`].
+    #[cfg(test)]
+    pub(crate) fn with_detach_after(mut self, d: Duration) -> HostSession {
+        self.detach_after = d;
+        self
+    }
+
     /// Share whether a client is attached with `presence`, the door's copy.
     pub(crate) fn with_presence(mut self, presence: Presence) -> HostSession {
         presence.heard(self.link.sink.connection(), self.last_heard);
         self.presence = presence;
@@ -259,31 +350,34 @@ impl HostSession {
         // during a blip the connection is open and we WANT the work to
         // continue, so the session resumes instantly when the peer comes back.
         // `DETACH_AFTER` is six times the client's heartbeat interval.
         let closed = self.link.sink.connection().close_reason().is_some();
-        let quiet_too_long = now.duration_since(self.last_heard) >= DETACH_AFTER;
+        let quiet_too_long = now.duration_since(self.last_heard) >= self.detach_after;
         let attached = !closed && !quiet_too_long;
         turn.detached = !attached;
 
         // ---- the terminal --------------------------------------------------
-        let moved = self.term.poll().context("draining the pty")?;
-        if attached {
-            if moved || self.screen_stale {
-                // The sequence number is a placeholder; `update` mints the real
-                // one, keeping numbering in exactly one place.
-                let snapshot = self.term.snapshot(1);
-                self.screen_tx.update(snapshot);
-                self.screen_stale = false;
+        // A lobby has none: its screen is the blank one it was given.
+        if let Some(term) = self.term.as_mut() {
+            let moved = term.poll().context("draining the pty")?;
+            if attached {
+                if moved || self.screen_stale {
+                    // The sequence number is a placeholder; `update` mints
+                    // the real one, keeping numbering in exactly one place.
+                    let snapshot = term.snapshot(1);
+                    self.screen_tx.update(snapshot);
+                    self.screen_stale = false;
+                }
+            } else if moved {
+                self.screen_stale = true;
             }
-        } else if moved {
-            self.screen_stale = true;
         }
 
         // ---- outbound: the screen ------------------------------------------
         if attached {
             turn.sent = self.offer_frame();
         }
-        turn.exited = self.term.child_exited();
+        turn.exited = self.term.as_mut().and_then(HostTerm::child_exited);
         Ok(turn)
     }
 
     /// Write newly acknowledged input to the PTY, exactly once.
@@ -301,11 +395,12 @@ impl HostSession {
             self.written = pending.len();
         }
         if self.written < pending.len() {
             let fresh = pending[self.written..].to_vec();
-            self.term
-                .write_input(&fresh)
-                .context("writing to the pty")?;
+            // A lobby has nowhere to put typing: it is taken and dropped.
+            if let Some(term) = self.term.as_mut() {
+                term.write_input(&fresh).context("writing to the pty")?;
+            }
             self.written = pending.len();
         }
         Ok(())
     }
@@ -361,9 +456,15 @@ impl HostSession {
     pub fn resize(&mut self, size: TermSize) -> Result<()> {
         if size == self.size {
             return Ok(());
         }
-        self.term.resize(size).context("resizing the pty")?;
+        match self.term.as_mut() {
+            Some(term) => term.resize(size).context("resizing the pty")?,
+            // A lobby's screen is blank at whatever size its client is.
+            None => self
+                .screen_tx
+                .update(ScreenState::blank(size.rows, size.cols)?),
+        }
         self.size = size;
         Ok(())
     }
 
@@ -373,8 +474,9 @@ impl HostSession {
     /// whose sender has already been dropped. `Some(a) = attaches.recv()`
     /// makes a closed receiver disable that arm rather than make it hot — see
     /// `run_with_attaches`' own note — so this costs nothing and every
     /// existing caller and test is unchanged.
+    #[cfg(test)]
     pub async fn run(&mut self) -> Result<i32> {
         let (tx, mut rx) = tokio::sync::mpsc::channel(1);
         // Dropped, not merely unnamed. `let (_tx, ...)` binds the sender for
         // the whole `await`, which leaves an open-and-empty channel whose
@@ -398,27 +500,32 @@ impl HostSession {
     /// call `&mut self` methods afterwards (C1). A `dup` shares the file
     /// description, harmless here in a way it is NOT for the client's
     /// keyboard: this description is ours and we set its `O_NONBLOCK`
     /// ourselves in `Pty::spawn`.
+    #[cfg(test)]
     pub async fn run_with_attaches(
         &mut self,
         attaches: &mut tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
     ) -> Result<i32> {
-        let output = self.term.output_fd().try_clone_to_owned()?;
-        let output = tokio::io::unix::AsyncFd::with_interest(output, tokio::io::Interest::READABLE)
-            .context("waiting on the pty")?;
-        let exit = match self.term.exit_wake().as_fd() {
-            Some(fd) => Some(
-                tokio::io::unix::AsyncFd::with_interest(
-                    fd.try_clone_to_owned()?,
-                    tokio::io::Interest::READABLE,
-                )
-                .context("waiting on the child")?,
-            ),
-            // Already gone when it was watched. The first turn below reports
-            // the exit before anything waits, so there is nothing to miss.
-            None => None,
-        };
+        let (tx, mut cmds) = tokio::sync::mpsc::channel(1);
+        // Dropped, for the reason `run` drops its sender.
+        drop(tx);
+        self.run_with_doors(attaches, &mut cmds).await
+    }
+
+    /// [`HostSession::run_with_attaches`], plus what the door asks of the
+    /// loop ([`LoopCmd`]): start a lobby's shell, kill this one's.
+    ///
+    /// Returns the shell's exit status, or `0` for a lobby that ended -- its
+    /// link closed, or its client silent for [`DETACH_AFTER`].
+    pub(crate) async fn run_with_doors(
+        &mut self,
+        attaches: &mut tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
+        cmds: &mut tokio::sync::mpsc::Receiver<LoopCmd>,
+    ) -> Result<i32> {
+        // The shell's descriptors, while there is a shell; watched again
+        // whenever the loop starts or ends one.
+        let mut watch = self.watch()?;
 
         // A frame taken off the source by the select, owed to the next turn.
         let mut pending: Option<Frame> = None;
         // The client's parked standby (spec §3.5): a link held, not used,
@@ -466,9 +573,13 @@ impl HostSession {
             // else happened, and the screen being current on reattach is the
             // whole reason a detached session keeps emulating at all. It is
             // kept as the cheap half of a guarantee whose expensive half
             // (`READ_BUDGET` versus the kernel's PTY buffer) is not ours.
-            if self.term.more_output_waiting() {
+            if self
+                .term
+                .as_ref()
+                .is_some_and(HostTerm::more_output_waiting)
+            {
                 continue;
             }
 
             // Armed only when a frame is owed but paced out, so a session with
@@ -483,8 +594,16 @@ impl HostSession {
                 let at = tokio::time::Instant::now() + IDLE_POLL;
                 deadline = Some(deadline.map_or(at, |d| d.min(at)));
             }
 
+            // A lobby has nothing to outlive its client for: it ends with its
+            // link, or after `detach_after` of silence. Its own two arms,
+            // armed only in a lobby -- a session must outlive a vanished
+            // client, which is the entire point (see below).
+            let lobby = self.is_lobby();
+            let conn = self.link.sink.connection().clone();
+            let silent_until = tokio::time::Instant::from_std(self.last_heard + self.detach_after);
+
             // Nothing here touches `self`; every borrow starts after the
             // select expression has ended and dropped these futures (C1).
             //
             // There is deliberately NO `conn.closed()` arm. A closed
@@ -493,17 +612,19 @@ impl HostSession {
             // outliving a vanished client is the entire point. `turn` re-reads
             // `close_reason` whenever something else wakes it, which is
             // exactly when the answer can matter.
             let wake: HostWake = tokio::select! {
-                r = output.readable() => match r {
+                r = async { watch.as_ref().expect("armed").output.readable().await },
+                    if watch.is_some() => match r {
                     // Cleared HERE, having just established above that the PTY
                     // came up empty. Read-then-clear is the ordering `try_io`
                     // uses, and clearing while bytes remain would stall the
                     // screen until the child happened to write again.
                     Ok(mut g) => { g.clear_ready(); HostWake::Pty }
                     Err(e) => return Err(e).context("waiting on the pty"),
                 },
-                r = async { exit.as_ref().expect("armed").readable().await }, if exit.is_some() => match r {
+                r = async { watch.as_ref().and_then(|w| w.exit.as_ref()).expect("armed").readable().await },
+                    if watch.as_ref().is_some_and(|w| w.exit.is_some()) => match r {
                     Ok(mut g) => { g.clear_ready(); HostWake::Exit }
                     Err(e) => return Err(e).context("waiting on the child"),
                 },
                 Some(frame) = self.link.source.recv() => HostWake::Frame(frame),
@@ -521,8 +642,11 @@ impl HostSession {
                     if standby.is_some() => match f {
                         Some(frame) => HostWake::StandbyFrame(frame),
                         None => HostWake::StandbyGone,
                     },
+                Some(c) = cmds.recv() => HostWake::Cmd(c),
+                _ = conn.closed(), if lobby => HostWake::LobbyOver,
+                () = tokio::time::sleep_until(silent_until), if lobby => HostWake::LobbyOver,
             };
 
             match wake {
                 HostWake::Frame(frame) => pending = Some(frame),
@@ -535,36 +659,118 @@ impl HostSession {
                 }
                 // Its connection is already closed, which is what ended the
                 // source; there is nothing left to close.
                 HostWake::StandbyGone => standby = None,
+                HostWake::LobbyOver => {
+                    let closed = self.link.sink.connection().close_reason().is_some();
+                    let silent = self.last_heard.elapsed() >= self.detach_after;
+                    if self.is_lobby() && (closed || silent) {
+                        if let Some(parked) = standby.take() {
+                            parked
+                                .sink
+                                .connection()
+                                .close(quinn::VarInt::from_u32(0), SUPERSEDED);
+                        }
+                        return Ok(0);
+                    }
+                }
+                HostWake::Cmd(LoopCmd::StartShell { reply }) => {
+                    let started = if self.is_lobby() {
+                        self.start_shell().map_err(|e| format!("{e:#}"))
+                    } else {
+                        Err("this session already has a shell".to_string())
+                    };
+                    let ok = started.is_ok();
+                    let _ = reply.send(started);
+                    if ok {
+                        watch = self.watch()?;
+                    }
+                }
+                HostWake::Cmd(LoopCmd::Kill {
+                    end,
+                    reply,
+                    written,
+                }) => {
+                    if self.is_lobby() {
+                        let _ = reply.send(-1);
+                        continue;
+                    }
+                    let code = self.hang_up_shell(KILL_GRACE).await;
+                    // The parked standby belonged to this shell's client;
+                    // it goes as a standby goes when the shell exits.
+                    if let Some(parked) = standby.take() {
+                        close_as_exited(parked.sink.connection(), code);
+                    }
+                    if end {
+                        // A sibling asked: end as a shell that exited, once
+                        // the door has told the sibling it is done.
+                        let _ = reply.send(code);
+                        let _ = tokio::time::timeout(DONE_WRITTEN_WITHIN, written).await;
+                        self.finish(code).await;
+                        return Ok(code);
+                    }
+                    // Our own client asked: a lobby from here.
+                    self.become_lobby()?;
+                    watch = None;
+                    let _ = reply.send(code);
+                }
             }
         }
     }
 
+    /// The shell's descriptors to wait on, or `None` in a lobby.
+    ///
+    /// Duplicated out of the terminal so the loop's arms borrow locals
+    /// rather than `self`, which is what lets the body call `&mut self`
+    /// methods afterwards (C1). A `dup` shares the file description,
+    /// harmless here in a way it is NOT for the client's keyboard: this
+    /// description is ours and we set its `O_NONBLOCK` ourselves in
+    /// `Pty::spawn`.
+    fn watch(&self) -> Result<Option<Watch>> {
+        let Some(term) = self.term.as_ref() else {
+            return Ok(None);
+        };
+        let output = term.output_fd().try_clone_to_owned()?;
+        let output = tokio::io::unix::AsyncFd::with_interest(output, tokio::io::Interest::READABLE)
+            .context("waiting on the pty")?;
+        let exit = match term.exit_wake().as_fd() {
+            Some(fd) => Some(
+                tokio::io::unix::AsyncFd::with_interest(
+                    fd.try_clone_to_owned()?,
+                    tokio::io::Interest::READABLE,
+                )
+                .context("waiting on the child")?,
+            ),
+            // Already gone when it was watched. The first turn reports the
+            // exit before anything waits, so there is nothing to miss.
+            None => None,
+        };
+        Ok(Some(Watch { output, exit }))
+    }
+
     /// End the shell on request: hang it up as a closing terminal would, and
     /// SIGKILL its process group if it is still there after `grace`
     /// ([`KILL_GRACE`] in the session). Returns its exit status once it is
     /// reaped -- or `-1` for one that could not be reaped even after the
     /// kill, which a session ends on all the same.
     ///
     /// Drains the pty while it waits: on macOS a child killed while writing
     /// to a pty is not reaped until its output is read (`Pty::reap`).
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the door's Kill calls it from Task 6 on")
-    )]
     pub(crate) async fn hang_up_shell(&mut self, grace: Duration) -> i32 {
-        self.term.hang_up();
+        let Some(term) = self.term.as_mut() else {
+            return -1;
+        };
+        term.hang_up();
         let start = tokio::time::Instant::now();
         let mut killed = false;
         loop {
-            let _ = self.term.poll();
-            if let Some(code) = self.term.child_exited() {
+            let _ = term.poll();
+            if let Some(code) = term.child_exited() {
                 return code;
             }
             let waited = start.elapsed();
             if !killed && waited >= grace {
-                self.term.kill_group();
+                term.kill_group();
                 killed = true;
             }
             if killed && waited >= grace + REAP_AFTER_KILL {
                 return -1;
@@ -595,10 +801,12 @@ impl HostSession {
     ///
     /// Infallible on purpose: nothing here is worth reporting instead of the
     /// status of a shell that has already exited.
     pub async fn finish(&mut self, code: i32) {
-        if self.term.poll().unwrap_or(false) {
-            let snapshot = self.term.snapshot(1);
+        if let Some(term) = self.term.as_mut()
+            && term.poll().unwrap_or(false)
+        {
+            let snapshot = term.snapshot(1);
             self.screen_tx.update(snapshot);
         }
         self.last_send = None;
         self.offer_frame_reliably().await;
@@ -626,9 +834,9 @@ impl HostSession {
     /// The shell's terminal, for the session tests, which type into it and
     /// read it directly.
     #[cfg(test)]
     pub(crate) fn term_mut(&mut self) -> &mut HostTerm {
-        &mut self.term
+        self.term.as_mut().expect("a session, not a lobby")
     }
 
     /// The authoritative screen, for tests. Nothing in the session loop reads
     /// it: the loop ships diffs and never inspects what it shipped.
@@ -778,6 +986,16 @@ enum HostWake {
     /// A frame arrived on the parked standby: the client has failed over.
     StandbyFrame(Frame),
     /// The parked standby's connection is gone.
     StandbyGone,
+    /// The door asked for something only the loop can do.
+    Cmd(LoopCmd),
+    /// A lobby's link closed, or its client may have been silent too long.
+    LobbyOver,
+}
+
+/// The shell's two descriptors, as the loop waits on them.
+struct Watch {
+    output: tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>,
+    exit: Option<tokio::io::unix::AsyncFd<std::os::fd::OwnedFd>>,
 }
 
````

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -93,9 +93,9 @@ fn run_host(args: &[String]) -> Result<()> {
         }
         // Works today: it needs the registry and nothing else.
         Some("--list") => run_host_list(),
         Some("--connect") => run_host_connect(),
-        Some("--serve") => serve::run_host_serve(),
+        Some("--serve") => serve::run_host_serve(serve::Begin::Session),
         Some("--attach") => match args.get(1) {
             Some(id) => run_host_attach(id),
             None => Err(anyhow::anyhow!(
                 "`oxutrm host --attach` needs a session id. \
@@ -210,9 +210,9 @@ fn run_host_connect() -> Result<()> {
             }
         };
 
     match choice {
-        Choice::New => serve::run_host_serve(),
+        Choice::New => serve::run_host_serve(serve::Begin::Session),
         Choice::Attach { id } => {
             // Refused HERE rather than inside the relay, because the client is
             // still listening on this channel: a `Failed` it can read is worth
             // more than an exit code it has to guess at.
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -16,9 +16,9 @@ const SCROLLBACK: usize = 10_000;
 /// The order of the first three statements is the design, not a style. See
 /// [`oxutrm_host::detach_process`]: it must run before a socket, a runtime or
 /// a thread exists, because `fork` copies only the calling thread and a
 /// runtime built beforehand wakes up in the child with its workers gone.
-pub fn run_host_serve() -> anyhow::Result<()> {
+pub fn run_host_serve(begin: Begin) -> anyhow::Result<()> {
     // R1. Nothing has run before this. The parent `_exit(0)`s, so ssh reports
     // the command finished; the channel stays open because the grandchild
     // still holds 0, 1 and 2.
     let detached = oxutrm_host::detach_process().context("detaching from ssh")?;
@@ -36,9 +36,9 @@ pub fn run_host_serve() -> anyhow::Result<()> {
     let runtime = tokio::runtime::Builder::new_multi_thread()
         .enable_all()
         .build()
         .context("building the runtime")?;
-    let outcome = runtime.block_on(serve(detached, &root));
+    let outcome = runtime.block_on(serve(detached, &root, begin));
 
     // Do not WAIT for the read that is parked on ssh's pipe.
     //
     // The signalling channel is descriptor 0, and `tokio::io::stdin` serves it
@@ -58,24 +58,28 @@ pub fn run_host_serve() -> anyhow::Result<()> {
     // removed by its guard; the only thing outstanding is a message from a
     // client we have stopped listening to.
     //
     // The middle clause is the one this depends on, and it is a claim about
-    // code rather than a hope: `serve()` awaits the aborted listener task
-    // before it drops the guard, so the last `Arc` really is gone and
-    // `RegistryGuard::drop` really has run by the time `serve()` returns. When
-    // that await was missing, the guard was still held by a task the runtime
-    // had not got round to dropping, `remove_dir_all` did not run, and this
-    // paragraph was licensing the process to walk away from a cleanup that had
-    // not happened.
+    // code rather than a hope: `run_process` ends on `Door::close`, which
+    // awaits the aborted accept and attach loops and then drops the guard
+    // itself, so `RegistryGuard::drop` really has run by the time `serve()`
+    // returns. When an await like that was missing, the guard was still held
+    // by a task the runtime had not got round to dropping, `remove_dir_all`
+    // did not run, and this paragraph was licensing the process to walk away
+    // from a cleanup that had not happened.
     runtime.shutdown_background();
     outcome
 }
 
 /// R4 to R16, with the ssh pipes as the signalling channel.
 ///
 /// Everything here runs in the grandchild. Descriptors 0, 1 and 2 are still
 /// ssh's until R12, which is why the whole handshake happens before it.
-async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::Result<()> {
+async fn serve(
+    detached: oxutrm_host::Detached,
+    root: &RegistryRoot,
+    begin: Begin,
+) -> anyhow::Result<()> {
     let cfg = NetConfig::default();
     let mut meta = SessionMeta {
         session_id: oxutrm_host::new_session_id().context("naming the session")?,
         attach_id: 0,
@@ -108,110 +112,122 @@ async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::
     // R12. `None` is rung 4: its QUIC traffic runs inside the ssh connection,
     // so it keeps its pipes and its ssh for life. Anything else severs, ssh
     // sees EOF, and the user's prompt comes back.
     //
-    // The socket, below, follows the same arm: `permit` is consumed here, so
+    // The socket, later, follows the same arm: `permit` is consumed here, so
     // it is `meta.detachable` -- the very fact `settle_detachability` just
-    // wrote -- that the listener setup reads to make the identical choice.
-    let sock = match permit {
+    // wrote -- that `Door::register` reads to make the identical choice.
+    match permit {
         Some(permit) => {
             oxutrm_host::sever_from_ssh(detached, permit).context("severing from ssh")?;
-            true
         }
         // Rung 4. The fork already happened and is harmless; what must not
         // happen is the sever. The token simply goes unused, which is the
         // shape the type system gives this case: there is no permit to pass,
         // so there is no call to write.
         None => {
             let oxutrm_host::Detached { .. } = detached;
-            false
         }
-    };
-
-    // R13. AFTER R12, or the socket path is closed a moment later:
-    // `close_inherited_descriptors` closes by enumeration and keeps no list of
-    // exceptions, which is the whole of its value.
-    let guard =
-        oxutrm_host::RegistryGuard::register_in(&oxutrm_host::Registry::dir_at(&root.base), &meta)
-            .context("recording the session in the registry")?;
-
-    // `RegistryGuard` becomes an `Arc` here because the door and this
-    // function both need it, and its `Drop` removes the session directory: the
-    // directory must outlive both, which is exactly what an `Arc` says.
-    let guard = std::sync::Arc::new(guard);
-    let shell = meta.shell.clone();
-    let presence = crate::host_session::Presence::default();
-
-    // The socket path has always been computed and registered. Nothing has
-    // ever bound it until now -- and only when this session severed from ssh:
-    // rung 4 keeps `sock` false, because its QUIC traffic runs inside the ssh
-    // connection and a socket bound anyway would offer an attach that cannot
-    // outlive it.
-    let registry = oxutrm_host::Registry::dir_at(&root.base);
-    let listening = if sock {
-        let path = guard.socket_path();
-        oxutrm_host::check_socket_path_length(&path)?;
-        let listener = tokio::net::UnixListener::bind(&path)
-            .with_context(|| format!("binding the session socket at {}", path.display()))?;
-        Some(open_doors(
-            listener,
-            registry,
-            meta,
-            std::sync::Arc::clone(&guard),
-            presence.clone(),
-            cfg,
-            attached.link.sink.connection().clone(),
-        ))
-    } else {
-        // No socket, so no attach loop to run a standby's exchange. Such a
-        // session (rung 4, whose QUIC runs inside ssh) has nothing a standby
-        // could outlive it through, but its hello advertised a control stream
-        // all the same, so the link is served through a door with no attach
-        // loop: probes are answered and a standby request ends at once
-        // rather than being left waiting.
-        let door = crate::door::Door::new(
-            registry,
-            meta,
-            Some(std::sync::Arc::clone(&guard)),
-            presence.clone(),
-            None,
-        );
-        crate::control::serve_control(attached.link.sink.connection().clone(), door);
-        None
-    };
+    }
 
-    // R14. `negotiate_term` takes no arguments: the child's TERM comes from
-    // the emulator, never from the client, or a client narrower than the
-    // emulator would bake degraded output into the authoritative screen for
-    // the life of the session.
+    // R13 onwards, AFTER R12, or the socket path is closed a moment later:
+    // `close_inherited_descriptors` closes by enumeration and keeps no list
+    // of exceptions, which is the whole of its value.
     //
     // A login shell in `$HOME`, as ssh would give you (switcher spec §3.4).
-    let start = oxutrm_term::Start::login_in(home_dir(std::env::var_os("HOME")));
-    let mut session = HostSession::spawn(
-        &shell,
-        &start,
-        attached.client_size,
-        SCROLLBACK,
+    let shell = Shell {
+        program: meta.shell.clone(),
+        start: oxutrm_term::Start::login_in(home_dir(std::env::var_os("HOME"))),
+    };
+    run_process(
         attached.link,
+        attached.client_size,
+        meta,
+        oxutrm_host::Registry::dir_at(&root.base),
+        cfg,
+        begin,
+        shell,
     )
-    .context("starting the shell")?
-    .with_presence(presence);
-
-    // R15, then R16: dropping the guard takes the session directory with it,
-    // so a session that exits cleanly leaves nothing for `--list` to prune.
-    let code = match listening {
-        Some((tasks, mut attach_rx)) => {
-            let code = session.run_with_attaches(&mut attach_rx).await;
-            // Abort AND await, and why both, is `close_the_door`'s own note.
-            for task in tasks {
-                crate::listener::close_the_door(task).await;
-            }
-            code
+    .await
+    .map(|_| ())
+}
+
+/// What a session process starts as: a session with its shell, or a lobby
+/// (switcher spec §2.1).
+#[derive(Clone, Debug, PartialEq, Eq)]
+pub(crate) enum Begin {
+    Session,
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "`Choice::Lobby` starts one from Task 7 on")
+    )]
+    Lobby,
+}
+
+/// The shell a session runs, and how it starts it.
+pub(crate) struct Shell {
+    pub(crate) program: String,
+    pub(crate) start: oxutrm_term::Start,
+}
+
+/// R13 to R16: one session process, from its first link to its end.
+///
+/// Everything a session process does after its first attach exchange, and
+/// the same for every way one starts -- a first connect over ssh, a sibling
+/// over a socket pair, a lobby. A function of its own, apart from the fork
+/// and the sever, so the whole of it runs in a test.
+///
+/// `Begin::Session` registers (under `meta.name`) and starts the shell;
+/// `Begin::Lobby` does neither until it is asked for `New`. Returns the
+/// shell's exit status, or `0` for a lobby that ended.
+pub(crate) async fn run_process(
+    link: crate::link::Link,
+    client_size: TermSize,
+    meta: SessionMeta,
+    registry: std::path::PathBuf,
+    cfg: NetConfig,
+    begin: Begin,
+    shell: Shell,
+) -> anyhow::Result<i32> {
+    let presence = crate::host_session::Presence::default();
+    let (cmds_tx, mut cmds) = tokio::sync::mpsc::channel(1);
+    let (attached_tx, mut attaches) = tokio::sync::mpsc::channel(1);
+    let first = link.sink.connection().clone();
+    let door = crate::door::Door::process(
+        registry,
+        meta,
+        presence.clone(),
+        cfg,
+        crate::door::LoopLink {
+            cmds: cmds_tx,
+            attached: attached_tx,
+        },
+    );
+    let mut session =
+        HostSession::lobby(&shell.program, &shell.start, client_size, SCROLLBACK, link)?
+            .with_presence(presence);
+    if begin == Begin::Session {
+        // R13: registered, and its socket bound where the rung allows one.
+        door.register()
+            .context("recording the session in the registry")?;
+        // R14. `negotiate_term` takes no arguments: the child's TERM comes
+        // from the emulator, never from the client, or a client narrower
+        // than the emulator would bake degraded output into the
+        // authoritative screen for the life of the session.
+        if let Err(e) = session.start_shell() {
+            door.close().await;
+            return Err(e.context("starting the shell"));
         }
-        None => session.run().await,
-    };
-    drop(guard);
-    code.map(|_| ())
+    }
+    // The first link's control stream: every later link's is started by
+    // the attach loop as it hands the link over.
+    crate::control::serve_control(first, std::sync::Arc::clone(&door));
+
+    // R15, then R16: the door's close takes the registry entry with it, so a
+    // session that exits cleanly leaves nothing for `--list` to prune.
+    let code = session.run_with_doors(&mut attaches, &mut cmds).await;
+    door.close().await;
+    code
 }
 
 /// The directory a new shell starts in: `$HOME` when it names a directory,
 /// else none -- the inherited one -- rather than a shell that fails to start.
@@ -219,41 +235,4 @@ pub(crate) fn home_dir(home: Option<std::ffi::OsString>) -> Option<std::path::Pa
     home.map(std::path::PathBuf::from)
         .filter(|p| p.is_absolute() && p.is_dir())
 }
 
-/// The two ways into a severed session after its first attach: the Unix
-/// socket, and the control stream of every link, both through one door
-/// ([`crate::door`]).
-///
-/// Returns the door's two tasks -- the socket's accept loop and the serial
-/// attach loop -- and where completed attaches arrive. A function of its own
-/// so the first link's control server, which nothing else starts, can be
-/// reached from a test: every later link's is started by the attach loop as
-/// it hands the link over.
-pub(crate) fn open_doors(
-    listener: tokio::net::UnixListener,
-    registry: std::path::PathBuf,
-    meta: SessionMeta,
-    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
-    presence: crate::host_session::Presence,
-    cfg: NetConfig,
-    first: quinn::Connection,
-) -> (
-    [tokio::task::JoinHandle<()>; 2],
-    tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
-) {
-    let (attach_tx, attach_rx) = tokio::sync::mpsc::channel(1);
-    let (queue, inbox) = crate::door::attach_queue();
-    let door = crate::door::Door::new(registry, meta, Some(guard), presence, Some(queue));
-    crate::control::serve_control(first, std::sync::Arc::clone(&door));
-    let accept = tokio::spawn(crate::listener::accept_doors(
-        listener,
-        std::sync::Arc::clone(&door),
-    ));
-    let attaches = tokio::spawn(crate::listener::serve_attaches(
-        door,
-        cfg,
-        crate::listener::ATTACH_TIMEOUT,
-        inbox,
-        attach_tx,
-    ));
-    ([accept, attaches], attach_rx)
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --bin oxutrm --jobs 4 serve:: host_session:: door:: listener:: control::`

Expected: all pass, among them `new_in_a_lobby_registers_it_by_name_and_it_becomes_the_session`, `new_in_a_lobby_with_a_taken_name_is_refused_and_it_stays_a_lobby`, `killing_this_session_leaves_a_lobby_that_can_start_again`, `killing_a_sibling_ends_it_as_a_shell_that_exited`, `a_shell_that_ignores_the_hang_up_is_killed_and_then_done` (takes the three-second grace), `rename_here_and_on_a_sibling_and_a_taken_name_is_refused`, `a_name_that_is_all_hex_never_reaches_the_session`, `a_new_sessions_shell_starts_in_home`, the lobby's `a_lobby_whose_link_closes_ends`, `a_lobby_whose_client_vanished_ends_after_the_silence`, `a_session_whose_client_vanished_does_not_end`, `a_lobby_answers_its_client_with_a_blank_screen`, and `a_lobbys_blank_screen_follows_its_clients_size`.

`a_new_sessions_shell_starts_in_home` asserts only the directory: a `#!` script's `$0` is its path whatever `argv[0]` was, so the dash is proven on the pty in Task 4.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add src/control.rs src/door.rs src/host_session.rs src/listener.rs src/main.rs src/serve.rs src/session.rs
git commit -m "feat(host): the lobby; New in a lobby, Kill and Rename through the door

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 7: The offer and the choice: `OfferEntry`, `--name`, `--attach <name>`, a lobby instead of the line picker

Spec §3.1, §4.2. `Signal::Sessions { list: Vec<OfferEntry> }` (built from `meta.json` only, before the fork); `Choice` is `Attach { id: SessionId } | New { name: Option<Name> } | Lobby`; `SessionSummary` and `attach::summarize` go. `choose::decide` is the §3.1 table: `--new` -> `New { name }`; `--attach x` -> the session named exactly `x`, else the one whose id `x` prefixes (four characters or more, case-sensitive: ids are lowercase hex); nothing running -> `New`; anything running -> `Lobby`. The line picker (`choose::pick`, `Decision::Ask`) is gone.

`connect` parses `--name <name>` (validated with `Name::parse` before ssh; without `--new` it is refused, exit 2); `dispatch` routes `--name` to it. `run_host_connect` refuses a taken name with a `Failed` before it forks -- still runtime-free: a blocking `flock` and a directory read -- and `Door::register` checks again under the same lock for the race this leaves. `oxutrm host --serve [--name <name>]`. The opening line and the splash caption say `new session <name> (<id>)`, or how many sessions are running when the lobby is about to open.

The rebuild still aims at the session id (Task 9 makes it aim at a lobby): until then a bare connect that lands in a lobby and then loses its link ends the client on the rebuild's "no such session". This window closes in Task 9.

**Files:**
- Modify: `crates/oxutrm-proto/src/signal.rs`, `crates/oxutrm-proto/src/lib.rs`, `crates/oxutrm-host/src/attach.rs`, `src/choose.rs` (rewritten), `src/connect.rs`, `src/main.rs`, `src/serve.rs`, `src/rebuild.rs`, `src/session.rs`, `tests/client_flags.rs`, `tests/host_connect.rs`

**Interfaces:**
- Consumes: Task 2's `Name`, `OfferEntry`, `SessionId`; Task 3's `SessionMeta::offer`, `NamesLock`, `name_refusal`; Task 6's `Begin`, `run_process`.
- Produces:
  - `Signal::Sessions { list: Vec<OfferEntry> }`; `Choice::{Attach { id: SessionId }, New { name: Option<Name> }, Lobby}` (tag `"c"`).
  - `choose::decide(offered: &[OfferEntry], attach: Option<&str>, new: bool, name: Option<Name>) -> Decision`; `Decision::{Chosen(Choice), Refused(String)}`.
  - `serve::Begin::Session { name: Option<Name> }`; `run_process` sets `meta.name` from it.
  - `connect::opening_line(chosen: &Choice, session_id: &str, offered: usize) -> String`.

- [ ] **Step 1: Write the failing tests**

`crates/oxutrm-proto/src/signal.rs`:

````diff
--- a/crates/oxutrm-proto/src/signal.rs
+++ b/crates/oxutrm-proto/src/signal.rs
@@ -1010,61 +990,58 @@ mod tests {
     }
 
     // ---- the session offer, before the hellos ----
 
+    fn offer_entry() -> OfferEntry {
+        OfferEntry {
+            id: "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().unwrap(),
+            name: Some(Name::parse("build").unwrap()),
+            shell: "/bin/bash".to_owned(),
+            created_unix: 1_757_200_000,
+            size: TermSize {
+                cols: 120,
+                rows: 40,
+            },
+            detachable: true,
+        }
+    }
+
     #[test]
     fn an_offer_round_trips_through_json() {
-        // The offer travels on the same newline-delimited JSON channel as the
-        // hellos, so it must survive the same encode/decode. Asserting on the
-        // decoded value and not on the string: the tag names are an internal
-        // detail, the round trip is the contract.
         let offer = Signal::Sessions {
-            sessions: vec![SessionSummary {
-                session_id: "f00d".repeat(8),
-                created_unix: 1_757_200_000,
-                shell: "/bin/bash".to_owned(),
-                size: TermSize {
-                    cols: 120,
-                    rows: 40,
-                },
-                detachable: true,
-                attach_id: 3,
-            }],
+            list: vec![offer_entry()],
         };
         let line = serde_json::to_string(&offer).expect("an offer encodes");
         let back: Signal = serde_json::from_str(&line).expect("an offer decodes");
         match back {
-            Signal::Sessions { sessions } => {
-                assert_eq!(sessions.len(), 1);
-                assert_eq!(sessions[0].session_id, "f00d".repeat(8));
-                assert_eq!(sessions[0].size.cols, 120);
-                assert!(sessions[0].detachable);
-                assert_eq!(sessions[0].attach_id, 3);
-            }
+            Signal::Sessions { list } => assert_eq!(list, vec![offer_entry()]),
             other => panic!("expected Sessions, got {other:?}"),
         }
     }
 
     #[test]
     fn an_empty_offer_is_a_legitimate_offer() {
-        // The ordinary first connect. It must not be expressed as an absent
-        // message: the client blocks on reading exactly one line here, and
-        // "nothing to offer" has to be sayable.
-        let line = serde_json::to_string(&Signal::Sessions { sessions: vec![] })
+        // The ordinary first connect. The client blocks on reading exactly one
+        // line here, and "nothing to offer" has to be sayable.
+        let line = serde_json::to_string(&Signal::Sessions { list: vec![] })
             .expect("an empty offer encodes");
         match serde_json::from_str::<Signal>(&line).expect("an empty offer decodes") {
-            Signal::Sessions { sessions } => assert!(sessions.is_empty()),
+            Signal::Sessions { list } => assert!(list.is_empty()),
             other => panic!("expected Sessions, got {other:?}"),
         }
     }
 
     #[test]
-    fn both_choices_round_trip() {
+    fn every_choice_round_trips() {
         for choice in [
-            Choice::New,
+            Choice::New { name: None },
+            Choice::New {
+                name: Some(Name::parse("build").unwrap()),
+            },
             Choice::Attach {
-                id: "abcd1234".to_owned(),
+                id: "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().unwrap(),
             },
+            Choice::Lobby,
         ] {
             let line = serde_json::to_string(&Signal::Choose {
                 choice: choice.clone(),
             })
@@ -1077,28 +1054,19 @@ mod tests {
     }
 
     #[test]
     fn an_offer_carries_no_process_identity() {
-        // SessionMeta also holds `pid` and `boot`. Both are local bookkeeping and
-        // mean nothing on the client's machine; a pid on the wire invites a
-        // client to reason about a process it cannot see. Asserting on the
-        // serialized text on purpose -- this one IS about what goes on the wire.
+        // SessionMeta also holds `pid` and `boot`: local bookkeeping that means
+        // nothing on the client's machine. About the wire, so on the text.
         let line = serde_json::to_string(&Signal::Sessions {
-            sessions: vec![SessionSummary {
-                session_id: "f00d".repeat(8),
-                created_unix: 1,
-                shell: "/bin/sh".to_owned(),
-                size: TermSize { cols: 80, rows: 24 },
-                detachable: false,
-                attach_id: 0,
-            }],
+            list: vec![offer_entry()],
         })
         .expect("an offer encodes");
         assert!(!line.contains("\"pid\""), "pid must not travel: {line}");
         assert!(!line.contains("\"boot\""), "boot must not travel: {line}");
     }
 
-    // ---- features, and the three signals for a standby ----
+    // ---- features ----
 
     #[test]
     fn a_hello_without_features_parses_as_none() {
         // A host from before P1 sends no `features` key at all. Built by removing
````

`src/choose.rs`:

````diff
--- a/src/choose.rs
+++ b/src/choose.rs
@@ -205,234 +126,152 @@
 #[cfg(test)]
 mod tests {
     use super::*;
     use oxutrm_proto::TermSize;
 
-    fn summary(id: &str, detachable: bool) -> SessionSummary {
-        SessionSummary {
-            session_id: id.to_owned(),
-            created_unix: 1_757_200_000,
-            shell: "/bin/sh".to_owned(),
-            size: TermSize { cols: 80, rows: 24 },
+    const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
+    const LOGS: &str = "3ff1a0c95b7d4c2e8f6a1b0c9d8e7f60";
+    const OTHER: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
+
+    fn offer(id: &str, name: Option<&str>, detachable: bool) -> OfferEntry {
+        OfferEntry {
+            id: id.parse().unwrap(),
+            name: name.map(|n| Name::parse(n).unwrap()),
+            shell: "/bin/bash".to_owned(),
+            created_unix: 1_791_450_840,
+            size: TermSize {
+                cols: 120,
+                rows: 40,
+            },
             detachable,
-            attach_id: 1,
+        }
+    }
+
+    fn attach(id: &str) -> Decision {
+        Decision::Chosen(Choice::Attach {
+            id: id.parse().unwrap(),
+        })
+    }
+
+    fn refused(d: Decision) -> String {
+        match d {
+            Decision::Refused(why) => why,
+            other => panic!("expected a refusal, got {other:?}"),
         }
     }
 
     #[test]
     fn nothing_offered_means_a_new_session() {
-        assert_eq!(decide(&[], None, false), Decision::Chosen(Choice::New));
+        assert_eq!(
+            decide(&[], None, false, None),
+            Decision::Chosen(Choice::New { name: None })
+        );
     }
 
     #[test]
-    fn exactly_one_detachable_session_is_resumed_without_asking() {
-        // The case that sent a user to a new shell while their old one sat
-        // parked. Asking here would be worse than deciding: there is only
-        // one answer.
-        let offered = [summary(&"a".repeat(32), true)];
+    fn one_session_or_more_lands_in_the_lobby() {
+        let one = [offer(BUILD, Some("build"), true)];
+        assert_eq!(
+            decide(&one, None, false, None),
+            Decision::Chosen(Choice::Lobby)
+        );
+        let three = [
+            offer(BUILD, Some("build"), true),
+            offer(LOGS, Some("logs"), true),
+            offer(OTHER, None, false),
+        ];
+        assert_eq!(
+            decide(&three, None, false, None),
+            Decision::Chosen(Choice::Lobby)
+        );
+        // Even when the only session cannot be attached: the lobby lists it
+        // dimmed and offers a new one.
+        let tunnelled = [offer(OTHER, None, false)];
         assert_eq!(
-            decide(&offered, None, false),
-            Decision::Chosen(Choice::Attach { id: "a".repeat(32) })
+            decide(&tunnelled, None, false, None),
+            Decision::Chosen(Choice::Lobby)
         );
     }
 
     #[test]
-    fn exactly_one_session_that_cannot_be_attached_starts_a_new_one() {
-        // A rung-4 session dies with its ssh. Resuming it is not possible, so
-        // the useful thing is a new session, not an error.
-        let offered = [summary(&"a".repeat(32), false)];
-        assert_eq!(decide(&offered, None, false), Decision::Chosen(Choice::New));
+    fn new_wins_over_everything_and_carries_its_name() {
+        let offered = [offer(BUILD, Some("build"), true)];
+        let name = Name::parse("logs").unwrap();
+        assert_eq!(
+            decide(&offered, None, true, Some(name.clone())),
+            Decision::Chosen(Choice::New { name: Some(name) })
+        );
     }
 
     #[test]
-    fn several_sessions_ask() {
+    fn attach_takes_an_exact_name() {
         let offered = [
-            summary(&"a".repeat(32), true),
-            summary(&"b".repeat(32), true),
+            offer(BUILD, Some("build"), true),
+            offer(OTHER, Some("logs"), true),
         ];
-        assert_eq!(decide(&offered, None, false), Decision::Ask);
+        assert_eq!(decide(&offered, Some("logs"), false, None), attach(OTHER));
+        // A name is exact: a prefix of one is not it.
+        assert!(refused(decide(&offered, Some("buil"), false, None)).contains("buil"));
     }
 
     #[test]
-    fn new_wins_over_everything() {
-        let offered = [summary(&"a".repeat(32), true)];
-        assert_eq!(decide(&offered, None, true), Decision::Chosen(Choice::New));
+    fn attach_takes_an_id_prefix_in_either_case() {
+        let offered = [offer(BUILD, Some("build"), true), offer(OTHER, None, true)];
+        assert_eq!(decide(&offered, Some("a3f9"), false, None), attach(OTHER));
+        assert_eq!(decide(&offered, Some("A3F9C0"), false, None), attach(OTHER));
+        assert_eq!(decide(&offered, Some(BUILD), false, None), attach(BUILD));
     }
 
     #[test]
-    fn a_prefix_selects_one_session() {
-        let offered = [
-            summary(&"abcd".repeat(8), true),
-            summary(&"ef01".repeat(8), true),
-        ];
-        assert_eq!(
-            decide(&offered, Some("abcd"), false),
-            Decision::Chosen(Choice::Attach {
-                id: "abcd".repeat(8)
-            })
-        );
+    fn an_ambiguous_prefix_is_refused_listing_the_candidates() {
+        let offered = [offer(BUILD, None, true), offer(LOGS, None, true)];
+        let why = refused(decide(&offered, Some("3ff1"), false, None));
+        assert!(why.contains(BUILD) && why.contains(LOGS), "{why}");
+        // One more character settles it.
+        assert_eq!(decide(&offered, Some("3ff12"), false, None), attach(BUILD));
     }
 
     #[test]
-    fn an_ambiguous_prefix_is_refused_by_name() {
-        let offered = [summary("abcd1111", true), summary("abcd2222", true)];
-        match decide(&offered, Some("abcd"), false) {
-            Decision::Refused(why) => {
-                assert!(
-                    why.contains("abcd1111"),
-                    "the message must list the candidates: {why}"
-                );
-                assert!(
-                    why.contains("abcd2222"),
-                    "the message must list the candidates: {why}"
-                );
-            }
-            other => panic!("an ambiguous prefix must be refused, got {other:?}"),
-        }
+    fn a_name_or_prefix_that_matches_nothing_says_what_is_live() {
+        let offered = [offer(BUILD, Some("build"), true)];
+        let why = refused(decide(&offered, Some("9999"), false, None));
+        assert!(why.contains("9999"), "{why}");
+        assert!(why.contains("build") && why.contains(BUILD), "{why}");
+        let why = refused(decide(&[], Some("build"), false, None));
+        assert!(why.contains("no live sessions"), "{why}");
     }
 
     #[test]
-    fn a_prefix_that_matches_nothing_says_what_is_live() {
-        match decide(&[summary("abcd1111", true)], Some("9999"), false) {
-            Decision::Refused(why) => {
-                assert!(
-                    why.contains("9999"),
-                    "the message must name what was asked for: {why}"
-                );
-                assert!(why.contains("abcd1111"), "and what is actually live: {why}");
-            }
-            other => panic!("a missing id must be refused, got {other:?}"),
+    fn a_session_that_cannot_be_attached_is_refused_with_the_reason() {
+        let offered = [offer(OTHER, Some("tunnel"), false)];
+        for x in ["tunnel", "a3f9"] {
+            let why = refused(decide(&offered, Some(x), false, None));
+            assert!(why.contains("ssh"), "{x}: {why}");
         }
     }
 
     #[test]
-    fn a_named_session_that_cannot_be_attached_is_refused_with_the_reason() {
-        match decide(&[summary("abcd1111", false)], Some("abcd1111"), false) {
-            Decision::Refused(why) => assert!(
-                why.contains("ssh"),
-                "the reason must say it dies with its ssh, not merely 'no': {why}"
-            ),
-            other => panic!("expected a refusal, got {other:?}"),
-        }
+    fn a_short_string_that_is_no_name_is_refused_as_too_short() {
+        let offered = [offer(BUILD, Some("build"), true)];
+        let why = refused(decide(&offered, Some("3f"), false, None));
+        assert!(why.contains("four"), "{why}");
+        // Characters, not bytes: two characters, four bytes.
+        let why = refused(decide(&offered, Some("ää"), false, None));
+        assert!(why.contains("four"), "{why}");
     }
 
     #[test]
-    fn a_prefix_shorter_than_four_characters_is_refused() {
-        match decide(&[summary("abcd1111", true)], Some("ab"), false) {
-            Decision::Refused(why) => assert!(why.contains("four"), "{why}"),
-            other => panic!("expected a refusal, got {other:?}"),
-        }
+    fn a_short_name_is_still_a_name() {
+        let offered = [offer(BUILD, Some("x"), true)];
+        assert_eq!(decide(&offered, Some("x"), false, None), attach(BUILD));
     }
 
     /// `MIN_ATTACH_PREFIX_WORD` is prose, not derived from
-    /// `MIN_ATTACH_PREFIX` -- nothing in the language keeps them in step.
-    /// This is the guard: change the number without updating the word (or the
-    /// reverse), and this fails rather than the refusal quietly lying about
-    /// the rule it enforces.
+    /// `MIN_ATTACH_PREFIX`. Change one without the other and this fails.
     #[test]
     fn the_minimum_prefix_matches_its_spelled_out_word() {
         assert_eq!(
             MIN_ATTACH_PREFIX, 4,
             "MIN_ATTACH_PREFIX_WORD says \"four\" -- change both together"
         );
     }
-
-    /// A prefix is measured in characters, not bytes: `--attach ää` is two
-    /// characters and four bytes, and "at least four characters" has to mean
-    /// what it says even though session ids are always ASCII hex in practice.
-    #[test]
-    fn a_two_character_multibyte_prefix_is_still_refused_as_too_short() {
-        match decide(&[summary("abcd1111", true)], Some("ää"), false) {
-            Decision::Refused(why) => assert!(why.contains("four"), "{why}"),
-            other => panic!(
-                "\"ää\" is two characters (four bytes); a byte count would \
-                 wrongly accept it, got {other:?}"
-            ),
-        }
-    }
-
-    #[test]
-    fn the_picker_takes_a_number() {
-        let offered = [
-            summary(&"a".repeat(32), true),
-            summary(&"b".repeat(32), true),
-        ];
-        let mut input = std::io::Cursor::new(b"2\n".to_vec());
-        let mut out = Vec::new();
-        let chosen = pick(&offered, &mut input, &mut out).expect("the picker runs");
-        assert_eq!(chosen, Some(Choice::Attach { id: "b".repeat(32) }));
-        let shown = String::from_utf8(out).expect("the prompt is text");
-        assert!(
-            shown.contains(&"a".repeat(32)),
-            "the list must show the ids: {shown}"
-        );
-    }
-
-    #[test]
-    fn the_picker_takes_n_for_new_and_q_for_quit() {
-        let offered = [summary(&"a".repeat(32), true)];
-        let mut new_input = std::io::Cursor::new(b"n\n".to_vec());
-        let mut out = Vec::new();
-        assert_eq!(
-            pick(&offered, &mut new_input, &mut out).expect("the picker runs"),
-            Some(Choice::New)
-        );
-        let mut quit_input = std::io::Cursor::new(b"q\n".to_vec());
-        let mut out = Vec::new();
-        assert_eq!(
-            pick(&offered, &mut quit_input, &mut out).expect("the picker runs"),
-            None
-        );
-    }
-
-    #[test]
-    fn the_picker_re_asks_after_nonsense_and_quits_at_eof() {
-        let offered = [summary(&"a".repeat(32), true)];
-        let mut input = std::io::Cursor::new(b"banana\n7\n1\n".to_vec());
-        let mut out = Vec::new();
-        assert_eq!(
-            pick(&offered, &mut input, &mut out).expect("the picker runs"),
-            Some(Choice::Attach { id: "a".repeat(32) })
-        );
-        // A picker that silently discarded "banana" and "7" without telling
-        // the user anything would still reach the same final answer, so the
-        // return value alone does not prove it re-asked rather than ignored.
-        let shown = String::from_utf8(out).expect("the prompt is text");
-        assert!(
-            shown.contains("not understood"),
-            "the picker must say something about the input it rejected: {shown}"
-        );
-        // EOF is a quit, not a hang and not a panic: a client whose stdin is
-        // closed must exit, and it must not pick something on the user's
-        // behalf.
-        let mut empty = std::io::Cursor::new(Vec::new());
-        let mut out = Vec::new();
-        assert_eq!(
-            pick(&offered, &mut empty, &mut out).expect("the picker runs"),
-            None
-        );
-    }
-
-    /// A session the list already marked "NOT detachable" must stay refused
-    /// even when picked by number, with the same reason `decide_attach` gives
-    /// for the identical fact -- not silently forwarded to the host to fail
-    /// on its own.
-    #[test]
-    fn the_picker_refuses_a_session_that_cannot_be_attached_and_re_asks() {
-        let offered = [
-            summary(&"a".repeat(32), false),
-            summary(&"b".repeat(32), true),
-        ];
-        let mut input = std::io::Cursor::new(b"1\n2\n".to_vec());
-        let mut out = Vec::new();
-        assert_eq!(
-            pick(&offered, &mut input, &mut out).expect("the picker runs"),
-            Some(Choice::Attach { id: "b".repeat(32) })
-        );
-        let shown = String::from_utf8(out).expect("the prompt is text");
-        assert!(
-            shown.contains("not detachable") && shown.contains("ssh"),
-            "the picker must explain the refusal, not merely skip to the next \
-             answer: {shown}"
-        );
-    }
 }
````

`src/connect.rs`:

````diff
--- a/src/connect.rs
+++ b/src/connect.rs
@@ -711,23 +716,26 @@ mod tests {
         let (host_read, mut host_write) = tokio::io::split(host_side);
         let (client_read, mut client_write) = tokio::io::split(client_side);
         let mut client_read = tokio::io::BufReader::new(client_read);
 
-        let offered = vec![SessionSummary {
-            session_id: "a".repeat(32),
+        let offered = vec![OfferEntry {
+            id: "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().unwrap(),
+            name: Some(Name::parse("build").unwrap()),
+            shell: "/bin/bash".to_owned(),
             created_unix: 1_757_200_000,
-            shell: "/bin/sh".to_owned(),
-            size: TermSize { cols: 80, rows: 24 },
+            size: TermSize {
+                cols: 120,
+                rows: 40,
+            },
             detachable: true,
-            attach_id: 1,
         }];
 
         // Stands in for `oxutrm host --connect`: sends the offer first, then
         // waits for the client's answer.
         let host = tokio::spawn({
             let offered = offered.clone();
             async move {
-                write_signal_async(&mut host_write, &Signal::Sessions { sessions: offered })
+                write_signal_async(&mut host_write, &Signal::Sessions { list: offered })
                     .await
                     .expect("writing the offer");
                 match read_signal_async(&mut tokio::io::BufReader::new(host_read))
                     .await
@@ -750,13 +758,11 @@ mod tests {
             let signal = read_signal_async(&mut client_read)
                 .await
                 .expect("reading the offer");
             let sessions = sessions_offered(signal).expect("the offer parses");
-            let decision = decide(&sessions, None, false);
+            let decision = decide(&sessions, Some("build"), false, None);
             let Decision::Chosen(choice) = decision else {
-                panic!(
-                    "with exactly one detachable session and no flags, decide must choose: {decision:?}"
-                );
+                panic!("--attach build must choose the session named build: {decision:?}");
             };
             write_signal_async(
                 &mut client_write,
                 &Signal::Choose {
@@ -776,10 +782,12 @@ mod tests {
             "the host must see exactly the choice the client's own decision made"
         );
         assert_eq!(
             choice,
-            Choice::Attach { id: "a".repeat(32) },
-            "the one live, detachable session must be resumed without asking"
+            Choice::Attach {
+                id: "3ff1218f5e0c4b7d9a1c2e3f40516273".parse().unwrap()
+            },
+            "--attach build must reach the session named build"
         );
     }
 
     /// The composition the reattach path has never had a test for.
@@ -896,31 +904,60 @@ mod tests {
             nat_type: NatType::Unknown,
             rtt_ms: 38,
             mtu: 1400,
         };
+        let new = Choice::New { name: None };
         assert_eq!(
-            splash_caption(&Choice::Attach { id: id.clone() }, &id, &path, 0),
+            splash_caption(
+                &Choice::Attach {
+                    id: id.parse().unwrap()
+                },
+                &id,
+                &path,
+                0
+            ),
             "resumed session 3ff1218f \u{b7} IPv4 punched"
         );
         assert_eq!(
-            splash_caption(&Choice::New, &id, &path, 0),
+            splash_caption(&new, &id, &path, 0),
             "new session 3ff1218f \u{b7} IPv4 punched"
         );
         assert_eq!(
-            splash_caption(&Choice::New, &id, &path, 1),
+            splash_caption(
+                &Choice::New {
+                    name: Some(Name::parse("build").unwrap())
+                },
+                &id,
+                &path,
+                0
+            ),
+            "new session build \u{b7} IPv4 punched"
+        );
+        assert_eq!(
+            splash_caption(&Choice::Lobby, &id, &path, 0),
+            "choose a session \u{b7} IPv4 punched"
+        );
+        assert_eq!(
+            splash_caption(&new, &id, &path, 1),
             "new session 3ff1218f \u{b7} IPv4 punched \u{b7} config: 1 warning"
         );
         assert_eq!(
-            splash_caption(&Choice::New, &id, &path, 3),
+            splash_caption(&new, &id, &path, 3),
             "new session 3ff1218f \u{b7} IPv4 punched \u{b7} config: 3 warnings"
         );
     }
 
     #[test]
     fn the_opening_line_says_whether_the_session_was_already_running() {
-        let id = "a".repeat(32);
-        let resumed = opening_line(&Choice::Attach { id: id.clone() }, &id);
-        let fresh = opening_line(&Choice::New, &id);
+        let id = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60".to_string();
+        let resumed = opening_line(
+            &Choice::Attach {
+                id: id.parse().unwrap(),
+            },
+            &id,
+            1,
+        );
+        let fresh = opening_line(&Choice::New { name: None }, &id, 1);
 
         // The one assertion the pre-fix line cannot satisfy: it produced
         // exactly the same string for both.
         assert_ne!(
@@ -939,8 +976,25 @@ mod tests {
             "the session id must survive in both: {resumed:?} / {fresh:?}"
         );
     }
 
+    #[test]
+    fn a_named_new_session_and_the_lobby_say_so() {
+        let id = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
+        let named = opening_line(
+            &Choice::New {
+                name: Some(Name::parse("build").unwrap()),
+            },
+            id,
+            0,
+        );
+        assert!(named.contains("build") && named.contains(id), "{named}");
+        let one = opening_line(&Choice::Lobby, id, 1);
+        assert!(one.contains("1 session is"), "{one}");
+        let three = opening_line(&Choice::Lobby, id, 3);
+        assert!(three.contains("3 sessions are"), "{three}");
+    }
+
     fn a_host_hello() -> Signal {
         Signal::HostHello {
             proto: PROTO_VERSION,
             session_id: "f00d".to_owned(),
````

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -585,9 +620,9 @@ mod tests {
 
     #[test]
     fn help_is_the_default_and_names_every_subcommand() {
         for needle in [
-            "oxutrm [--attach <session-id>] [--new] <ssh-target>",
+            "oxutrm [--attach <name|session-id>] [--new [--name <name>]] <ssh-target>",
             "oxutrm host --serve",
             "oxutrm host --list",
             "oxutrm host --attach",
             "oxutrm loopback",
````

`src/rebuild.rs`:

````diff
--- a/src/rebuild.rs
+++ b/src/rebuild.rs
@@ -868,9 +869,9 @@ mod tests {
     fn fake_host_replying_failed(reason: &str) -> FakeHost {
         let dir = tempfile::tempdir().expect("a scratch directory");
         let body = format!(
             "#!/bin/sh\n\
-             printf '%s\\n' '{{\"t\":\"Sessions\",\"sessions\":[]}}'\n\
+             printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
              read -r choice\n\
              printf '%s\\n' '{{\"t\":\"Failed\",\"reason\":\"{reason}\"}}'\n"
         );
         FakeHost::new(dir, &body)
@@ -918,9 +919,9 @@ mod tests {
         let dir = tempfile::tempdir().expect("a scratch directory");
         let record = dir.path().join("choice.json");
         let body = format!(
             "#!/bin/sh\n\
-             printf '%s\\n' '{{\"t\":\"Sessions\",\"sessions\":[]}}'\n\
+             printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
              read -r choice\n\
              printf '%s' \"$choice\" > '{}'\n",
             record.display()
         );
@@ -967,9 +968,9 @@ mod tests {
         // retrying for ever, and an answer from the far end is not. Retrying a
         // Failed would spawn ssh every eight seconds until the user noticed.
         let outcome = attempt_against(
             fake_host_replying_failed("no such session on this host"),
-            "abc123",
+            "3ff1218f5e0c4b7d9a1c2e3f40516273",
         )
         .await;
         match outcome {
             AttemptOutcome::Definite(reason) => {
@@ -980,9 +981,14 @@ mod tests {
     }
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn an_ssh_that_dies_is_retried() {
-        match attempt_against(fake_host_that_exits_immediately(), "abc123").await {
+        match attempt_against(
+            fake_host_that_exits_immediately(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273",
+        )
+        .await
+        {
             // The reason, and not merely the variant: `Retry` is what
             // `classify` returns for everything that is not one of the two
             // definite cases, so a `classify` that had stopped reading its
             // argument at all would satisfy `Retry(_)`. What this has to show
@@ -999,9 +1005,14 @@ mod tests {
     /// asked again. Spec 2.3: both ends have to be upgraded together, and
     /// saying so beats an ssh every eight seconds for ever.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_remote_that_rejects_the_option_is_definite() {
-        match attempt_against(fake_host_rejecting_the_option(), "abc123").await {
+        match attempt_against(
+            fake_host_rejecting_the_option(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273",
+        )
+        .await
+        {
             AttemptOutcome::Definite(reason) => {
                 assert!(
                     reason.contains("bastion.example.net"),
                     "must name the host to upgrade: {reason}"
@@ -1034,9 +1045,9 @@ mod tests {
             attempt_within(
                 std::time::Duration::from_millis(200),
                 &fake.launcher,
                 "bastion.example.net",
-                "abc123",
+                "3ff1218f5e0c4b7d9a1c2e3f40516273",
                 a_size(),
                 &test_config(),
                 &Ties::default(),
             ),
@@ -1075,9 +1086,9 @@ mod tests {
         let dir = tempfile::tempdir().expect("a scratch directory");
         let record = dir.path().join("choice.json");
         let body = format!(
             "#!/bin/sh\n\
-             printf '%s\\n' '{{\"t\":\"Sessions\",\"sessions\":[]}}'\n\
+             printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
              read -r choice\n\
              printf '%s' \"$choice\" > '{}'\n\
              exec sleep 300\n",
             record.display()
@@ -1086,10 +1097,13 @@ mod tests {
     }
 
     /// A `Rebuild` that runs its attempts against `fake`, with one begun.
     fn rebuilding_against(fake: &FakeHost) -> Rebuild {
-        let mut rebuild = Rebuild::new("bastion.example.net".to_owned(), "abc123".to_owned())
-            .via(fake.launcher.clone(), test_config());
+        let mut rebuild = Rebuild::new(
+            "bastion.example.net".to_owned(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
+        )
+        .via(fake.launcher.clone(), test_config());
         let (tx, _rx) = tokio::sync::mpsc::channel(1);
         rebuild.begin(a_size(), tx, Instant::now());
         rebuild
     }
@@ -1164,9 +1178,9 @@ mod tests {
                 nat_type: oxutrm_proto::NatType::Unknown,
                 rtt_ms: 38,
                 mtu: 1400,
             },
-            session_id: "abc123".to_owned(),
+            session_id: "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
             attach_id: 2,
             host_features: vec![],
         };
         let report = Report {
@@ -1260,9 +1274,9 @@ mod tests {
     fn fake_host_that_will_not_read_the_choice() -> FakeHost {
         let dir = tempfile::tempdir().expect("a scratch directory");
         let body = "#!/bin/sh\n\
                     exec 0<&-\n\
-                    printf '%s\\n' '{\"t\":\"Sessions\",\"sessions\":[]}'\n\
+                    printf '%s\\n' '{\"t\":\"Sessions\",\"list\":[]}'\n\
                     exec sleep 300\n";
         FakeHost::new(dir, body)
     }
 
@@ -1278,10 +1292,13 @@ mod tests {
     /// returned on the failed write first and never claimed the line at all.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn an_attempt_commits_before_it_writes_its_attach() {
         let fake = fake_host_that_will_not_read_the_choice();
-        let mut rebuild = Rebuild::new("bastion.example.net".to_owned(), "abc123".to_owned())
-            .via(fake.launcher.clone(), test_config());
+        let mut rebuild = Rebuild::new(
+            "bastion.example.net".to_owned(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
+        )
+        .via(fake.launcher.clone(), test_config());
         let (tx, mut rx) = tokio::sync::mpsc::channel(1);
         rebuild.begin(a_size(), tx, Instant::now());
 
         let report = tokio::time::timeout(std::time::Duration::from_secs(10), rx.recv())
@@ -1319,9 +1336,9 @@ mod tests {
     /// The arguments the rebuild's ssh was started with, given what `ssh -G`
     /// said about its configuration.
     async fn rebuild_arguments(config: &str) -> Vec<String> {
         let fake = fake_ssh_recording_its_arguments(config);
-        let _ = run_attempt(&fake, "abc123").await;
+        let _ = run_attempt(&fake, "3ff1218f5e0c4b7d9a1c2e3f40516273").await;
         std::fs::read_to_string(fake.path("args"))
             .expect("the fake ssh recorded no arguments")
             .lines()
             .map(str::to_owned)
@@ -1353,10 +1370,13 @@ mod tests {
     /// network settings with it.
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_retuned_rebuild_runs_its_next_attempt_with_the_new_settings() {
         let fake = fake_ssh_recording_its_arguments("connecttimeout none");
-        let mut rebuild = Rebuild::new("bastion.example.net".to_owned(), "abc123".to_owned())
-            .via(fake.launcher.clone(), test_config());
+        let mut rebuild = Rebuild::new(
+            "bastion.example.net".to_owned(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
+        )
+        .via(fake.launcher.clone(), test_config());
         let cfg = oxutrm_net::NetConfig {
             enable_birthday: false,
             ..test_config()
         };
@@ -1407,10 +1427,13 @@ mod tests {
     /// Two attempts of one rebuild, run to their end against `fake`: how
     /// often each was asked `ssh -G`, and whether each carried the
     /// connect timeout.
     async fn two_attempts(fake: &FakeHost) -> (usize, [bool; 2]) {
-        let mut rebuild = Rebuild::new("bastion.example.net".to_owned(), "abc123".to_owned())
-            .via(fake.launcher.clone(), test_config());
+        let mut rebuild = Rebuild::new(
+            "bastion.example.net".to_owned(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
+        )
+        .via(fake.launcher.clone(), test_config());
         let (tx, mut rx) = tokio::sync::mpsc::channel(1);
         let mut bounded = [false; 2];
         for b in &mut bounded {
             rebuild.begin(a_size(), tx.clone(), Instant::now());
@@ -1496,8 +1519,15 @@ mod tests {
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn an_attempt_asks_for_the_session_it_came_from() {
         // The side effect, not the return value: a rebuild that answered `New`
         // would silently start a second shell and look like it had worked.
-        let recorded = attempt_against_recording(fake_host_recording_the_choice(), "abc123").await;
-        assert_eq!(recorded, serde_json::json!({"c": "Attach", "id": "abc123"}));
+        let recorded = attempt_against_recording(
+            fake_host_recording_the_choice(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273",
+        )
+        .await;
+        assert_eq!(
+            recorded,
+            serde_json::json!({"c": "Attach", "id": "3ff1218f5e0c4b7d9a1c2e3f40516273"})
+        );
     }
 }
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -266,9 +272,16 @@ pub(crate) mod fixtures {
         begin: Begin,
         shell: Shell,
     ) -> Process {
         let (host, client) = crate::link::fixtures::link_pair().await;
-        let mut meta = crate::door::fixtures::meta(id, name);
+        // A session's name travels in its `Begin`, as `--new --name` sends it.
+        let begin = match begin {
+            Begin::Session { .. } => Begin::Session {
+                name: name.map(|n| oxutrm_proto::Name::parse(n).expect("a name")),
+            },
+            Begin::Lobby => Begin::Lobby,
+        };
+        let mut meta = crate::door::fixtures::meta(id, None);
         meta.size = SIZE;
         let task = tokio::spawn(run_process(
             host,
             SIZE,
@@ -434,9 +447,16 @@ mod tests {
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn killing_this_session_leaves_a_lobby_that_can_start_again() {
         let dir = tempfile::tempdir().unwrap();
-        let p = process(dir.path(), BUILD, Some("build"), Begin::Session, sh()).await;
+        let p = process(
+            dir.path(),
+            BUILD,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
         registry_holds(dir.path(), &[BUILD]).await;
 
         done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
         // Done only after the entry went: no waiting here.
@@ -477,10 +497,24 @@ mod tests {
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn killing_a_sibling_ends_it_as_a_shell_that_exited() {
         let dir = tempfile::tempdir().unwrap();
-        let me = process(dir.path(), BUILD, Some("build"), Begin::Session, sh()).await;
-        let sibling = process(dir.path(), LOGS, Some("logs"), Begin::Session, sh()).await;
+        let me = process(
+            dir.path(),
+            BUILD,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        let sibling = process(
+            dir.path(),
+            LOGS,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
         registry_holds(dir.path(), &[BUILD, LOGS]).await;
 
         done(ask_over(me.client.sink.connection(), Request::Kill { id: id(LOGS) }).await);
         let live = oxutrm_host::Registry::list_in(dir.path()).unwrap();
@@ -528,9 +562,9 @@ mod tests {
         let p = process(
             &registry,
             BUILD,
             None,
-            Begin::Session,
+            Begin::Session { name: None },
             Shell {
                 program: script.to_str().unwrap().to_string(),
                 start: oxutrm_term::Start::default(),
             },
@@ -555,10 +589,17 @@ mod tests {
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn rename_here_and_on_a_sibling_and_a_taken_name_is_refused() {
         let dir = tempfile::tempdir().unwrap();
-        let me = process(dir.path(), BUILD, None, Begin::Session, sh()).await;
-        let _sibling = process(dir.path(), LOGS, Some("logs"), Begin::Session, sh()).await;
+        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
+        let _sibling = process(
+            dir.path(),
+            LOGS,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
         registry_holds(dir.path(), &[BUILD, LOGS]).await;
         let conn = me.client.sink.connection();
 
         let e = entry(
@@ -618,9 +659,9 @@ mod tests {
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_request_for_a_session_that_is_not_there_is_refused_with_its_short_id() {
         let dir = tempfile::tempdir().unwrap();
-        let me = process(dir.path(), BUILD, None, Begin::Session, sh()).await;
+        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
         registry_holds(dir.path(), &[BUILD]).await;
         let why =
             refused(ask_over(me.client.sink.connection(), Request::Kill { id: id(LOGS) }).await);
         assert!(why.contains("a3f9c01e"), "{why}");
@@ -628,9 +669,9 @@ mod tests {
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_name_that_is_all_hex_never_reaches_the_session() {
         let dir = tempfile::tempdir().unwrap();
-        let me = process(dir.path(), BUILD, None, Begin::Session, sh()).await;
+        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
         registry_holds(dir.path(), &[BUILD]).await;
         // Written by hand: `Name` cannot hold it, so the line is malformed and
         // the door answers nothing.
         let (mut send, recv) = me.client.sink.connection().open_bi().await.unwrap();
@@ -672,9 +713,9 @@ mod tests {
         let p = process(
             registry_dir.path(),
             BUILD,
             None,
-            Begin::Session,
+            Begin::Session { name: None },
             Shell {
                 program: script.to_str().unwrap().to_string(),
                 start: oxutrm_term::Start::login_in(home_dir(Some(home.clone().into_os_string()))),
             },
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -6368,9 +6368,9 @@ mod tests {
         std::fs::write(
             &script,
             format!(
                 "#!/bin/sh\n{ASKED_SSH_G}\
-                 printf '%s\\n' '{{\"t\":\"Sessions\",\"sessions\":[]}}'\n\
+                 printf '%s\\n' '{{\"t\":\"Sessions\",\"list\":[]}}'\n\
                  read -r choice\n\
                  exec sleep 300\n"
             ),
         )
````

`tests/client_flags.rs`:

````diff
--- a/tests/client_flags.rs
+++ b/tests/client_flags.rs
@@ -1,5 +1,5 @@
-//! `oxutrm [--attach <id>] [--new] <target>`: the flags, through the real
+//! `oxutrm [--attach <name|id>] [--new [--name <name>]] <target>`: the flags, through the real
 //! binary.
 //!
 //! `run_connect` (`src/connect.rs`) parses these before ever touching ssh, so
 //! a usage mistake here must be reported and exited before any network
@@ -61,9 +61,9 @@ fn attach_without_an_id_exits_two() {
         "a missing --attach argument must exit 2"
     );
     let stderr = String::from_utf8_lossy(&output.stderr);
     assert!(
-        stderr.contains("needs a session id"),
+        stderr.contains("needs a session's name or id"),
         "the error does not say what was actually wrong -- an \"unknown \
          option\" message from dispatch's old catch-all would also contain \
          \"--attach\" without ever reaching run_connect's own parser: {stderr}"
     );
@@ -128,9 +128,9 @@ fn attach_followed_by_a_flag_is_refused_before_ssh() {
         "a flag where the id should be must exit 2 before ssh, not run off \
          and try to connect: {stderr}"
     );
     assert!(
-        stderr.contains("needs a session id"),
+        stderr.contains("needs a session's name or id"),
         "the error must say what was actually wrong rather than reporting a \
          session the user never asked for: {stderr}"
     );
 }
@@ -154,9 +154,43 @@ fn help_mentions_the_attach_flag() {
 
     assert!(output.status.success(), "--help must exit successfully");
     let stdout = String::from_utf8_lossy(&output.stdout);
     assert!(
-        stdout.contains("[--attach <session-id>]"),
+        stdout.contains("[--attach <name|session-id>]"),
         "the help text does not mention the client's --attach flag (as \
          opposed to the pre-existing, unrelated `host --attach`): {stdout}"
     );
 }
+
+/// Run oxutrm with `args`, which must be refused before ssh: its exit
+/// status and stderr.
+fn refused_before_ssh(args: &[&str]) -> (Option<i32>, String) {
+    let output = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
+        .args(args)
+        .stdin(Stdio::null())
+        .output()
+        .expect("running oxutrm");
+    (
+        output.status.code(),
+        String::from_utf8_lossy(&output.stderr).into_owned(),
+    )
+}
+
+/// `--name` names a NEW session; on its own it would be a name for nothing.
+#[test]
+fn name_without_new_is_refused() {
+    let (code, stderr) = refused_before_ssh(&["--name", "build", "no-such-host.invalid"]);
+    assert_eq!(code, Some(2), "{stderr}");
+    assert!(
+        stderr.contains("--name") && stderr.contains("--new"),
+        "{stderr}"
+    );
+}
+
+/// A name that breaks a rule is a usage mistake, reported with the rule,
+/// before ssh: an all-hex name would read as a session id.
+#[test]
+fn a_name_that_could_be_an_id_is_refused_with_the_rule() {
+    let (code, stderr) = refused_before_ssh(&["--new", "--name", "cafe", "no-such-host.invalid"]);
+    assert_eq!(code, Some(2), "{stderr}");
+    assert!(stderr.contains("session id"), "{stderr}");
+}
````

`tests/host_connect.rs`:

````diff
--- a/tests/host_connect.rs
+++ b/tests/host_connect.rs
@@ -28,12 +28,9 @@ fn connect_offers_an_empty_list_when_nothing_is_running() {
         offer["t"], "Sessions",
         "the FIRST line must be the offer: {line}"
     );
     assert_eq!(
-        offer["sessions"]
-            .as_array()
-            .expect("a sessions array")
-            .len(),
+        offer["list"].as_array().expect("a list array").len(),
         0,
         "an empty registry offers nothing: {line}"
     );
 
@@ -106,4 +103,70 @@ fn attaching_to_a_session_that_is_not_there_fails_definitively() {
 
     let _ = child.kill();
     let _ = child.wait();
 }
+
+/// Spawn `oxutrm host --connect` on `dir`, read its offer, answer `choice`
+/// and return the next line it writes.
+fn answer_offer(dir: &std::path::Path, choice: serde_json::Value) -> serde_json::Value {
+    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
+        .args(["host", "--connect"])
+        .env("OXUTRM_STATE_DIR", dir)
+        .stdin(Stdio::piped())
+        .stdout(Stdio::piped())
+        .stderr(Stdio::piped())
+        .spawn()
+        .expect("spawning the remote half");
+    let mut out = BufReader::new(child.stdout.take().expect("stdout"));
+    let mut line = String::new();
+    out.read_line(&mut line).expect("reading the offer");
+    let mut stdin = child.stdin.take().expect("stdin");
+    writeln!(
+        stdin,
+        "{}",
+        serde_json::json!({"t": "Choose", "choice": choice})
+    )
+    .expect("sending the choice");
+    stdin.flush().expect("flushing the choice");
+    let mut next = String::new();
+    out.read_line(&mut next).expect("reading the answer");
+    let _ = child.kill();
+    let _ = child.wait();
+    serde_json::from_str(&next).expect("the answer is JSON")
+}
+
+#[test]
+fn a_lobby_choice_reaches_the_serve_path() {
+    let dir = tempfile::tempdir().expect("a scratch directory");
+    let hello = answer_offer(dir.path(), serde_json::json!({"c": "Lobby"}));
+    assert_eq!(hello["t"], "HostHello", "{hello}");
+}
+
+#[test]
+fn a_new_session_with_a_taken_name_is_refused_before_the_hello() {
+    let dir = tempfile::tempdir().expect("a scratch directory");
+    // A live session named "build": this test's own pid keeps it live.
+    let id = "3ff1218f5e0c4b7d9a1c2e3f40516273";
+    let entry = dir.path().join("oxutrm").join(id);
+    std::fs::create_dir_all(&entry).expect("an entry directory");
+    let now = std::time::SystemTime::now()
+        .duration_since(std::time::UNIX_EPOCH)
+        .unwrap()
+        .as_secs();
+    std::fs::write(
+        entry.join("meta.json"),
+        serde_json::json!({
+            "session_id": id, "attach_id": 1, "pid": std::process::id(),
+            "created_unix": now, "shell": "/bin/bash",
+            "size": {"cols": 120, "rows": 40}, "detachable": true, "name": "build"
+        })
+        .to_string(),
+    )
+    .expect("writing meta.json");
+
+    let answer = answer_offer(dir.path(), serde_json::json!({"c": "New", "name": "build"}));
+    assert_eq!(answer["t"], "Failed", "{answer}");
+    assert!(
+        answer["reason"].as_str().unwrap_or("").contains("taken"),
+        "{answer}"
+    );
+}
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --bin oxutrm --jobs 4 choose:: connect:: && cargo test -p oxutrm-proto --jobs 4`

Expected: compile errors: `Choice::Lobby` and `OfferEntry` in `Signal::Sessions` do not exist, `decide` takes three arguments.

- [ ] **Step 3: Implement**

`crates/oxutrm-host/src/attach.rs`:

````diff
--- a/crates/oxutrm-host/src/attach.rs
+++ b/crates/oxutrm-host/src/attach.rs
@@ -8,9 +8,9 @@
 //! only works on reattach.
 
 use std::path::{Path, PathBuf};
 
-use oxutrm_proto::{ProtoError, SessionSummary};
+use oxutrm_proto::ProtoError;
 use tokio::io::{AsyncBufReadExt, AsyncWrite};
 use tokio::net::UnixStream;
 
 use crate::registry::{Registry, SessionMeta, check_socket_path_length};
@@ -164,60 +164,4 @@ pub fn format_session_list(sessions: &[SessionMeta]) -> String {
         ));
     }
     out
 }
-
-/// The registry's entries, as a client is allowed to see them.
-///
-/// The filtering is the caller's: `Registry::list_in` has already dropped
-/// stale entries, and a NOT-detachable session is offered rather than hidden
-/// (see [`SessionSummary::detachable`]).
-#[must_use]
-pub fn summarize(sessions: &[SessionMeta]) -> Vec<SessionSummary> {
-    sessions
-        .iter()
-        .map(|m| SessionSummary {
-            session_id: m.session_id.clone(),
-            created_unix: m.created_unix,
-            shell: m.shell.clone(),
-            size: m.size,
-            detachable: m.detachable,
-            attach_id: m.attach_id,
-        })
-        .collect()
-}
-
-#[cfg(test)]
-mod tests {
-    use super::*;
-    use oxutrm_proto::TermSize;
-
-    #[test]
-    fn a_summary_keeps_the_id_and_drops_the_pid() {
-        let meta = SessionMeta {
-            session_id: "a".repeat(32),
-            attach_id: 2,
-            pid: 4242,
-            created_unix: 1_757_200_000,
-            shell: "/bin/zsh".to_owned(),
-            size: TermSize {
-                cols: 100,
-                rows: 30,
-            },
-            detachable: true,
-            boot: Some("boot-token".to_owned()),
-            name: None,
-        };
-        let summaries = summarize(std::slice::from_ref(&meta));
-        assert_eq!(summaries.len(), 1);
-        assert_eq!(summaries[0].session_id, meta.session_id);
-        assert_eq!(summaries[0].attach_id, 2);
-        assert_eq!(summaries[0].size.cols, 100);
-        // The side effect that matters: nothing local survives the crossing.
-        let encoded = serde_json::to_string(&summaries[0]).expect("a summary encodes");
-        assert!(!encoded.contains("4242"), "the pid escaped: {encoded}");
-        assert!(
-            !encoded.contains("boot-token"),
-            "the boot token escaped: {encoded}"
-        );
-    }
-}
````

`crates/oxutrm-proto/src/lib.rs`:

````diff
--- a/crates/oxutrm-proto/src/lib.rs
+++ b/crates/oxutrm-proto/src/lib.rs
@@ -112,9 +112,9 @@ pub use open::{
     Answer, Attached, OfferEntry, Open, Reply, Request, Role, SessionEntry, encode_line,
     parse_answer, parse_line,
 };
 pub use screen::{Cursor, CursorShape, Modes, MouseMode, ScreenState};
-pub use signal::{Choice, MAX_SIGNAL_LINE, SessionSummary, Signal, read_signal, write_signal};
+pub use signal::{Choice, MAX_SIGNAL_LINE, Signal, read_signal, write_signal};
 pub use stream::{ControlMsg, ScrollbackReq};
 pub use text::{check_cell_text, check_title, fit_cell_text, fit_title, is_control_scalar};
 pub use types::{
     Candidate, CandidateKind, MAX_CELL_TEXT, MAX_SCREEN_CELLS, MAX_SCREEN_DIM, MAX_TITLE, NatType,
````

`crates/oxutrm-proto/src/signal.rs`:

````diff
--- a/crates/oxutrm-proto/src/signal.rs
+++ b/crates/oxutrm-proto/src/signal.rs
@@ -6,10 +6,10 @@
 
 use serde::{Deserialize, Serialize};
 
 use crate::{
-    Candidate, ClientSpki, HostSpki, NatType, PROTO_VERSION, PathDescription, ProtoError, Psk,
-    TermSize, TerminalCaps,
+    Candidate, ClientSpki, HostSpki, Name, NatType, OfferEntry, PROTO_VERSION, PathDescription,
+    ProtoError, Psk, SessionId, TermSize, TerminalCaps,
 };
 
 /// The most bytes one signalling line may occupy, newline included.
 ///
@@ -35,39 +35,19 @@ use crate::{
 /// The limit covers the terminating newline, so a line of exactly this many
 /// bytes is the largest one that is still accepted.
 pub const MAX_SIGNAL_LINE: usize = 1024 * 1024;
 
-/// One live session, as offered to a client.
-///
-/// Deliberately NOT `SessionMeta`. That struct also carries `pid` and `boot`,
-/// which are this host's own bookkeeping: a pid means nothing on the client's
-/// machine, and a boot token even less. What a client needs is enough to
-/// choose between sessions and to be told when a choice cannot work.
-#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
-pub struct SessionSummary {
-    /// 32 lowercase hex characters, the same id `--list` and `--attach` use.
-    pub session_id: String,
-    pub created_unix: u64,
-    pub shell: String,
-    /// The size the session was last driven at, which is what a returning
-    /// client sees redrawn before its own size takes effect.
-    pub size: TermSize,
-    /// A rung-4 session tunnels QUIC through its ssh connection and dies with
-    /// it. It is offered anyway, and refused with a reason: a list that
-    /// silently omits a session the host's own `--list` shows is a list that
-    /// makes a user doubt the tool.
-    pub detachable: bool,
-    pub attach_id: u64,
-}
-
 /// What the client wants done, in answer to [`Signal::Sessions`].
 #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
 #[serde(tag = "c")]
 pub enum Choice {
     /// Relay me into this session.
-    Attach { id: String },
-    /// Start a fresh one.
-    New,
+    Attach { id: SessionId },
+    /// Start a fresh one, named `name` if given.
+    New { name: Option<Name> },
+    /// Start a lobby: no shell, no registry entry, the selector over a blank
+    /// screen (switcher spec §2.1).
+    Lobby,
 }
 
 #[derive(Clone, Debug, Serialize, Deserialize)]
 #[serde(tag = "t")]
@@ -124,9 +104,9 @@ pub enum Signal {
     /// The offer is what makes reattach reachable from an ordinary connect:
     /// the client learns what is already running and says which of it it
     /// wants. Empty is the ordinary first-connect case and is not an error.
     Sessions {
-        sessions: Vec<SessionSummary>,
+        list: Vec<OfferEntry>,
     },
     /// client -> host, the only answer to `Sessions`.
     ///
     /// Everything after this point is the exchange that `--serve` and
````

`src/choose.rs`:

````diff
--- a/src/choose.rs
+++ b/src/choose.rs
@@ -1,204 +1,125 @@
-//! Deciding what a bare `oxutrm <target>` should do about a live session, and
-//! asking when the decision is not the client's to make alone.
+//! Deciding what `oxutrm <target>` does about the sessions a host offers.
 //!
-//! [`decide`] is the whole spec's table as one pure function: no I/O, so
-//! every row is a test with nothing to fake. [`pick`] is the one path that
-//! needs a human, and it is line I/O so it can run before raw mode -- like
-//! everything else that may need the terminal (`ssh`'s own prompts, a
-//! `Decision::Refused` message) does.
+//! [`decide`] is the switcher spec's §3.1 table as one pure function: no
+//! I/O, so every row is a test with nothing to fake. There is no case left
+//! that asks on the line: with any session running, a bare connect lands in
+//! a lobby, and the selector asks there (switcher spec §1.1).
 
-use std::io::{BufRead, Write};
+use oxutrm_proto::{Choice, Name, OfferEntry};
 
-use oxutrm_proto::{Choice, SessionSummary};
-
-/// The shortest prefix `--attach` accepts, in characters.
+/// The shortest id prefix `--attach` accepts, in characters.
 ///
 /// A session id is 32 hex characters; nothing shorter than this is a prefix
 /// anyone would type by accident, and it keeps `--attach a` from matching
 /// half the sessions on a busy host.
 const MIN_ATTACH_PREFIX: usize = 4;
 
-/// `MIN_ATTACH_PREFIX`, spelled out for `decide_attach`'s refusal message: a
-/// digit interpolated into that sentence reads worse than the word.
+/// `MIN_ATTACH_PREFIX`, spelled out for the refusal message: a digit
+/// interpolated into that sentence reads worse than the word.
 ///
-/// This is its own constant rather than a comment's promise to remember,
-/// because a promise like that has already drifted false on this branch once.
-/// `the_minimum_prefix_matches_its_spelled_out_word` is the guard: change
-/// `MIN_ATTACH_PREFIX` without updating this, and that test fails rather than
-/// the message quietly lying about the rule.
+/// Its own constant rather than a comment's promise to remember.
+/// `the_minimum_prefix_matches_its_spelled_out_word` is the guard.
 const MIN_ATTACH_PREFIX_WORD: &str = "four";
 
-/// What a bare connect (or `--attach` / `--new`) resolves to.
+/// What a connect resolves to.
 #[derive(Debug, PartialEq, Eq)]
 pub(crate) enum Decision {
     /// The choice is made; send it and move on.
     Chosen(Choice),
-    /// More than one session is live and nothing else settled it: leave the
-    /// terminal alone and ask.
-    Ask,
     /// Nothing to send. The reason is for the user, printed as-is before raw
     /// mode and before this process exits.
     Refused(String),
 }
 
-/// The spec's table, in the order it gives it: `--new` first, then
-/// `--attach`, then the no-flags cases.
-pub(crate) fn decide(offered: &[SessionSummary], attach: Option<&str>, new: bool) -> Decision {
+/// The table, in the order the spec gives it: `--new` first, then
+/// `--attach`, then the no-flags cases. `name` is `--name`, which `connect`
+/// accepts only with `--new`.
+pub(crate) fn decide(
+    offered: &[OfferEntry],
+    attach: Option<&str>,
+    new: bool,
+    name: Option<Name>,
+) -> Decision {
     if new {
-        return Decision::Chosen(Choice::New);
+        return Decision::Chosen(Choice::New { name });
     }
-
-    if let Some(prefix) = attach {
-        return decide_attach(offered, prefix);
+    if let Some(x) = attach {
+        return decide_attach(offered, x);
     }
-
-    match offered {
-        [] => Decision::Chosen(Choice::New),
-        [one] if one.detachable => Decision::Chosen(Choice::Attach {
-            id: one.session_id.clone(),
-        }),
-        // A rung-4 session dies with its ssh, so resuming it is not an
-        // option -- but that is not a reason to refuse the connect.
-        [_one] => Decision::Chosen(Choice::New),
-        _ => Decision::Ask,
+    if offered.is_empty() {
+        Decision::Chosen(Choice::New { name: None })
+    } else {
+        // Even exactly one: nothing is resumed silently (spec §1.1).
+        Decision::Chosen(Choice::Lobby)
     }
 }
 
-/// `--attach <prefix>`: exactly one candidate, or a refusal that says why.
-fn decide_attach(offered: &[SessionSummary], prefix: &str) -> Decision {
-    // Characters, not bytes: a session id is ASCII hex, so this only ever
-    // affects garbage input, but `prefix.len()` would count `--attach ää` as
-    // four when the user typed two characters, not what "at least four
-    // characters" means.
-    if prefix.chars().count() < MIN_ATTACH_PREFIX {
+/// `--attach <x>`: the one session named `x`, or the one whose id `x`
+/// prefixes. A name always has a character outside `[0-9a-f]` (spec §2.3),
+/// so `x` cannot be both, and no precedence is needed.
+fn decide_attach(offered: &[OfferEntry], x: &str) -> Decision {
+    if let Some(named) = offered
+        .iter()
+        .find(|e| e.name.as_ref().is_some_and(|n| n.as_str() == x))
+    {
+        return attach_to(named);
+    }
+    // Characters, not bytes: `--attach ää` is two characters.
+    if x.chars().count() < MIN_ATTACH_PREFIX {
         return Decision::Refused(format!(
-            "--attach needs at least {MIN_ATTACH_PREFIX_WORD} characters of a session id, got {prefix:?}"
+            "no session is named {x:?}, and an id needs at least \
+             {MIN_ATTACH_PREFIX_WORD} characters. {}",
+            describe_live(offered)
         ));
     }
-
-    let matches: Vec<&SessionSummary> = offered
-        .iter()
-        .filter(|s| s.session_id.starts_with(prefix))
-        .collect();
-
+    let matches: Vec<&OfferEntry> = offered.iter().filter(|e| e.id.starts_with(x)).collect();
     match matches.as_slice() {
         [] => Decision::Refused(format!(
-            "no live session starts with {prefix:?}. {}",
+            "no session is named {x:?} or has an id starting with it. {}",
             describe_live(offered)
         )),
-        [one] if one.detachable => Decision::Chosen(Choice::Attach {
-            id: one.session_id.clone(),
-        }),
-        [one] => Decision::Refused(not_detachable_reason(&one.session_id)),
+        [one] => attach_to(one),
         many => Decision::Refused(format!(
-            "{prefix:?} matches more than one live session: {}. Give more characters.",
+            "{x:?} begins more than one session's id: {}. Give more characters.",
             many.iter()
-                .map(|s| s.session_id.as_str())
+                .map(|e| e.id.to_string())
                 .collect::<Vec<_>>()
                 .join(", ")
         )),
     }
 }
 
-/// Why a specific session cannot be attached to.
-///
-/// Shared between `decide_attach`'s refusal and `pick`'s own rejection of the
-/// same case (a session offered as not-detachable, picked by number anyway),
-/// so the two ways of reaching the same fact cannot say different things
-/// about it.
-fn not_detachable_reason(session_id: &str) -> String {
-    format!(
-        "session {session_id} is not detachable: it tunnels its data through \
-         the ssh connection that created it, so it cannot outlive it and \
-         cannot be reattached. Start a new session."
-    )
+/// Attach to `e`, unless it cannot be attached.
+fn attach_to(e: &OfferEntry) -> Decision {
+    if e.detachable {
+        Decision::Chosen(Choice::Attach { id: e.id })
+    } else {
+        Decision::Refused(format!(
+            "session {} is not detachable: it tunnels its data through the ssh \
+             connection that created it, so it cannot outlive it and cannot be \
+             reattached. Start a new session.",
+            label(e)
+        ))
+    }
+}
+
+/// A session as a refusal names it: its name and id, or its id.
+fn label(e: &OfferEntry) -> String {
+    match &e.name {
+        Some(n) => format!("{n} ({})", e.id),
+        None => e.id.to_string(),
+    }
 }
 
 /// What IS live, for a refusal to point at.
-fn describe_live(offered: &[SessionSummary]) -> String {
+fn describe_live(offered: &[OfferEntry]) -> String {
     if offered.is_empty() {
         "There are no live sessions on this host.".to_owned()
     } else {
         format!(
             "Live sessions: {}.",
-            offered
-                .iter()
-                .map(|s| s.session_id.as_str())
-                .collect::<Vec<_>>()
-                .join(", ")
+            offered.iter().map(label).collect::<Vec<_>>().join(", ")
         )
     }
 }
 
-/// Ask, on an ordinary terminal, before raw mode: a numbered list built from
-/// the same fields `oxutrm_host::attach::format_session_list` uses (that
-/// function lives on the host side and works from `SessionMeta`; this one
-/// works from the client's own [`SessionSummary`], which is what is actually
-/// in hand here), then `[1-N] resume, n new session, q quit`.
-///
-/// Loops on anything it does not understand. `Ok(None)` for `q` and for EOF
-/// -- a client whose stdin is closed must exit rather than hang, and must not
-/// pick something on the user's behalf.
-pub(crate) fn pick(
-    offered: &[SessionSummary],
-    input: &mut impl BufRead,
-    out: &mut impl Write,
-) -> anyhow::Result<Option<Choice>> {
-    writeln!(out, "sessions live on this host:")?;
-    for (i, s) in offered.iter().enumerate() {
-        writeln!(
-            out,
-            "{:>2}) {}  {:>3}x{:<3}  attach {}  {}  {}",
-            i + 1,
-            s.session_id,
-            s.size.cols,
-            s.size.rows,
-            s.attach_id,
-            s.shell,
-            if s.detachable {
-                "detachable"
-            } else {
-                "NOT detachable (dies with its ssh)"
-            },
-        )?;
-    }
-    writeln!(out, "[1-{}] resume, n new session, q quit", offered.len())?;
-
-    loop {
-        write!(out, "> ")?;
-        out.flush()?;
-
-        let mut line = String::new();
-        if input.read_line(&mut line)? == 0 {
-            // EOF, not a hang and not a panic.
-            return Ok(None);
-        }
-
-        match line.trim() {
-            "q" => return Ok(None),
-            "n" => return Ok(Some(Choice::New)),
-            other => match other.parse::<usize>() {
-                Ok(n) if n >= 1 && n <= offered.len() => {
-                    let chosen = &offered[n - 1];
-                    if !chosen.detachable {
-                        // The list already labelled this one "NOT
-                        // detachable"; sending the choice anyway would just
-                        // hand the host a request it has to refuse on its
-                        // own. Same wording `decide_attach` refuses with for
-                        // the identical fact, and the same re-ask as any
-                        // other input that did not work out.
-                        writeln!(out, "{}", not_detachable_reason(&chosen.session_id))?;
-                        continue;
-                    }
-                    return Ok(Some(Choice::Attach {
-                        id: chosen.session_id.clone(),
-                    }));
-                }
-                _ => {
-                    writeln!(out, "not understood: {other:?}. Try again.")?;
-                }
-            },
-        }
-    }
-}
-
````

`src/connect.rs`:

````diff
--- a/src/connect.rs
+++ b/src/connect.rs
@@ -8,16 +8,16 @@ use oxutrm_client::{RawGuard, terminal_size};
 use oxutrm_host::signalling::{read_signal_async, write_signal_async};
 use oxutrm_host::ssh::{SshChannel, SshLauncher};
 use oxutrm_net::{IceRole, NetConfig};
 use oxutrm_proto::{
-    Candidate, Choice, ClientSpki, HostSpki, NatType, PROTO_VERSION, PathDescription, Psk,
-    SessionSummary, Signal, TermSize,
+    Candidate, Choice, ClientSpki, HostSpki, Name, NatType, OfferEntry, PROTO_VERSION,
+    PathDescription, Psk, Signal, TermSize,
 };
 use oxutrm_term::detect_caps;
 
 use crate::activity::{Activity, LogFile};
 use crate::candidates::{inbound_candidates, outbound_candidates};
-use crate::choose::{Decision, decide, pick};
+use crate::choose::{Decision, decide};
 use crate::ladder::nominate;
 use crate::link::Link;
 use crate::rebuild::Rebuild;
 use crate::session::ClientSession;
@@ -31,16 +31,18 @@ use crate::view::Identity;
 /// passphrase or a host-key confirmation, and raw mode would corrupt the
 /// prompt it asks with; [`RawGuard`] goes on at L11, after every prompt ssh
 /// could possibly have shown.
 pub fn run_connect(args: &[String]) -> Result<()> {
-    // `--attach <id>` and `--new` come before the target, in either order.
+    // `--attach <name|id>`, `--new` and `--name <name>` come before the
+    // target, in any order.
     //
     // Every usage mistake here exits 2, the same convention `dispatch`'s own
     // "unknown option" and `run_host`'s missing-subcommand cases use: a typo
     // in the invocation is not a connection failure, and it is reported and
     // exited before anything -- ssh included -- has been started.
     let mut attach: Option<String> = None;
     let mut new = false;
+    let mut name: Option<Name> = None;
     let mut rest = args;
     loop {
         match rest.first().map(String::as_str) {
             Some("--attach") => {
@@ -52,9 +54,11 @@ pub fn run_connect(args: &[String]) -> Result<()> {
                 // session. A flag where the id should be is a usage mistake and
                 // is reported like every other one here, before anything at all
                 // has been started.
                 let Some(id) = rest.get(1).filter(|id| !id.starts_with('-')) else {
-                    eprintln!("oxutrm: --attach needs a session id. Try `oxutrm --help`.");
+                    eprintln!(
+                        "oxutrm: --attach needs a session's name or id. Try `oxutrm --help`."
+                    );
                     std::process::exit(2);
                 };
                 attach = Some(id.clone());
                 rest = &rest[2..];
@@ -62,16 +66,36 @@ pub fn run_connect(args: &[String]) -> Result<()> {
             Some("--new") => {
                 new = true;
                 rest = &rest[1..];
             }
+            // Checked here, before ssh: a name that breaks a rule (switcher
+            // spec §2.3) is a usage mistake like any other.
+            Some("--name") => {
+                let Some(text) = rest.get(1).filter(|t| !t.starts_with('-')) else {
+                    eprintln!("oxutrm: --name needs a name. Try `oxutrm --help`.");
+                    std::process::exit(2);
+                };
+                match Name::parse(text) {
+                    Ok(n) => name = Some(n),
+                    Err(why) => {
+                        eprintln!("oxutrm: --name {text:?}: {why}.");
+                        std::process::exit(2);
+                    }
+                }
+                rest = &rest[2..];
+            }
             _ => break,
         }
     }
 
     if attach.is_some() && new {
         eprintln!("oxutrm: --attach and --new cannot both be given. Try `oxutrm --help`.");
         std::process::exit(2);
     }
+    if name.is_some() && !new {
+        eprintln!("oxutrm: --name names a new session, so it needs --new. Try `oxutrm --help`.");
+        std::process::exit(2);
+    }
 
     let Some(target) = rest.first() else {
         eprintln!("oxutrm: needs an ssh target. Try `oxutrm --help`.");
         std::process::exit(2);
@@ -82,9 +106,9 @@ pub fn run_connect(args: &[String]) -> Result<()> {
     let runtime = tokio::runtime::Builder::new_multi_thread()
         .enable_all()
         .build()
         .context("building the runtime")?;
-    let outcome = runtime.block_on(connect(target, attach.as_deref(), new));
+    let outcome = runtime.block_on(connect(target, attach.as_deref(), new, name));
 
     // Same reasoning as `host --serve`: the ssh channel's reader may be parked
     // on a pipe the far end is in no hurry to close, and waiting for a read we
     // have stopped caring about is not a shutdown.
@@ -97,9 +121,9 @@ pub fn run_connect(args: &[String]) -> Result<()> {
     std::process::exit(code);
 }
 
 /// L3 to L13.
-async fn connect(target: &str, attach: Option<&str>, new: bool) -> Result<i32> {
+async fn connect(target: &str, attach: Option<&str>, new: bool, name: Option<Name>) -> Result<i32> {
     // The config file, for this target. Nothing is printed about it: a
     // line written here would be painted over within milliseconds. Its
     // warnings go to the activity log once the session has one.
     let config_dir = crate::config::config_dir(
@@ -123,45 +147,16 @@ async fn connect(target: &str, attach: Option<&str>, new: bool) -> Result<i32> {
     // child and turns an EOF into `RemoteBinaryMissing`, `SshFailed` or
     // `NoSignal`. `establish` reads a plain stream and cannot do that, which
     // is exactly why the offer is read here and not inside it.
     let offered = read_offer(&mut channel).await?;
-    let choice = match decide(&offered, attach, new) {
+    let choice = match decide(&offered, attach, new, name) {
         Decision::Chosen(choice) => choice,
         // Before raw mode: this is an ordinary line on an ordinary terminal,
         // the same as any other reason `connect` cannot go on.
         Decision::Refused(why) => {
             eprintln!("oxutrm: {why}");
             std::process::exit(2);
         }
-        // More than one session is live and nothing else settled it. The
-        // terminal is still ordinary here -- raw mode is L11, well below --
-        // so the picker is plain line I/O on stdin and stdout.
-        //
-        // Run on tokio's blocking pool, not on this async task: `pick` waits
-        // on a human with no bound on how long that takes, and `SshChannel`'s
-        // own stderr drainer (`oxutrm_host::ssh::SshChannel::open`) is a
-        // `tokio::spawn` task on this same multi-threaded runtime. On a
-        // single-worker or cgroup-limited host, a human sitting at the `>`
-        // prompt would monopolise the only worker, starving that drainer;
-        // once its pipe buffer fills, `ssh` itself blocks on the write.
-        // `spawn_blocking` keeps the wait off the async workers entirely.
-        Decision::Ask => {
-            let offered = offered.clone();
-            let picked = tokio::task::spawn_blocking(move || {
-                pick(
-                    &offered,
-                    &mut std::io::stdin().lock(),
-                    &mut std::io::stdout(),
-                )
-            })
-            .await
-            .context("running the picker")??;
-            match picked {
-                Some(choice) => choice,
-                // The user quit. Not an error: they were asked, and declined.
-                None => std::process::exit(0),
-            }
-        }
     };
     // Kept past the send, because the line printed below has to say which of
     // the two things happened and the `Choice` is the only place that knows:
     // `established.session_id` names whichever session resulted, but not
@@ -194,9 +189,12 @@ async fn connect(target: &str, attach: Option<&str>, new: bool) -> Result<i32> {
     //
     // The attach generation used to be here too and is not any more. It is
     // internal bookkeeping -- which generation of the sync counters this is --
     // and unlike the id there is nothing a user can do with it.
-    println!("{}", opening_line(&chosen, &established.session_id));
+    println!(
+        "{}",
+        opening_line(&chosen, &established.session_id, offered.len())
+    );
 
     // L11. Late, deliberately: after every prompt ssh could have shown, and
     // after the last thing that could have failed with a message worth
     // reading on an ordinary terminal.
@@ -294,14 +292,19 @@ fn splash_seed() -> u64 {
 /// reached from a test at all -- it wants a real ssh, a real far end and a
 /// real terminal -- and this line said the same words for both outcomes until
 /// somebody read it. The text is the whole behaviour, so the text is what is
 /// pinned.
-fn opening_line(chosen: &Choice, session_id: &str) -> String {
+fn opening_line(chosen: &Choice, session_id: &str, offered: usize) -> String {
     match chosen {
-        // Spec §4.2: the "exactly one, detachable" row resumes without asking,
-        // "with one line saying so". This is that line.
         Choice::Attach { .. } => format!("oxutrm: resumed session {session_id}."),
-        Choice::New => format!("oxutrm: new session {session_id}."),
+        Choice::New { name: Some(n) } => format!("oxutrm: new session {n} ({session_id})."),
+        Choice::New { name: None } => format!("oxutrm: new session {session_id}."),
+        // Nothing is resumed silently (switcher spec §1.1): the selector
+        // opens instead, and this says why it is there.
+        Choice::Lobby => match offered {
+            1 => "oxutrm: 1 session is running here; choose it or start another.".to_string(),
+            n => format!("oxutrm: {n} sessions are running here; choose one or start another."),
+        },
     }
 }
 
 /// What the splash says under the name: the opening line's news -- which
@@ -323,12 +326,14 @@ fn splash_caption(
     warnings: usize,
 ) -> String {
     let id: String = session_id.chars().take(8).collect();
     let news = match chosen {
-        Choice::Attach { .. } => "resumed session",
-        Choice::New => "new session",
+        Choice::Attach { .. } => format!("resumed session {id}"),
+        Choice::New { name: Some(n) } => format!("new session {n}"),
+        Choice::New { name: None } => format!("new session {id}"),
+        Choice::Lobby => "choose a session".to_string(),
     };
-    let mut caption = format!("{news} {id} \u{b7} {}", oxutrm_client::rung_label(path));
+    let mut caption = format!("{news} \u{b7} {}", oxutrm_client::rung_label(path));
     match warnings {
         0 => {}
         1 => caption.push_str(" \u{b7} config: 1 warning"),
         n => caption.push_str(&format!(" \u{b7} config: {n} warnings")),
@@ -551,9 +556,9 @@ impl std::error::Error for HostRefused {}
 /// [`sessions_offered`] so that part -- and only that part -- can be driven
 /// without ssh in the test below. [`crate::rebuild::attempt`] reads its own
 /// fresh channel's offer through this same function, and for the same reason:
 /// the first read is where a missing binary or a dead ssh is diagnosed.
-pub(crate) async fn read_offer(channel: &mut SshChannel) -> Result<Vec<SessionSummary>> {
+pub(crate) async fn read_offer(channel: &mut SshChannel) -> Result<Vec<OfferEntry>> {
     sessions_offered(
         channel
             .recv()
             .await
@@ -561,11 +566,11 @@ pub(crate) async fn read_offer(channel: &mut SshChannel) -> Result<Vec<SessionSu
     )
 }
 
 /// What a `Signal` means as the host's offer, once it has been read.
-fn sessions_offered(signal: Signal) -> Result<Vec<SessionSummary>> {
+fn sessions_offered(signal: Signal) -> Result<Vec<OfferEntry>> {
     match signal {
-        Signal::Sessions { sessions } => Ok(sessions),
+        Signal::Sessions { list } => Ok(list),
         // The host's own words, exactly as at L5: it is the only explanation
         // there is for why this connection is not going to happen.
         Signal::Failed { reason } => Err(anyhow::Error::new(HostRefused(reason))),
         other => Err(anyhow::anyhow!(
````

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -70,12 +70,12 @@ fn dispatch(args: &[String]) -> Result<()> {
             Ok(())
         }
         Some("host") => run_host(&args[1..]),
         Some("loopback") => run_loopback(&args[1..]),
-        // `--attach` and `--new` are `run_connect`'s own flags, not unknown
+        // `--attach`, `--new` and `--name` are `run_connect`'s own flags, not unknown
         // options: they come before the target, so they must reach it rather
         // than be caught by the catch-all below.
-        Some("--attach") | Some("--new") => connect::run_connect(args),
+        Some("--attach") | Some("--new") | Some("--name") => connect::run_connect(args),
         Some(other) if other.starts_with('-') => {
             eprintln!("oxutrm: unknown option {other:?}\nTry `oxutrm --help`.");
             std::process::exit(2);
         }
@@ -93,9 +93,18 @@ fn run_host(args: &[String]) -> Result<()> {
         }
         // Works today: it needs the registry and nothing else.
         Some("--list") => run_host_list(),
         Some("--connect") => run_host_connect(),
-        Some("--serve") => serve::run_host_serve(serve::Begin::Session),
+        Some("--serve") => match args.get(1..).unwrap_or_default() {
+            [] => serve::run_host_serve(serve::Begin::Session { name: None }),
+            [flag, name] if flag == "--name" => match oxutrm_proto::Name::parse(name) {
+                Ok(name) => serve::run_host_serve(serve::Begin::Session { name: Some(name) }),
+                Err(why) => Err(anyhow::anyhow!("--name {name:?}: {why}")),
+            },
+            _ => Err(anyhow::anyhow!(
+                "`oxutrm host --serve` takes nothing but `--name <name>`. Try `oxutrm host --help`."
+            )),
+        },
         Some("--attach") => match args.get(1) {
             Some(id) => run_host_attach(id),
             None => Err(anyhow::anyhow!(
                 "`oxutrm host --attach` needs a session id. \
@@ -174,9 +183,12 @@ fn run_host_connect() -> Result<()> {
     let mut stdout = std::io::stdout();
     oxutrm_proto::write_signal(
         &mut stdout,
         &Signal::Sessions {
-            sessions: oxutrm_host::attach::summarize(&sessions),
+            list: sessions
+                .iter()
+                .filter_map(oxutrm_host::SessionMeta::offer)
+                .collect(),
         },
     )
     .context("offering the live sessions")?;
 
@@ -209,23 +221,43 @@ fn run_host_connect() -> Result<()> {
                 ));
             }
         };
 
+    // A refusal goes back on this channel, because the client is still
+    // listening on it: a `Failed` it can read is worth more than an exit code
+    // it has to guess at.
+    let refuse = |reason: String| {
+        let _ = oxutrm_proto::write_signal(
+            &mut std::io::stdout(),
+            &Signal::Failed {
+                reason: reason.clone(),
+            },
+        );
+        Err(anyhow::anyhow!(reason))
+    };
     match choice {
-        Choice::New => serve::run_host_serve(serve::Begin::Session),
+        Choice::New { name } => {
+            // A taken name is refused before the fork and the exchange, so
+            // the client hears why instead of reaching a session that then
+            // fails to register. `Door::register` checks again under the
+            // same lock, for the race this leaves.
+            if let Some(n) = &name {
+                let dir = oxutrm_host::Registry::dir_at(&root.base);
+                let _lock = oxutrm_host::NamesLock::take(&dir)?;
+                if let Some(why) =
+                    oxutrm_host::name_refusal(&dir, n.as_str(), "").context("checking the name")?
+                {
+                    return refuse(why);
+                }
+            }
+            serve::run_host_serve(serve::Begin::Session { name })
+        }
+        Choice::Lobby => serve::run_host_serve(serve::Begin::Lobby),
         Choice::Attach { id } => {
-            // Refused HERE rather than inside the relay, because the client is
-            // still listening on this channel: a `Failed` it can read is worth
-            // more than an exit code it has to guess at.
+            let id = id.to_string();
+            // Refused HERE rather than inside the relay.
             if !sessions.iter().any(|m| m.session_id == id) {
-                let reason = format!("no such session on this host: {id}");
-                let _ = oxutrm_proto::write_signal(
-                    &mut std::io::stdout(),
-                    &Signal::Failed {
-                        reason: reason.clone(),
-                    },
-                );
-                return Err(anyhow::anyhow!(reason));
+                return refuse(format!("no such session on this host: {id}"));
             }
             run_host_attach(&id)
         }
     }
@@ -477,8 +509,9 @@ USAGE
   oxutrm host --connect       The command the client spawns over ssh: offer
                               what is running, then serve or attach.
   oxutrm host --list          Sessions on this machine, oldest first.
   oxutrm host --serve         Create a session and hand it to a client.
+              [--name <name>] Name it.
   oxutrm host --attach <id>   Relay a new attach into a running session.
 
 --connect is not normally typed by hand either: it is what a client actually
 runs over ssh, offering its live sessions before committing to --serve or
@@ -512,14 +545,16 @@ const USAGE: &str = "\
 oxutrm — a remote terminal that survives bad networks, changing IP addresses
 and NAT on both ends.
 
 USAGE
-  oxutrm [--attach <session-id>] [--new] <ssh-target>
-      Connect to that host. If a session of yours is already running
-      there it is resumed; with several, oxutrm asks which. --attach
-      names one directly, --new always starts a fresh session.
-
-  oxutrm host --serve
+  oxutrm [--attach <name|session-id>] [--new [--name <name>]] <ssh-target>
+      Connect to that host. With no session of yours running there, a
+      new one starts. With any running -- even one -- the session
+      selector opens, to switch to one, start, rename or kill one.
+      --attach goes straight to a session by its name or by the start
+      of its id; --new always starts a fresh one, named with --name.
+
+  oxutrm host --serve [--name <name>]
       Run the remote half. Spawned over SSH; not normally typed by hand.
 
   oxutrm host --list
       List sessions on this machine, pruning any whose process is gone.
````

`src/rebuild.rs`:

````diff
--- a/src/rebuild.rs
+++ b/src/rebuild.rs
@@ -445,11 +445,12 @@ async fn one_attempt(
     // the abandon ended this attempt's generation (see `Report`).
     if !ties.commitment.commit() {
         return AttemptOutcome::Retry("abandoned for the standby".to_owned());
     }
-    let choice = Choice::Attach {
-        id: session_id.to_owned(),
+    let Ok(id) = session_id.parse() else {
+        return AttemptOutcome::Definite(format!("{session_id:?} is not a session id"));
     };
+    let choice = Choice::Attach { id };
     if let Err(e) = channel.send(&Signal::Choose { choice }).await {
         return classify(target, &anyhow::Error::new(e));
     }
 
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -154,13 +154,12 @@ async fn serve(
 /// What a session process starts as: a session with its shell, or a lobby
 /// (switcher spec §2.1).
 #[derive(Clone, Debug, PartialEq, Eq)]
 pub(crate) enum Begin {
-    Session,
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "`Choice::Lobby` starts one from Task 7 on")
-    )]
+    /// Registered as `name`, if given, with its shell started.
+    Session {
+        name: Option<oxutrm_proto::Name>,
+    },
     Lobby,
 }
 
 /// The shell a session runs, and how it starts it.
@@ -175,20 +174,27 @@ pub(crate) struct Shell {
 /// the same for every way one starts -- a first connect over ssh, a sibling
 /// over a socket pair, a lobby. A function of its own, apart from the fork
 /// and the sever, so the whole of it runs in a test.
 ///
-/// `Begin::Session` registers (under `meta.name`) and starts the shell;
+/// `Begin::Session` registers (under its name) and starts the shell;
 /// `Begin::Lobby` does neither until it is asked for `New`. Returns the
 /// shell's exit status, or `0` for a lobby that ended.
 pub(crate) async fn run_process(
     link: crate::link::Link,
     client_size: TermSize,
-    meta: SessionMeta,
+    mut meta: SessionMeta,
     registry: std::path::PathBuf,
     cfg: NetConfig,
     begin: Begin,
     shell: Shell,
 ) -> anyhow::Result<i32> {
+    let starts_shell = match begin {
+        Begin::Session { name } => {
+            meta.name = name.map(String::from);
+            true
+        }
+        Begin::Lobby => false,
+    };
     let presence = crate::host_session::Presence::default();
     let (cmds_tx, mut cmds) = tokio::sync::mpsc::channel(1);
     let (attached_tx, mut attaches) = tokio::sync::mpsc::channel(1);
     let first = link.sink.connection().clone();
@@ -204,9 +210,9 @@ pub(crate) async fn run_process(
     );
     let mut session =
         HostSession::lobby(&shell.program, &shell.start, client_size, SCROLLBACK, link)?
             .with_presence(presence);
-    if begin == Begin::Session {
+    if starts_shell {
         // R13: registered, and its socket bound where the rung allows one.
         door.register()
             .context("recording the session in the registry")?;
         // R14. `negotiate_term` takes no arguments: the child's TERM comes
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo build --bin oxutrm --jobs 4 && cargo test --bin oxutrm --jobs 4 choose:: connect:: serve:: rebuild:: && cargo test -p oxutrm-proto --jobs 4 && cargo test --test client_flags --test host_connect --jobs 4`

Expected: all pass, among them `one_session_or_more_lands_in_the_lobby`, `attach_takes_an_exact_name`, `attach_takes_an_id_prefix_in_either_case`, `an_ambiguous_prefix_is_refused_listing_the_candidates`, `name_without_new_is_refused`, `a_name_that_could_be_an_id_is_refused_with_the_rule`, `a_lobby_choice_reaches_the_serve_path` and `a_new_session_with_a_taken_name_is_refused_before_the_hello`.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add crates/oxutrm-host/src/attach.rs crates/oxutrm-proto/src/lib.rs crates/oxutrm-proto/src/signal.rs src/choose.rs src/connect.rs src/main.rs src/rebuild.rs src/serve.rs src/session.rs tests/client_flags.rs tests/host_connect.rs
git commit -m "feat(connect): the offer lists OfferEntries; --name, --attach <name>, and a lobby instead of the line picker

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 8: `Switch` relays to the target's attach; `New` in a session starts a sibling as `host --serve`

Spec §3.3, §3.4. Both through the client's door only; through the socket they are refused.

- **`Switch { to }`:** refused for this session, for one not live (its short id in the reason), for one not detachable. Otherwise the door connects to the target's socket, writes `Attach { Primary }`, and relays (`relay_attach`): the target's first line is read here, so a refusal or another protocol version reaches the client as a reason (`... runs another version of oxutrm`); a `HostHello` is passed on and everything after it is relayed both ways, decoded and re-encoded, bounded by `ATTACH_TIMEOUT`. The target runs its ordinary attach exchange; an `in use` target displaces its other client with `TAKEN_OVER`, as `--attach` does. Nothing here changes this session.
- **`New { name }` in a session:** the name is checked under the lock (refused at once if taken), then `spawn_sibling` runs `<exe> host --serve [--name <name>]` with its stdin and stdout on one end of a socket pair and `OXUTRM_STATE_DIR` set to the directory the registry is in, so the sibling registers beside its parent; the first child (which forks the session and exits) is waited for on a task. The stream is relayed like a switch. **The binary** is this session's own, resolved once at startup (`own_binary`: `/proc/self/exe` on Linux, else `current_exe()`) and passed in through `Shell::sibling` to `Door::process`. A sibling that loses a name race between the parent's check and its own registration fails after its exchange: the client sees the new link close (judgement call).

The client's side of this is `connect::establish_answering` in this task (the tests use it); Task 9 replaces it with `establish_from`, which the switcher calls.

`serve::fixtures::built_binary()` finds the binary cargo built: `$CARGO_TARGET_DIR` else `<workspace>/target`, then `debug/oxutrm` (`CARGO_BIN_EXE_oxutrm` is set only for integration tests). Build it before the unit tests: `cargo build --bin oxutrm`. The sibling test registers under `/tmp` (socket path length on macOS) and kills whatever it leaves behind (`Reaper`).

Review focus 3 is pinned here: `new_with_no_binary_to_run_is_refused_and_changes_nothing`.

**Files:**
- Modify: `src/door.rs`, `src/serve.rs`, `src/connect.rs`, `src/session.rs` (`Door::process` takes `exe`)

**Interfaces:**
- Consumes: Task 5's door and `relay_signals` (`oxutrm_host::attach`); Task 6's `Door::process`, `run_process`, fixtures; Task 7's `Begin::Session { name }`, `host --serve --name`.
- Produces:
  - `Door::process(registry, meta, presence, cfg, to_loop, exe: Option<PathBuf>)`; `serve::Shell { program, start, sibling: Option<PathBuf> }`; `serve::own_binary() -> Option<PathBuf>` (private).
  - `connect::establish_answering(reader, writer, size, cfg) -> Result<Established>` (Task 9 removes it); `connect::establish_from(first: Signal, reader, writer, size, cfg, admit) -> Result<Established>` (private here, `pub(crate)` in Task 9); `establish` is `read_signal_async` + `establish_from`.
  - Test fixture `serve::fixtures::built_binary() -> PathBuf`.

- [ ] **Step 1: Write the failing tests**

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -288,17 +308,34 @@ pub(crate) mod fixtures {
             meta,
             registry.to_path_buf(),
             crate::attach_exchange::fixtures::stun_free(),
             begin,
-            shell,
+            Shell {
+                sibling: Some(built_binary()),
+                ..shell
+            },
         ));
         Process { client, task }
     }
 
+    /// The `oxutrm` binary `cargo test` built, for a sibling to run.
+    ///
+    /// `CARGO_BIN_EXE_oxutrm` is set for integration tests only, so it is
+    /// found where cargo puts it: `$CARGO_TARGET_DIR`, else `target/` in
+    /// the workspace, then `debug/`. `make test` builds it (the integration
+    /// tests need it); a lone `cargo test --bin oxutrm` may not.
+    pub(crate) fn built_binary() -> std::path::PathBuf {
+        let target = std::env::var_os("CARGO_TARGET_DIR")
+            .map(std::path::PathBuf::from)
+            .unwrap_or_else(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target"));
+        target.join("debug").join("oxutrm")
+    }
+
     pub(crate) fn sh() -> Shell {
         Shell {
             program: "/bin/sh".to_string(),
             start: oxutrm_term::Start::default(),
+            sibling: None,
         }
     }
 
     /// Ask `req` over a fresh control stream of `conn`, as the client does,
@@ -566,8 +603,9 @@ mod tests {
             Begin::Session { name: None },
             Shell {
                 program: script.to_str().unwrap().to_string(),
                 start: oxutrm_term::Start::default(),
+                sibling: None,
             },
         )
         .await;
         registry_holds(&registry, &[BUILD]).await;
@@ -717,8 +755,9 @@ mod tests {
             Begin::Session { name: None },
             Shell {
                 program: script.to_str().unwrap().to_string(),
                 start: oxutrm_term::Start::login_in(home_dir(Some(home.clone().into_os_string()))),
+                sibling: None,
             },
         )
         .await;
         let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
@@ -736,5 +775,220 @@ mod tests {
         // a `#!` script's `$0` is its path, whatever `argv[0]` was.
         assert!(text.ends_with(&format!(":{}", home.display())), "{text}");
         done(ask_over(p.client.sink.connection(), Request::Kill { id: id(BUILD) }).await);
     }
+
+    /// Open a control stream on `conn`, send `req`, and run the client's side
+    /// of the attach exchange that follows, as a switch or a new sibling
+    /// does.
+    async fn attach_by(
+        conn: &quinn::Connection,
+        req: Request,
+    ) -> anyhow::Result<crate::connect::Established> {
+        use oxutrm_host::signalling::write_line_async;
+        let (mut send, recv) = conn.open_bi().await?;
+        write_line_async(&mut send, &oxutrm_proto::Open::new(req)).await?;
+        tokio::time::timeout(
+            std::time::Duration::from_secs(20),
+            crate::connect::establish_answering(
+                tokio::io::BufReader::new(recv),
+                send,
+                SIZE,
+                &crate::attach_exchange::fixtures::stun_free(),
+            ),
+        )
+        .await?
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_switch_runs_the_targets_ordinary_attach_and_leaves_this_session_be() {
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(
+            dir.path(),
+            BUILD,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        let logs = process(
+            dir.path(),
+            LOGS,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(dir.path(), &[BUILD, LOGS]).await;
+
+        let landed = attach_by(
+            me.client.sink.connection(),
+            Request::Switch { to: id(LOGS) },
+        )
+        .await
+        .expect("the switch lands");
+        assert_eq!(landed.session_id, LOGS);
+
+        // The target's other client was displaced, as by any primary attach.
+        let reason = tokio::time::timeout(
+            std::time::Duration::from_secs(5),
+            logs.client.sink.connection().closed(),
+        )
+        .await
+        .expect("the target's old client was not displaced");
+        assert!(
+            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
+                if c.reason.as_ref() == crate::session::TAKEN_OVER),
+            "{reason:?}"
+        );
+        // This session is untouched: its link is up and it is still listed.
+        assert!(me.client.sink.connection().close_reason().is_none());
+        assert!(!me.task.is_finished());
+        registry_holds(dir.path(), &[BUILD, LOGS]).await;
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_switch_to_a_session_that_is_gone_is_refused_and_changes_nothing() {
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
+        registry_holds(dir.path(), &[BUILD]).await;
+        let err = attach_by(
+            me.client.sink.connection(),
+            Request::Switch { to: id(LOGS) },
+        )
+        .await
+        .err()
+        .expect("there is nothing to switch to");
+        assert!(format!("{err:#}").contains("a3f9c01e"), "{err:#}");
+        let err = attach_by(
+            me.client.sink.connection(),
+            Request::Switch { to: id(BUILD) },
+        )
+        .await
+        .err()
+        .expect("a switch to here");
+        assert!(format!("{err:#}").contains("you are in"), "{err:#}");
+        assert!(me.client.sink.connection().close_reason().is_none());
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_switch_to_another_version_is_refused_with_that_reason() {
+        use tokio::io::AsyncWriteExt as _;
+        let dir = tempfile::tempdir().unwrap();
+        let me = process(dir.path(), BUILD, None, Begin::Session { name: None }, sh()).await;
+        let _old = oxutrm_host::RegistryGuard::register_in(
+            dir.path(),
+            &crate::door::fixtures::meta(LOGS, None),
+        )
+        .unwrap();
+        let old =
+            tokio::net::UnixListener::bind(oxutrm_host::Registry::socket_path_in(dir.path(), LOGS))
+                .unwrap();
+        tokio::spawn(async move {
+            while let Ok((mut s, _)) = old.accept().await {
+                let hello = format!(
+                    concat!(
+                        r#"{{"t":"HostHello","proto":2,"session_id":"{}","attach_id":1,"#,
+                        r#""cert_spki_sha256":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
+                        r#""psk":"AQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyA=","#,
+                        r#""candidates":[],"nat_type":"Unknown","bound_port":443,"#,
+                        r#""detachable":true}}"#,
+                        "\n"
+                    ),
+                    LOGS
+                );
+                let _ = s.write_all(hello.as_bytes()).await;
+            }
+        });
+        registry_holds(dir.path(), &[BUILD, LOGS]).await;
+        let err = attach_by(
+            me.client.sink.connection(),
+            Request::Switch { to: id(LOGS) },
+        )
+        .await
+        .err()
+        .expect("an old session cannot be switched to");
+        assert!(format!("{err:#}").contains("another version"), "{err:#}");
+    }
+
+    /// Kills whatever sessions are left in `registry` when a test ends, by
+    /// pid: a sibling is a real detached process, and one a failed test
+    /// leaves behind would run for ever.
+    struct Reaper(std::path::PathBuf);
+
+    impl Drop for Reaper {
+        fn drop(&mut self) {
+            for m in oxutrm_host::Registry::list_in(&self.0).unwrap_or_default() {
+                if m.pid != std::process::id()
+                    && let Some(pid) = rustix::process::Pid::from_raw(m.pid as i32)
+                {
+                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
+                }
+            }
+        }
+    }
+
+    /// The whole of `New` from a session: a real sibling, started as
+    /// `oxutrm host --serve --name logs` from the binary cargo built, runs
+    /// the ordinary attach exchange over the relayed stream and registers.
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn new_in_a_session_starts_a_named_sibling_through_the_one_startup_path() {
+        // Under /tmp: a socket path under macOS's per-user temp directory
+        // is too long once `oxutrm/<id>/sock` is added.
+        let base = tempfile::Builder::new().tempdir_in("/tmp").unwrap();
+        let registry = base.path().join("oxutrm");
+        let _reaper = Reaper(registry.clone());
+        assert!(
+            built_binary().exists(),
+            "{} is not built; run the tests through `make test`",
+            built_binary().display()
+        );
+        let me = process(
+            &registry,
+            BUILD,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(&registry, &[BUILD]).await;
+
+        let landed = attach_by(
+            me.client.sink.connection(),
+            Request::New { name: name("logs") },
+        )
+        .await
+        .expect("the sibling's exchange completes");
+        assert_ne!(
+            landed.session_id, BUILD,
+            "the client landed in a new session"
+        );
+
+        let sibling = landed.session_id.clone();
+        registry_holds(&registry, &[BUILD, &sibling]).await;
+        let live = oxutrm_host::Registry::list_in(&registry).unwrap();
+        let entry = live.iter().find(|m| m.session_id == sibling).unwrap();
+        assert_eq!(entry.name.as_deref(), Some("logs"));
+        assert_ne!(entry.pid, std::process::id(), "a process of its own");
+
+        // A taken name is refused before anything starts.
+        let err = attach_by(
+            me.client.sink.connection(),
+            Request::New { name: name("logs") },
+        )
+        .await
+        .err()
+        .expect("logs is taken");
+        assert!(format!("{err:#}").contains("taken"), "{err:#}");
+
+        // And it is a session like any other: killed through this one.
+        done(
+            ask_over(
+                me.client.sink.connection(),
+                Request::Kill { id: id(&sibling) },
+            )
+            .await,
+        );
+        registry_holds(&registry, &[BUILD]).await;
+        drop(landed);
+    }
 }
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -6705,8 +6705,9 @@ mod tests {
             crate::door::LoopLink {
                 cmds: cmds_tx,
                 attached: attached_tx,
             },
+            None,
         );
         door.register().expect("register");
         crate::control::serve_control(host.link.sink.connection().clone(), Arc::clone(&door));
         let host_loop =
````

`src/serve.rs` -- append inside `mod tests`, at its end (review focus 3):

````rust
    /// The binary a sibling would run is gone -- an upgrade removed it, a
    /// path that no longer resolves: `New` is refused with the reason, and
    /// the session it was asked of carries on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn new_with_no_binary_to_run_is_refused_and_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (host, client) = crate::link::fixtures::link_pair().await;
        let mut meta = crate::door::fixtures::meta(BUILD, None);
        meta.size = SIZE;
        let task = tokio::spawn(run_process(
            host,
            SIZE,
            meta,
            dir.path().to_path_buf(),
            crate::attach_exchange::fixtures::stun_free(),
            Begin::Session { name: None },
            Shell {
                sibling: Some(std::path::PathBuf::from("/nonexistent/oxutrm")),
                ..sh()
            },
        ));
        registry_holds(dir.path(), &[BUILD]).await;
        let err = attach_by(client.sink.connection(), Request::New { name: None })
            .await
            .err()
            .expect("there is no binary to start");
        assert!(
            format!("{err:#}").contains("starting a new session"),
            "{err:#}"
        );
        assert!(client.sink.connection().close_reason().is_none());
        assert!(!task.is_finished());
        registry_holds(dir.path(), &[BUILD]).await;
    }
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo build --bin oxutrm --jobs 4 && cargo test --bin oxutrm --jobs 4 serve::`

Expected: compile errors: `establish_answering` and `Shell::sibling` are not defined.

- [ ] **Step 3: Implement**

`src/connect.rs`:

````diff
--- a/src/connect.rs
+++ b/src/connect.rs
@@ -396,28 +396,79 @@ pub(crate) async fn establish<R, W>(
     size: TermSize,
     cfg: &NetConfig,
     admit_remote: Option<oxutrm_net::RemoteFilter>,
 ) -> Result<Established>
+where
+    R: tokio::io::AsyncBufRead + Unpin + Send,
+    W: tokio::io::AsyncWrite + Unpin + Send,
+{
+    let mut reader = reader;
+    // L5. Banner and motd are skipped inside `read_signal_async`; version skew
+    // fails loudly.
+    let first = read_signal_async(&mut reader)
+        .await
+        .context("reading the host's offer")?;
+    establish_from(first, reader, writer, size, cfg, admit_remote).await
+}
+
+/// [`establish`] on a control stream after an `Open` that runs an attach
+/// exchange -- a `Switch`, a `New` in a session (switcher spec §4.1): the
+/// one answer is the exchange's `HostHello`, or a refusal, which is the
+/// host's words for the user ([`HostRefused`]).
+#[cfg_attr(
+    not(test),
+    expect(dead_code, reason = "the switcher's requests use it from Task 9 on")
+)]
+pub(crate) async fn establish_answering<R, W>(
+    reader: R,
+    writer: W,
+    size: TermSize,
+    cfg: &NetConfig,
+) -> Result<Established>
+where
+    R: tokio::io::AsyncBufRead + Unpin + Send,
+    W: tokio::io::AsyncWrite + Unpin + Send,
+{
+    let mut reader = reader;
+    match oxutrm_host::signalling::read_answer_async(&mut reader)
+        .await
+        .context("reading the host's answer")?
+    {
+        oxutrm_proto::Answer::Signal(first) => {
+            establish_from(first, reader, writer, size, cfg, None).await
+        }
+        oxutrm_proto::Answer::Reply(oxutrm_proto::Reply::Refused(why)) => {
+            Err(anyhow::Error::new(HostRefused(why)))
+        }
+        oxutrm_proto::Answer::Reply(other) => Err(anyhow::anyhow!(
+            "the host answered with {other:?} where an attach should have begun"
+        )),
+    }
+}
+
+/// L4 and L6 to L10, once the host's first line -- its hello, or why not --
+/// is in hand.
+async fn establish_from<R, W>(
+    first: Signal,
+    reader: R,
+    writer: W,
+    size: TermSize,
+    cfg: &NetConfig,
+    admit_remote: Option<oxutrm_net::RemoteFilter>,
+) -> Result<Established>
 where
     R: tokio::io::AsyncBufRead + Unpin + Send,
     W: tokio::io::AsyncWrite + Unpin + Send,
 {
     let mut reader = reader;
     let mut writer = writer;
+    let host = host_facts(first)?;
 
     // L4. One socket for STUN, ICE and QUIC.
     let bound = oxutrm_net::bind_socket(cfg).context("binding a UDP socket")?;
     let mut candidates = oxutrm_net::local_candidates(&bound);
     let socket = crate::ladder::adopt(bound).context("handing the socket to the runtime")?;
 
-    // L5. Banner and motd are skipped inside `read_signal_async`; version skew
-    // fails loudly.
-    let host = host_facts(
-        read_signal_async(&mut reader)
-            .await
-            .context("reading the host's offer")?,
-    )?;
-
     // L6.
     let (reflexive, nat) = oxutrm_net::stun_discover(&socket, cfg).await;
     candidates.extend(reflexive);
 
````

`src/door.rs`:

````diff
--- a/src/door.rs
+++ b/src/door.rs
@@ -118,8 +118,11 @@ pub(crate) struct Door {
     own: std::sync::Mutex<Own>,
     presence: Presence,
     /// `None` for a door with no loop behind it: the tests' doors.
     to_loop: Option<LoopLink>,
+    /// The binary a sibling runs: this session's own (switcher spec §3.4),
+    /// resolved once at startup. `None` where none could be.
+    exe: Option<PathBuf>,
 }
 
 impl Door {
     /// The door of a session process: a lobby until [`Door::register`].
@@ -128,8 +131,9 @@ impl Door {
         meta: SessionMeta,
         presence: Presence,
         cfg: NetConfig,
         to_loop: LoopLink,
+        exe: Option<PathBuf>,
     ) -> Arc<Door> {
         Arc::new(Door {
             registry,
             cfg,
@@ -141,8 +145,9 @@ impl Door {
                 lobby: true,
             }),
             presence,
             to_loop: Some(to_loop),
+            exe,
         })
     }
 
     /// A door assembled from its parts, with no loop behind it: a session
@@ -166,8 +171,9 @@ impl Door {
                 lobby: false,
             }),
             presence,
             to_loop: None,
+            exe: None,
         })
     }
 
     fn own(&self) -> std::sync::MutexGuard<'_, Own> {
@@ -382,8 +388,190 @@ impl Door {
         }
     }
 }
 
+/// `Switch` (switcher spec §3.3): open the target's socket, ask it for a
+/// primary attach, and relay. The target runs its ordinary attach exchange
+/// with this session's client, which keeps its link here until the new one
+/// is up; nothing here changes this session.
+async fn switch<R, W>(door: &Door, to: SessionId, reader: R, mut writer: W)
+where
+    R: AsyncBufRead + Unpin + Send,
+    W: AsyncWrite + Unpin + Send,
+{
+    let refuse = |why: String| Reply::Refused(why);
+    let reply = if to.to_string() == door.id() {
+        refuse("this is the session you are in".to_string())
+    } else {
+        let live = Registry::list_in(&door.registry).unwrap_or_default();
+        match live.iter().find(|m| m.session_id == to.to_string()) {
+            None => refuse(format!("no session {} on this host", to.short())),
+            Some(m) if !m.detachable => refuse(format!(
+                "session {} is not detachable: it dies with its ssh",
+                to.short()
+            )),
+            Some(m) => {
+                let path = Registry::socket_path_in(&door.registry, &m.session_id);
+                match tokio::net::UnixStream::connect(&path).await {
+                    Err(e) => refuse(format!("session {} did not answer: {e}", to.short())),
+                    Ok(stream) => {
+                        let (r, mut w) = stream.into_split();
+                        let asked = write_line_async(
+                            &mut w,
+                            &Open::new(Request::Attach {
+                                role: Role::Primary,
+                            }),
+                        )
+                        .await;
+                        if let Err(e) = asked {
+                            refuse(format!("session {} did not answer: {e}", to.short()))
+                        } else {
+                            let what = format!("session {}", to.short());
+                            relay_attach(&what, reader, writer, tokio::io::BufReader::new(r), w)
+                                .await;
+                            return;
+                        }
+                    }
+                }
+            }
+        }
+    };
+    let _ = write_line_async(&mut writer, &reply).await;
+}
+
+/// `New` in a session (switcher spec §3.4): start a sibling as the ordinary
+/// `oxutrm host --serve`, its stdin and stdout on a socket pair held here,
+/// and relay the client's stream into that pair exactly as ssh carries a
+/// first connect. One startup path makes every session process.
+async fn new_sibling<R, W>(door: &Door, name: Option<Name>, reader: R, mut writer: W)
+where
+    R: AsyncBufRead + Unpin + Send,
+    W: AsyncWrite + Unpin + Send,
+{
+    let refused = |why: String| Reply::Refused(why);
+    let reply = match &door.exe {
+        None => refused("this session does not know its own binary".to_string()),
+        Some(exe) => match name_taken(&door.registry, name.as_ref()) {
+            Some(why) => refused(why),
+            None => match spawn_sibling(exe, &door.registry, name.as_ref()) {
+                Err(e) => refused(format!("starting a new session: {e}")),
+                Ok(pair) => {
+                    let (r, w) = pair.into_split();
+                    relay_attach(
+                        "the new session",
+                        reader,
+                        writer,
+                        tokio::io::BufReader::new(r),
+                        w,
+                    )
+                    .await;
+                    return;
+                }
+            },
+        },
+    };
+    let _ = write_line_async(&mut writer, &reply).await;
+}
+
+/// Why `name` cannot be a new session's, checked under the name lock: a
+/// refusal the client can read now, rather than a sibling that fails to
+/// register after its exchange. The sibling checks again as it registers.
+fn name_taken(registry: &std::path::Path, name: Option<&Name>) -> Option<String> {
+    let name = name?;
+    let checked = oxutrm_host::NamesLock::take(registry)
+        .and_then(|_lock| oxutrm_host::name_refusal(registry, name.as_str(), ""));
+    match checked {
+        Ok(refusal) => refusal,
+        Err(e) => Some(format!("checking the name: {e:#}")),
+    }
+}
+
+/// `exe host --serve [--name <name>]` with its stdin and stdout on one end
+/// of a socket pair, the other end returned. Its registry is this one,
+/// passed as `OXUTRM_STATE_DIR` -- the directory `registry` is in -- so the
+/// sibling registers beside its parent whatever its environment says.
+///
+/// The child is `host --serve`'s first process, which forks the session and
+/// exits at once (`detach_process`); it is waited for on a task of its own.
+fn spawn_sibling(
+    exe: &std::path::Path,
+    registry: &std::path::Path,
+    name: Option<&Name>,
+) -> std::io::Result<tokio::net::UnixStream> {
+    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
+    let theirs = std::os::fd::OwnedFd::from(theirs);
+    let base = registry.parent().unwrap_or(registry);
+    let mut command = tokio::process::Command::new(exe);
+    command.args(["host", "--serve"]);
+    if let Some(n) = name {
+        command.args(["--name", n.as_str()]);
+    }
+    command
+        .env("OXUTRM_STATE_DIR", base)
+        .stdin(std::process::Stdio::from(theirs.try_clone()?))
+        .stdout(std::process::Stdio::from(theirs))
+        .stderr(std::process::Stdio::null());
+    let mut child = command.spawn()?;
+    // `command` held the child's ends; they go with it, so only the child
+    // has them now.
+    drop(command);
+    tokio::spawn(async move {
+        let _ = child.wait().await;
+    });
+    ours.set_nonblocking(true)?;
+    tokio::net::UnixStream::from_std(ours)
+}
+
+/// Relay an attach exchange between this session's client and another
+/// session process (`what`, for the reasons): its first line is read here,
+/// so a refusal or another protocol version reaches the client as a reason,
+/// then everything is relayed both ways -- decoded and re-encoded, so
+/// garbage cannot pass -- until either side ends. Bounded by
+/// [`crate::listener::ATTACH_TIMEOUT`], as the exchange itself is.
+async fn relay_attach<CR, CW, TR, TW>(
+    what: &str,
+    client_r: CR,
+    client_w: CW,
+    target_r: TR,
+    target_w: TW,
+) where
+    CR: AsyncBufRead + Unpin + Send,
+    CW: AsyncWrite + Unpin + Send,
+    TR: AsyncBufRead + Unpin + Send,
+    TW: AsyncWrite + Unpin + Send,
+{
+    let (mut client_r, mut client_w, mut target_r, mut target_w) =
+        (client_r, client_w, target_r, target_w);
+    let first = tokio::time::timeout(OPEN_TIMEOUT, read_answer_async(&mut target_r)).await;
+    let refusal = match first {
+        Ok(Ok(Answer::Signal(hello @ oxutrm_proto::Signal::HostHello { .. }))) => {
+            if oxutrm_host::signalling::write_signal_async(&mut client_w, &hello)
+                .await
+                .is_err()
+            {
+                return;
+            }
+            let both = async {
+                tokio::select! {
+                    _ = oxutrm_host::attach::relay_signals(&mut client_r, &mut target_w) => {}
+                    _ = oxutrm_host::attach::relay_signals(&mut target_r, &mut client_w) => {}
+                }
+            };
+            let _ = tokio::time::timeout(crate::listener::ATTACH_TIMEOUT, both).await;
+            return;
+        }
+        Ok(Ok(Answer::Signal(oxutrm_proto::Signal::Failed { reason }))) => reason,
+        Ok(Ok(Answer::Reply(Reply::Refused(why)))) => why,
+        Ok(Ok(other)) => format!("{what} answered with {other:?}"),
+        Ok(Err(ProtoError::Malformed(_) | ProtoError::VersionMismatch { .. })) => {
+            format!("{what} runs another version of oxutrm")
+        }
+        Ok(Err(e)) => format!("{what} did not start: {e}"),
+        Err(_) => format!("{what} did not answer in time"),
+    };
+    let _ = write_line_async(&mut client_w, &Reply::Refused(refusal)).await;
+}
+
 /// How long a request forwarded to a sibling may take: a kill waits out
 /// the shell's grace and its reaping.
 pub(crate) const FORWARD_TIMEOUT: Duration = Duration::from_secs(10);
 
@@ -478,15 +666,19 @@ where
         Request::Rename { id, name } => {
             let reply = forward(&door.registry, id, Request::Rename { id, name }).await;
             let _ = write_line_async(&mut writer, &reply).await;
         }
-        Request::Switch { .. } | Request::New { .. } => {
+        // Only a session's own client switches or starts a sibling: a
+        // socket is a sibling's or `--attach`'s, and neither does.
+        Request::Switch { .. } | Request::New { .. } if via == Via::Socket => {
             let _ = write_line_async(
                 &mut writer,
-                &Reply::Refused("this host does not do that yet".to_string()),
+                &Reply::Refused("asked through the wrong door".to_string()),
             )
             .await;
         }
+        Request::Switch { to } => switch(&door, to, reader, writer).await,
+        Request::New { name } => new_sibling(&door, name, reader, writer).await,
     }
 }
 
 /// Hand the stream to the serial attach loop, whose exchange's `HostHello`
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -21,8 +21,11 @@ pub fn run_host_serve(begin: Begin) -> anyhow::Result<()> {
     // R1. Nothing has run before this. The parent `_exit(0)`s, so ssh reports
     // the command finished; the channel stays open because the grandchild
     // still holds 0, 1 and 2.
     let detached = oxutrm_host::detach_process().context("detaching from ssh")?;
+    // The binary a sibling runs (switcher spec §3.4), resolved once here and
+    // passed in: never looked up inside the code that spawns one.
+    let exe = own_binary();
 
     // R2. This reaches the user, because stderr is still the ssh pipe — and a
     // user wondering why a session vanished at logout needs this sentence
     // before they need anything else.
@@ -36,9 +39,9 @@ pub fn run_host_serve(begin: Begin) -> anyhow::Result<()> {
     let runtime = tokio::runtime::Builder::new_multi_thread()
         .enable_all()
         .build()
         .context("building the runtime")?;
-    let outcome = runtime.block_on(serve(detached, &root, begin));
+    let outcome = runtime.block_on(serve(detached, &root, begin, exe));
 
     // Do not WAIT for the read that is parked on ssh's pipe.
     //
     // The signalling channel is descriptor 0, and `tokio::io::stdin` serves it
@@ -77,8 +80,9 @@ pub fn run_host_serve(begin: Begin) -> anyhow::Result<()> {
 async fn serve(
     detached: oxutrm_host::Detached,
     root: &RegistryRoot,
     begin: Begin,
+    exe: Option<std::path::PathBuf>,
 ) -> anyhow::Result<()> {
     let cfg = NetConfig::default();
     let mut meta = SessionMeta {
         session_id: oxutrm_host::new_session_id().context("naming the session")?,
@@ -136,8 +140,9 @@ async fn serve(
     // A login shell in `$HOME`, as ssh would give you (switcher spec §3.4).
     let shell = Shell {
         program: meta.shell.clone(),
         start: oxutrm_term::Start::login_in(home_dir(std::env::var_os("HOME"))),
+        sibling: exe,
     };
     run_process(
         attached.link,
         attached.client_size,
@@ -150,8 +155,19 @@ async fn serve(
     .await
     .map(|_| ())
 }
 
+/// This session's own binary, for a sibling to run: on Linux
+/// `/proc/self/exe`, which still opens after the file was replaced by a
+/// rebuild; elsewhere the path this process was started from.
+fn own_binary() -> Option<std::path::PathBuf> {
+    if cfg!(target_os = "linux") {
+        Some(std::path::PathBuf::from("/proc/self/exe"))
+    } else {
+        std::env::current_exe().ok()
+    }
+}
+
 /// What a session process starts as: a session with its shell, or a lobby
 /// (switcher spec §2.1).
 #[derive(Clone, Debug, PartialEq, Eq)]
 pub(crate) enum Begin {
@@ -161,12 +177,15 @@ pub(crate) enum Begin {
     },
     Lobby,
 }
 
-/// The shell a session runs, and how it starts it.
+/// What a session process runs: its shell, how the shell starts, and the
+/// binary a sibling runs (switcher spec §3.4) -- this process's own,
+/// resolved once at startup; `None` where none could be.
 pub(crate) struct Shell {
     pub(crate) program: String,
     pub(crate) start: oxutrm_term::Start,
+    pub(crate) sibling: Option<std::path::PathBuf>,
 }
 
 /// R13 to R16: one session process, from its first link to its end.
 ///
@@ -206,8 +225,9 @@ pub(crate) async fn run_process(
         crate::door::LoopLink {
             cmds: cmds_tx,
             attached: attached_tx,
         },
+        shell.sibling.clone(),
     );
     let mut session =
         HostSession::lobby(&shell.program, &shell.start, client_size, SCROLLBACK, link)?
             .with_presence(presence);
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo build --bin oxutrm --jobs 4 && cargo test --bin oxutrm --jobs 4 serve::`

Expected: all pass: `a_switch_runs_the_targets_ordinary_attach_and_leaves_this_session_be`, `a_switch_to_a_session_that_is_gone_is_refused_and_changes_nothing`, `a_switch_to_another_version_is_refused_with_that_reason`, `new_in_a_session_starts_a_named_sibling_through_the_one_startup_path` (a real sibling process; afterwards `ps -axo command | grep "oxutrm host"` shows none left), `new_with_no_binary_to_run_is_refused_and_changes_nothing`.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add src/connect.rs src/door.rs src/serve.rs src/session.rs
git commit -m "feat(door): Switch relays to the target's attach; New in a session starts a sibling as host --serve

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

## Part 3 -- the client

### Task 9: The client's requests: switch make-before-break, the lobby, the rebuild's aim

Spec §3.3-§3.5, §4.3. `src/switcher.rs` sends one `Ask` on a fresh control stream of the live link and comes back with one `Answered`: `Sessions`, `Landed` (a switch or a new sibling, its link `Established` by `establish_from`), `Started` (a lobby's `New` -- `Reply::Entry`), `Killed`, `Renamed`, or `Failed(reason)`. Bounded by `ANSWER_WITHIN` (15 s) and, for the two that attach, `SEARCH_DEADLINE`.

`ClientSession` holds one request at a time (`ask` queues it, the loop's lap sends it from a task held as `AbortOnDrop`, its answer wakes the loop as `Wake::Answered`) and acts on the answer in `answered`, which returns whether the loop must follow a new link:
- `Landed` -> `moved`: a rebuild attempt in flight is stood down (it was for the old session), the new link is swapped in and the old one closed as `MOVED_AWAY` (`swap_in_as`, which starts the screen over), the standby forgotten, the identity and the rebuild's aim moved to the new session; the activity log says `switched to logs` / `new session logs`.
- `Started`: no longer a lobby; same id; the rebuild aims at it.
- `Killed` of this session: a lobby -- the rebuild aims at a fresh lobby, the standby is forgotten (and the loop drops its search and probe tasks).
- `Failed`: a shown activity entry `<what> failed: <why>`.

`Rebuild` aims at `Aim::Session(id)` or `Aim::Lobby` (`Choice::Lobby`); `retarget` moves it; `connect` starts a lobby connect with `with_lobby()` and a lobby aim. No standby is searched for in a lobby (`standby_step`). `q` in a lobby closes the link with the new reason `QUIT`, so the lobby ends at once rather than after `DETACH_AFTER` (**deviation**: §4.3 lists `MOVED_AWAY` as the only new reason).

`ask`, `asking`, `sessions` and `in_lobby` have no non-test caller until Tasks 10-12; `Ask` neither: they carry `expect(dead_code)` attributes naming Task 12, which Tasks 10 and 12 remove.

**Files:**
- Create: `src/switcher.rs`
- Modify: `src/session.rs`, `src/rebuild.rs`, `src/connect.rs` (`establish_from` `pub(crate)`, `establish_answering` removed, the lobby connect), `src/activity.rs` (`Kind::Session`), `src/main.rs`, `src/serve.rs` (`attach_by` uses `switcher::ask`)

**Interfaces:**
- Consumes: Task 8's `establish_from`; Task 5-8's door requests; Task 7's `Choice::Lobby`; the existing `swap_in_as`, `Standby::forget`, `Rebuild::stood_down`.
- Produces:
  - `crate::switcher::{Ask { Sessions, Switch { to }, New { name }, Kill { id }, Rename { id, name } }, Answered { Sessions(Vec<SessionEntry>), Landed(Box<Established>), Started(SessionEntry), Killed(SessionId), Renamed(SessionEntry), Failed(String) }, ask(conn: quinn::Connection, ask: Ask, size: TermSize, cfg: &NetConfig) -> Answered, ANSWER_WITHIN}`.
  - `crate::rebuild::{Aim { Session(String), Lobby }}`, `Rebuild::aimed(target: String, aim: Aim) -> Rebuild`, `Rebuild::retarget(&mut self, Aim)`, `#[cfg(test)] Rebuild::aim(&self) -> &Aim`; `attempt` takes `&Aim`.
  - `ClientSession::with_lobby(self)`, `in_lobby(&self)`, `ask(&mut self, Ask) -> bool`, `asking(&self) -> bool`, `sessions(&self) -> &[SessionEntry]`, `answered(&mut self, Answered, Instant) -> Result<bool>`; private `take_ask`, `label_of`, `here`, `aim_rebuild`, `moved`; field `net: NetConfig` set by `apply`.
  - `session::{MOVED_AWAY, QUIT}`; `activity::Kind::Session` (`"session"`).

- [ ] **Step 1: Write the failing tests**

The session tests drive a real host process (`serve::fixtures::process`) and send the request the way the loop does (`ask_now`), then run the loop itself for the end of the story (`exit 7` reaches the shell switched to).

`src/rebuild.rs`:

````diff
--- a/src/rebuild.rs
+++ b/src/rebuild.rs
@@ -928,12 +959,16 @@ mod tests {
         FakeHost::new(dir, &body)
     }
 
     async fn run_attempt(fake: &FakeHost, session_id: &str) -> AttemptOutcome {
+        run_attempt_to(fake, &Aim::Session(session_id.to_owned())).await
+    }
+
+    async fn run_attempt_to(fake: &FakeHost, aim: &Aim) -> AttemptOutcome {
         attempt(
             &fake.launcher,
             "bastion.example.net",
-            session_id,
+            aim,
             a_size(),
             &test_config(),
             &Ties::default(),
         )
@@ -1045,9 +1080,9 @@ mod tests {
             attempt_within(
                 std::time::Duration::from_millis(200),
                 &fake.launcher,
                 "bastion.example.net",
-                "3ff1218f5e0c4b7d9a1c2e3f40516273",
+                &Aim::Session("3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned()),
                 a_size(),
                 &test_config(),
                 &Ties::default(),
             ),
@@ -1529,5 +1564,32 @@ mod tests {
             recorded,
             serde_json::json!({"c": "Attach", "id": "3ff1218f5e0c4b7d9a1c2e3f40516273"})
         );
     }
+
+    /// A client in a lobby is rebuilt into a fresh lobby, never into the old
+    /// one, which has ended or will (switcher spec §3.5).
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn an_attempt_from_a_lobby_asks_for_a_fresh_lobby() {
+        let fake = fake_host_recording_the_choice();
+        let record = fake.path("choice.json");
+        let _ = run_attempt_to(&fake, &Aim::Lobby).await;
+        let line = std::fs::read_to_string(&record).expect("the fake host recorded no choice");
+        let signal: serde_json::Value = serde_json::from_str(line.trim()).expect("JSON");
+        assert_eq!(signal["choice"], serde_json::json!({"c": "Lobby"}));
+    }
+
+    #[test]
+    fn a_rebuild_is_retargeted_by_a_move() {
+        let mut r = Rebuild::new(
+            "bastion.example.net".to_owned(),
+            "3ff1218f5e0c4b7d9a1c2e3f40516273".to_owned(),
+        );
+        r.retarget(Aim::Lobby);
+        assert_eq!(r.aim(), &Aim::Lobby);
+        r.retarget(Aim::Session("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60".to_owned()));
+        assert_eq!(
+            r.aim(),
+            &Aim::Session("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60".to_owned())
+        );
+    }
 }
````

`src/serve.rs`:

````diff
--- a/src/serve.rs
+++ b/src/serve.rs
@@ -783,21 +783,26 @@ mod tests {
     async fn attach_by(
         conn: &quinn::Connection,
         req: Request,
     ) -> anyhow::Result<crate::connect::Established> {
-        use oxutrm_host::signalling::write_line_async;
-        let (mut send, recv) = conn.open_bi().await?;
-        write_line_async(&mut send, &oxutrm_proto::Open::new(req)).await?;
-        tokio::time::timeout(
-            std::time::Duration::from_secs(20),
-            crate::connect::establish_answering(
-                tokio::io::BufReader::new(recv),
-                send,
-                SIZE,
-                &crate::attach_exchange::fixtures::stun_free(),
-            ),
+        use crate::switcher::{Answered, Ask};
+        let ask = match req {
+            Request::Switch { to } => Ask::Switch { to },
+            Request::New { name } => Ask::New { name },
+            other => panic!("{other:?} runs no attach"),
+        };
+        match crate::switcher::ask(
+            conn.clone(),
+            ask,
+            SIZE,
+            &crate::attach_exchange::fixtures::stun_free(),
         )
-        .await?
+        .await
+        {
+            Answered::Landed(e) => Ok(*e),
+            Answered::Failed(why) => Err(anyhow::anyhow!(why)),
+            other => panic!("{other:?}"),
+        }
     }
 
     #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
     async fn a_switch_runs_the_targets_ordinary_attach_and_leaves_this_session_be() {
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -9695,5 +9954,289 @@ mod tests {
             let shown = format!("config: {w}");
             assert_eq!(times(&shown), 1, "{shown}: {:#?}", shown_log(&session));
         }
     }
+
+    // ---- the switcher, on the client (switcher spec §3.3 to §3.5) ----------
+
+    const BUILD_ID: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
+    const LOGS_ID: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
+
+    /// A client on `link`, in session `id` on `thinlinc`, whose switcher
+    /// exchanges reach no STUN server.
+    fn client_on(link: Link, id: &str) -> ClientSession {
+        let mut c = ClientSession::new(crate::serve::fixtures::SIZE, caps(), link, None)
+            .unwrap()
+            .with_identity(Identity {
+                target: "thinlinc".to_owned(),
+                session_id: id.to_owned(),
+            });
+        c.net = crate::attach_exchange::fixtures::stun_free();
+        c
+    }
+
+    /// Send the request the client has waiting, as the loop does, and hand
+    /// the answer back to it. Returns what `answered` returned.
+    async fn ask_now(c: &mut ClientSession, a: crate::switcher::Ask) -> bool {
+        assert!(c.ask(a), "another request was in flight");
+        let a = c.take_ask().expect("the request just asked");
+        let answered =
+            crate::switcher::ask(c.link.sink.connection().clone(), a, c.size, &c.net.clone()).await;
+        c.answered(answered, Instant::now()).expect("answered")
+    }
+
+    fn id_of(s: &str) -> oxutrm_proto::SessionId {
+        s.parse().unwrap()
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_switch_moves_the_client_and_the_old_session_stays_listed() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{process, registry_holds, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let build = process(
+            dir.path(),
+            BUILD_ID,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        let logs = process(
+            dir.path(),
+            LOGS_ID,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(dir.path(), &[BUILD_ID, LOGS_ID]).await;
+
+        let mut client = client_on(build.client, BUILD_ID);
+        client.rebuild = Some(Rebuild::new("thinlinc".to_owned(), BUILD_ID.to_owned()));
+        assert!(!ask_now(&mut client, crate::switcher::Ask::Sessions).await);
+        assert_eq!(client.sessions().len(), 2);
+
+        let old = client.link.sink.connection().clone();
+        assert!(
+            ask_now(
+                &mut client,
+                crate::switcher::Ask::Switch { to: id_of(LOGS_ID) }
+            )
+            .await,
+            "a switch moves the loop to a new link"
+        );
+        assert_ne!(client.link.sink.connection().stable_id(), old.stable_id());
+        assert!(
+            matches!(
+                old.close_reason(),
+                Some(quinn::ConnectionError::LocallyClosed)
+            ),
+            "the old link was not closed: {:?}",
+            old.close_reason()
+        );
+        assert_eq!(client.identity.as_ref().unwrap().session_id, LOGS_ID);
+        assert_eq!(
+            client.rebuild.as_ref().unwrap().aim(),
+            &crate::rebuild::Aim::Session(LOGS_ID.to_owned())
+        );
+        assert_eq!(
+            last_entry(&client),
+            Some((Kind::Session, "switched to logs".to_owned()))
+        );
+        // The old session detached and is still there.
+        assert!(!build.task.is_finished());
+        registry_holds(dir.path(), &[BUILD_ID, LOGS_ID]).await;
+
+        // And the client is in `logs` now: its shell's exit is the client's.
+        let (keys, mut typing) = keyboard();
+        let mut out = SharedOut::default();
+        let looping = tokio::spawn(async move { client.run_on(keys, &mut out).await });
+        typing.write_all(b"exit 7\n").expect("type");
+        let code = tokio::time::timeout(Duration::from_secs(15), looping)
+            .await
+            .expect("the client never ended")
+            .unwrap()
+            .unwrap();
+        assert_eq!(code, 7);
+        assert_eq!(logs.task.await.unwrap().unwrap(), 7);
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_switch_that_fails_leaves_the_client_where_it_was_and_says_why() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{process, registry_holds, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let build = process(
+            dir.path(),
+            BUILD_ID,
+            None,
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(dir.path(), &[BUILD_ID]).await;
+        let mut client = client_on(build.client, BUILD_ID);
+        let before = client.link.sink.connection().stable_id();
+        assert!(
+            !ask_now(
+                &mut client,
+                crate::switcher::Ask::Switch { to: id_of(LOGS_ID) }
+            )
+            .await
+        );
+        assert_eq!(client.link.sink.connection().stable_id(), before);
+        assert!(client.link.sink.connection().close_reason().is_none());
+        let (kind, text) = last_entry(&client).unwrap();
+        assert_eq!(kind, Kind::Session);
+        assert!(
+            text.starts_with("switching to a3f9c01e failed: ") && text.contains("no session"),
+            "{text}"
+        );
+        assert!(!client.asking(), "the failed request is still in flight");
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn killing_this_session_puts_the_client_in_the_lobby_and_new_takes_it_out() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{process, registry_holds, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let build = process(
+            dir.path(),
+            BUILD_ID,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(dir.path(), &[BUILD_ID]).await;
+        let mut client = client_on(build.client, BUILD_ID);
+        client.rebuild = Some(Rebuild::new("thinlinc".to_owned(), BUILD_ID.to_owned()));
+        assert!(!ask_now(&mut client, crate::switcher::Ask::Sessions).await);
+
+        assert!(
+            ask_now(
+                &mut client,
+                crate::switcher::Ask::Kill {
+                    id: id_of(BUILD_ID)
+                }
+            )
+            .await
+        );
+        assert!(client.in_lobby());
+        assert_eq!(
+            client.rebuild.as_ref().unwrap().aim(),
+            &crate::rebuild::Aim::Lobby
+        );
+        assert_eq!(
+            last_entry(&client),
+            Some((Kind::Session, "killed build".to_owned()))
+        );
+        assert!(
+            client.link.sink.connection().close_reason().is_none(),
+            "the link stays"
+        );
+        registry_holds(dir.path(), &[]).await;
+
+        let again = oxutrm_proto::Name::parse("again").unwrap();
+        assert!(!ask_now(&mut client, crate::switcher::Ask::New { name: Some(again) }).await);
+        assert!(!client.in_lobby());
+        assert_eq!(
+            client.rebuild.as_ref().unwrap().aim(),
+            &crate::rebuild::Aim::Session(BUILD_ID.to_owned()),
+            "a lobby keeps its id when it becomes the session"
+        );
+        assert_eq!(
+            last_entry(&client),
+            Some((Kind::Session, "new session again".to_owned()))
+        );
+        registry_holds(dir.path(), &[BUILD_ID]).await;
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn quitting_in_a_lobby_ends_the_lobby_with_the_link() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{process, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let lobby = process(dir.path(), BUILD_ID, None, Begin::Lobby, sh()).await;
+        let mut client = client_on(lobby.client, BUILD_ID).with_lobby();
+        let mut out = Vec::new();
+        let at = Instant::now();
+        assert_eq!(
+            client
+                .route_keys_at(&[CTRL_BACKSLASH, b'q'], at, &mut out)
+                .unwrap(),
+            Some(0)
+        );
+        let code = tokio::time::timeout(Duration::from_secs(5), lobby.task)
+            .await
+            .expect("the lobby outlived its client's quit")
+            .unwrap()
+            .unwrap();
+        assert_eq!(code, 0);
+    }
+
+    #[tokio::test]
+    async fn no_standby_is_searched_for_in_a_lobby() {
+        let (_host, client) = crate::link::fixtures::link_pair().await;
+        let due = crate::standby::Standby::new(
+            crate::attach_exchange::fixtures::stun_free(),
+            Instant::now()
+                .checked_sub(crate::linkstate::STANDBY_DELAY)
+                .expect("a clock this young"),
+        );
+        let mut c = client_on(client, BUILD_ID).with_lobby().with_standby(due);
+        let action = c.standby_step(Phase::Live, Instant::now(), RebuildStage::Idle);
+        assert!(
+            matches!(action, crate::standby::StandbyAction::Nothing),
+            "a lobby searched for a standby"
+        );
+        c.in_lobby = false;
+        let action = c.standby_step(Phase::Live, Instant::now(), RebuildStage::Idle);
+        assert!(
+            matches!(action, crate::standby::StandbyAction::Search { .. }),
+            "the control: in a session the search is due"
+        );
+    }
+
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_rename_is_recorded_and_a_refused_one_says_why() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{process, registry_holds, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let build = process(
+            dir.path(),
+            BUILD_ID,
+            None,
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        let _logs = process(
+            dir.path(),
+            LOGS_ID,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(dir.path(), &[BUILD_ID, LOGS_ID]).await;
+        let mut client = client_on(build.client, BUILD_ID);
+
+        let name = |n: &str| Some(oxutrm_proto::Name::parse(n).unwrap());
+        let rename = |n| crate::switcher::Ask::Rename {
+            id: id_of(BUILD_ID),
+            name: n,
+        };
+        assert!(!ask_now(&mut client, rename(name("build"))).await);
+        assert_eq!(
+            last_entry(&client),
+            Some((Kind::Session, "renamed 3ff1218f to build".to_owned()))
+        );
+        assert!(!ask_now(&mut client, rename(name("logs"))).await);
+        let (_, text) = last_entry(&client).unwrap();
+        assert!(
+            text.starts_with("renaming 3ff1218f failed: ") && text.contains("taken"),
+            "{text}"
+        );
+    }
 }
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --bin oxutrm --jobs 4 rebuild:: session::tests`

Expected: compile errors: `crate::switcher`, `Aim`, `ClientSession::ask`, `answered`, `with_lobby` and `MOVED_AWAY` are not defined.

- [ ] **Step 3: Implement**

`src/activity.rs`:

````diff
--- a/src/activity.rs
+++ b/src/activity.rs
@@ -39,8 +39,11 @@ pub(crate) enum Kind {
     /// how it ended.
     Outage,
     /// The config file: a warning about it, a save, a save that failed.
     Config,
+    /// The session switcher: a switch, a new, killed or renamed session, and
+    /// what was refused.
+    Session,
 }
 
 impl Kind {
     pub(crate) fn name(self) -> &'static str {
@@ -52,8 +55,9 @@ impl Kind {
             Kind::Input => "input",
             Kind::Log => "log",
             Kind::Outage => "outage",
             Kind::Config => "config",
+            Kind::Session => "session",
         }
     }
 }
 
````

`src/connect.rs`:

````diff
--- a/src/connect.rs
+++ b/src/connect.rs
@@ -202,9 +202,17 @@ async fn connect(target: &str, attach: Option<&str>, new: bool, name: Option<Nam
 
     // The two identities a rebuild needs: the target the user typed, and
     // whichever session actually resulted -- which for `Choice::New` is an id
     // the client had no way to guess in advance.
-    let rebuild = Rebuild::new(target.to_owned(), established.session_id.clone());
+    //
+    // A lobby is never reattached: a rebuild from one asks for a fresh one
+    // (switcher spec §3.5).
+    let lobby = chosen == Choice::Lobby;
+    let rebuild = if lobby {
+        Rebuild::aimed(target.to_owned(), crate::rebuild::Aim::Lobby)
+    } else {
+        Rebuild::new(target.to_owned(), established.session_id.clone())
+    };
     let standby = offers_standby(&established.host_features);
     let mut session = ClientSession::new(size, detect_caps(), established.link, Some(rebuild))
         .context("preparing the client session")?
         .with_identity(Identity {
@@ -218,8 +226,11 @@ async fn connect(target: &str, attach: Option<&str>, new: bool, name: Option<Nam
             LogFile::open_default(),
             target,
             &established.session_id,
         ));
+    if lobby {
+        session = session.with_lobby();
+    }
     // Spec §2.1: only a host that said it can park a standby is asked for
     // one. An older host would read the request as a stray line and drop it.
     // Created whatever `network.standby` says: the setting only switches it
     // (config spec §2.3), so it can be switched on from the screen.
@@ -409,46 +420,12 @@ where
         .context("reading the host's offer")?;
     establish_from(first, reader, writer, size, cfg, admit_remote).await
 }
 
-/// [`establish`] on a control stream after an `Open` that runs an attach
-/// exchange -- a `Switch`, a `New` in a session (switcher spec §4.1): the
-/// one answer is the exchange's `HostHello`, or a refusal, which is the
-/// host's words for the user ([`HostRefused`]).
-#[cfg_attr(
-    not(test),
-    expect(dead_code, reason = "the switcher's requests use it from Task 9 on")
-)]
-pub(crate) async fn establish_answering<R, W>(
-    reader: R,
-    writer: W,
-    size: TermSize,
-    cfg: &NetConfig,
-) -> Result<Established>
-where
-    R: tokio::io::AsyncBufRead + Unpin + Send,
-    W: tokio::io::AsyncWrite + Unpin + Send,
-{
-    let mut reader = reader;
-    match oxutrm_host::signalling::read_answer_async(&mut reader)
-        .await
-        .context("reading the host's answer")?
-    {
-        oxutrm_proto::Answer::Signal(first) => {
-            establish_from(first, reader, writer, size, cfg, None).await
-        }
-        oxutrm_proto::Answer::Reply(oxutrm_proto::Reply::Refused(why)) => {
-            Err(anyhow::Error::new(HostRefused(why)))
-        }
-        oxutrm_proto::Answer::Reply(other) => Err(anyhow::anyhow!(
-            "the host answered with {other:?} where an attach should have begun"
-        )),
-    }
-}
-
 /// L4 and L6 to L10, once the host's first line -- its hello, or why not --
-/// is in hand.
-async fn establish_from<R, W>(
+/// is in hand: from [`establish`], and from the switcher's `Switch` and
+/// `New`, whose first answer may be a refusal instead (`switcher::ask`).
+pub(crate) async fn establish_from<R, W>(
     first: Signal,
     reader: R,
     writer: W,
     size: TermSize,
````

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -41,8 +41,9 @@ mod rebuild;
 mod roam;
 mod serve;
 mod session;
 mod standby;
+mod switcher;
 mod ui;
 mod view;
 
 use std::io::{Read as _, Write as _};
````

`src/rebuild.rs`:

````diff
--- a/src/rebuild.rs
+++ b/src/rebuild.rs
@@ -332,9 +332,31 @@ impl Default for Ties {
         }
     }
 }
 
-/// One attempt at getting back into `session_id` on `target`.
+/// Where a rebuild goes (switcher spec §3.5): the session the client is in,
+/// or -- while it is in a lobby -- a fresh lobby. A lobby is never
+/// reattached: it ended after `DETACH_AFTER` of silence, or will.
+#[derive(Clone, Debug, PartialEq, Eq)]
+pub(crate) enum Aim {
+    Session(String),
+    Lobby,
+}
+
+impl Aim {
+    /// What the attempt answers the offer with.
+    fn choice(&self) -> Result<Choice, String> {
+        match self {
+            Aim::Session(id) => id
+                .parse()
+                .map(|id| Choice::Attach { id })
+                .map_err(|_| format!("{id:?} is not a session id")),
+            Aim::Lobby => Ok(Choice::Lobby),
+        }
+    }
+}
+
+/// One attempt at getting back to `aim` on `target`.
 ///
 /// `launcher` is the injection point, exactly as it is for
 /// [`SshChannel::open`]: production passes [`SshLauncher::ssh`] and the tests
 /// point it at a script that speaks the protocol on stdio. [`BATCH_MODE`] and
@@ -361,23 +383,14 @@ impl Default for Ties {
 /// cannot be nominated.
 pub(crate) async fn attempt(
     launcher: &SshLauncher,
     target: &str,
-    session_id: &str,
+    aim: &Aim,
     size: TermSize,
     cfg: &NetConfig,
     ties: &Ties,
 ) -> AttemptOutcome {
-    attempt_within(
-        ATTEMPT_DEADLINE,
-        launcher,
-        target,
-        session_id,
-        size,
-        cfg,
-        ties,
-    )
-    .await
+    attempt_within(ATTEMPT_DEADLINE, launcher, target, aim, size, cfg, ties).await
 }
 
 /// [`attempt`], with its outer bound as a parameter.
 ///
@@ -393,14 +406,14 @@ pub(crate) async fn attempt(
 async fn attempt_within(
     deadline: std::time::Duration,
     launcher: &SshLauncher,
     target: &str,
-    session_id: &str,
+    aim: &Aim,
     size: TermSize,
     cfg: &NetConfig,
     ties: &Ties,
 ) -> AttemptOutcome {
-    let body = one_attempt(launcher, target, session_id, size, cfg, ties);
+    let body = one_attempt(launcher, target, aim, size, cfg, ties);
     match tokio::time::timeout(deadline, body).await {
         Ok(outcome) => outcome,
         Err(_) => AttemptOutcome::Retry(format!(
             "the attempt to reach {target} timed out after {}s -- \
@@ -414,9 +427,9 @@ async fn attempt_within(
 /// what that puts the clock on.
 async fn one_attempt(
     launcher: &SshLauncher,
     target: &str,
-    session_id: &str,
+    aim: &Aim,
     size: TermSize,
     cfg: &NetConfig,
     ties: &Ties,
 ) -> AttemptOutcome {
@@ -445,12 +458,12 @@ async fn one_attempt(
     // the abandon ended this attempt's generation (see `Report`).
     if !ties.commitment.commit() {
         return AttemptOutcome::Retry("abandoned for the standby".to_owned());
     }
-    let Ok(id) = session_id.parse() else {
-        return AttemptOutcome::Definite(format!("{session_id:?} is not a session id"));
+    let choice = match aim.choice() {
+        Ok(choice) => choice,
+        Err(why) => return AttemptOutcome::Definite(why),
     };
-    let choice = Choice::Attach { id };
     if let Err(e) = channel.send(&Signal::Choose { choice }).await {
         return classify(target, &anyhow::Error::new(e));
     }
 
@@ -472,9 +485,9 @@ async fn one_attempt(
 /// link can drop it. Everything the loop selects on stays a local in `run_on`;
 /// this is only ever touched between laps.
 pub(crate) struct Rebuild {
     target: String,
-    session_id: String,
+    aim: Aim,
     /// How to start ssh. The injection point, as in [`attempt`].
     launcher: SshLauncher,
     cfg: NetConfig,
     in_flight: Option<tokio::task::JoinHandle<()>>,
@@ -505,11 +518,16 @@ pub(crate) struct Rebuild {
 }
 
 impl Rebuild {
     pub(crate) fn new(target: String, session_id: String) -> Rebuild {
+        Rebuild::aimed(target, Aim::Session(session_id))
+    }
+
+    /// A rebuild that goes to `aim`.
+    pub(crate) fn aimed(target: String, aim: Aim) -> Rebuild {
         Rebuild {
             target,
-            session_id,
+            aim,
             launcher: SshLauncher::ssh(),
             cfg: NetConfig::default(),
             in_flight: None,
             generation: 0,
@@ -520,8 +538,21 @@ impl Rebuild {
             displacing: false,
         }
     }
 
+    /// Where the next attempt goes, after a switch, a new session or a kill
+    /// moved the client (switcher spec §3.5). An attempt already running
+    /// keeps its own.
+    pub(crate) fn retarget(&mut self, aim: Aim) {
+        self.aim = aim;
+    }
+
+    /// Where the next attempt goes.
+    #[cfg(test)]
+    pub(crate) fn aim(&self) -> &Aim {
+        &self.aim
+    }
+
     /// The network settings and the connect timeout the next attempt runs
     /// with, from `apply`. An attempt already running keeps its own.
     pub(crate) fn retune(&mut self, cfg: NetConfig, connect_timeout: std::time::Duration) {
         self.cfg = cfg;
@@ -641,9 +672,9 @@ impl Rebuild {
         self.generation = self.generation.wrapping_add(1);
         let generation = self.generation;
         let launcher = self.launcher.clone();
         let target = self.target.clone();
-        let session_id = self.session_id.clone();
+        let aim = self.aim.clone();
         let cfg = self.cfg.clone();
         let ties = Ties {
             commitment: Commitment::default(),
             bound: self.bound.clone(),
@@ -651,9 +682,9 @@ impl Rebuild {
         };
         self.commitment = ties.commitment.clone();
         self.displacing = true;
         self.in_flight = Some(tokio::spawn(async move {
-            let outcome = attempt(&launcher, &target, &session_id, size, &cfg, &ties).await;
+            let outcome = attempt(&launcher, &target, &aim, size, &cfg, &ties).await;
             // A closed receiver means the session this was for has ended.
             // There is nobody to tell, and that is not a failure.
             let _ = outcomes
                 .send(Report {
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -136,8 +136,10 @@ enum Wake {
     /// A standby search or probe reported back.
     Standby(crate::standby::StandbyEvent),
     /// The standby's connection closed, with this reason.
     StandbyClosed(quinn::ConnectionError),
+    /// A switcher request was answered.
+    Answered(crate::switcher::Answered),
     Closed(quinn::ConnectionError),
     /// A readiness that turned out to be nothing. Costs one lap.
     Nothing,
 }
@@ -236,8 +238,17 @@ pub const SUPERSEDED: &[u8] = b"superseded by a newer standby";
 /// is already carrying the session. It exists so that a `close` here is as
 /// legible in a packet trace as [`TAKEN_OVER`] is.
 pub const REBUILT: &[u8] = b"replaced by a rebuilt link";
 
+/// Why the client closed a link of its own: it moved to another session
+/// (switcher spec §4.3). The session it left detaches quietly, as after any
+/// client loss; a lobby ends.
+pub const MOVED_AWAY: &[u8] = b"the client moved to another session";
+
+/// Why a client in a lobby closed its link: the user quit. The lobby ends
+/// with its link rather than after `DETACH_AFTER`.
+pub const QUIT: &[u8] = b"the client quit";
+
 /// What `conn` reports right now, as plain values for [`Quality`].
 fn reading_of(conn: &quinn::Connection) -> Reading {
     let stats = conn.stats();
     Reading {
@@ -364,8 +375,19 @@ pub struct ClientSession {
     config_warned: std::collections::HashSet<String>,
     /// `network.standby` as last applied. The standby itself is switched by
     /// the loop, which owns the tasks it stops (`standby_switch`).
     standby_wanted: bool,
+    /// In a lobby: the far end has no shell (switcher spec §2.1). No standby
+    /// is searched for, and a rebuild aims at a fresh lobby.
+    in_lobby: bool,
+    /// A request for the host, waiting for the loop to send it.
+    asking: Option<crate::switcher::Ask>,
+    /// The request in flight, which its answer is read against.
+    asked: Option<crate::switcher::Ask>,
+    /// The host's sessions, as last fetched.
+    sessions: Vec<oxutrm_proto::SessionEntry>,
+    /// What an attach exchange of the switcher's runs with: `apply`'s.
+    net: oxutrm_net::NetConfig,
 }
 
 /// The startup splash while it shows; the picture is
 /// [`oxutrm_client::splash`]'s.
@@ -511,8 +533,13 @@ impl ClientSession {
             unpainted_splash_end: false,
             config: ConfigState::defaults(),
             config_warned: std::collections::HashSet::new(),
             standby_wanted: true,
+            in_lobby: false,
+            asking: None,
+            asked: None,
+            sessions: Vec::new(),
+            net: oxutrm_net::NetConfig::default(),
         })
     }
 
     /// Open with the startup splash, drawn from `seed`, with `caption` under
@@ -626,8 +653,195 @@ impl ClientSession {
         self.identity = Some(id);
         self
     }
 
+    /// Start in a lobby: the connect chose `Lobby` (switcher spec §3.1).
+    pub(crate) fn with_lobby(mut self) -> ClientSession {
+        self.in_lobby = true;
+        self
+    }
+
+    /// Whether the far end is a lobby.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the selector uses it from Task 12 on")
+    )]
+    pub(crate) fn in_lobby(&self) -> bool {
+        self.in_lobby
+    }
+
+    /// Ask the host `a` on the loop's next lap. One at a time: `false`, and
+    /// nothing asked, while another is waiting or in flight.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the selector uses it from Task 12 on")
+    )]
+    pub(crate) fn ask(&mut self, a: crate::switcher::Ask) -> bool {
+        if self.asking.is_some() || self.asked.is_some() {
+            return false;
+        }
+        self.asking = Some(a);
+        true
+    }
+
+    /// Whether a request is waiting or in flight.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the selector uses it from Task 12 on")
+    )]
+    pub(crate) fn asking(&self) -> bool {
+        self.asking.is_some() || self.asked.is_some()
+    }
+
+    /// The request to send now, if one is waiting; from here it is in
+    /// flight.
+    fn take_ask(&mut self) -> Option<crate::switcher::Ask> {
+        let a = self.asking.take()?;
+        self.asked = Some(a.clone());
+        Some(a)
+    }
+
+    /// The host's sessions, as last fetched.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the selector uses it from Task 12 on")
+    )]
+    pub(crate) fn sessions(&self) -> &[oxutrm_proto::SessionEntry] {
+        &self.sessions
+    }
+
+    /// How the activity log names session `id`: its name from the last
+    /// fetched list, else the start of its id.
+    fn label_of(&self, id: &oxutrm_proto::SessionId) -> String {
+        self.sessions
+            .iter()
+            .find(|e| e.id == *id)
+            .and_then(|e| e.name.as_ref())
+            .map_or_else(|| id.short(), |n| oxutrm_client::legible(n.as_str()))
+    }
+
+    /// The session this client is in, as an id.
+    fn here(&self) -> Option<oxutrm_proto::SessionId> {
+        self.identity.as_ref()?.session_id.parse().ok()
+    }
+
+    /// Point a rebuild at where the client now is.
+    fn aim_rebuild(&mut self) {
+        let aim = match (self.in_lobby, &self.identity) {
+            (true, _) => crate::rebuild::Aim::Lobby,
+            (false, Some(id)) => crate::rebuild::Aim::Session(id.session_id.clone()),
+            (false, None) => return,
+        };
+        if let Some(r) = self.rebuild.as_mut() {
+            r.retarget(aim);
+        }
+    }
+
+    /// What came of the request in flight. True when the link the session
+    /// runs on, or its standby, changed under the loop -- a switch, a new
+    /// sibling, a kill into a lobby -- and the loop must follow.
+    pub(crate) fn answered(&mut self, a: crate::switcher::Answered, now: Instant) -> Result<bool> {
+        use crate::switcher::{Answered, Ask};
+        let asked = self.asked.take();
+        match a {
+            Answered::Sessions(list) => {
+                self.sessions = list;
+                Ok(false)
+            }
+            Answered::Landed(e) => {
+                let label = match &asked {
+                    Some(Ask::Switch { to }) => format!("switched to {}", self.label_of(to)),
+                    Some(Ask::New { name: Some(n) }) => {
+                        format!("new session {}", oxutrm_client::legible(n.as_str()))
+                    }
+                    _ => format!("new session {}", &e.session_id[..8.min(e.session_id.len())]),
+                };
+                self.moved(*e, now)?;
+                self.activity.record_shown(Kind::Session, &label, &label);
+                Ok(true)
+            }
+            Answered::Started(entry) => {
+                self.in_lobby = false;
+                if let Some(id) = self.identity.as_mut() {
+                    id.session_id = entry.id.to_string();
+                }
+                self.aim_rebuild();
+                let label = match &entry.name {
+                    Some(n) => format!("new session {}", oxutrm_client::legible(n.as_str())),
+                    None => format!("new session {}", entry.id.short()),
+                };
+                self.activity.record_shown(Kind::Session, &label, &label);
+                Ok(false)
+            }
+            Answered::Killed(id) => {
+                let label = format!("killed {}", self.label_of(&id));
+                self.activity.record_shown(Kind::Session, &label, &label);
+                if Some(id) != self.here() {
+                    return Ok(false);
+                }
+                // This session's shell is gone, and the process is a lobby
+                // now (switcher spec §3.4): no standby there, and a rebuild
+                // goes to a fresh lobby.
+                self.in_lobby = true;
+                self.aim_rebuild();
+                if let Some(s) = self.standby.as_mut() {
+                    s.forget(now);
+                }
+                Ok(true)
+            }
+            Answered::Renamed(entry) => {
+                let label = match &entry.name {
+                    Some(n) => format!(
+                        "renamed {} to {}",
+                        entry.id.short(),
+                        oxutrm_client::legible(n.as_str())
+                    ),
+                    None => format!("{} has no name now", entry.id.short()),
+                };
+                self.activity.record_shown(Kind::Session, &label, &label);
+                Ok(false)
+            }
+            Answered::Failed(why) => {
+                let what = match &asked {
+                    Some(Ask::Sessions) => "listing the sessions".to_string(),
+                    Some(Ask::Switch { to }) => format!("switching to {}", self.label_of(to)),
+                    Some(Ask::New { .. }) => "starting a new session".to_string(),
+                    Some(Ask::Kill { id }) => format!("killing {}", self.label_of(id)),
+                    Some(Ask::Rename { id, .. }) => format!("renaming {}", self.label_of(id)),
+                    None => "a request".to_string(),
+                };
+                self.activity.record_shown(
+                    Kind::Session,
+                    &format!("{what} failed: {why}"),
+                    &format!("{what} failed"),
+                );
+                Ok(false)
+            }
+        }
+    }
+
+    /// Move to the session `e` reached, make-before-break (switcher spec
+    /// §3.3): the new link is up, so it is adopted, the old one closed as
+    /// [`MOVED_AWAY`], the old link's parked standby dropped, the screen
+    /// started over, and the rebuild pointed at the new session. A rebuild
+    /// attempt still running was for the old one and is stood down.
+    fn moved(&mut self, e: crate::connect::Established, now: Instant) -> Result<()> {
+        if let Some(r) = self.rebuild.as_mut() {
+            r.stood_down();
+        }
+        self.swap_in_as(e.link, now, MOVED_AWAY)?;
+        if let Some(s) = self.standby.as_mut() {
+            s.forget(now);
+        }
+        self.in_lobby = false;
+        if let Some(id) = self.identity.as_mut() {
+            id.session_id = e.session_id.clone();
+        }
+        self.aim_rebuild();
+        self.path = Some(e.path);
+        Ok(())
+    }
+
     /// Keep the activity log in `activity` -- the one with the file, which
     /// `connect` opens -- instead of the ring-only log `new` starts with.
     pub(crate) fn with_activity(mut self, activity: Activity) -> ClientSession {
         self.activity = activity;
@@ -667,8 +881,9 @@ impl ClientSession {
             .retune(s.popup_key, s.linger, s.effective_auto_open());
         self.link_state
             .retune(s.silent_after, s.effective_rebuild_after());
         let cfg = s.net_config();
+        self.net = cfg.clone();
         if let Some(r) = self.rebuild.as_mut() {
             r.retune(cfg.clone(), s.connect_timeout);
         }
         if let Some(st) = self.standby.as_mut() {
@@ -1077,9 +1292,19 @@ impl ClientSession {
         if !routed.to_hold.is_empty() {
             self.link_state.hold_keys(&routed.to_hold);
         }
         match routed.command {
-            Some(Command::Quit) => return Ok(Some(0)),
+            Some(Command::Quit) => {
+                // A lobby ends with its link: closed here, it does not wait
+                // out `DETACH_AFTER`. A session detaches, as it always did.
+                if self.in_lobby {
+                    self.link
+                        .sink
+                        .connection()
+                        .close(quinn::VarInt::from_u32(0), QUIT);
+                }
+                return Ok(Some(0));
+            }
             Some(Command::SendHeld) => {
                 let held = self.link_state.take_held();
                 self.activity.record(
                     Kind::Input,
@@ -1503,9 +1728,11 @@ impl ClientSession {
         phase: Phase,
         now: Instant,
         rebuild: RebuildStage,
     ) -> crate::standby::StandbyAction {
-        let Some(s) = self.standby.as_mut() else {
+        // Searched for only in a session, never in a lobby (switcher spec
+        // §2.1): a lobby has nothing a second path would keep alive.
+        let Some(s) = self.standby.as_mut().filter(|_| !self.in_lobby) else {
             return crate::standby::StandbyAction::Nothing;
         };
         let first_probe = s.probe() == crate::linkstate::ProbeState::Idle;
         let action = s.step(phase, now, rebuild);
@@ -1887,8 +2114,14 @@ impl ClientSession {
         // would go on holding the primary and a socket of its own.
         let mut _search_task: Option<crate::attach_exchange::AbortOnDrop> = None;
         let mut _probe_task: Option<crate::attach_exchange::AbortOnDrop> = None;
 
+        // Where a switcher request reports back, and the request in flight:
+        // locals for the reason `outcomes` is one (C1). One deep: one
+        // request at a time.
+        let (answers_tx, mut answers) = tokio::sync::mpsc::channel::<crate::switcher::Answered>(1);
+        let mut _ask_task: Option<crate::attach_exchange::AbortOnDrop> = None;
+
         let mut winch =
             tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())
                 .context("watching for window size changes")?;
 
@@ -1938,8 +2171,9 @@ impl ClientSession {
                 () = tokio::time::sleep_until(deadline) => Wake::Due,
                 () = splash_due(splash_at) => Wake::SplashFrame,
                 Some(report) = outcomes.recv() => Wake::Rebuilt(report),
                 Some(event) = standby_rx.recv() => Wake::Standby(event),
+                Some(a) = answers.recv() => Wake::Answered(a),
                 // Quiet until the standby goes away; a closed connection is
                 // ready for ever, which is why the handler disarms it.
                 reason = async { standby_conn.as_ref().expect("armed").closed().await },
                     if standby_conn.is_some() => Wake::StandbyClosed(reason),
@@ -2027,8 +2261,21 @@ impl ClientSession {
                 Wake::StandbyClosed(reason) => {
                     standby_conn = None;
                     self.on_standby_closed(&reason, Instant::now());
                 }
+                Wake::Answered(a) => {
+                    _ask_task = None;
+                    if self.answered(a, Instant::now())? {
+                        // As after a landed rebuild: the arm watches the new
+                        // connection, the old one was closed on purpose, and
+                        // the standby -- the old session's -- is gone.
+                        conn = self.link.sink.connection().clone();
+                        takeover_expected = false;
+                        standby_conn = None;
+                        _search_task = None;
+                        _probe_task = None;
+                    }
+                }
                 // The link is gone, but what already arrived over it is not.
                 // Paint it before answering, or `ls; exit` shows the user
                 // nothing at all.
                 Wake::Closed(reason) => {
@@ -2102,8 +2349,20 @@ impl ClientSession {
             {
                 s.route_moved(now);
             }
 
+            // A switcher request, on its own stream of the live link.
+            if let Some(ask) = self.take_ask() {
+                let primary = self.link.sink.connection().clone();
+                let (size, cfg) = (self.size, self.net.clone());
+                let tx = answers_tx.clone();
+                let task = tokio::spawn(async move {
+                    let a = crate::switcher::ask(primary, ask, size, &cfg).await;
+                    let _ = tx.send(a).await;
+                });
+                _ask_task = Some(crate::attach_exchange::AbortOnDrop(task.abort_handle()));
+            }
+
             // The `bool` is for the tests, which hold the clock still and ask
             // whether a prod was due. The loop does not care: it prods or it
             // does not, and either way the next lap is the same.
             let _ = self.heartbeat(now);
````

`src/switcher.rs` (new):

````rust
//! The client's half of the session switcher's requests (switcher spec
//! §3.3, §3.4): one control stream per request on the live link, its
//! `Open`, and the one answer -- or, for a switch or a new sibling, the
//! attach exchange that follows.
//!
//! Nothing here changes the session. [`ask`] runs on a task of the loop's
//! and comes back as [`Answered`]; `ClientSession::answered` acts on it, so
//! make-before-break is structural: the old link carries the session until
//! a new one is `Established` and handed over whole.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::Duration;

use oxutrm_host::signalling::{read_answer_async, write_line_async};
use oxutrm_net::NetConfig;
use oxutrm_proto::{Answer, Name, Open, Reply, Request, SessionEntry, SessionId, TermSize};

use crate::connect::Established;

/// What the selector asks the host.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the selector asks from Task 12 on")
)]
pub(crate) enum Ask {
    Sessions,
    Switch { to: SessionId },
    New { name: Option<Name> },
    Kill { id: SessionId },
    Rename { id: SessionId, name: Option<Name> },
}

/// What came of an [`Ask`].
pub(crate) enum Answered {
    /// The host's sessions, oldest first.
    Sessions(Vec<SessionEntry>),
    /// A switch or a new sibling: a link to another session, up and ready
    /// to be swapped in.
    Landed(Box<Established>),
    /// A lobby started its shell and is this session now.
    Started(SessionEntry),
    /// The session is gone: its shell reaped, its entry removed.
    Killed(SessionId),
    Renamed(SessionEntry),
    /// Refused or failed, in words for the user.
    Failed(String),
}

impl std::fmt::Debug for Answered {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Answered::Sessions(l) => f.debug_tuple("Sessions").field(l).finish(),
            Answered::Landed(e) => f.debug_tuple("Landed").field(&e.session_id).finish(),
            Answered::Started(e) => f.debug_tuple("Started").field(e).finish(),
            Answered::Killed(id) => f.debug_tuple("Killed").field(id).finish(),
            Answered::Renamed(e) => f.debug_tuple("Renamed").field(e).finish(),
            Answered::Failed(why) => f.debug_tuple("Failed").field(why).finish(),
        }
    }
}

/// How long a request that runs no attach exchange may take to be answered.
/// A kill waits out a shell's grace and its reaping on the host (3 s, then
/// up to 2 s more), a sibling's door adds its own bound: this sits clear of
/// them all. The QUIC idle timeout is off, so without it a request sent
/// into a path that has just gone dark would wait for ever.
pub(crate) const ANSWER_WITHIN: Duration = Duration::from_secs(15);

/// Ask `ask` over a fresh control stream of `conn`. `size` and `cfg` are
/// what an attach exchange runs with, for a switch or a new sibling.
pub(crate) async fn ask(
    conn: quinn::Connection,
    ask: Ask,
    size: TermSize,
    cfg: &NetConfig,
) -> Answered {
    let attaches = matches!(ask, Ask::Switch { .. } | Ask::New { .. });
    let bound = if attaches {
        crate::control::SEARCH_DEADLINE
    } else {
        ANSWER_WITHIN
    };
    match tokio::time::timeout(bound, asked(conn, ask, size, cfg)).await {
        Ok(Ok(answered)) => answered,
        Ok(Err(e)) => Answered::Failed(format!("{e:#}")),
        Err(_) => Answered::Failed("the host did not answer in time".to_string()),
    }
}

async fn asked(
    conn: quinn::Connection,
    ask: Ask,
    size: TermSize,
    cfg: &NetConfig,
) -> anyhow::Result<Answered> {
    use anyhow::Context as _;
    let req = match &ask {
        Ask::Sessions => Request::Sessions,
        Ask::Switch { to } => Request::Switch { to: *to },
        Ask::New { name } => Request::New { name: name.clone() },
        Ask::Kill { id } => Request::Kill { id: *id },
        Ask::Rename { id, name } => Request::Rename {
            id: *id,
            name: name.clone(),
        },
    };
    let (mut send, recv) = conn.open_bi().await.context("opening a control stream")?;
    write_line_async(&mut send, &Open::new(req))
        .await
        .context("asking the host")?;
    let mut recv = tokio::io::BufReader::new(recv);
    let answer = read_answer_async(&mut recv)
        .await
        .context("reading the host's answer")?;
    Ok(match (answer, ask) {
        (Answer::Reply(Reply::Refused(why)), _) => Answered::Failed(why),
        // The attach exchange's own first line: the rest of it follows on
        // this stream, run exactly as a first connect runs it.
        (Answer::Signal(first), Ask::Switch { .. } | Ask::New { .. }) => {
            let e = crate::connect::establish_from(first, recv, send, size, cfg, None).await?;
            Answered::Landed(Box::new(e))
        }
        (Answer::Reply(Reply::Sessions(list)), Ask::Sessions) => Answered::Sessions(list),
        (Answer::Reply(Reply::Entry(e)), Ask::New { .. }) => Answered::Started(e),
        (Answer::Reply(Reply::Done), Ask::Kill { id }) => Answered::Killed(id),
        (Answer::Reply(Reply::Entry(e)), Ask::Rename { .. }) => Answered::Renamed(e),
        (other, ask) => Answered::Failed(format!(
            "the host answered {ask:?} with {}",
            match other {
                Answer::Reply(r) => format!("{r:?}"),
                Answer::Signal(_) => "an attach exchange".to_string(),
            }
        )),
    })
}
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo build --bin oxutrm --jobs 4 && cargo test --bin oxutrm --jobs 4 rebuild:: session::tests serve::`

Expected: all pass, among them `an_attempt_from_a_lobby_asks_for_a_fresh_lobby`, `a_rebuild_is_retargeted_by_a_move`, `a_switch_moves_the_client_and_the_old_session_stays_listed`, `a_switch_that_fails_leaves_the_client_where_it_was_and_says_why`, `killing_this_session_puts_the_client_in_the_lobby_and_new_takes_it_out`, `quitting_in_a_lobby_ends_the_lobby_with_the_link`, `no_standby_is_searched_for_in_a_lobby`, `a_rename_is_recorded_and_a_refused_one_says_why`.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add src/activity.rs src/connect.rs src/main.rs src/rebuild.rs src/serve.rs src/session.rs src/switcher.rs
git commit -m "feat(client): switcher requests over the control stream; switch make-before-break, lobby, rebuild aim

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 10: The selector's state and keys, as the popup's `Sessions` mode

Spec §3.2. `src/selector.rs` is pure: the host's list (`set_rows`, ordered by start time, `+ new session` after it; the selection starts on `this`, else the first row, and a refetch keeps it on the same session), the cursor, a question with the time it was asked (`ANSWER_GUARD` applies), the rename field, and the line under the list. `keys` reads one read's bytes (arrows in both encodings, `j`/`k`) and stops at the first that produces an `Out`: `Ask(..)`, `Back`, `Close`, `Quit`.

- `⏎` on `this` closes the popup (**judgement call**: the spec says "closes the selector"; the user is where they want to be). On `+`, or `n` anywhere: `New { name: None }`. On a session not detachable or of another version: refused with the reason. On one `in use`: `take over a3f9c01e from its other client? y/n`. Otherwise: `Switch`.
- `r`: the name in place, starting from the current one; `⏎` saves (empty clears it; a name `Name::parse` refuses keeps the field, with the rule under the list); `Esc` cancels.
- `x`: `kill build? y/n`, or `kill a3f9c01e (in use elsewhere)? y/n`; another version cannot be killed from here.
- `q`/`Esc`: back to the popup -- or, in a lobby, quit.
- While the link is not `Live`, or a request is in flight, an action is refused with `NOT_LIVE` / `BUSY` under the list; moving still works.

The config screen's line editing becomes `ui::Field`, shared by both (spec §3.2: "the config screen's line editing"). `Mode::Sessions` holds the selector in `Ui` beside the field. `s` on the status view opens it on a live link and returns `Command::Ask(Ask::Sessions)`, the fetch; while the link is down it only touches the popup. The popup key on the selector closes the popup -- except in a lobby, where it does nothing (**judgement call**: there is nothing behind a lobby's selector) -- and never while a name is typed. The held-input question takes the selector down to the status view, as it does the config screen (**judgement call**). `s` and `c` now act on the status view, so two `ui` tests that typed `ls` into the open popup type `pw`/`pwd -P` instead.

The session forwards `Command::Ask` to `ClientSession::ask`. `set_switcher`, `selector_mut` and `close_popup` have no caller until Task 12 (their `expect(dead_code)` comes off there); `selector` is used from Task 11; `mod selector` carries `#[allow(dead_code)]` until Task 12.

Review focus 4 is pinned here: `the_popup_key_while_renaming_is_neither_a_close_nor_a_character`.

**Files:**
- Create: `src/selector.rs`
- Modify: `src/ui.rs`, `src/session.rs` (`Command::Ask`), `src/switcher.rs` (an `expect` comes off), `src/main.rs`

**Interfaces:**
- Consumes: Task 9's `Ask`, `ClientSession::ask`; Task 2's `SessionEntry`, `Attached`, `Name`.
- Produces:
  - `crate::selector::{Selector, Out { Ask(Ask), Back, Close, Quit }, Question { TakeOver { id, label }, Kill { id, label, in_use } }, Ctx { live, lobby, busy }, NOT_LIVE, BUSY, label(&SessionEntry) -> String}`; `Selector::{open, set_rows(Vec<SessionEntry>), fail(String), rows, cursor, loading, question, rename -> Option<&str>, note, keys(&[u8], Ctx, Instant) -> Option<Out>}`; `Question::text`; test fixtures `selector::fixtures::{entry, three, BUILD, FISH, LOGS}`.
  - `crate::ui::{Field (as_str, set, clear, erase, type_byte), Key (pub(crate)), escape (pub(crate)), Mode::Sessions, Command::Ask(Ask)}`; `Ui::{set_switcher(lobby, busy), open_sessions(), selector(), selector_mut(), close_popup()}`.

- [ ] **Step 1: Write the failing tests**

`src/selector.rs` (new): this test module is the end of the file; the code above it comes in the implementation step. Create the file with it now.

````rust
#[cfg(test)]
pub(crate) mod fixtures {
    use oxutrm_proto::{Attached, Name, SessionEntry, TermSize};

    /// A session as a host lists it: real-looking ids, names, shells and
    /// sizes, because tiny dummy values once hid a cut-off column.
    pub(crate) fn entry(
        id: &str,
        name: Option<&str>,
        shell: &str,
        created_unix: u64,
        attached: Attached,
    ) -> SessionEntry {
        SessionEntry {
            id: id.parse().unwrap(),
            name: name.map(|n| Name::parse(n).unwrap()),
            shell: shell.to_string(),
            created_unix,
            size: TermSize {
                cols: 120,
                rows: 40,
            },
            detachable: true,
            attached,
            this: attached == Attached::Here,
        }
    }

    pub(crate) const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    pub(crate) const FISH: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
    pub(crate) const LOGS: &str = "5d2e8a7c1b9f40e3a6c8d1f2b3e4a5c6";

    /// The spec's picture: `build` (this), an unnamed fish session in use
    /// elsewhere, `logs` detached.
    pub(crate) fn three() -> Vec<SessionEntry> {
        vec![
            entry(
                BUILD,
                Some("build"),
                "/bin/bash",
                1_791_450_840,
                Attached::Here,
            ),
            entry(
                FISH,
                None,
                "/usr/bin/fish",
                1_791_190_000,
                Attached::Elsewhere,
            ),
            entry(LOGS, Some("logs"), "/bin/zsh", 1_791_458_520, Attached::No),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::time::Duration;

    const LIVE: Ctx = Ctx {
        live: true,
        lobby: false,
        busy: false,
    };

    fn opened(rows: Vec<SessionEntry>) -> Selector {
        let mut s = Selector::default();
        s.open();
        s.set_rows(rows);
        s
    }

    fn id(s: &str) -> SessionId {
        s.parse().unwrap()
    }

    /// Past the question's guard.
    fn later(t: Instant) -> Instant {
        t + ANSWER_GUARD + Duration::from_millis(1)
    }

    #[test]
    fn rows_are_ordered_by_start_and_the_selection_starts_on_this() {
        let s = opened(three());
        let names: Vec<String> = s.rows().iter().map(label).collect();
        assert_eq!(names, ["a3f9c01e", "build", "logs"]);
        assert_eq!(s.cursor(), 1, "on build, which is this");
        assert!(!s.loading());
    }

    #[test]
    fn without_this_the_selection_starts_on_the_first_row() {
        let mut rows = three();
        rows.retain(|e| !e.this);
        let s = opened(rows);
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn arrows_and_j_k_move_within_the_rows_and_the_new_row() {
        let mut s = opened(three());
        let t = Instant::now();
        s.keys(b"\x1b[B", LIVE, t);
        s.keys(b"j", LIVE, t);
        assert_eq!(s.cursor(), 3, "the + new session row");
        s.keys(b"j", LIVE, t);
        assert_eq!(s.cursor(), 3, "and not past it");
        s.keys(b"\x1bOA", LIVE, t);
        s.keys(b"kkkk", LIVE, t);
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn enter_switches_creates_on_new_and_closes_on_this() {
        let t = Instant::now();
        let mut s = opened(three());
        assert_eq!(s.keys(b"\r", LIVE, t), Some(Out::Close), "on this");
        s.keys(b"j", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Switch { to: id(LOGS) }))
        );
        s.keys(b"j", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::New { name: None }))
        );
        assert_eq!(
            s.keys(b"n", LIVE, t),
            Some(Out::Ask(Ask::New { name: None }))
        );
    }

    #[test]
    fn a_session_in_use_asks_before_it_is_taken_over_and_the_guard_holds() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"k", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert_eq!(
            s.question().unwrap().text(),
            "take over a3f9c01e from its other client? y/n"
        );
        // Typing still in flight does not answer it.
        assert_eq!(s.keys(b"y", LIVE, t), None);
        assert!(s.question().is_some());
        assert_eq!(
            s.keys(b"y", LIVE, later(t)),
            Some(Out::Ask(Ask::Switch { to: id(FISH) }))
        );
        assert!(s.question().is_none());
    }

    #[test]
    fn x_asks_before_it_kills_and_n_or_esc_drop_the_question() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"jx", LIVE, t);
        assert_eq!(s.question().unwrap().text(), "kill logs? y/n");
        assert_eq!(s.keys(b"n", LIVE, later(t)), None);
        assert!(s.question().is_none());
        s.keys(b"kkx", LIVE, t);
        assert_eq!(
            s.question().unwrap().text(),
            "kill a3f9c01e (in use elsewhere)? y/n"
        );
        assert_eq!(s.keys(b"\x1b", LIVE, later(t)), None);
        assert!(s.question().is_none());
        s.keys(b"x", LIVE, t);
        assert_eq!(
            s.keys(b"y", LIVE, later(t)),
            Some(Out::Ask(Ask::Kill { id: id(FISH) }))
        );
    }

    #[test]
    fn r_edits_the_name_in_place() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"r", LIVE, t);
        assert_eq!(s.rename(), Some("build"), "starts from the name it has");
        s.keys(b"\x7f\x7f\x7f\x7f\x7fci", LIVE, t);
        assert_eq!(s.rename(), Some("ci"));
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Rename {
                id: id(BUILD),
                name: Some(Name::parse("ci").unwrap())
            }))
        );
        assert_eq!(s.rename(), None);

        // Nothing typed clears the name; Esc cancels.
        s.keys(b"r\x7f\x7f\x7f\x7f\x7f", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Rename {
                id: id(BUILD),
                name: None
            }))
        );
        s.keys(b"rxyz", LIVE, t);
        assert_eq!(s.keys(b"\x1b", LIVE, t), None);
        assert_eq!(s.rename(), None);
    }

    #[test]
    fn a_name_the_rules_refuse_keeps_the_field_and_says_why() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"jr", LIVE, t);
        s.keys(b"\x7f\x7f\x7f\x7fcafe", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert!(s.note().unwrap().contains("session id"), "{:?}", s.note());
        assert_eq!(s.rename(), Some("cafe"), "the field stays for a fix");
    }

    #[test]
    fn q_and_esc_go_back_to_the_popup_or_in_a_lobby_quit() {
        let t = Instant::now();
        let mut s = opened(three());
        assert_eq!(s.keys(b"q", LIVE, t), Some(Out::Back));
        assert_eq!(s.keys(b"\x1b", LIVE, t), Some(Out::Back));
        let lobby = Ctx {
            lobby: true,
            ..LIVE
        };
        assert_eq!(s.keys(b"q", lobby, t), Some(Out::Quit));
        assert_eq!(s.keys(b"\x1b", lobby, t), Some(Out::Quit));
    }

    #[test]
    fn a_lobby_with_no_sessions_offers_only_a_new_one() {
        let t = Instant::now();
        let lobby = Ctx {
            lobby: true,
            ..LIVE
        };
        let mut s = opened(vec![]);
        assert_eq!(s.cursor(), 0);
        assert_eq!(
            s.keys(b"\r", lobby, t),
            Some(Out::Ask(Ask::New { name: None }))
        );
        // `r` and `x` have no session to act on.
        assert_eq!(s.keys(b"rx", lobby, t), None);
        assert!(s.question().is_none() && s.rename().is_none());
    }

    #[test]
    fn actions_are_refused_while_the_link_is_down_or_a_request_is_out() {
        let t = Instant::now();
        let mut s = opened(three());
        let down = Ctx {
            live: false,
            ..LIVE
        };
        s.keys(b"j", down, t);
        assert_eq!(s.cursor(), 2, "moving still works");
        assert_eq!(s.keys(b"\r", down, t), None);
        assert_eq!(s.note(), Some(NOT_LIVE));
        let busy = Ctx { busy: true, ..LIVE };
        assert_eq!(s.keys(b"n", busy, t), None);
        assert_eq!(s.note(), Some(BUSY));
        // The note lasts one read.
        s.keys(b"k", LIVE, t);
        assert_eq!(s.note(), None);
    }

    #[test]
    fn a_session_that_cannot_be_switched_to_says_why() {
        let t = Instant::now();
        let mut rows = three();
        rows[2].detachable = false;
        rows.push(entry(
            "77c0ffee77c0ffee77c0ffee77c0ffee",
            Some("old"),
            "/bin/bash",
            1_791_460_000,
            Attached::OtherVersion,
        ));
        let mut s = opened(rows);
        s.keys(b"j", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert!(s.note().unwrap().contains("not detachable"));
        s.keys(b"j", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert!(s.note().unwrap().contains("another version"));
        assert_eq!(s.keys(b"x", LIVE, t), None);
        assert!(
            s.question().is_none(),
            "another version cannot be killed from here"
        );
    }

    #[test]
    fn a_refetch_keeps_the_selection_on_the_same_session() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"j", LIVE, t);
        assert_eq!(s.cursor(), 2);
        // `a3f9c01e` was killed: logs moved up one row, and stays selected.
        let mut rows = three();
        rows.remove(1);
        s.set_rows(rows);
        assert_eq!(s.rows()[s.cursor()].id, id(LOGS));
        // The selected session itself gone: the same place, clamped.
        let mut rows = three();
        rows.retain(|e| e.id != id(LOGS) && e.id != id(FISH));
        s.set_rows(rows);
        assert_eq!(s.cursor(), 1, "the + new session row");
    }

    #[test]
    fn a_failure_is_the_note_and_drops_a_question() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"x", LIVE, t);
        s.fail("session a3f9c01e did not answer in time".to_string());
        assert!(s.question().is_none());
        assert_eq!(s.note(), Some("session a3f9c01e did not answer in time"));
    }
}
````

`src/ui.rs`:

````diff
--- a/src/ui.rs
+++ b/src/ui.rs
@@ -1015,10 +1158,12 @@ mod tests {
     #[test]
     fn every_key_belongs_to_the_open_popup() {
         let t = Instant::now();
         let mut ui = open_at(t);
+        // No `s` and no `c` in it: those open the selector and the config
+        // screen.
         assert_eq!(
-            ui.keys(b"ls -l\r", Phase::Live, ms(t, 10)),
+            ui.keys(b"pwd -P\r", Phase::Live, ms(t, 10)),
             Routed::default()
         );
         assert_eq!(
             ui.mode(),
@@ -1087,32 +1232,45 @@ mod tests {
         );
     }
 
     #[test]
-    fn s_is_offered_later_and_does_nothing_now() {
+    fn s_opens_the_selector_on_a_live_link_and_asks_for_the_list() {
         let t = Instant::now();
         let mut ui = open_at(t);
-        assert_eq!(ui.keys(b"s", Phase::Live, t), Routed::default());
-        assert!(matches!(ui.mode(), Mode::Open { .. }));
+        assert_eq!(
+            ui.keys(b"s", Phase::Live, t),
+            command(Command::Ask(crate::switcher::Ask::Sessions))
+        );
+        assert_eq!(ui.mode(), Mode::Sessions);
+        assert!(ui.selector().loading(), "a fresh list is on its way");
+    }
+
+    #[test]
+    fn s_does_nothing_while_the_link_is_down() {
+        let t = Instant::now();
         let mut ui = auto_at(t);
         assert_eq!(ui.keys(b"s", silent(t), t), Routed::default());
+        assert!(
+            matches!(ui.mode(), Mode::Open { .. }),
+            "touched, not opened"
+        );
     }
-
     #[test]
     fn send_and_drop_are_commands_only_under_confirming() {
         let t = Instant::now();
         for (key, want) in [(b's', Command::SendHeld), (b'd', Command::DropHeld)] {
             assert_eq!(
                 confirming_at(t).keys(&[key], Phase::Confirming, t + ANSWER_GUARD),
-                command(want)
+                command(want.clone())
             );
             assert_eq!(
                 auto_at(t).keys(&[key], silent(t), t),
                 Routed::default(),
                 "{} was honoured under an outage",
                 key as char
             );
-            assert_eq!(open_at(t).keys(&[key], Phase::Live, t), Routed::default());
+            // On a live link `s` opens the selector: it is never `SendHeld`.
+            assert_ne!(open_at(t).keys(&[key], Phase::Live, t), command(want));
         }
     }
 
     #[test]
@@ -1357,9 +1515,9 @@ mod tests {
     fn a_lingering_popup_is_kept_by_any_key_and_closed_by_its_own() {
         let t = Instant::now();
         let mut ui = lingering_at(t);
         assert_eq!(
-            ui.keys(b"ls", Phase::Live, ms(t, 200)),
+            ui.keys(b"pw", Phase::Live, ms(t, 200)),
             Routed::default(),
             "typing reached the host"
         );
         assert_eq!(ui.mode(), Mode::Open { pressed: None });
@@ -2161,5 +2319,88 @@ mod tests {
             ui.keys(b"s", Phase::Confirming, t + ANSWER_GUARD),
             command(Command::SendHeld)
         );
     }
+
+    // ---- the session selector -------------------------------------------
+
+    fn sessions_at(t: Instant) -> Ui {
+        let mut ui = open_at(t);
+        ui.keys(b"s", Phase::Live, t);
+        ui.selector_mut()
+            .set_rows(crate::selector::fixtures::three());
+        ui
+    }
+
+    #[test]
+    fn the_selectors_keys_are_its_own_and_q_goes_back_to_the_popup() {
+        let t = Instant::now();
+        let mut ui = sessions_at(t);
+        assert_eq!(
+            ui.keys(b"j\r", Phase::Live, t),
+            command(Command::Ask(crate::switcher::Ask::Switch {
+                to: crate::selector::fixtures::LOGS.parse().unwrap()
+            }))
+        );
+        assert_eq!(ui.keys(b"q", Phase::Live, t), Routed::default());
+        assert!(matches!(ui.mode(), Mode::Open { .. }));
+    }
+
+    #[test]
+    fn in_a_lobby_q_quits_and_the_popup_key_does_not_close_it() {
+        let t = Instant::now();
+        let mut ui = Ui::new();
+        ui.set_switcher(true, false);
+        ui.open_sessions();
+        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
+        assert_eq!(
+            ui.mode(),
+            Mode::Sessions,
+            "nothing behind a lobby's selector"
+        );
+        assert_eq!(ui.keys(b"q", Phase::Live, t), command(Command::Quit));
+    }
+
+    #[test]
+    fn the_popup_key_closes_the_selector_in_a_session() {
+        let t = Instant::now();
+        let mut ui = sessions_at(t);
+        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
+        assert_eq!(ui.mode(), Mode::Closed);
+    }
+
+    #[test]
+    fn the_selector_stays_through_an_outage_and_refuses_its_actions() {
+        let t = Instant::now();
+        let mut ui = sessions_at(t);
+        ui.tick(silent(t), t);
+        ui.tick(silent(t), ms(t, 5_000));
+        assert_eq!(ui.mode(), Mode::Sessions, "an outage does not take it down");
+        assert_eq!(ui.keys(b"n", silent(t), ms(t, 5_000)), Routed::default());
+        assert_eq!(ui.selector().note(), Some(crate::selector::NOT_LIVE));
+    }
+
+    #[test]
+    fn a_request_in_flight_refuses_the_next() {
+        let t = Instant::now();
+        let mut ui = sessions_at(t);
+        ui.set_switcher(false, true);
+        assert_eq!(ui.keys(b"n", Phase::Live, t), Routed::default());
+        assert_eq!(ui.selector().note(), Some(crate::selector::BUSY));
+    }
+
+    #[test]
+    fn the_held_input_question_takes_the_selector_down() {
+        let t = Instant::now();
+        let mut ui = sessions_at(t);
+        ui.tick(Phase::Confirming, t);
+        assert!(matches!(ui.mode(), Mode::Open { .. }));
+    }
+
+    #[test]
+    fn closing_the_popup_from_the_session_closes_whatever_it_shows() {
+        let t = Instant::now();
+        let mut ui = sessions_at(t);
+        ui.close_popup();
+        assert_eq!(ui.mode(), Mode::Closed);
+    }
 }
````

`src/ui.rs` -- append inside `mod tests`, at its end (review focus 4):

````rust
    /// The popup key while a name is being typed: it neither closes the
    /// selector under the field nor ends up in the name.
    #[test]
    fn the_popup_key_while_renaming_is_neither_a_close_nor_a_character() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        ui.keys(b"r", Phase::Live, t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Sessions);
        assert_eq!(ui.selector().rename(), Some("build"));
    }
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test --bin oxutrm --jobs 4 selector:: ui::`

Expected: compile errors: `crate::selector`, `ui::Field`, `Mode::Sessions`, `Command::Ask` and `Ui::open_sessions` are not defined.

- [ ] **Step 3: Implement**

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -38,8 +38,11 @@ mod listener;
 mod loopback;
 mod quality;
 mod rebuild;
 mod roam;
+// The session drives it from Task 12 on, which removes this attribute.
+#[allow(dead_code)]
+mod selector;
 mod serve;
 mod session;
 mod standby;
 mod switcher;
````

`src/selector.rs` (new) (the test module from the tests step follows it):

````rust
//! The session selector's state and keys (switcher spec §3.2).
//!
//! Pure, like `ui`: every method takes what it needs -- the keys, whether
//! the link is live, whether the client is in a lobby, the time -- and the
//! session does everything that touches the host. The list is the host's,
//! set by [`Selector::set_rows`] whenever a fetch comes back; the keys come
//! back as an [`Out`]: a request to send, or where the popup goes.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::Instant;

use oxutrm_proto::{Attached, Name, SessionEntry, SessionId};

use crate::switcher::Ask;
use crate::ui::{ANSWER_GUARD, ESC, Field, Key};

/// What a key on the selector asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Out {
    /// Send this to the host.
    Ask(Ask),
    /// Back to the popup's status view.
    Back,
    /// Close the popup: `⏎` on the session the client is in.
    Close,
    /// End the client: `q` in a lobby.
    Quit,
}

/// A question the selector asks before it acts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Question {
    /// Switching to a session another client is attached to.
    TakeOver { id: SessionId, label: String },
    Kill {
        id: SessionId,
        label: String,
        in_use: bool,
    },
}

impl Question {
    /// The line the question is put in.
    pub(crate) fn text(&self) -> String {
        match self {
            Question::TakeOver { label, .. } => {
                format!("take over {label} from its other client? y/n")
            }
            Question::Kill {
                label,
                in_use: true,
                ..
            } => format!("kill {label} (in use elsewhere)? y/n"),
            Question::Kill { label, .. } => format!("kill {label}? y/n"),
        }
    }
}

/// What the selector needs to know about the session, read by read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ctx {
    /// The link is `Live`: requests can be sent.
    pub(crate) live: bool,
    /// In a lobby: `q` ends the client, and there is no popup to go back to.
    pub(crate) lobby: bool,
    /// A request is in flight: no second one until it is answered.
    pub(crate) busy: bool,
}

/// Why an action is not taken while the link is down.
pub(crate) const NOT_LIVE: &str = "the link is down; try again once it is back";
/// Why an action is not taken while another is under way.
pub(crate) const BUSY: &str = "waiting for the host to answer";

#[derive(Debug, Default)]
pub(crate) struct Selector {
    /// The host's sessions, oldest first.
    rows: Vec<SessionEntry>,
    /// A row of `rows`, or `rows.len()`: `+ new session`.
    cursor: usize,
    /// No list has come back since the selector opened.
    loading: bool,
    /// The question up, and when it was asked, for [`ANSWER_GUARD`].
    question: Option<(Question, Instant)>,
    /// The name being typed for the cursor's row, while `r` is open.
    rename: Option<Field>,
    /// The line under the list: why something was refused or failed.
    note: Option<String>,
}

impl Selector {
    /// Opened afresh: whatever the last list said is gone until the next
    /// one arrives, and the selection will start on this session.
    pub(crate) fn open(&mut self) {
        *self = Selector {
            loading: true,
            ..Selector::default()
        };
    }

    /// A fetched list. Ordered by start time, `+ new session` last. The
    /// first after opening puts the selection on `this`, else the first
    /// row; a later one keeps it on the same session if it is still there.
    pub(crate) fn set_rows(&mut self, mut rows: Vec<SessionEntry>) {
        rows.sort_by_key(|e| e.created_unix);
        let at = match (self.loading, self.selected()) {
            (true, _) => rows.iter().position(|e| e.this).unwrap_or(0),
            (false, Some(id)) => rows
                .iter()
                .position(|e| e.id == id)
                .unwrap_or(self.cursor.min(rows.len())),
            (false, None) => rows.len(),
        };
        self.rows = rows;
        self.cursor = at;
        self.loading = false;
    }

    /// Something failed: say why under the list. A question still up is
    /// dropped with it.
    pub(crate) fn fail(&mut self, why: String) {
        self.question = None;
        self.note = Some(why);
    }

    pub(crate) fn rows(&self) -> &[SessionEntry] {
        &self.rows
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    pub(crate) fn question(&self) -> Option<&Question> {
        self.question.as_ref().map(|(q, _)| q)
    }

    /// The name being typed, while `r` is open.
    pub(crate) fn rename(&self) -> Option<&str> {
        self.rename.as_ref().map(Field::as_str)
    }

    pub(crate) fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The session the cursor is on; `None` on `+ new session`.
    fn selected(&self) -> Option<SessionId> {
        self.rows.get(self.cursor).map(|e| e.id)
    }

    /// One read's bytes. Stops at the first key that produces an [`Out`]:
    /// the rest of the read is dropped, as on the popup.
    pub(crate) fn keys(&mut self, bytes: &[u8], ctx: Ctx, now: Instant) -> Option<Out> {
        // A note lasts until the next read.
        self.note = None;
        let lone_esc = bytes == [ESC];
        let mut rest = bytes;
        while let Some((&b, tail)) = rest.split_first() {
            let key = if b == ESC && !lone_esc {
                let (key, used) = crate::ui::escape(tail);
                rest = &tail[used..];
                match key {
                    Some(k) => k,
                    // Any other sequence -- a function key, a paste's
                    // markers -- means nothing here.
                    None => continue,
                }
            } else {
                rest = tail;
                Key::Byte(b)
            };
            if let Some(out) = self.key(key, ctx, now) {
                return Some(out);
            }
        }
        None
    }

    /// One key.
    fn key(&mut self, key: Key, ctx: Ctx, now: Instant) -> Option<Out> {
        if self.question.is_some() {
            return self.answer(key, ctx, now);
        }
        if self.rename.is_some() {
            return self.rename_key(key, ctx);
        }
        let last = self.rows.len();
        match key {
            Key::Up | Key::Byte(b'k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Byte(b'j') => self.cursor = (self.cursor + 1).min(last),
            Key::Byte(b'\r' | b'\n') => return self.enter(ctx, now),
            Key::Byte(b'n') => return self.act(ctx, Ask::New { name: None }),
            Key::Byte(b'r') => {
                let row = self.rows.get(self.cursor)?;
                if row.attached == Attached::OtherVersion {
                    self.note = Some(other_version(row));
                } else {
                    let mut field = Field::default();
                    field.set(row.name.as_ref().map(Name::to_string).unwrap_or_default());
                    self.rename = Some(field);
                }
            }
            Key::Byte(b'x') => {
                let row = self.rows.get(self.cursor)?;
                if row.attached == Attached::OtherVersion {
                    self.note = Some(other_version(row));
                } else {
                    let question = Question::Kill {
                        id: row.id,
                        label: label(row),
                        in_use: row.attached == Attached::Elsewhere,
                    };
                    self.question = Some((question, now));
                }
            }
            Key::Byte(b'q') | Key::Byte(ESC) => {
                return Some(if ctx.lobby { Out::Quit } else { Out::Back });
            }
            _ => {}
        }
        None
    }

    /// `⏎` on the cursor's row.
    fn enter(&mut self, ctx: Ctx, now: Instant) -> Option<Out> {
        let Some(row) = self.rows.get(self.cursor) else {
            return self.act(ctx, Ask::New { name: None });
        };
        if row.this {
            return Some(Out::Close);
        }
        if !row.detachable {
            self.note = Some(format!(
                "{} is not detachable: it tunnels its data through the ssh that \
                 created it, so it cannot be reattached",
                label(row)
            ));
            return None;
        }
        if row.attached == Attached::OtherVersion {
            self.note = Some(other_version(row));
            return None;
        }
        if row.attached == Attached::Elsewhere {
            let question = Question::TakeOver {
                id: row.id,
                label: label(row),
            };
            self.question = Some((question, now));
            return None;
        }
        let to = row.id;
        self.act(ctx, Ask::Switch { to })
    }

    /// A key while a question is up: `y` acts, `n` and `Esc` drop it, and
    /// anything else -- or anything before [`ANSWER_GUARD`] has passed,
    /// which is typing still in flight -- does nothing.
    fn answer(&mut self, key: Key, ctx: Ctx, now: Instant) -> Option<Out> {
        let (question, asked) = self.question.as_ref()?;
        if now.saturating_duration_since(*asked) < ANSWER_GUARD {
            return None;
        }
        match key {
            Key::Byte(b'y') => {
                let ask = match question {
                    Question::TakeOver { id, .. } => Ask::Switch { to: *id },
                    Question::Kill { id, .. } => Ask::Kill { id: *id },
                };
                self.question = None;
                self.act(ctx, ask)
            }
            Key::Byte(b'n') | Key::Byte(ESC) => {
                self.question = None;
                None
            }
            _ => None,
        }
    }

    /// A key while a name is typed in place: the config screen's line
    /// editing. `⏎` saves -- nothing typed clears the name -- and `Esc`
    /// cancels.
    fn rename_key(&mut self, key: Key, ctx: Ctx) -> Option<Out> {
        let selected = self.selected();
        let field = self.rename.as_mut()?;
        match key {
            Key::Byte(ESC) => self.rename = None,
            Key::Byte(b'\r' | b'\n') => {
                let id = selected?;
                let text = field.as_str().trim().to_string();
                let name = if text.is_empty() {
                    None
                } else {
                    match Name::parse(&text) {
                        Ok(n) => Some(n),
                        Err(why) => {
                            self.note = Some(why);
                            return None;
                        }
                    }
                };
                let out = self.act(ctx, Ask::Rename { id, name });
                if out.is_some() {
                    self.rename = None;
                }
                return out;
            }
            Key::Byte(0x7f | 0x08) => field.erase(),
            Key::Byte(b) if b >= 0x20 => field.type_byte(b),
            _ => {}
        }
        None
    }

    /// `ask`, unless the link is down or a request is under way: then the
    /// note says why not, and nothing is asked.
    fn act(&mut self, ctx: Ctx, ask: Ask) -> Option<Out> {
        if !ctx.live {
            self.note = Some(NOT_LIVE.to_string());
            return None;
        }
        if ctx.busy {
            self.note = Some(BUSY.to_string());
            return None;
        }
        Some(Out::Ask(ask))
    }
}

/// A session as the selector names it: its name, else the start of its id.
pub(crate) fn label(e: &SessionEntry) -> String {
    match &e.name {
        Some(n) => oxutrm_client::legible(n.as_str()),
        None => e.id.short(),
    }
}

fn other_version(e: &SessionEntry) -> String {
    format!(
        "{} runs another version of oxutrm; it can be ended only by ending its shell",
        label(e)
    )
}

````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -670,12 +670,8 @@ impl ClientSession {
     }
 
     /// Ask the host `a` on the loop's next lap. One at a time: `false`, and
     /// nothing asked, while another is waiting or in flight.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the selector uses it from Task 12 on")
-    )]
     pub(crate) fn ask(&mut self, a: crate::switcher::Ask) -> bool {
         if self.asking.is_some() || self.asked.is_some() {
             return false;
         }
@@ -1318,8 +1314,11 @@ impl ClientSession {
                 self.activity
                     .record(Kind::Input, &format!("held input dropped ({n})"));
             }
             Some(Command::Config(c)) => self.config_command(c),
+            Some(Command::Ask(a)) => {
+                self.ask(a);
+            }
             None => {}
         }
         Ok(None)
     }
````

`src/switcher.rs`:

````diff
--- a/src/switcher.rs
+++ b/src/switcher.rs
@@ -21,12 +21,8 @@ use oxutrm_proto::{Answer, Name, Open, Reply, Request, SessionEntry, SessionId,
 use crate::connect::Established;
 
 /// What the selector asks the host.
 #[derive(Clone, Debug, PartialEq, Eq)]
-#[cfg_attr(
-    not(test),
-    expect(dead_code, reason = "the selector asks from Task 12 on")
-)]
 pub(crate) enum Ask {
     Sessions,
     Switch { to: SessionId },
     New { name: Option<Name> },
````

`src/ui.rs`:

````diff
--- a/src/ui.rs
+++ b/src/ui.rs
@@ -66,8 +66,11 @@ pub(crate) enum Mode {
     Config {
         cursor: usize,
         editing: Editing,
     },
+    /// The session selector (switcher spec §3.2). Its state lives in `Ui`
+    /// beside the config screen's field, because `Mode` is `Copy`.
+    Sessions,
 }
 
 /// What the config screen is doing with the keys.
 #[derive(Clone, Copy, PartialEq, Eq, Debug)]
@@ -102,8 +105,10 @@ pub(crate) enum Command {
     DropHeld,
     /// Something the config screen needs the session to check or do. The
     /// session answers with [`Ui::accepted`] or [`Ui::say`].
     Config(ConfigCmd),
+    /// Something the selector asks of the host.
+    Ask(crate::switcher::Ask),
 }
 
 /// What the config screen asks of the session. Rows are rows of
 /// [`crate::config::SETTINGS`].
@@ -134,11 +139,63 @@ pub(crate) struct ConfigScreen<'a> {
     /// Why the last change was refused, or what a save did.
     pub(crate) note: Option<&'a str>,
 }
 
-/// A key the config screen reads out of an escape sequence or a byte.
+/// One line of text being typed: the config screen's fields and the
+/// selector's rename. Input is UTF-8, so a character is added once all of
+/// its bytes have arrived; control characters never are; at most
+/// [`FIELD_MAX`] characters, so a pasted file stops there.
+#[derive(Clone, Debug, Default, PartialEq, Eq)]
+pub(crate) struct Field {
+    text: String,
+    /// The bytes of a character that has not arrived whole yet.
+    partial: Vec<u8>,
+}
+
+impl Field {
+    pub(crate) fn as_str(&self) -> &str {
+        &self.text
+    }
+
+    /// Start over holding `text`.
+    pub(crate) fn set(&mut self, text: String) {
+        self.text = text;
+        self.partial.clear();
+    }
+
+    pub(crate) fn clear(&mut self) {
+        self.set(String::new());
+    }
+
+    /// Backspace: the last character.
+    pub(crate) fn erase(&mut self) {
+        self.partial.clear();
+        self.text.pop();
+    }
+
+    /// A byte typed into the field.
+    pub(crate) fn type_byte(&mut self, b: u8) {
+        self.partial.push(b);
+        match std::str::from_utf8(&self.partial) {
+            Ok(s) => {
+                for c in s.chars().filter(|c| !c.is_control()) {
+                    if self.text.chars().count() < FIELD_MAX {
+                        self.text.push(c);
+                    }
+                }
+                self.partial.clear();
+            }
+            // The rest of the character is still to come.
+            Err(e) if e.error_len().is_none() => {}
+            Err(_) => self.partial.clear(),
+        }
+    }
+}
+
+/// A key the config screen and the selector read out of an escape sequence
+/// or a byte.
 #[derive(Clone, Copy, PartialEq, Eq, Debug)]
-enum Key {
+pub(crate) enum Key {
     Up,
     Down,
     Byte(u8),
 }
@@ -189,20 +246,24 @@ pub(crate) struct Ui {
     /// the config screen was up: it is decided once per outage, as
     /// `dismissed` is (spec §4.2).
     config_decided: bool,
     /// The config screen's text field.
-    field: String,
-    /// The bytes of a character typed into the field that has not arrived
-    /// whole yet.
-    partial: Vec<u8>,
+    field: Field,
     /// The `stun_servers` sub-list as the screen shows it.
     servers: Vec<String>,
     /// The sub-list as the last command proposed it, until the session
     /// accepts it.
     proposed: Option<Vec<String>>,
     /// What the help line says instead of the help: why a change was
     /// refused, or what a save did. Gone with the next read.
     note: Option<String>,
+    /// The session selector's state, while it is up and between openings.
+    selector: crate::selector::Selector,
+    /// The client is in a lobby: the selector's `q` ends the client, and
+    /// the popup key does not close it (there is nothing behind it).
+    lobby: bool,
+    /// A switcher request is in flight.
+    busy: bool,
     /// Inside a bracketed paste on the config screen whose end has not
     /// arrived yet, and how many bytes of that end the last read finished
     /// on: a large paste is cut into reads wherever the buffer ends, its end
     /// marker too.
@@ -223,17 +284,66 @@ impl Ui {
             // A session's `apply` sets the configured value, which is never
             // below `silent_after` and so changes nothing a lap could see.
             auto_open_after: Some(Duration::ZERO),
             config_decided: false,
-            field: String::new(),
-            partial: Vec::new(),
+            field: Field::default(),
             servers: Vec::new(),
             proposed: None,
             note: None,
+            selector: crate::selector::Selector::default(),
+            lobby: false,
+            busy: false,
             pasting: None,
         }
     }
 
+    /// What the selector needs to know about the session: whether it is in
+    /// a lobby, and whether a request is in flight. Set before every read.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the session uses it from Task 12 on")
+    )]
+    pub(crate) fn set_switcher(&mut self, lobby: bool, busy: bool) {
+        self.lobby = lobby;
+        self.busy = busy;
+    }
+
+    /// Open the selector, as `s` does: from a connect-time lobby, or after
+    /// the session it was showing ended. The session fetches the list.
+    pub(crate) fn open_sessions(&mut self) {
+        self.leave_config();
+        self.selector.open();
+        self.mode = Mode::Sessions;
+    }
+
+    /// The selector, for the view and for the session to fill in.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the session uses it from Task 12 on")
+    )]
+    pub(crate) fn selector(&self) -> &crate::selector::Selector {
+        &self.selector
+    }
+
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the session uses it from Task 12 on")
+    )]
+    pub(crate) fn selector_mut(&mut self) -> &mut crate::selector::Selector {
+        &mut self.selector
+    }
+
+    /// Close the popup, wherever it was: a switch landed, or a lobby
+    /// became a session.
+    #[cfg_attr(
+        not(test),
+        expect(dead_code, reason = "the session uses it from Task 12 on")
+    )]
+    pub(crate) fn close_popup(&mut self) {
+        self.leave_config();
+        self.mode = Mode::Closed;
+    }
+
     /// The popup's three settings, from `apply`. They act from the next
     /// read or lap: a popup already open stays open.
     pub(crate) fn retune(
         &mut self,
@@ -269,10 +379,12 @@ impl Ui {
     pub(crate) fn tick(&mut self, phase: Phase, now: Instant) -> Option<LinkChange> {
         self.note_question(phase, now);
         if phase == Phase::Confirming {
             // The question takes over the config screen, field and all, so
-            // its `s` and `d` can never land in a field.
+            // its `s` and `d` can never land in a field -- and the
+            // selector, whose `s`-less keys would leave it unanswerable.
             self.leave_config();
+            self.leave_sessions();
         }
         if phase.is_outage() {
             let change = if self.outage_since.is_none() {
                 // `Recovering` is only ever entered from `Silent`, so `now`
@@ -337,8 +449,13 @@ impl Ui {
     pub(crate) fn keys(&mut self, bytes: &[u8], phase: Phase, now: Instant) -> Routed {
         self.note_question(phase, now);
         if phase == Phase::Confirming {
             self.leave_config();
+            self.leave_sessions();
+        }
+        if self.mode == Mode::Sessions {
+            self.pasting = None;
+            return self.sessions_keys(bytes, phase, now);
         }
         if matches!(self.mode, Mode::Config { .. }) {
             // The rest of a paste that began in an earlier read is the
             // paste's.
@@ -411,8 +528,19 @@ impl Ui {
         if command.is_some() {
             r.command = command;
             return true;
         }
+        if b == b's' && !confirming {
+            // Only on a live link (switcher spec §3.2): the key bar shows it
+            // dimmed otherwise, and it does nothing but touch the popup.
+            if phase == Phase::Live {
+                self.open_sessions();
+                r.command = Some(Command::Ask(crate::switcher::Ask::Sessions));
+                return true;
+            }
+            self.touch();
+            return false;
+        }
         if b == b'c' && !confirming {
             self.mode = Mode::Config {
                 cursor: 0,
                 editing: Editing::None,
@@ -427,10 +555,9 @@ impl Ui {
         match b {
             // The question about held input has to be answered before the
             // popup can go.
             _ if (b == ESC || Some(b) == self.key) && !confirming => self.close(b, phase, now, r),
-            // `s` is offered by a later version (the session switcher); it,
-            // and every other key, only count as touching the popup.
+            // Every other key only counts as touching the popup.
             _ => self.touch(),
         }
         false
     }
@@ -456,18 +583,18 @@ impl Ui {
         };
         Some(ConfigScreen {
             cursor,
             editing,
-            field: &self.field,
+            field: self.field.as_str(),
             servers: &self.servers,
             note: self.note.as_deref(),
         })
     }
 
     /// Open a text field on the cursor's row, holding `text`.
     pub(crate) fn open_text(&mut self, text: String) {
         if let Mode::Config { cursor, .. } = self.mode {
-            self.field = text;
+            self.field.set(text);
             self.mode = Mode::Config {
                 cursor,
                 editing: Editing::Text,
             };
@@ -504,9 +631,8 @@ impl Ui {
         let Mode::Config { cursor, editing } = self.mode else {
             return;
         };
         self.field.clear();
-        self.partial.clear();
         let editing = match editing {
             Editing::Servers { cursor: at, .. } => {
                 if let Some(list) = self.proposed.take() {
                     self.servers = list;
@@ -528,15 +654,53 @@ impl Ui {
         self.proposed = None;
         self.note = Some(note);
     }
 
+    /// Back from the selector to the status view.
+    fn leave_sessions(&mut self) {
+        if self.mode == Mode::Sessions {
+            self.mode = Mode::Open { pressed: None };
+        }
+    }
+
+    /// One read on the selector. The popup key closes the popup as it does
+    /// everywhere -- except in a lobby, where there is nothing behind it.
+    fn sessions_keys(&mut self, bytes: &[u8], phase: Phase, now: Instant) -> Routed {
+        use crate::selector::{Ctx, Out};
+        let mut r = Routed::default();
+        if let Some(key) = self.key
+            && bytes.first() == Some(&key)
+            && self.selector.rename().is_none()
+        {
+            if !self.lobby {
+                self.mode = Mode::Closed;
+                if phase.is_outage() {
+                    self.dismissed = true;
+                }
+            }
+            return r;
+        }
+        let ctx = Ctx {
+            live: phase == Phase::Live,
+            lobby: self.lobby,
+            busy: self.busy,
+        };
+        match self.selector.keys(bytes, ctx, now) {
+            None => {}
+            Some(Out::Ask(a)) => r.command = Some(Command::Ask(a)),
+            Some(Out::Back) => self.mode = Mode::Open { pressed: None },
+            Some(Out::Close) => self.mode = Mode::Closed,
+            Some(Out::Quit) => r.command = Some(Command::Quit),
+        }
+        r
+    }
+
     /// Back from the config screen to the status view, dropping whatever
     /// field was open. The pending edits are the session's and stay.
     fn leave_config(&mut self) {
         if matches!(self.mode, Mode::Config { .. }) {
             self.mode = Mode::Open { pressed: None };
             self.field.clear();
-            self.partial.clear();
             self.proposed = None;
             self.note = None;
             self.pasting = None;
         }
@@ -616,28 +780,9 @@ impl Ui {
                 editing: Editing::Text | Editing::Servers { field: true, .. },
                 ..
             }
         ) {
-            self.type_byte(b);
-        }
-    }
-
-    /// A byte typed into the field: input is UTF-8, so a character is added
-    /// once all of its bytes have arrived. Control characters never are.
-    fn type_byte(&mut self, b: u8) {
-        self.partial.push(b);
-        match std::str::from_utf8(&self.partial) {
-            Ok(s) => {
-                for c in s.chars().filter(|c| !c.is_control()) {
-                    if self.field.chars().count() < FIELD_MAX {
-                        self.field.push(c);
-                    }
-                }
-                self.partial.clear();
-            }
-            // The rest of the character is still to come.
-            Err(e) if e.error_len().is_none() => {}
-            Err(_) => self.partial.clear(),
+            self.field.type_byte(b);
         }
     }
 
     /// One key on the config screen. Returns whether the read is over.
@@ -654,9 +799,8 @@ impl Ui {
             // A text field: every byte is its own; an arrow is nothing.
             Editing::Text | Editing::Servers { field: true, .. } => match key {
                 Key::Byte(ESC) => {
                     self.field.clear();
-                    self.partial.clear();
                     self.mode = to(match editing {
                         Editing::Servers { cursor: at, .. } => Editing::Servers {
                             cursor: at.min(self.servers.len().saturating_sub(1)),
                             field: false,
@@ -664,9 +808,9 @@ impl Ui {
                         _ => Editing::None,
                     });
                 }
                 k if enter(k) => {
-                    let text = self.field.trim().to_string();
+                    let text = self.field.as_str().trim().to_string();
                     r.command = match editing {
                         Editing::Servers { cursor: at, .. } => {
                             let mut list = self.servers.clone();
                             match (list.get_mut(at), text.is_empty()) {
@@ -690,12 +834,11 @@ impl Ui {
                     // paste is dropped, never run as commands.
                     return true;
                 }
                 k if erase(k) => {
-                    self.partial.clear();
-                    self.field.pop();
+                    self.field.erase();
                 }
-                Key::Byte(b) if b >= 0x20 => self.type_byte(b),
+                Key::Byte(b) if b >= 0x20 => self.field.type_byte(b),
                 _ => {}
             },
             Editing::Capture => match key {
                 Key::Byte(ESC) => self.mode = to(Editing::None),
@@ -733,9 +876,9 @@ impl Ui {
                             field: false,
                         });
                     }
                     k if enter(k) && at < self.servers.len() => {
-                        self.field = self.servers[at].clone();
+                        self.field.set(self.servers[at].clone());
                         self.mode = to(Editing::Servers {
                             cursor: at,
                             field: true,
                         });
@@ -829,9 +972,9 @@ impl Ui {
 /// The key an escape sequence stands for on the config screen, and how many
 /// bytes after the ESC the sequence took. Only the arrows mean anything; any
 /// other sequence is skipped whole. A bracketed paste's start is `None` too,
 /// with its five bytes used: the caller tells it from the rest by looking.
-fn escape(tail: &[u8]) -> (Option<Key>, usize) {
+pub(crate) fn escape(tail: &[u8]) -> (Option<Key>, usize) {
     match tail {
         [b'[' | b'O', b'A', ..] => (Some(Key::Up), 2),
         [b'[' | b'O', b'B', ..] => (Some(Key::Down), 2),
         [b'[', b'2', b'0', b'0', b'~', ..] => (None, 5),
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test --bin oxutrm --jobs 4 selector:: ui:: session::`

Expected: all pass, the thirteen `selector` tests and the new `ui` ones (`s_opens_the_selector_on_a_live_link_and_asks_for_the_list`, `s_does_nothing_while_the_link_is_down`, `in_a_lobby_q_quits_and_the_popup_key_does_not_close_it`, `the_selector_stays_through_an_outage_and_refuses_its_actions`, ...) among them.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add src/main.rs src/selector.rs src/session.rs src/switcher.rs src/ui.rs
git commit -m "feat(ui): the selector's state and keys, as the popup's Sessions mode

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 11: The selector on the screen: `SessionsView`, `layout_sessions`; `s` offered on a live link

Spec §3.2. `oxutrm-client` lays the selector out under the popup's box rules: as wide and as tall as what it says, capped at `min(cols - 4, 72)` by `min(rows - 2, 24)`, centred; rows, `+ new session`, the line under the list, a plain rule, the key bar. Columns are measured **in cells** (`unicode-width`), so a name in wide characters keeps the columns aligned (review focus 1, `a_name_of_wide_characters_keeps_the_columns_aligned`); a name being typed is cut at its start. The selected row is bold, a session that cannot be reattached dimmed; a long list scrolls to keep the cursor in view; a key bar too wide for the box keeps the last key (`q back` / `q quit`) whole and drops the other labels. Below `MIN_BOX`, one reverse-video line.

`view::sessions` builds the view: name (or the start of the id), the shell's basename, the start time (`HH:MM` today in the zone shown, else `Mon DD`), `120×40`, the mark (`this`, `in use`, `?`, `old version`, blank); the line under the list is the question, else the note, else `asking the host for its sessions…` while the first list is on its way; the header is `no reply 4 s` during an outage; the action keys are dimmed while the link is not live. The title is `sessions on <target>`.

The status view's `s sessions` is enabled on a live link and dimmed otherwise. **Deviation:** the spec says dimmed "with that reason"; the reason is the outage the popup's header already shows, and no separate text is added.

The box's width follows its content, so it changes as the line under the list does (judgement call: "fits content").

`layer_at` shows `Popup::Sessions` while `Mode::Sessions` is up.

**Files:**
- Modify: `crates/oxutrm-client/src/popup.rs`, `crates/oxutrm-client/src/lib.rs`, `src/view.rs`, `src/session.rs`, `src/ui.rs` (one `expect` comes off)
- Create (snapshots): `crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_{80x24,question_72x24,at_connect_40x12,19x5}.snap`, `src/snapshots/oxutrm__view__tests__snapshot_sessions_{from_the_popup_80x24,at_connect_72x24,with_an_error_line_and_a_question_narrow}.snap`

**Interfaces:**
- Consumes: Task 10's `Selector` and `selector::label`; Task 2's `SessionEntry`, `Attached`.
- Produces:
  - `oxutrm_client::{SessionRow { name, shell, started, size, mark, dimmed, field }, SessionsView { title, header, rows, new_row, cursor, line, keys, small }, Popup::Sessions(SessionsView), layout_sessions(&SessionsView, TermSize) -> Overlay}`.
  - `crate::view::{SessionsFacts { identity, selector, lobby, phase, last_heard, now, wall: SystemTime, zone }, sessions(&SessionsFacts) -> SessionsView}`; `ClientSession::sessions_view` (private).

- [ ] **Step 1: Write the failing tests**

The `.snap` files are the expected screens; with them in place `cargo test` compares rather than writes. `popup.rs`'s part includes review focus 1.

`crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_19x5.snap` (new):

````text
---
source: crates/oxutrm-client/src/popup.rs
expression: "text_of(&layout_sessions(&sessions_view(), TermSize { cols: 19, rows: 5 }))"
---
oxutrm sessions · b
````

`crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_80x24.snap` (new):

````text
---
source: crates/oxutrm-client/src/popup.rs
expression: "text_of(&layout_sessions(&sessions_view(), TermSize { cols: 80, rows: 24 }))"
---
╭ sessions on thinlinc ─────────────────────╮
│   a3f9c01e  fish  Oct 05  80×24   in use  │
│ ▸ build     bash  09:14   120×40  this    │
│   logs      zsh   11:02   120×40          │
│   + new session                           │
├───────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q back │
╰───────────────────────────────────────────╯
````

`crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_at_connect_40x12.snap` (new):

````text
---
source: crates/oxutrm-client/src/popup.rs
expression: "text_of(&layout_sessions(&v, TermSize { cols: 40, rows: 12 }))"
---
╭ sessions on thinlinc ────────────╮
│ no reply 4s                      │
│ ▸ logs      zsh  11:02  120×40   │
│   + new session                  │
│ the link is down; try again once │
├──────────────────────────────────┤
│ ⏎ n r x q quit                   │
╰──────────────────────────────────╯
````

`crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_question_72x24.snap` (new):

````text
---
source: crates/oxutrm-client/src/popup.rs
expression: "text_of(&layout_sessions(&v, TermSize { cols: 72, rows: 24 }))"
---
╭ sessions on thinlinc ─────────────────────────╮
│ ▸ a3f9c01e  fish  Oct 05  80×24   in use      │
│   build     bash  09:14   120×40  this        │
│   logs      zsh   11:02   120×40              │
│   + new session                               │
│ take over a3f9c01e from its other client? y/n │
├───────────────────────────────────────────────┤
│ y yes  n no                                   │
╰───────────────────────────────────────────────╯
````

`src/snapshots/oxutrm__view__tests__snapshot_sessions_at_connect_72x24.snap` (new):

````text
---
source: src/view.rs
expression: "sessions_text(&sessions_facts(&id, &sel, true, t), oxutrm_proto::TermSize\n{ cols: 72, rows: 24 })"
---
╭ sessions on thinlinc ─────────────────────╮
│ ▸ a3f9c01e  fish  Oct 05  80×24   in use  │
│   logs      zsh   11:02   120×40          │
│   + new session                           │
├───────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q quit │
╰───────────────────────────────────────────╯
````

`src/snapshots/oxutrm__view__tests__snapshot_sessions_from_the_popup_80x24.snap` (new):

````text
---
source: src/view.rs
expression: "sessions_text(&sessions_facts(&id, &sel, false, t), oxutrm_proto::TermSize\n{ cols: 80, rows: 24 })"
---
╭ sessions on thinlinc ─────────────────────╮
│   a3f9c01e  fish  Oct 05  80×24   in use  │
│ ▸ build     bash  09:14   120×40  this    │
│   logs      zsh   11:02   120×40          │
│   + new session                           │
├───────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q back │
╰───────────────────────────────────────────╯
````

`src/snapshots/oxutrm__view__tests__snapshot_sessions_with_an_error_line_and_a_question_narrow.snap` (new):

````text
---
source: src/view.rs
expression: "format!(\"{failed}\\n\\n{asked}\")"
---
╭ sessions on thinlinc ─────────────────────╮
│   a3f9c01e  fish  Oct 05  80×24   in use  │
│ ▸ build     bash  09:14   120×40  this    │
│   logs      zsh   11:02   120×40          │
│   + new session                           │
│ session a3f9c01e did not answer in time   │
├───────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q back │
╰───────────────────────────────────────────╯

╭ sessions on thinlinc ────────────────────╮
│ ▸ a3f9c01e  fish  Oct 05  80×24   in use │
│   build     bash  09:14   120×40  this   │
│   logs      zsh   11:02   120×40         │
│   + new session                          │
│ kill a3f9c01e (in use elsewhere)? y/n    │
├──────────────────────────────────────────┤
│ y yes  n no                              │
╰──────────────────────────────────────────╯
````

`src/view.rs`:

````diff
--- a/src/view.rs
+++ b/src/view.rs
@@ -958,23 +1088,25 @@ mod tests {
                 .map(|k| (format!("{} {}", k.key, k.label), k.enabled))
                 .collect()
         };
         let own = |s: &str, on: bool| (s.to_string(), on);
-        let shown = [
-            own("Esc close", true),
-            own("q quit", true),
-            own("c config", true),
-            own("s sessions", false),
-        ];
+        let shown = |live: bool| {
+            [
+                own("Esc close", true),
+                own("q quit", true),
+                own("c config", true),
+                own("s sessions", live),
+            ]
+        };
+        assert_eq!(bar(Phase::Live), shown(true));
         for phase in [
-            Phase::Live,
             Phase::Silent { since: t },
             Phase::Recovering {
                 attempt: 0,
                 next_try: t,
             },
         ] {
-            assert_eq!(bar(phase), shown, "{phase:?}");
+            assert_eq!(bar(phase), shown(false), "{phase:?}");
         }
         assert_eq!(
             bar(Phase::Confirming),
             [
@@ -2117,5 +2249,209 @@ mod tests {
             })
             .collect();
         insta::assert_snapshot!(text.join("\n"));
     }
+
+    // ---- the session selector -------------------------------------------
+
+    /// 2026-10-07 12:00:00 UTC: the wall clock the selector's tests run at.
+    const NOON: u64 = 1_791_374_400;
+
+    fn wall() -> SystemTime {
+        SystemTime::UNIX_EPOCH + Duration::from_secs(NOON)
+    }
+
+    /// `build` (this, started 09:14 today), an unnamed fish session in use
+    /// elsewhere since two days ago, `logs` detached since 11:02.
+    fn listed() -> Vec<oxutrm_proto::SessionEntry> {
+        use crate::selector::fixtures::{BUILD, FISH, LOGS, entry};
+        use oxutrm_proto::Attached;
+        let mut fish = entry(
+            FISH,
+            None,
+            "/usr/bin/fish",
+            NOON - 2 * 86_400,
+            Attached::Elsewhere,
+        );
+        fish.size = oxutrm_proto::TermSize { cols: 80, rows: 24 };
+        vec![
+            entry(
+                BUILD,
+                Some("build"),
+                "/bin/bash",
+                NOON - 2 * 3600 - 46 * 60,
+                Attached::Here,
+            ),
+            fish,
+            entry(LOGS, Some("logs"), "/bin/zsh", NOON - 58 * 60, Attached::No),
+        ]
+    }
+
+    fn sessions_text(f: &SessionsFacts<'_>, size: oxutrm_proto::TermSize) -> String {
+        let o = oxutrm_client::layout_sessions(&sessions(f), size);
+        (0..o.rows)
+            .map(|r| {
+                (0..o.cols)
+                    .map(|c| {
+                        o.cells[usize::from(r) * usize::from(o.cols) + usize::from(c)]
+                            .text
+                            .to_string()
+                    })
+                    .collect::<String>()
+            })
+            .collect::<Vec<_>>()
+            .join("\n")
+    }
+
+    fn sessions_facts<'a>(
+        id: &'a Identity,
+        selector: &'a crate::selector::Selector,
+        lobby: bool,
+        t: Instant,
+    ) -> SessionsFacts<'a> {
+        SessionsFacts {
+            identity: Some(id),
+            selector,
+            lobby,
+            phase: Phase::Live,
+            last_heard: t,
+            now: t,
+            wall: wall(),
+            zone: &UTC,
+        }
+    }
+
+    fn thinlinc() -> Identity {
+        Identity {
+            target: "thinlinc".to_string(),
+            session_id: crate::selector::fixtures::BUILD.to_string(),
+        }
+    }
+
+    #[test]
+    fn a_start_time_is_the_clock_today_and_the_date_before() {
+        let z = TimeZone::UTC;
+        assert_eq!(started(NOON - 2 * 3600 - 46 * 60, wall(), &z), "09:14");
+        assert_eq!(started(NOON - 2 * 86_400, wall(), &z), "Oct 05");
+        // Midnight is the line, in the zone shown.
+        assert_eq!(started(NOON - 12 * 3600, wall(), &z), "00:00");
+        assert_eq!(started(NOON - 12 * 3600 - 60, wall(), &z), "Oct 06");
+    }
+
+    #[test]
+    fn every_row_says_name_shell_start_size_and_mark() {
+        let t = Instant::now();
+        let mut sel = crate::selector::Selector::default();
+        sel.open();
+        sel.set_rows(listed());
+        let id = thinlinc();
+        let v = sessions(&sessions_facts(&id, &sel, false, t));
+        assert_eq!(v.title, "sessions on thinlinc");
+        let shown: Vec<(&str, &str, &str, &str, &str)> = v
+            .rows
+            .iter()
+            .map(|r| {
+                (
+                    r.name.as_str(),
+                    r.shell.as_str(),
+                    r.started.as_str(),
+                    r.size.as_str(),
+                    r.mark.as_str(),
+                )
+            })
+            .collect();
+        assert_eq!(
+            shown,
+            [
+                ("a3f9c01e", "fish", "Oct 05", "80\u{d7}24", "in use"),
+                ("build", "bash", "09:14", "120\u{d7}40", "this"),
+                ("logs", "zsh", "11:02", "120\u{d7}40", ""),
+            ]
+        );
+        assert_eq!(v.cursor, 1, "on this");
+        assert_eq!(v.keys.last().unwrap().label, "back");
+        let lobby = sessions(&sessions_facts(&id, &sel, true, t));
+        assert_eq!(lobby.keys.last().unwrap().label, "quit");
+    }
+
+    #[test]
+    fn while_the_link_is_down_the_header_says_so_and_the_actions_are_dimmed() {
+        let t = Instant::now();
+        let mut sel = crate::selector::Selector::default();
+        sel.open();
+        sel.set_rows(listed());
+        let id = thinlinc();
+        let v = sessions(&SessionsFacts {
+            phase: Phase::Silent { since: t },
+            now: t + Duration::from_secs(4),
+            ..sessions_facts(&id, &sel, false, t)
+        });
+        assert_eq!(v.header, "no reply 4 s");
+        assert!(v.keys.iter().filter(|k| k.key != "q").all(|k| !k.enabled));
+        assert!(v.keys.last().unwrap().enabled, "q always works");
+    }
+
+    #[test]
+    fn the_status_views_s_is_offered_only_on_a_live_link() {
+        let s_of = |p: Phase| keys(p).into_iter().find(|k| k.key == "s").unwrap();
+        assert!(s_of(Phase::Live).enabled);
+        assert!(
+            !s_of(Phase::Silent {
+                since: Instant::now()
+            })
+            .enabled
+        );
+    }
+
+    #[test]
+    fn snapshot_sessions_from_the_popup_80x24() {
+        let t = Instant::now();
+        let mut sel = crate::selector::Selector::default();
+        sel.open();
+        sel.set_rows(listed());
+        let id = thinlinc();
+        insta::assert_snapshot!(sessions_text(
+            &sessions_facts(&id, &sel, false, t),
+            oxutrm_proto::TermSize { cols: 80, rows: 24 }
+        ));
+    }
+
+    #[test]
+    fn snapshot_sessions_at_connect_72x24() {
+        let t = Instant::now();
+        let mut rows = listed();
+        rows.retain(|e| !e.this);
+        let mut sel = crate::selector::Selector::default();
+        sel.open();
+        sel.set_rows(rows);
+        let id = thinlinc();
+        insta::assert_snapshot!(sessions_text(
+            &sessions_facts(&id, &sel, true, t),
+            oxutrm_proto::TermSize { cols: 72, rows: 24 }
+        ));
+    }
+
+    #[test]
+    fn snapshot_sessions_with_an_error_line_and_a_question_narrow() {
+        let t = Instant::now();
+        let mut sel = crate::selector::Selector::default();
+        sel.open();
+        sel.set_rows(listed());
+        sel.fail("session a3f9c01e did not answer in time".to_string());
+        let id = thinlinc();
+        let failed = sessions_text(
+            &sessions_facts(&id, &sel, false, t),
+            oxutrm_proto::TermSize { cols: 50, rows: 14 },
+        );
+        let ctx = crate::selector::Ctx {
+            live: true,
+            lobby: false,
+            busy: false,
+        };
+        sel.keys(b"kx", ctx, t);
+        let asked = sessions_text(
+            &sessions_facts(&id, &sel, false, t),
+            oxutrm_proto::TermSize { cols: 50, rows: 14 },
+        );
+        insta::assert_snapshot!(format!("{failed}\n\n{asked}"));
+    }
 }
````

`crates/oxutrm-client/src/popup.rs`:

````diff
--- a/crates/oxutrm-client/src/popup.rs
+++ b/crates/oxutrm-client/src/popup.rs
@@ -1780,5 +2063,204 @@ mod tests {
             &config_view(),
             TermSize { cols: 40, rows: 12 }
         )));
     }
+
+    // ---- the session selector -------------------------------------------
+
+    fn session_keys(back: &str) -> Vec<KeyHint> {
+        [
+            ("\u{23ce}", "switch"),
+            ("n", "new"),
+            ("r", "rename"),
+            ("x", "kill"),
+            ("q", back),
+        ]
+        .into_iter()
+        .map(|(key, label)| KeyHint {
+            key: key.to_string(),
+            label: label.to_string(),
+            enabled: true,
+        })
+        .collect()
+    }
+
+    fn session(name: &str, shell: &str, started: &str, size: &str, mark: &str) -> SessionRow {
+        SessionRow {
+            name: name.to_string(),
+            shell: shell.to_string(),
+            started: started.to_string(),
+            size: size.to_string(),
+            mark: mark.to_string(),
+            ..SessionRow::default()
+        }
+    }
+
+    /// The spec's picture: three sessions on thinlinc, `build` the one the
+    /// client is in, an unnamed fish session in use by another client.
+    fn sessions_view() -> SessionsView {
+        SessionsView {
+            title: "sessions on thinlinc".to_string(),
+            header: String::new(),
+            rows: vec![
+                session("a3f9c01e", "fish", "Oct 05", "80\u{d7}24", "in use"),
+                session("build", "bash", "09:14", "120\u{d7}40", "this"),
+                session("logs", "zsh", "11:02", "120\u{d7}40", ""),
+            ],
+            new_row: "+ new session".to_string(),
+            cursor: 1,
+            line: String::new(),
+            keys: session_keys("back"),
+            small: "oxutrm sessions \u{b7} build".to_string(),
+        }
+    }
+
+    #[test]
+    fn the_selector_lines_up_its_columns_and_marks_the_cursor() {
+        let o = layout_sessions(&sessions_view(), TermSize { cols: 80, rows: 24 });
+        let text = text_of(&o);
+        let build = row(&o, find(&o, "build").unwrap());
+        let fish = row(&o, find(&o, "fish").unwrap());
+        assert!(build.contains("\u{25b8} build"), "{text}");
+        for col in ["bash", "09:14", "120\u{d7}40", "this"] {
+            assert!(build.contains(col), "{col} missing: {text}");
+        }
+        // In characters: the marker is three bytes.
+        let at = |line: &str, what: &str| line[..line.find(what).unwrap()].chars().count();
+        assert_eq!(at(&build, "bash"), at(&fish, "fish"), "{text}");
+        assert_eq!(at(&build, "09:14"), at(&fish, "Oct 05"), "{text}");
+        assert!(find(&o, "+ new session").is_some(), "{text}");
+        assert!(row(&o, o.rows - 2).contains("q back"), "{text}");
+        assert!(is_rule(&row(&o, o.rows - 3), None), "{text}");
+        // As wide as it needs and no wider, centred.
+        assert!(o.cols < 72, "{text}");
+        assert_eq!(o.col, (80 - o.cols) / 2);
+    }
+
+    #[test]
+    fn a_session_that_cannot_be_switched_to_is_dimmed() {
+        let mut v = sessions_view();
+        v.rows[2].dimmed = true;
+        let o = layout_sessions(&v, TermSize { cols: 80, rows: 24 });
+        let r = find(&o, "logs").unwrap();
+        assert!(cell_of(&o, r, "l").attrs.contains(Attrs::DIM));
+        let b = find(&o, "build").unwrap();
+        assert!(!cell_of(&o, b, "b").attrs.contains(Attrs::DIM));
+    }
+
+    #[test]
+    fn a_long_list_scrolls_to_keep_the_cursor_in_view() {
+        let mut v = sessions_view();
+        v.rows = (0..30)
+            .map(|i| session(&format!("job-{i:02}"), "bash", "09:14", "120\u{d7}40", ""))
+            .collect();
+        v.cursor = 27;
+        let o = layout_sessions(&v, TermSize { cols: 80, rows: 12 });
+        let text = text_of(&o);
+        assert!(find(&o, "\u{25b8} job-27").is_some(), "{text}");
+        assert!(row(&o, o.rows - 2).contains("q back"), "{text}");
+    }
+
+    #[test]
+    fn a_name_being_typed_keeps_its_end_in_view() {
+        let mut v = sessions_view();
+        v.rows[1].name = format!("{}\u{258f}", "n".repeat(40));
+        v.rows[1].field = true;
+        let o = layout_sessions(&v, TermSize { cols: 80, rows: 24 });
+        let line = row(&o, find(&o, "\u{258f}").expect("no cursor shown"));
+        assert!(line.contains('\u{2026}'), "the cut is not marked: {line}");
+        assert!(line.contains("bash"), "the columns after it moved: {line}");
+    }
+
+    #[test]
+    fn snapshot_sessions_80x24() {
+        insta::assert_snapshot!(text_of(&layout_sessions(
+            &sessions_view(),
+            TermSize { cols: 80, rows: 24 }
+        )));
+    }
+
+    #[test]
+    fn snapshot_sessions_question_72x24() {
+        let mut v = sessions_view();
+        v.cursor = 0;
+        v.line = "take over a3f9c01e from its other client? y/n".to_string();
+        v.keys = vec![
+            KeyHint {
+                key: "y".to_string(),
+                label: "yes".to_string(),
+                enabled: true,
+            },
+            KeyHint {
+                key: "n".to_string(),
+                label: "no".to_string(),
+                enabled: true,
+            },
+        ];
+        insta::assert_snapshot!(text_of(&layout_sessions(
+            &v,
+            TermSize { cols: 72, rows: 24 }
+        )));
+    }
+
+    #[test]
+    fn snapshot_sessions_at_connect_40x12() {
+        let v = SessionsView {
+            title: "sessions on thinlinc".to_string(),
+            header: "no reply 4s".to_string(),
+            rows: vec![session("logs", "zsh", "11:02", "120\u{d7}40", "")],
+            cursor: 0,
+            line: "the link is down; try again once it is back".to_string(),
+            keys: session_keys("quit"),
+            ..sessions_view()
+        };
+        insta::assert_snapshot!(text_of(&layout_sessions(
+            &v,
+            TermSize { cols: 40, rows: 12 }
+        )));
+    }
+
+    #[test]
+    fn snapshot_sessions_19x5() {
+        insta::assert_snapshot!(text_of(&layout_sessions(
+            &sessions_view(),
+            TermSize { cols: 19, rows: 5 }
+        )));
+    }
+
+    #[test]
+    fn no_size_panics() {
+        for cols in 0..=90 {
+            for rows in 0..=30 {
+                let _ = layout_sessions(&sessions_view(), TermSize { cols, rows });
+            }
+        }
+    }
+
+    #[test]
+    fn a_narrow_selector_keeps_the_key_that_leaves_it() {
+        let o = layout_sessions(&sessions_view(), TermSize { cols: 30, rows: 12 });
+        let bar = row(&o, o.rows - 2);
+        assert!(bar.contains("q back"), "{}", text_of(&o));
+    }
+
+    /// A name of wide characters takes two cells a character: the columns
+    /// after it stay aligned with every other row's.
+    #[test]
+    fn a_name_of_wide_characters_keeps_the_columns_aligned() {
+        let mut v = sessions_view();
+        v.rows[2].name = "\u{69cb}\u{7bc9}\u{30ed}\u{30b0}".to_string();
+        let o = layout_sessions(&v, TermSize { cols: 80, rows: 24 });
+        let text = text_of(&o);
+        let column_of = |r: u16, first: &str| {
+            let start = r as usize * o.cols as usize;
+            o.cells[start..start + o.cols as usize]
+                .iter()
+                .position(|c| c.text == first)
+                .unwrap_or_else(|| panic!("no {first} on row {r}: {text}"))
+        };
+        let wide = find(&o, "zsh").unwrap();
+        let narrow = find(&o, "bash").unwrap();
+        // The start time's colon is the first on either row.
+        assert_eq!(column_of(wide, ":"), column_of(narrow, ":"), "{text}");
+    }
 }
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo test -p oxutrm-client --jobs 4 popup:: && cargo test --bin oxutrm --jobs 4 view::`

Expected: compile errors: `SessionsView`, `SessionRow`, `layout_sessions`, `SessionsFacts` and `view::sessions` are not defined.

- [ ] **Step 3: Implement**

`crates/oxutrm-client/src/lib.rs`:

````diff
--- a/crates/oxutrm-client/src/lib.rs
+++ b/crates/oxutrm-client/src/lib.rs
@@ -37,10 +37,10 @@ pub mod status;
 pub use color::down_convert;
 pub use guard::{RawGuard, TERMINAL_RESTORE};
 pub use overlay::{Overlay, overlay_from_buffer};
 pub use popup::{
-    ConfigRow, ConfigSection, ConfigView, KeyHint, Marker, Popup, PopupView, Row, layout,
-    layout_config, layout_popup, legible, summarised,
+    ConfigRow, ConfigSection, ConfigView, KeyHint, Marker, Popup, PopupView, Row, SessionRow,
+    SessionsView, layout, layout_config, layout_popup, layout_sessions, legible, summarised,
 };
 pub use renderer::Renderer;
 pub use status::{rung_label, status_line};
 
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -1373,8 +1373,11 @@ impl ClientSession {
         }
         if !self.ui.visible(phase) {
             return None;
         }
+        if self.ui.mode() == Mode::Sessions {
+            return Some(Popup::Sessions(self.sessions_view(phase, now)));
+        }
         Some(match self.ui.config_screen() {
             Some(screen) => Popup::Config(self.config_view(screen, phase, now)),
             None => Popup::Status(self.view(phase, now)),
         })
@@ -1385,12 +1388,29 @@ impl ClientSession {
     #[cfg(test)]
     fn popup_at(&mut self, now: Instant) -> Option<PopupView> {
         match self.layer_at(now)? {
             Popup::Status(v) => Some(v),
-            Popup::Config(_) => None,
+            Popup::Config(_) | Popup::Sessions(_) => None,
         }
     }
 
+    /// What the session selector says at `now`.
+    ///
+    /// The wall clock is read here, for "today": a start time turns from
+    /// `HH:MM` into `Mon DD` at midnight, which a repaint then shows.
+    fn sessions_view(&self, phase: Phase, now: Instant) -> oxutrm_client::SessionsView {
+        crate::view::sessions(&crate::view::SessionsFacts {
+            identity: self.identity.as_ref(),
+            selector: self.ui.selector(),
+            lobby: self.in_lobby,
+            phase,
+            last_heard: self.link_state.last_heard(),
+            now,
+            wall: std::time::SystemTime::now(),
+            zone: &self.zone,
+        })
+    }
+
     /// What the config screen says at `now`.
     fn config_view(&self, screen: ConfigScreen<'_>, phase: Phase, now: Instant) -> ConfigView {
         crate::view::config(&ConfigFacts {
             identity: self.identity.as_ref(),
````

`src/ui.rs`:

````diff
--- a/src/ui.rs
+++ b/src/ui.rs
@@ -315,12 +315,8 @@ impl Ui {
         self.mode = Mode::Sessions;
     }
 
     /// The selector, for the view and for the session to fill in.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the session uses it from Task 12 on")
-    )]
     pub(crate) fn selector(&self) -> &crate::selector::Selector {
         &self.selector
     }
 
````

`src/view.rs`:

````diff
--- a/src/view.rs
+++ b/src/view.rs
@@ -17,10 +17,10 @@
 use std::time::{Duration, Instant, SystemTime};
 
 use jiff::tz::TimeZone;
 use oxutrm_client::{
-    ConfigRow, ConfigSection, ConfigView, KeyHint, Marker, PopupView, Row, legible, rung_label,
-    summarised,
+    ConfigRow, ConfigSection, ConfigView, KeyHint, Marker, PopupView, Row, SessionRow,
+    SessionsView, legible, rung_label, summarised,
 };
 use oxutrm_proto::PathDescription;
 
 use crate::activity::Activity;
@@ -458,8 +458,136 @@ fn hh_mm(t: SystemTime, zone: &TimeZone) -> String {
         Err(_) => "--:--".to_string(),
     }
 }
 
+/// What the session selector is built from (switcher spec §3.2).
+pub(crate) struct SessionsFacts<'a> {
+    pub(crate) identity: Option<&'a Identity>,
+    pub(crate) selector: &'a crate::selector::Selector,
+    /// In a lobby: `q quit` rather than `q back`.
+    pub(crate) lobby: bool,
+    pub(crate) phase: Phase,
+    pub(crate) last_heard: Instant,
+    pub(crate) now: Instant,
+    /// `now` on the wall clock, which decides what "today" is.
+    pub(crate) wall: SystemTime,
+    pub(crate) zone: &'a TimeZone,
+}
+
+/// The session selector: a row per session -- name, shell, start, size,
+/// mark -- `+ new session`, and under the list the question, the reason
+/// something was refused, or that the list is on its way.
+pub(crate) fn sessions(f: &SessionsFacts<'_>) -> SessionsView {
+    let sel = f.selector;
+    let rows: Vec<SessionRow> = sel
+        .rows()
+        .iter()
+        .enumerate()
+        .map(|(i, e)| {
+            let (name, field) = match sel.rename() {
+                Some(text) if i == sel.cursor() => (field_text(&legible(text)), true),
+                _ => (crate::selector::label(e), false),
+            };
+            SessionRow {
+                name,
+                shell: legible(
+                    std::path::Path::new(&e.shell)
+                        .file_name()
+                        .map_or(e.shell.as_str(), |b| b.to_str().unwrap_or(&e.shell)),
+                ),
+                started: started(e.created_unix, f.wall, f.zone),
+                size: format!("{}\u{d7}{}", e.size.cols, e.size.rows),
+                mark: match e.attached {
+                    oxutrm_proto::Attached::Here => "this",
+                    oxutrm_proto::Attached::Elsewhere => "in use",
+                    oxutrm_proto::Attached::No => "",
+                    oxutrm_proto::Attached::Unknown => "?",
+                    oxutrm_proto::Attached::OtherVersion => "old version",
+                }
+                .to_string(),
+                dimmed: !e.detachable,
+                field,
+            }
+        })
+        .collect();
+    let line = match (sel.question(), sel.note()) {
+        (Some(q), _) => legible(&q.text()),
+        (None, Some(note)) => summarised(note),
+        (None, None) if sel.loading() => "asking the host for its sessions\u{2026}".to_string(),
+        (None, None) => String::new(),
+    };
+    let selected = sel
+        .rows()
+        .get(sel.cursor())
+        .map_or_else(|| "+ new session".to_string(), crate::selector::label);
+    SessionsView {
+        title: match f.identity {
+            Some(id) => format!("sessions on {}", legible(&id.target)),
+            None => "sessions".to_string(),
+        },
+        header: outage_note(f.phase, f.last_heard, f.now),
+        rows,
+        new_row: "+ new session".to_string(),
+        cursor: sel.cursor(),
+        line,
+        keys: sessions_keys(sel, f.lobby, f.phase == Phase::Live),
+        small: format!("oxutrm sessions \u{b7} {selected}"),
+    }
+}
+
+/// When a session started: `HH:MM` today, else `Mon DD`, in `zone`.
+fn started(created_unix: u64, wall: SystemTime, zone: &TimeZone) -> String {
+    let at = i64::try_from(created_unix)
+        .ok()
+        .and_then(|s| jiff::Timestamp::from_second(s).ok());
+    let now = jiff::Timestamp::try_from(wall).ok();
+    let (Some(at), Some(now)) = (at, now) else {
+        return "?".to_string();
+    };
+    let (at, now) = (zone.to_datetime(at), zone.to_datetime(now));
+    if at.date() == now.date() {
+        format!("{:02}:{:02}", at.hour(), at.minute())
+    } else {
+        at.strftime("%b %d").to_string()
+    }
+}
+
+/// The selector's keys: the question's, the name field's, or its own --
+/// the ones that ask the host dimmed while the link is down.
+fn sessions_keys(sel: &crate::selector::Selector, lobby: bool, live: bool) -> Vec<KeyHint> {
+    let hint = |key: &str, label: &str, enabled: bool| KeyHint {
+        key: key.to_string(),
+        label: label.to_string(),
+        enabled,
+    };
+    if sel.question().is_some() {
+        return vec![hint("y", "yes", live), hint("n", "no", true)];
+    }
+    if sel.rename().is_some() {
+        return vec![hint("\u{23ce}", "save", live), hint("Esc", "cancel", true)];
+    }
+    vec![
+        hint("\u{23ce}", "switch", live),
+        hint("n", "new", live),
+        hint("r", "rename", live),
+        hint("x", "kill", live),
+        hint("q", if lobby { "quit" } else { "back" }, true),
+    ]
+}
+
+/// `no reply 5s` while the link is down, else nothing: the outage, in a
+/// screen's header.
+fn outage_note(phase: Phase, last_heard: Instant, now: Instant) -> String {
+    let silent_since = match phase {
+        Phase::Silent { since } => Some(since),
+        Phase::Recovering { .. } => Some(last_heard),
+        _ => None,
+    };
+    silent_since.map_or_else(String::new, |since| {
+        format!("no reply {}", clock(now.saturating_duration_since(since)))
+    })
+}
+
 fn keys(phase: Phase) -> Vec<KeyHint> {
     let hint = |key: &str, label: &str, enabled: bool| KeyHint {
         key: key.to_string(),
         label: label.to_string(),
@@ -473,13 +601,15 @@ fn keys(phase: Phase) -> Vec<KeyHint> {
             hint("d", "drop", true),
             hint("q", "quit", true),
             hint("c", "config", false),
         ],
+        // `s` only on a live link (switcher spec §3.2): dimmed through an
+        // outage or a rebuild, when nothing it asks could be answered.
         _ => vec![
             hint("Esc", "close", true),
             hint("q", "quit", true),
             hint("c", "config", true),
-            hint("s", "sessions", false),
+            hint("s", "sessions", phase == Phase::Live),
         ],
     }
 }
 
````

`crates/oxutrm-client/src/popup.rs`:

````diff
--- a/crates/oxutrm-client/src/popup.rs
+++ b/crates/oxutrm-client/src/popup.rs
@@ -576,18 +576,301 @@ impl ConfigView {
 #[derive(Clone, PartialEq, Eq, Debug)]
 pub enum Popup {
     Status(PopupView),
     Config(ConfigView),
+    Sessions(SessionsView),
 }
 
 /// Lay out whichever view the popup shows.
 pub fn layout(p: &Popup, size: TermSize) -> Overlay {
     match p {
         Popup::Status(v) => layout_popup(v, size),
         Popup::Config(v) => layout_config(v, size),
+        Popup::Sessions(v) => layout_sessions(v, size),
     }
 }
 
+/// One row of the session selector, its columns as text.
+#[derive(Clone, PartialEq, Eq, Debug, Default)]
+pub struct SessionRow {
+    /// The name, else the start of the id; or the name being typed.
+    pub name: String,
+    /// The shell's basename.
+    pub shell: String,
+    /// `HH:MM` today, else `Mon DD`.
+    pub started: String,
+    /// `120×40`.
+    pub size: String,
+    /// `this`, `in use`, `?`, `old version`, or nothing.
+    pub mark: String,
+    /// A session that cannot be switched to: drawn dimmed.
+    pub dimmed: bool,
+    /// The name is an open text field: cut at its start, not its end.
+    pub field: bool,
+}
+
+/// What the session selector says, as content rather than as cells.
+#[derive(Clone, PartialEq, Eq, Debug, Default)]
+pub struct SessionsView {
+    /// Drawn into the top border.
+    pub title: String,
+    /// The first line inside the box -- an outage -- or empty.
+    pub header: String,
+    pub rows: Vec<SessionRow>,
+    /// The last row, after the sessions: `+ new session`.
+    pub new_row: String,
+    /// A row of `rows`, or `rows.len()` for the new row.
+    pub cursor: usize,
+    /// The line under the list: a question, why something was refused, or
+    /// that the list is on its way. Empty for none.
+    pub line: String,
+    pub keys: Vec<KeyHint>,
+    /// What a screen below [`MIN_BOX`] shows on its one line.
+    pub small: String,
+}
+
+/// The widest the selector's name column grows: a name's own limit.
+const SESSION_NAME_COLS: usize = 24;
+/// The narrowest: the start of an id.
+const SESSION_NAME_MIN: usize = 8;
+/// The gap between the selector's columns.
+const SESSION_GAP: &str = "  ";
+
+/// The selector's column widths, from what its rows say.
+struct SessionCols {
+    name: usize,
+    shell: usize,
+    started: usize,
+    size: usize,
+}
+
+/// How many cells `text` takes: a CJK character takes two, so a name of
+/// them is measured by width, never by characters.
+fn cells(text: &str) -> usize {
+    unicode_width::UnicodeWidthStr::width(text)
+}
+
+/// `text` padded with spaces to `cols` cells.
+fn padded(text: &str, cols: usize) -> String {
+    format!("{text}{}", " ".repeat(cols.saturating_sub(cells(text))))
+}
+
+/// `text` in at most `cols` cells, a cut marked with `…` -- at the start for
+/// an open text field, whose end is where the cursor is.
+fn cut_cells(text: &str, cols: usize, keep_end: bool) -> String {
+    if cells(text) <= cols {
+        return text.to_string();
+    }
+    let room = cols.saturating_sub(1);
+    let chars: Vec<char> = if keep_end {
+        text.chars().rev().collect()
+    } else {
+        text.chars().collect()
+    };
+    let mut kept: Vec<char> = Vec::new();
+    let mut used = 0;
+    for c in chars {
+        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
+        if used + w > room {
+            break;
+        }
+        used += w;
+        kept.push(c);
+    }
+    if keep_end {
+        kept.reverse();
+        format!("\u{2026}{}", kept.into_iter().collect::<String>())
+    } else {
+        format!("{}\u{2026}", kept.into_iter().collect::<String>())
+    }
+}
+
+impl SessionCols {
+    fn of(rows: &[SessionRow]) -> SessionCols {
+        let widest =
+            |f: fn(&SessionRow) -> &str| rows.iter().map(|r| cells(f(r))).max().unwrap_or(0);
+        SessionCols {
+            name: widest(|r| &r.name).clamp(SESSION_NAME_MIN, SESSION_NAME_COLS),
+            shell: widest(|r| &r.shell),
+            started: widest(|r| &r.started),
+            size: widest(|r| &r.size),
+        }
+    }
+
+    /// One row as text: the marker, then the columns, each padded to its
+    /// width in cells.
+    fn line(&self, r: &SessionRow, selected: bool) -> String {
+        let marker = if selected { '\u{25b8}' } else { ' ' };
+        let name = cut_cells(&r.name, self.name, r.field);
+        format!(
+            "{marker} {}{SESSION_GAP}{}{SESSION_GAP}{}{SESSION_GAP}{}{SESSION_GAP}{}",
+            padded(&name, self.name),
+            padded(&r.shell, self.shell),
+            padded(&r.started, self.started),
+            padded(&r.size, self.size),
+            r.mark,
+        )
+        .trim_end()
+        .to_string()
+    }
+}
+
+/// Lay the session selector out for this screen (switcher spec §3.2): the
+/// popup's box rules -- as wide and as tall as what it says, capped at
+/// `min(cols - 4, 72)` by `min(rows - 2, 24)`, centred. The header, the
+/// rows, `+ new session`, the line under the list, then a plain rule and
+/// the key bar. When the rows do not fit, the list scrolls so the cursor's
+/// row is in view. Below [`MIN_BOX`], one reverse-video line.
+pub fn layout_sessions(v: &SessionsView, size: TermSize) -> Overlay {
+    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
+        return reversed_line(v.small.clone(), size);
+    }
+    let cols = SessionCols::of(&v.rows);
+    let mut lines: Vec<String> = v
+        .rows
+        .iter()
+        .enumerate()
+        .map(|(i, r)| cols.line(r, i == v.cursor))
+        .collect();
+    let new_marker = if v.cursor >= v.rows.len() {
+        '\u{25b8}'
+    } else {
+        ' '
+    };
+    lines.push(format!("{new_marker} {}", v.new_row));
+    let width = lines
+        .iter()
+        .chain([&v.header, &v.line, &v.title])
+        .map(|l| Line::from(l.as_str()).width())
+        .chain([key_line(&v.keys).width()])
+        .max()
+        .unwrap_or(0);
+    // The border and a cell of padding either side.
+    let wanted = u16::try_from(width + 4).unwrap_or(u16::MAX);
+    let box_cols = wanted.min((size.cols - 4).min(MAX_BOX.cols));
+    let cap = (size.rows - 2).min(MAX_BOX.rows);
+    let inside = lines.len()
+        + usize::from(!v.header.is_empty())
+        + usize::from(!v.line.is_empty())
+        // The rule and the key bar.
+        + 2;
+    let box_rows = u16::try_from(inside)
+        .unwrap_or(u16::MAX)
+        .saturating_add(2)
+        .min(cap);
+    let buf = draw_sessions(v, &lines, box_cols, box_rows);
+    overlay_from_buffer(&buf, (size.rows - box_rows) / 2, (size.cols - box_cols) / 2)
+}
+
+/// The key bar in `width`: whole when it fits, else every key but the last
+/// without its label -- the last is how the selector is left, `q back` or
+/// `q quit`, and says which.
+fn fitted_key_line(keys: &[KeyHint], width: u16) -> Line<'static> {
+    let whole = key_line(keys);
+    if whole.width() <= usize::from(width) {
+        return whole;
+    }
+    let Some((last, rest)) = keys.split_last() else {
+        return whole;
+    };
+    let mut short: Vec<KeyHint> = rest
+        .iter()
+        .map(|k| KeyHint {
+            label: String::new(),
+            ..k.clone()
+        })
+        .collect();
+    short.push(last.clone());
+    let mut spans = Vec::new();
+    for (i, k) in short.iter().enumerate() {
+        if i > 0 {
+            spans.push(Span::raw(" "));
+        }
+        let style = if k.enabled {
+            Style::default().add_modifier(Modifier::BOLD)
+        } else {
+            Style::default().add_modifier(Modifier::DIM)
+        };
+        spans.push(Span::styled(k.key.clone(), style));
+        if !k.label.is_empty() {
+            spans.push(Span::styled(format!(" {}", k.label), Style::default()));
+        }
+    }
+    Line::from(spans)
+}
+
+fn draw_sessions(v: &SessionsView, lines: &[String], cols: u16, rows: u16) -> Buffer {
+    let area = Rect::new(0, 0, cols, rows);
+    let mut buf = Buffer::empty(area);
+    let block = Block::default()
+        .borders(Borders::ALL)
+        .border_type(BorderType::Rounded)
+        .padding(Padding::horizontal(1))
+        .title(format!(" {} ", v.title));
+    let inner = block.inner(area);
+    block.render(area, &mut buf);
+
+    // From the bottom: the key bar, which is how the selector is left, and
+    // a plain rule above it while there is room. At least two rows inside
+    // the border here.
+    let bar = inner.bottom() - 1;
+    Paragraph::new(fitted_key_line(&v.keys, inner.width)).render(
+        Rect {
+            y: bar,
+            height: 1,
+            ..inner
+        },
+        &mut buf,
+    );
+    if inner.height >= 2 {
+        rule(&mut buf, bar - 1, cols, None);
+    }
+    let mut body = Rect {
+        height: inner.height.saturating_sub(2),
+        ..inner
+    };
+    if !v.header.is_empty() {
+        body = place_one(Line::from(v.header.clone()), body, &mut buf);
+    }
+    // The line under the list keeps its row: a question there is what the
+    // next key answers.
+    if !v.line.is_empty() && body.height >= 2 {
+        let y = body.bottom() - 1;
+        Paragraph::new(Line::from(v.line.clone())).render(
+            Rect {
+                y,
+                height: 1,
+                ..body
+            },
+            &mut buf,
+        );
+        body.height -= 1;
+    }
+    let height = usize::from(body.height);
+    let at = v.cursor.min(lines.len().saturating_sub(1));
+    // Scrolled just enough for the cursor's row to be the last one shown.
+    let start = (at + 1).saturating_sub(height);
+    for (i, text) in lines.iter().enumerate().skip(start).take(height) {
+        let y = body.y + u16::try_from(i - start).unwrap_or(u16::MAX);
+        let mut style = Style::default();
+        if i == at {
+            style = style.add_modifier(Modifier::BOLD);
+        }
+        if v.rows.get(i).is_some_and(|r| r.dimmed) {
+            style = style.add_modifier(Modifier::DIM);
+        }
+        Paragraph::new(Line::from(Span::styled(text.clone(), style))).render(
+            Rect {
+                y,
+                height: 1,
+                ..body
+            },
+            &mut buf,
+        );
+    }
+    buf
+}
+
 /// The config screen's column widths: the name, then the value.
 const NAME_COLS: usize = 18;
 const VALUE_COLS: usize = 18;
 /// The origin column, after the `*`.
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo test -p oxutrm-client --jobs 4 popup:: && cargo test --bin oxutrm --jobs 4 view:: session::`

Expected: all pass, the seven snapshots matching their files, `no_size_panics` (every size from 0×0 to 90×30), `a_start_time_is_the_clock_today_and_the_date_before` and `the_status_views_s_is_offered_only_on_a_live_link` among them.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add crates/oxutrm-client/src/lib.rs crates/oxutrm-client/src/popup.rs crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_19x5.snap crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_80x24.snap crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_at_connect_40x12.snap crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_sessions_question_72x24.snap src/session.rs src/snapshots/oxutrm__view__tests__snapshot_sessions_at_connect_72x24.snap src/snapshots/oxutrm__view__tests__snapshot_sessions_from_the_popup_80x24.snap src/snapshots/oxutrm__view__tests__snapshot_sessions_with_an_error_line_and_a_question_narrow.snap src/ui.rs src/view.rs
git commit -m "feat(view): the selector's view and layout; s offered on a live link

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

### Task 12: Wiring: the connect-time lobby opens the selector, answers fill it, a kill keeps it open

Spec §3.1, §3.2, §3.4, §3.5. Before every read the session tells the `Ui` whether it is in a lobby and whether a request is in flight. A connect-time lobby opens the selector once the splash is down (`open_selector`, checked in `layer_at`) and fetches the list. Answers reach the selector: a list is set into it; a switch or a lobby's `New` closes the popup (the user is in the new shell); a kill or a rename fetches the list again, and the selector stays -- without the `this` row after this session was killed, with `q quit` because the client is in a lobby now; a failure is the line under the list as `<what> failed: <why>` (summarised; the whole of it is in `client.log`). A rebuild that lands while in a lobby takes the new lobby's id and fetches the list again if the selector is up.

Review focus 5 is pinned here: `a_shell_that_exits_while_the_selector_is_open_ends_the_client`.

**Files:**
- Modify: `src/session.rs`, `src/ui.rs` (the last `expect`s come off), `src/main.rs` (`#[allow(dead_code)]` comes off `mod selector`)

**Interfaces:**
- Consumes: Tasks 9-11.
- Produces: `ClientSession` field `open_selector: bool`; no new interfaces.

- [ ] **Step 1: Write the failing tests**

The session tests that do a kill or a rename now see the list fetched again after it: `ask_now` sends every request the answer queues, as the loop would, and `a_rename_is_recorded_and_a_refused_one_says_why` expects `renaming build failed:` -- the name comes from the fetched list.

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -9994,14 +10022,22 @@ mod tests {
     }
 
     /// Send the request the client has waiting, as the loop does, and hand
     /// the answer back to it. Returns what `answered` returned.
+    ///
+    /// A list the answer asks for in turn -- after a kill or a rename -- is
+    /// fetched too, as the loop would.
     async fn ask_now(c: &mut ClientSession, a: crate::switcher::Ask) -> bool {
         assert!(c.ask(a), "another request was in flight");
-        let a = c.take_ask().expect("the request just asked");
-        let answered =
-            crate::switcher::ask(c.link.sink.connection().clone(), a, c.size, &c.net.clone()).await;
-        c.answered(answered, Instant::now()).expect("answered")
+        let mut first = None;
+        while let Some(a) = c.take_ask() {
+            let answered =
+                crate::switcher::ask(c.link.sink.connection().clone(), a, c.size, &c.net.clone())
+                    .await;
+            let moved = c.answered(answered, Instant::now()).expect("answered");
+            first.get_or_insert(moved);
+        }
+        first.expect("the request just asked")
     }
 
     fn id_of(s: &str) -> oxutrm_proto::SessionId {
         s.parse().unwrap()
@@ -10253,9 +10289,142 @@ mod tests {
         );
         assert!(!ask_now(&mut client, rename(name("logs"))).await);
         let (_, text) = last_entry(&client).unwrap();
         assert!(
-            text.starts_with("renaming 3ff1218f failed: ") && text.contains("taken"),
+            text.starts_with("renaming build failed: ") && text.contains("taken"),
             "{text}"
         );
     }
+
+    /// The spec's success line, from a connect: a bare connect to a host
+    /// with a session lands in a lobby, the selector opens over its blank
+    /// screen, and `⏎` switches to the session listed. That session has a
+    /// client of its own, so it is `in use`: the switch asks first, and the
+    /// other client is taken over (switcher spec §3.3).
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn a_lobby_opens_the_selector_and_enter_switches_into_the_session() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{SIZE, process, registry_holds, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let logs = process(
+            dir.path(),
+            LOGS_ID,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        let lobby = process(dir.path(), BUILD_ID, None, Begin::Lobby, sh()).await;
+        registry_holds(dir.path(), &[LOGS_ID]).await;
+
+        let client = client_on(lobby.client, BUILD_ID).with_lobby();
+        let (keys, mut typing) = keyboard();
+        let out = SharedOut::default();
+        let looping = tokio::spawn({
+            let mut out = out.clone();
+            let mut client = client;
+            async move { client.run_on(keys, &mut out).await }
+        });
+
+        wait_for_screen(&out, SIZE, "sessions on thinlinc", Duration::from_secs(10)).await;
+        wait_for_screen(&out, SIZE, "\u{25b8} logs", Duration::from_secs(10)).await;
+        wait_for_screen(&out, SIZE, "q quit", Duration::from_secs(10)).await;
+        wait_for_screen(&out, SIZE, "in use", Duration::from_secs(10)).await;
+        typing.write_all(b"\r").expect("type");
+        wait_for_screen(
+            &out,
+            SIZE,
+            "take over logs from its other client? y/n",
+            Duration::from_secs(10),
+        )
+        .await;
+        tokio::time::sleep(crate::ui::ANSWER_GUARD + Duration::from_millis(100)).await;
+        typing.write_all(b"y").expect("type");
+        wait_off_screen(&out, SIZE, "sessions on thinlinc", Duration::from_secs(20)).await;
+        let reason = tokio::time::timeout(
+            Duration::from_secs(5),
+            logs.client.sink.connection().closed(),
+        )
+        .await
+        .expect("the other client was not taken over");
+        assert!(
+            matches!(&reason, quinn::ConnectionError::ApplicationClosed(c)
+                if c.reason.as_ref() == TAKEN_OVER),
+            "{reason:?}"
+        );
+        typing.write_all(b"exit 7\n").expect("type");
+        let code = tokio::time::timeout(Duration::from_secs(15), looping)
+            .await
+            .expect("the client never ended")
+            .unwrap()
+            .unwrap();
+        assert_eq!(code, 7, "the shell that exited was logs'");
+        assert_eq!(logs.task.await.unwrap().unwrap(), 7);
+        // The lobby the client left ended with its link.
+        let ended = tokio::time::timeout(Duration::from_secs(10), lobby.task)
+            .await
+            .expect("the lobby outlived the client's move");
+        assert_eq!(ended.unwrap().unwrap(), 0);
+    }
+
+    /// Killing the session you are in: the selector stays, without `this`,
+    /// and `q` there ends the client -- and the lobby with it.
+    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
+    async fn killing_this_session_from_the_selector_keeps_it_open_as_a_lobby() {
+        use crate::serve::Begin;
+        use crate::serve::fixtures::{SIZE, process, registry_holds, sh};
+        let dir = tempfile::tempdir().unwrap();
+        let build = process(
+            dir.path(),
+            BUILD_ID,
+            Some("build"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        let _logs = process(
+            dir.path(),
+            LOGS_ID,
+            Some("logs"),
+            Begin::Session { name: None },
+            sh(),
+        )
+        .await;
+        registry_holds(dir.path(), &[BUILD_ID, LOGS_ID]).await;
+
+        let client = client_on(build.client, BUILD_ID);
+        let (keys, mut typing) = keyboard();
+        let out = SharedOut::default();
+        let looping = tokio::spawn({
+            let mut out = out.clone();
+            let mut client = client;
+            async move {
+                let code = client.run_on(keys, &mut out).await;
+                (code, client)
+            }
+        });
+
+        typing.write_all(&[CTRL_BACKSLASH]).expect("type");
+        wait_for_screen(&out, SIZE, "s sessions", Duration::from_secs(10)).await;
+        typing.write_all(b"s").expect("type");
+        wait_for_screen(&out, SIZE, "\u{25b8} build", Duration::from_secs(10)).await;
+        typing.write_all(b"x").expect("type");
+        wait_for_screen(&out, SIZE, "kill build? y/n", Duration::from_secs(10)).await;
+        tokio::time::sleep(crate::ui::ANSWER_GUARD + Duration::from_millis(100)).await;
+        typing.write_all(b"y").expect("type");
+        wait_for_screen(&out, SIZE, "q quit", Duration::from_secs(15)).await;
+        wait_off_screen(&out, SIZE, "this", Duration::from_secs(10)).await;
+        registry_holds(dir.path(), &[LOGS_ID]).await;
+
+        typing.write_all(b"q").expect("type");
+        let (code, client) = tokio::time::timeout(Duration::from_secs(10), looping)
+            .await
+            .expect("q in the lobby did not end the client")
+            .unwrap();
+        assert_eq!(code.unwrap(), 0);
+        assert!(client.in_lobby());
+        let ended = tokio::time::timeout(Duration::from_secs(10), build.task)
+            .await
+            .expect("the lobby outlived its client's quit");
+        assert_eq!(ended.unwrap().unwrap(), 0);
+    }
 }
````

`src/session.rs` -- append inside `mod tests`, at its end (review focus 5):

````rust
    /// The shell exiting on its own while the selector is open ends the
    /// client with its status, exactly as without the selector: only a kill
    /// from the selector leads to a lobby (switcher spec §1.1).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_shell_that_exits_while_the_selector_is_open_ends_the_client() {
        use crate::serve::fixtures::{SIZE, process, registry_holds};
        use crate::serve::{Begin, Shell};
        use std::os::unix::fs::PermissionsExt as _;
        let scripts = tempfile::tempdir().unwrap();
        let go = scripts.path().join("go");
        let script = scripts.path().join("quits");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nwhile [ ! -e '{}' ]; do sleep 0.1; done\nexit 3\n",
                go.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let build = process(
            dir.path(),
            BUILD_ID,
            Some("build"),
            Begin::Session { name: None },
            Shell {
                program: script.to_str().unwrap().to_string(),
                start: oxutrm_term::Start::default(),
                sibling: None,
            },
        )
        .await;
        registry_holds(dir.path(), &[BUILD_ID]).await;

        let client = client_on(build.client, BUILD_ID);
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let looping = tokio::spawn({
            let mut out = out.clone();
            let mut client = client;
            async move { client.run_on(keys, &mut out).await }
        });
        typing.write_all(&[CTRL_BACKSLASH]).expect("type");
        wait_for_screen(&out, SIZE, "s sessions", Duration::from_secs(10)).await;
        typing.write_all(b"s").expect("type");
        wait_for_screen(&out, SIZE, "\u{25b8} build", Duration::from_secs(10)).await;
        std::fs::write(&go, b"").unwrap();
        let code = tokio::time::timeout(Duration::from_secs(15), looping)
            .await
            .expect("the client outlived its shell")
            .unwrap()
            .unwrap();
        assert_eq!(code, 3);
    }
````

- [ ] **Step 2: Run the tests to see them fail**

Run: `cargo build --bin oxutrm --jobs 4 && cargo test --bin oxutrm --jobs 4 session::tests`

Expected: `a_lobby_opens_the_selector_and_enter_switches_into_the_session` and `killing_this_session_from_the_selector_keeps_it_open_as_a_lobby` fail: the selector never appears on the screen, and the list is never filled.

- [ ] **Step 3: Implement**

`src/main.rs`:

````diff
--- a/src/main.rs
+++ b/src/main.rs
@@ -38,10 +38,8 @@ mod listener;
 mod loopback;
 mod quality;
 mod rebuild;
 mod roam;
-// The session drives it from Task 12 on, which removes this attribute.
-#[allow(dead_code)]
 mod selector;
 mod serve;
 mod session;
 mod standby;
````

`src/session.rs`:

````diff
--- a/src/session.rs
+++ b/src/session.rs
@@ -386,8 +386,11 @@ pub struct ClientSession {
     /// The host's sessions, as last fetched.
     sessions: Vec<oxutrm_proto::SessionEntry>,
     /// What an attach exchange of the switcher's runs with: `apply`'s.
     net: oxutrm_net::NetConfig,
+    /// A connect-time lobby's selector is still to open: once the splash
+    /// is down (switcher spec §3.1).
+    open_selector: bool,
 }
 
 /// The startup splash while it shows; the picture is
 /// [`oxutrm_client::splash`]'s.
@@ -538,8 +541,9 @@ impl ClientSession {
             asking: None,
             asked: None,
             sessions: Vec::new(),
             net: oxutrm_net::NetConfig::default(),
+            open_selector: false,
         })
     }
 
     /// Open with the startup splash, drawn from `seed`, with `caption` under
@@ -654,18 +658,18 @@ impl ClientSession {
         self
     }
 
     /// Start in a lobby: the connect chose `Lobby` (switcher spec §3.1).
+    ///
+    /// The selector opens over the blank screen once the splash is down.
     pub(crate) fn with_lobby(mut self) -> ClientSession {
         self.in_lobby = true;
+        self.open_selector = true;
         self
     }
 
     /// Whether the far end is a lobby.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the selector uses it from Task 12 on")
-    )]
+    #[cfg(test)]
     pub(crate) fn in_lobby(&self) -> bool {
         self.in_lobby
     }
 
@@ -679,12 +683,8 @@ impl ClientSession {
         true
     }
 
     /// Whether a request is waiting or in flight.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the selector uses it from Task 12 on")
-    )]
     pub(crate) fn asking(&self) -> bool {
         self.asking.is_some() || self.asked.is_some()
     }
 
@@ -696,12 +696,9 @@ impl ClientSession {
         Some(a)
     }
 
     /// The host's sessions, as last fetched.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the selector uses it from Task 12 on")
-    )]
+    #[cfg(test)]
     pub(crate) fn sessions(&self) -> &[oxutrm_proto::SessionEntry] {
         &self.sessions
     }
 
@@ -739,8 +736,9 @@ impl ClientSession {
         use crate::switcher::{Answered, Ask};
         let asked = self.asked.take();
         match a {
             Answered::Sessions(list) => {
+                self.ui.selector_mut().set_rows(list.clone());
                 self.sessions = list;
                 Ok(false)
             }
             Answered::Landed(e) => {
@@ -752,8 +750,10 @@ impl ClientSession {
                     _ => format!("new session {}", &e.session_id[..8.min(e.session_id.len())]),
                 };
                 self.moved(*e, now)?;
                 self.activity.record_shown(Kind::Session, &label, &label);
+                // The selector closes: the user is in the new shell.
+                self.ui.close_popup();
                 Ok(true)
             }
             Answered::Started(entry) => {
                 self.in_lobby = false;
@@ -765,13 +765,17 @@ impl ClientSession {
                     Some(n) => format!("new session {}", oxutrm_client::legible(n.as_str())),
                     None => format!("new session {}", entry.id.short()),
                 };
                 self.activity.record_shown(Kind::Session, &label, &label);
+                self.ui.close_popup();
                 Ok(false)
             }
             Answered::Killed(id) => {
                 let label = format!("killed {}", self.label_of(&id));
                 self.activity.record_shown(Kind::Session, &label, &label);
+                // The list changed: fetched again, the selector staying
+                // open -- without the `this` row if that was this session.
+                self.ask(Ask::Sessions);
                 if Some(id) != self.here() {
                     return Ok(false);
                 }
                 // This session's shell is gone, and the process is a lobby
@@ -793,8 +797,9 @@ impl ClientSession {
                     ),
                     None => format!("{} has no name now", entry.id.short()),
                 };
                 self.activity.record_shown(Kind::Session, &label, &label);
+                self.ask(Ask::Sessions);
                 Ok(false)
             }
             Answered::Failed(why) => {
                 let what = match &asked {
@@ -809,8 +814,13 @@ impl ClientSession {
                     Kind::Session,
                     &format!("{what} failed: {why}"),
                     &format!("{what} failed"),
                 );
+                // Under the list, as one line; the detail is in client.log.
+                self.ui.selector_mut().fail(format!(
+                    "{what} failed: {}",
+                    oxutrm_client::summarised(&why)
+                ));
                 Ok(false)
             }
         }
     }
@@ -1280,8 +1290,9 @@ impl ClientSession {
         if self.end_splash() {
             self.paint(out, "painting the screen after the splash")?;
         }
         let phase = self.link_state.phase_now();
+        self.ui.set_switcher(self.in_lobby, self.asking());
         let routed = self.ui.keys(keys, phase, now);
         if !routed.to_host.is_empty() {
             self.turn(&routed.to_host, out)?;
         }
@@ -1351,8 +1362,15 @@ impl ClientSession {
     /// whole seconds, so an open popup with nothing happening repaints
     /// rarely and costs one comparison otherwise; a change of phase is
     /// reported the lap it happens.
     fn layer_at(&mut self, now: Instant) -> Option<Popup> {
+        // A connect-time lobby: the selector, once the splash is down.
+        if self.open_selector && self.splash.is_none() {
+            self.open_selector = false;
+            self.ui.set_switcher(self.in_lobby, self.asking());
+            self.ui.open_sessions();
+            self.ask(crate::switcher::Ask::Sessions);
+        }
         let owed = self.input_tx.current().seq() != self.screen_rx.peer_ack();
         let phase = self.link_state.evaluate(now, owed);
         match self.ui.tick(phase, now) {
             Some(LinkChange::WentSilent) => {
@@ -1629,8 +1647,18 @@ impl ClientSession {
                 Kind::Rebuild,
                 &format!("landed via {label} after the old link came back"),
             );
         }
+        // From a lobby a rebuild lands in a fresh one, with an id of its
+        // own; the selector, still open, lists what is there now.
+        if self.in_lobby {
+            if let Some(id) = self.identity.as_mut() {
+                id.session_id = e.session_id.clone();
+            }
+            if self.ui.mode() == Mode::Sessions {
+                self.ask(crate::switcher::Ask::Sessions);
+            }
+        }
         self.path = Some(e.path);
         Ok(())
     }
 
````

`src/ui.rs`:

````diff
--- a/src/ui.rs
+++ b/src/ui.rs
@@ -297,12 +297,8 @@ impl Ui {
     }
 
     /// What the selector needs to know about the session: whether it is in
     /// a lobby, and whether a request is in flight. Set before every read.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the session uses it from Task 12 on")
-    )]
     pub(crate) fn set_switcher(&mut self, lobby: bool, busy: bool) {
         self.lobby = lobby;
         self.busy = busy;
     }
@@ -319,22 +315,14 @@ impl Ui {
     pub(crate) fn selector(&self) -> &crate::selector::Selector {
         &self.selector
     }
 
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the session uses it from Task 12 on")
-    )]
     pub(crate) fn selector_mut(&mut self) -> &mut crate::selector::Selector {
         &mut self.selector
     }
 
     /// Close the popup, wherever it was: a switch landed, or a lobby
     /// became a session.
-    #[cfg_attr(
-        not(test),
-        expect(dead_code, reason = "the session uses it from Task 12 on")
-    )]
     pub(crate) fn close_popup(&mut self) {
         self.leave_config();
         self.mode = Mode::Closed;
     }
````

- [ ] **Step 4: Run the tests to see them pass**

Run: `cargo build --bin oxutrm --jobs 4 && cargo test --bin oxutrm --jobs 4 session::tests`

Expected: all pass: the connect-time lobby test (the spec's success line: lobby, selector, `⏎` on an `in use` session, `y` after the guard, its other client `TAKEN_OVER`, `exit 7` reaches its shell, the lobby ends), the kill-from-the-selector test (`kill build? y/n`, the selector stays with `q quit` and no `this`, `q` ends the client and the lobby), and the review-focus test.

- [ ] **Step 5: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0`. A failure in one of the load-sensitive loopback tests listed under Global Constraints is re-run alone before anything else is concluded.

- [ ] **Step 6: Commit**

````bash
git add src/main.rs src/session.rs src/ui.rs
git commit -m "feat(client): the selector on the screen: connect-time lobby, answers fill it, kill keeps it open

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

---

## Part 4 -- docs and the hand test

### Task 13: The changelog, the README, `docs/sessions.md`, and the hand test

**Files:**
- Modify: `CHANGES.md`, `README.md`
- Create: `docs/sessions.md`

**Interfaces:**
- Consumes: everything above.
- Produces: nothing in code.

- [ ] **Step 1: Write the docs**

The diffs and the new file below are the text to write. In `CHANGES.md`, the earlier Unreleased entry *Connecting to a host resumes your session there instead of replacing it* is replaced -- it describes the line picker and the silent resume that D1 removes -- by the two *New* entries below; the *Compatibility* entry about ending old sessions and the two *Changed* entries go at the top of their sections.

`CHANGES.md`:

````diff
--- a/CHANGES.md
+++ b/CHANGES.md
@@ -10,28 +10,38 @@
   bound. It is bound now: attaching hands the same shell to the new terminal,
   with the screen it has right now, and tells the terminal that had it that it
   was taken over rather than leaving it to report silence.
 
-- **Connecting to a host resumes your session there instead of replacing it.**
-  `oxutrm <ssh-target>` now asks the far end what is already running before
-  either side commits to anything. One session of yours, and it is resumed —
-  same shell, same scrollback, same screen — without asking. Several, and
-  oxutrm lists them and asks which; `q` leaves without touching any of them.
-  None, and you get a new one, exactly as before. `--attach <id>` names one
-  directly, by as few as four characters of its id, and `--new` starts a fresh
-  session however many are already there. Killing a client and reconnecting
-  used to strand the old session: it stayed live, holding your shell, and the
-  reconnect started a second one beside it.
-
-  Connecting prints one line saying which of the two happened — `oxutrm:
-  resumed session <id>.` or `oxutrm: new session <id>.` — before the terminal
-  goes into raw mode. The id is the one thing worth writing down to `--attach`
-  back into later, and the first word is there because landing in a two-day-old
-  session with a half-typed command already at the prompt should not be
-  something you have to work out from the screen. A session that cannot be
-  resumed because it tunnels its data through the ssh connection that created
-  it is still offered, and refused with that reason rather than quietly
-  omitted.
+- **A session selector, at connect and in the status popup.** A host may run
+  several oxutrm sessions; `s` in the status popup now lists them -- name,
+  shell, when each started, its size, and whether it is the one you are in,
+  attached to another client (`in use`), or did not answer (`?`) -- and from
+  the list `⏎` switches to one, `n` starts a new one, `r` names or renames one
+  in place and `x` kills one after asking. A switch travels over the live
+  link and never touches ssh: the new session's link is built while you stay
+  in the old one, and only once it is up does the client move, so a switch
+  that fails leaves you where you were with the reason under the list. A
+  session another client is attached to is taken over after asking, as
+  `--attach` does. Killing the session you are in leaves the selector open
+  over a blank screen until you pick another, start one, or quit with `q`;
+  killing never ends the client by itself. Every switch, kill and rename is in
+  the popup's log and in `client.log`. `docs/sessions.md` has the whole of it.
+
+- **Connecting to a host shows its sessions instead of resuming one.**
+  `oxutrm <ssh-target>` asks the far end what is running before either side
+  commits to anything. With nothing of yours there you get a new session,
+  exactly as before. With any session running -- even exactly one -- the
+  selector opens over a blank screen, after the splash: nothing is resumed
+  silently any more. `--attach <name>` or `--attach <id-prefix>` (four
+  characters or more) goes straight to one session, and `--new` starts a fresh
+  one however many are running, named with `--name <name>`. A name is up to 24
+  characters and needs at least one outside `0-9a-f`, so it can never be read
+  as an id; names are unique on a host. Connecting prints one line before the
+  terminal goes into raw mode: `oxutrm: resumed session <id>.`, `oxutrm: new
+  session <name> (<id>).`, or how many sessions are running when the selector
+  is about to open. A session that cannot be resumed because it tunnels its
+  data through the ssh connection that created it is listed dimmed and refused
+  with that reason.
 
 - **A client whose network dies reconnects by itself.** After twenty seconds of
   silence — long enough that a blip is not raced against an outage about to end
   on its own — the client builds a new link back into the same session: a fresh
@@ -160,8 +170,16 @@
   described in `docs/config.md`.
 
 ### Compatibility
 
+- **The client and every host session must be the same version: end your old
+  sessions before upgrading.** The session switcher changed the wire between
+  the client and a session process, and between sessions, and
+  `PROTO_VERSION` is now 3. A client of this version refuses a host session
+  started by an older binary with both version numbers in the message, and the
+  selector lists such a session as `old version`, which can be ended only by
+  ending its shell. There is no compatibility mode.
+
 - **`network.birthday = false` stops the birthday punch at both ends only
   against an upgraded host.** The client asks with a new `no-birthday` hello
   feature, which an older host ignores: there only the client's half of the
   blast stops. Behind a symmetric NAT the birthday punch is the last rung that
@@ -178,8 +196,17 @@
   field lives in the hellos and this exchange happens before them.
 
 ### Changed
 
+- **New shells are login shells in your home directory**, as ssh would start
+  them: `argv[0]` is `-bash` (or whatever your shell is), so your profile is
+  read, and the shell starts in `$HOME` rather than in `/`. This holds for a
+  first connect and for sessions started from the selector alike.
+
+- **`oxutrm host --list` shows each session's name**, in a column after its
+  id, and `meta.json` is now replaced atomically, so a listing never reads a
+  half-written one.
+
 - **The hellos now say what a peer can do.** `HostHello` and `ClientHello`
   carry a `features` list — empty today except for the host, which advertises
   `control` and `standby`. Absent on either side, it is read as "nothing", so
   an older peer on the other end of the exchange is unaffected and there is no
````

`README.md`:

````diff
--- a/README.md
+++ b/README.md
@@ -73,21 +73,24 @@ and no macOS package is built, so treat it as unproven rather than supported.
 
 ## Using it
 
 ```
-oxutrm <ssh-target>          # resume your session there, or start one
-oxutrm --attach <id> <tgt>   # resume that one in particular
-oxutrm --new <ssh-target>    # start a fresh one regardless
-oxutrm host --list           # sessions on this machine
+oxutrm <ssh-target>                    # pick a session there, or start one
+oxutrm --attach <name|id> <ssh-target> # go straight to that one
+oxutrm --new [--name <name>] <tgt>     # start a fresh one regardless
+oxutrm host --list                     # sessions on this machine
 oxutrm loopback              # both halves in one process, no network
 ```
 
 `oxutrm <ssh-target>` works: it drives ssh, races the connection ladder, brings
 up QUIC on whichever rung wins, and hands you the shell. The session survives
 the client going away, and you get it back by connecting again: the far end
-offers what is already running, a single session of yours is resumed without
-asking, and with several oxutrm asks which. `--attach` names one directly and
-`--new` always starts a fresh one.
+offers what is already running, and with any session there the **session
+selector** opens -- to switch to one, start a new one, rename or kill one.
+Nothing is resumed silently. `--attach` goes straight to a session by its name
+or the start of its id, and `--new` always starts a fresh one, named with
+`--name`. The same selector is `s` in the status popup, and switching there
+uses the live link, not ssh. See `docs/sessions.md`.
 
 A client whose network dies reconnects by itself. After twenty seconds of
 silence it rebuilds the link — a new ssh, back into the same session. The first
 attempt is immediate, since the twenty seconds have already been waited, and
````

`docs/sessions.md` (new):

````markdown
# Sessions and the session selector

A host can run several oxutrm sessions at once: each is one process with one
shell, listed by `oxutrm host --list`. The **session selector** is how you move
between them from the client.

## Connecting

| You type | With no session running | With sessions running |
|---|---|---|
| `oxutrm <target>` | a new session | the selector, over a blank screen |
| `oxutrm --new [--name <name>] <target>` | a new session | a new session |
| `oxutrm --attach <name\|id> <target>` | refused: nothing to attach to | that session |

Even exactly one running session opens the selector: nothing is resumed
silently. `--attach` takes a session's exact name, or at least four
characters of its id; a name always contains a character outside `0-9a-f`,
so the two can never be confused. `--name` without `--new` is refused.

## The selector

`s` in the status popup opens it on a live link (it is dimmed during an
outage); after a bare connect to a host with sessions it opens by itself once
the splash is down.

```
╭ sessions on thinlinc ─────────────────────╮
│   a3f9c01e  fish  Oct 05  80×24   in use  │
│ ▸ build     bash  09:14   120×40  this    │
│   logs      zsh   11:02   120×40          │
│   + new session                           │
├───────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q back │
╰───────────────────────────────────────────╯
```

Each row is a session: its name (or the start of its id), its shell, when it
started (the time today, the date before), its size, and a mark -- `this` for
the session you are in, `in use` for one attached to another client, `?` for
one that did not answer in time, `old version` for one started by another
version of oxutrm. A session that cannot be reattached (it tunnels its data
through the ssh connection that created it) is dimmed.

| Key | Does |
|---|---|
| `↑` `↓` (`k` `j`) | move |
| `⏎` | switch to that session; on `+ new session`, start one; on `this`, close |
| `n` | start a new session |
| `r` | name or rename the session in place: `⏎` saves, `Esc` cancels, an empty name clears it |
| `x` | kill the session, after `kill build? y/n` |
| `q` `Esc` | back to the popup -- or, with no session to go back to, quit |

Switching to a session that is `in use` asks first, then takes it over: its
other client is told it was taken over, as with `--attach`. A question's
`y` does nothing for the first half second, so typing already in flight
cannot answer it.

A switch travels over the live link -- never over ssh. The new session's link
is built while you stay in the old one, and only once it is up does the client
move; a switch that fails leaves you where you were, with the reason under the
list and in the popup's log. The session you left is detached, as if your
client had gone away, and is still listed.

Killing a session hangs its shell up the way a closing terminal does, and
kills what is left of it three seconds later. Killing the session you are in
keeps the selector open over a blank screen -- with only `+ new session` if it
was the last one -- until you pick another, start one, or quit. Killing never
ends the client by itself; a shell that exits on its own (`exit`, a crash)
still does.

While the link is down the selector stays open, says so in its header, and
refuses its actions until the link is back.

## Names

A name is 1 to 24 characters, printable, with no space at either end, and at
least one character outside `0-9a-f`. Names are unique among a host's live
sessions. Set one with `--new --name`, or with `r` in the selector.

## New shells

Every new shell -- from a first connect, from the selector, after a kill -- is
a login shell (`-bash`) started in your home directory, as ssh would start it.

## Versions

The client and every host session speak one protocol version. After an
upgrade, end the sessions an older binary started before connecting with the
new client: it refuses them, and the selector lists them as `old version`.
````

- [ ] **Step 2: The gate**

Run: `cargo fmt --all`, then `make check > /tmp/oxutrm-check.log 2>&1; echo $?`

Expected: `0` (the help text tests in `src/main.rs` and `tests/client_flags.rs` already match the usage Task 7 wrote).

- [ ] **Step 3: Commit**

````bash
git add CHANGES.md README.md docs/sessions.md
git commit -m "docs: the session selector -- changelog, README, docs/sessions.md

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>"
````

- [ ] **Step 4: The hand test (after the branch is pushed; not part of the gate)**

On **thinlinc** (built there after the push) and on the **Mac**, each as the host, from the other as the client. **The protocol version changed: end every host session an older binary started first** (`oxutrm host --list` with the old binary, end their shells), or the new client refuses them.

1. `oxutrm <host>` with nothing running: `oxutrm: new session <id>.`, the splash, a shell. `echo $0; pwd` says `-bash` (or your shell) and your home directory.
2. `oxutrm --new --name build <host>`, then `oxutrm --new --name logs <host>` from another terminal; `oxutrm host --list` on the host shows both names. `oxutrm --new --name cafe <host>` is refused before ssh, naming the rule; `oxutrm --name x <host>` is refused, naming `--new`.
3. Detach both (close the terminals). `oxutrm <host>`: `oxutrm: 3 sessions are running here; ...` (or however many), the splash, then the selector over a blank screen, `q quit` in its key bar. `⏎` on `build`: within about one ICE exchange you are in `build`, its screen as you left it.
4. `Ctrl-\`, `s`: the list, `▸` on `build` marked `this`, start times as `HH:MM`. `↓`, `⏎` on `logs`: in `logs`; the popup's log says `switched to logs`. `oxutrm host --list`: `build` still there, detached.
5. `r` on `logs`, type `tail`, `⏎`: renamed in the list and in `--list`; `oxutrm --attach tail <host>` from another terminal goes straight to it (that terminal now holds it).
6. Back in the first client: `s`; `tail` is `in use`. `⏎` asks `take over tail from its other client? y/n`; `y` within half a second does nothing, after it switches; the other terminal says it was taken over and exits.
7. `x` on another session, `y`: gone from the list and from `--list`. Start a session running `vim` (or `sh -c "trap '' HUP; sleep 1000"`) and kill it: gone within about three seconds.
8. `x` on `this`, `y`: the selector stays, without `this`, `q quit` in the key bar, a blank screen behind it. `n`: a new session, popup closed. Kill it again, then `q`: the client ends, and `--list` shows no lobby (none ever appears there).
9. With the selector open, take the network away (Wi-Fi off): the header says `no reply ...`, `⏎`/`n`/`x` say the link is down, arrows still move. Network back: the actions work again. Close the selector and let the outage run past the rebuild: it rebuilds into the same session. Do the same in a lobby: it rebuilds into a fresh lobby with the selector open.
10. In a session, type `exit`: the client ends with the shell's status, as before -- no lobby.
11. If an old-binary session was kept on purpose: it is listed `old version`, `⏎` and `x` refuse with the reason, `oxutrm --attach <it>` fails with both version numbers.

---

## Spec coverage

| Spec | Where |
|---|---|
| §1.1 transport: requests over the live link, ssh only to initiate | Tasks 5, 8, 9 (no ssh after `Established`); the rebuild is unchanged |
| §1.1 compatibility: `PROTO_VERSION` bumped, no fallbacks | Task 5; Task 13's changelog |
| §1.1 names: `r`, `--new --name`, `--attach <x>`, `--list` | Tasks 3, 6, 7, 10 |
| §1.1 `in use`: confirm, then take over | Tasks 8, 10, 12 |
| §1.1 connect without flags: any session -> the selector | Tasks 7, 12 |
| §1.1 killing the session you are in / the last one; a silent lobby lives `DETACH_AFTER` | Tasks 6, 9, 12 |
| §1.1 new shells in `$HOME` as login shells | Task 4 (pty), Task 6 (process) |
| §1.1 names never read as id prefixes | Tasks 2, 7 |
| §1.1 a shell exiting on its own ends the client | unchanged path; pinned with the selector open in Task 12 |
| §2.1 lobby: id from the start, unregistered, becomes the session on `New` | Tasks 6, 9 |
| §2.2 doors, task per connection, only `Attach` to the serial loop, owner serves | Tasks 5, 6, 8 |
| §2.3 names, `names.lock`, atomic `meta.json`, `serde(default)` | Tasks 2, 3 |
| §3.1 the table, `--name` without `--new` refused, picker removed, lobby path | Task 7 |
| §3.2 the selector: rows, keys, confirmations, errors, greyed `s`, outage | Tasks 10, 11, 12 |
| §3.3 switch: relay, ordinary attach, make-before-break, `MOVED_AWAY`, failure | Tasks 8, 9 |
| §3.4 new (lobby, sibling, one startup path, which binary), kill, rename | Tasks 4, 6, 8, 9 |
| §3.5 recovery aims at the session or a fresh lobby | Task 9 |
| §4.1 `Open`, `Request`, `Reply`, `SessionEntry`, `Attached`, validated newtypes, `Signal` purely the exchange | Tasks 2, 5 |
| §4.2 the ssh offer, `Choice` | Task 7 |
| §4.3 `MOVED_AWAY` | Task 9 |
| §5 code layout, Task 1 split | Tasks 1, 5, 10, 11 |
| §6 tests: pure, snapshots, loopback integration | every task; the loopback list below |
| §6 hand test, changelog, README, docs | Task 13 |

The §6 loopback list, test by test: switch between two sessions -- `a_switch_moves_the_client_and_the_old_session_stays_listed` (Task 9); a dead target -- `a_switch_to_a_session_that_is_gone_is_refused_and_changes_nothing` (8), `a_switch_that_fails_leaves_the_client_where_it_was_and_says_why` (9); an `in use` target -- `a_switch_runs_the_targets_ordinary_attach_and_leaves_this_session_be` (8), `a_lobby_opens_the_selector_and_enter_switches_into_the_session` (12); new from a session and from a lobby, named -- `new_in_a_session_starts_a_named_sibling_through_the_one_startup_path` (8), `new_in_a_lobby_registers_it_by_name_and_it_becomes_the_session` (6); kill a sibling, kill self, kill the last, a shell ignoring SIGHUP -- `killing_a_sibling_ends_it_as_a_shell_that_exited`, `killing_this_session_leaves_a_lobby_that_can_start_again`, `a_shell_that_ignores_the_hang_up_is_killed_and_then_done` (6); `Myself` while mid-attach, `Sessions` not cancelling a standby search -- Task 5's two listener tests; a vanished client's lobby -- `a_lobby_whose_client_vanished_ends_after_the_silence` (6); a rebuild after kill-self or from a lost lobby lands in a fresh lobby -- `an_attempt_from_a_lobby_asks_for_a_fresh_lobby` and `killing_this_session_puts_the_client_in_the_lobby_and_new_takes_it_out` (9), **at unit level only** (an end-to-end rebuild needs ssh; hand test step 9); rename, taken, all-hex -- (6); login shells in `$HOME` -- (4, 6); bare connect with one session -> lobby -- `one_session_or_more_lands_in_the_lobby` (7), `a_lobby_choice_reaches_the_serve_path` (7), the Task 12 lobby test; `--attach <name>` and `<prefix>` -- (7).

## Deviations from the spec

1. **`Reply::ProbeAck { nonce }`** is a fifth `Reply` variant (Task 5). §4.1 lists four; `Probe` keeps its semantics and needs an answer now that `Signal` is purely the attach exchange.
2. **`HostHello.session_id` stays a `String`** (Task 2). The `SessionId` newtype carries every new wire type (`Open`'s requests, `SessionEntry`, `OfferEntry`, `Choice`); the hello was left alone to keep the attach exchange's diff out of D1.
3. **`meta.json`'s `name` is a plain string**, checked as a `Name` only where shown or sent (Task 3), so an entry with a name a later rule refuses still lists.
4. **A kill closes the pty master after the shell is reaped**, not right after the SIGHUP (Task 4): the reap on macOS needs the pty drained.
5. **A new close reason, `QUIT`**, for a client leaving a lobby with `q` (Task 9), so the lobby ends with its link instead of after `DETACH_AFTER`.
6. **`Myself` answered with anything but an `Entry` is `OtherVersion`** (Task 5); there is no dedicated version refusal in `Reply`, and a version refusal is a `Refused` like any other.
7. **`s` is dimmed without a reason text of its own** (Task 11); the popup's header already says why the link is not live.
8. **The rebuild into a fresh lobby is not tested end to end** (Task 9): it is covered by unit tests and hand-test step 9 only, because an end-to-end rebuild needs ssh.

## Judgement calls to scrutinise

- `⏎` on `this` closes the popup, not just the selector (Task 10).
- The popup key does nothing on a lobby's selector (Task 10).
- The held-input question takes the selector down to the status view (Task 10).
- A switch stands down a rebuild attempt in flight before it swaps (`moved`, Task 9): the attempt was for the session being left.
- A sibling's name is checked by the parent before it spawns and again at the sibling's registration; a lost race fails the sibling after its exchange, and the client sees its new link close (Task 8).
- `serve::fixtures::built_binary()` finds the binary by path (`$CARGO_TARGET_DIR` or `target/`, then `debug/oxutrm`), because `CARGO_BIN_EXE_oxutrm` is not set for unit tests (Task 8).
- `NamesLock::take` is a blocking `flock` called from door tasks (Task 3).
- The lobby's `New` starts the same shell, the same way, as the process's first connect would have (`$SHELL`, login, `$HOME`), resolved before the lobby existed (Tasks 4, 6).
- `--attach x` with fewer than four characters is still tried as an exact name before it is refused as too short an id (Task 7).
- The selector's box width follows its content and so changes with the line under the list (Task 11).
- **The loopback attach exchanges these tests add make the suite more sensitive to a noisy network.** Every such test runs real ICE over the machine's own interface addresses (`local_candidates` leaves loopback out). On the machine this plan was made on, `make check` passed after each of Tasks 1-12 as they were built; later the same day, with two interfaces on one subnet and a video call running, it failed three to six of these exchanges (`control::tests::a_standby_search_lands_a_second_connection_to_the_same_host`, the `listener` standby tests, `serve`'s switch and sibling tests) at four and at two test threads -- each with "no validated path after 3s" -- while the branch the plan starts from still passed. The full suite passed with `--test-threads 1` (1344 tests), and once the load dropped, `make check` on the finished branch passed again: exit 0, 1344 tests. The test binary used 11 s of CPU over a 40 s run, so it is not a busy loop; it is more ICE at once than the old suite had. If the executor's machine shows the same, the remedy belongs to the test harness (a loopback-only candidate set for tests), not to these tasks -- say so rather than weakening a test.

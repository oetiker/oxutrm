# Tier B1 — the host accepts a second attach

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A running, detached oxutrm session accepts a second attach over its
Unix socket, so `oxutrm host --attach <id>` moves a live shell to a new client.

**Architecture:** `serve()`'s R4–R10 — key material, socket, STUN, hello
exchange, ICE, QUIC accept — is extracted into one function generic over its
signalling stream, so the ssh pipes and an accepted `UnixStream` run *the same
code*. R11–R13 (settle detachability, sever from ssh, register) stay behind in
`serve()` as first-connect-only. After registering, `serve()` binds the socket
path that has always been computed and never bound, and a task accepts on it,
runs the same exchange, and sends the finished `Link` to the running
`HostSession` over an mpsc that its loop selects on as a **local**.

**Tech Stack:** Rust edition 2024, MSRV 1.96, tokio, quinn (vendored
`quinn-proto`), `oxutrm-sync`, `oxutrm-proto`, `oxutrm-host`.

**Spec:** `docs/superpowers/specs/2026-08-29-session-recovery-design.md` — §5.1
(the rule), §5.2 (host side), §5.4 (what survives the swap), §6 (takeover),
§12 (phasing: this is phase 3, less the client rebuild loop).

## Scope

This plan is **B1 only**, the first of Tier B's four slices:

- **B1 (this plan)** — host listener, generic exchange, `host --attach`.
- **B2** — the client rebuild loop and `REBUILD_AFTER`. Not here.
- **B3** — `oxutrm askpass` (spec §5.5). Not here.
- **B4** — the `Displaced` state and take-it-back key (spec §6). Not here.

B1 closes the displaced link **with a reason naming the takeover** — without
that the displaced client reports "the host has stopped answering", which is
false. The `Displaced` UI state itself is B4.

## Global Constraints

Copied verbatim from the standing record. Every task's requirements include
these.

- **`oxutrm-host` MUST NOT depend on `oxutrm-net`.** `HostSession` and `Link`
  live in the root binary crate, which is why all of this lands in `src/`.
- **The host loop's arms borrow locals, never `self`** (constraint C1) — the
  descriptors are duplicated before the loop for exactly this reason. The new
  mpsc arm follows the rule or the code does not compile.
- **Do not add a `conn.closed()` arm to the host loop.** A closed connection is
  permanently ready and the arm would spin.
- **Reattachment is not a second code path** (`crates/oxutrm-host/src/attach.rs:3`).
  No reattach-specific handshake. If something only works on reattach, it is
  untested by every ordinary connect.
- **Both sides reset their sequence counters to 1 at every attach, and the
  host's first datagram of each attach is a full state** (design spec §8.5).
- **`max_idle_timeout` stays `None`, and explicitly `None`.** Do not
  reintroduce it as a liveness signal for a stale attach.
- **A send failure must never end a session and a rejected frame must never
  disconnect.**
- **`IDLE_POLL` is not to be reintroduced as a pace.**
- Cap build/test parallelism at 4 — use `make`, which does it. `make check` is
  the gate: `cargo fmt --all -- --check`, `cargo clippy --workspace
  --all-targets -- -D warnings`, then the tests.
- **A changelog entry is part of the work** (`CHANGES.md`, under `Unreleased`).
- `oxutrm-client` is `deny(unsafe_code)`; `src/main.rs` is `forbid`.

## Test discipline — read before writing any test

This project's two signature defects, both of which have recurred:

1. **A guard that cannot fail.** Five instances so far. **Every task below ends
   with an injection step: break the thing deliberately, watch the new test
   fail, put it back.** A test that has not been seen to fail is not a guard.
2. **Asserting a return value that means two things.** Assert the side effect —
   the `attach_id` that arrived, the frame that was applied, the reason on the
   close — not a bare `bool`.

Also standing: **a test that waits on a host must turn the host**, or it is a
fixture that stalls rather than a host that answers.

## File Structure

- **Create `src/attach_exchange.rs`** — the seam. `Attached`, and
  `run_attach_exchange`, generic over the signalling stream. One
  responsibility: turn a pair of signalling pipes into a live `Link`.
- **Create `src/listener.rs`** — bind the session socket, accept, run the
  exchange, hand the result to the session. One responsibility: the second
  attach's front door.
- **Modify `src/serve.rs`** — `serve()` calls `run_attach_exchange` for the
  first connect, keeps R11–R13, then starts the listener. Loses the R4–R10
  body it gives away.
- **Modify `src/session.rs`** — `HostSession::adopt`, the mpsc arm, and
  `HostWake::Attached`.
- **Modify `src/main.rs`** — `--attach` stops returning its error.
- **Modify `CHANGES.md`**.

---

### Task 1: The seam — extract R4–R10 into a stream-generic function

The single riskiest step in the plan. R11 (`settle_detachability`), R12
(`sever_from_ssh`) and R13 (register) sit in the middle of today's linear flow
and **must not travel into the reusable part**: a reattach that severs a second
time or re-registers a live session is the failure this seam exists to prevent.

**Files:**
- Create: `src/attach_exchange.rs`
- Modify: `src/serve.rs:77-205` (the body of `serve`), `src/main.rs` (add `mod attach_exchange;`)
- Test: `src/attach_exchange.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `oxutrm_host::{begin_attach, Attach}`, `oxutrm_net::{bind_socket,
  local_candidates, stun_discover, quic_server, generate_cert, NetConfig,
  IceRole}`, `crate::ladder::{nominate, Ladder, Nomination}`,
  `crate::candidates::{inbound_candidates, outbound_candidates}`,
  `crate::accept::accept_one`, `crate::link::Link`.
- Produces, and later tasks rely on these names exactly:

```rust
/// One completed attach: the transport, and the two facts the caller needs
/// about it.
pub(crate) struct Attached {
    pub link: Link,
    pub path: PathDescription,
    /// The client's terminal size, from its `ClientHello`.
    pub client_size: TermSize,
    /// The generation this attach ran as. `SessionMeta::attach_id` after
    /// `begin_attach` bumped it.
    pub attach_id: u64,
}

pub(crate) async fn run_attach_exchange<R, W>(
    reader: R,
    writer: W,
    meta: &mut SessionMeta,
    cfg: &NetConfig,
) -> anyhow::Result<Attached>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static;
```

The `Send + 'static` bounds are not decoration: `inbound_candidates` and
`outbound_candidates` are moved into `tokio::spawn` today, and they must keep
being. A `tokio::net::UnixStream` satisfies them via
`into_split()` → `OwnedReadHalf`/`OwnedWriteHalf`; wrap the read half in
`tokio::io::BufReader`.

- [ ] **Step 1: Write the failing test — the seam does not sever or register**

This is the test that pins the seam. It does not need a network: it asserts on
what the *function's own body* is allowed to reach, by checking that the
detachability field the caller settles is untouched by the exchange.

Add to `src/attach_exchange.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A session record as R4 finds it, before any attach.
    fn fresh_meta(session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_owned(),
            attach_id: 0,
            pid: std::process::id(),
            created_unix: 0,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: false,
        }
    }

    /// R11 is the caller's, not the exchange's.
    ///
    /// `detachable` is settled from the *nominated rung* by
    /// `settle_detachability`, which the first connect calls and a reattach
    /// must not. If the exchange ever sets it, a reattach would re-decide a
    /// question that was answered once, with a rung that means something
    /// different over a Unix socket than it does over ssh.
    #[tokio::test]
    async fn the_exchange_never_settles_detachability() {
        let mut meta = fresh_meta("seam");
        // The client hangs up immediately, so the exchange fails at R7. What
        // it did to `meta` before failing is the point.
        let (client, host) = tokio::io::duplex(64);
        drop(client);
        let (r, w) = tokio::io::split(host);
        let _ = run_attach_exchange(
            tokio::io::BufReader::new(r),
            w,
            &mut meta,
            &NetConfig::default(),
        )
        .await;

        assert!(
            !meta.detachable,
            "the exchange settled detachability itself; that is R11 and it \
             belongs to the caller, or a reattach re-decides it: {meta:?}"
        );
        assert_eq!(
            meta.attach_id, 1,
            "the exchange must still bump the generation — that IS R4 — but it \
             got {}: {meta:?}",
            meta.attach_id
        );
    }
}
```

- [ ] **Step 2: Run it and watch it fail to compile**

Run: `cargo test --workspace --jobs 4 attach_exchange -- --test-threads 4`
Expected: FAIL — `run_attach_exchange` does not exist.

- [ ] **Step 3: Move R4–R10 across, unchanged**

Create `src/attach_exchange.rs` and move the body of `serve()` from the R4
comment (`src/serve.rs:83`) through the `Established` write (`src/serve.rs:205`)
into `run_attach_exchange`, changing only what has to change:

- `let mut stdin = ...` / `let mut stdout = ...` become the `reader`/`writer`
  parameters. Delete the two `tokio::io::stdin()`/`stdout()` lines; they belong
  to the caller now.
- `meta` is a `&mut SessionMeta` parameter instead of a local. Delete its
  construction; the caller builds it.
- `cfg` is a parameter. Delete `let cfg = NetConfig::default();`.
- The function ends by returning `Attached { link: Link::new(connection,
  endpoint, nomination.socket), path, client_size: client.size, attach_id:
  meta.attach_id }` instead of falling through to R11.
- **`meta.size = client.size;` stays inside** — it is R7's own result, not R11.

Everything else — the two-task candidate pump, the `drop(learned_tx)` ordering
that keeps `Established` unmixed with a truncated `CandidateUpdate`, the
`Signal::Failed` arm that names all five rungs, `accept_one`'s `?` — moves
verbatim. **Do not improve it while moving it.**

Add `mod attach_exchange;` to `src/main.rs` beside the other `mod` lines.

- [ ] **Step 4: Rewrite `serve()` around the call**

`src/serve.rs`'s `serve()` becomes:

```rust
async fn serve(detached: oxutrm_host::Detached, root: &RegistryRoot) -> anyhow::Result<()> {
    let cfg = NetConfig::default();
    let mut meta = SessionMeta {
        session_id: oxutrm_host::new_session_id().context("naming the session")?,
        attach_id: 0,
        pid: std::process::id(),
        created_unix: oxutrm_host::now_unix(),
        shell: std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_owned()),
        size: TermSize { cols: 80, rows: 24 },
        detachable: false,
    };

    let attached = crate::attach_exchange::run_attach_exchange(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
        &mut meta,
        &cfg,
    )
    .await?;

    // R11. The outcome, at last, from the rung that was actually nominated.
    let permit = oxutrm_host::settle_detachability(&mut meta, attached.path.rung);

    // R12, R13, R14, R15, R16 exactly as before.
    ...
}
```

R12's `match permit` block, R13's `RegistryGuard::register_in`, R14's
`HostSession::spawn`, and R15/R16 stay byte-for-byte as they are, with
`client.size` becoming `attached.client_size` and the `Link::new(...)`
expression becoming `attached.link`.

- [ ] **Step 5: Run the new test and the whole suite**

Run: `make check`
Expected: PASS, and **the same 35 test-result lines with 0 failures as before
the move**. The existing `serve` tests still cover `host_hello`,
`exchange_hellos` and `path_description`; if any of them no longer compile
because a helper moved, move the test with its helper rather than deleting it.

- [ ] **Step 6: Injection — prove the seam test can fail**

Add `meta.detachable = true;` inside `run_attach_exchange`, run
`cargo test --workspace --jobs 4 the_exchange_never_settles_detachability -- --test-threads 4`,
and confirm it **FAILS** with the "that is R11 and it belongs to the caller"
message. Remove the line. Re-run: PASS.

Do the same for the generation half: change `begin_attach`'s call site to run
twice and confirm the `attach_id == 1` assertion fails at 2. Put it back.

- [ ] **Step 7: Commit**

```bash
git add src/attach_exchange.rs src/serve.rs src/main.rs
git commit -m "refactor(serve): the attach exchange, generic over its pipes

R4-R10 are the same work whether the signalling arrives on ssh's pipes or on
a session's Unix socket, and spec 5.1 forbids a second code path for the
second case. They move out of serve() unchanged, behind a reader/writer pair.

What deliberately does NOT move is R11-R13: settling detachability, severing
from ssh, and registering are the first connect's own, and a reattach that
re-ran them would sever twice and re-decide a question already answered. The
seam is pinned by a test, and the test was checked by injection."
```

---

### Task 2: `HostSession` adopts a new link

**Files:**
- Modify: `src/session.rs:140-158` (struct), `src/session.rs:160-199` (`spawn`), `src/session.rs:477-495` (the select), and the `HostWake` enum
- Test: `src/session.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::attach_exchange::Attached` from Task 1.
- Produces:

```rust
/// Why a link was closed because another one arrived.
///
/// Read by the displaced client so it can say it was taken over rather than
/// reporting silence. Spec §6; the `Displaced` state itself is B4.
pub const TAKEN_OVER: &[u8] = b"taken over by a newer attach";

impl HostSession {
    /// Swap a freshly attached link in for the current one.
    ///
    /// Design spec §8.5: both sync channels restart at sequence 1 and the
    /// first datagram of the new attach is a full state. `screen_stale` is
    /// what forces that snapshot on the next turn.
    pub fn adopt(&mut self, link: Link, size: TermSize) -> Result<()>;
}
```

- [ ] **Step 1: Write the failing tests**

Add to `src/session.rs`'s test module. The existing `pair(shell)` helper
(`src/session.rs:1630`) builds a connected `HostSession`/`ClientSession`; a
second `pair` gives a second link to adopt.

```rust
/// The displaced connection is closed, and closed with a reason that says
/// what happened.
///
/// Without the reason the displaced client reports "the host has stopped
/// answering", which is false: the host is answering, to somebody else.
#[tokio::test]
async fn adopting_a_link_closes_the_old_one_saying_it_was_taken_over() {
    let (mut host, client) = pair("/bin/sh").await;
    let (_host2, client2) = pair("/bin/sh").await;

    let old = client.link.sink.connection().clone();
    host.adopt(client2.link, TermSize { cols: 80, rows: 24 })
        .expect("adopting a second attach");

    let reason = old.closed().await;
    let quinn::ConnectionError::ApplicationClosed(closed) = reason else {
        panic!("the displaced connection ended with {reason:?}, not an \
                application close naming the takeover");
    };
    assert_eq!(
        closed.reason.as_ref(),
        TAKEN_OVER,
        "displaced with the wrong reason; the client cannot tell a takeover \
         from silence"
    );
}

/// §8.5: the first frame of a new attach is a FULL state, not a diff against
/// a base the new client has never seen.
///
/// Asserts the side effect — `from_state` on the frame that actually arrives —
/// rather than any return value.
#[tokio::test]
async fn the_first_frame_after_adopting_is_a_full_state() {
    let (mut host, client) = pair("/bin/sh").await;
    // Let the first client get a real screen, so the emulator is NOT blank
    // and a diff-from-current would be visibly different from a full state.
    host.turn_at(Instant::now(), None).expect("first turn");
    drop(client);

    let (_host2, client2) = pair("/bin/sh").await;
    let mut newcomer = client2;
    host.adopt(newcomer.link_take(), TermSize { cols: 80, rows: 24 })
        .expect("adopting");

    let turn = host.turn_at(Instant::now(), None).expect("turn after adopting");
    let frame = turn.sent.expect("a frame is owed to a client that just arrived");
    assert_eq!(
        frame.from_state, 0,
        "the newcomer was sent a diff against state {} it has never seen; \
         §8.5 requires a full state on the first datagram of an attach",
        frame.from_state
    );
}
```

If `ClientSession` has no way to surrender its `Link`, add
`pub(crate) fn link_take(self) -> Link { self.link }` beside it rather than
making the field public.

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --workspace --jobs 4 adopting -- --test-threads 4` and
`cargo test --workspace --jobs 4 the_first_frame_after_adopting -- --test-threads 4`
Expected: FAIL — `adopt` and `TAKEN_OVER` do not exist.

- [ ] **Step 3: Implement `adopt`**

```rust
pub const TAKEN_OVER: &[u8] = b"taken over by a newer attach";

impl HostSession {
    pub fn adopt(&mut self, link: Link, size: TermSize) -> Result<()> {
        // Close the displaced connection FIRST, and say why. A displaced
        // client that is merely dropped reports silence, which is the one
        // thing that did not happen.
        self.link
            .sink
            .connection()
            .close(quinn::VarInt::from_u32(0), TAKEN_OVER);

        self.link = link;
        self.size = size;

        // §8.5. New generation, so both channels restart at 1. The screen
        // itself is NOT reset — the emulator kept running — so the base state
        // is blank and `screen_stale` forces the snapshot that fills it.
        let blank = ScreenState::blank(size.rows, size.cols)?;
        self.screen_tx = oxutrm_sync::Sender::new(blank);
        self.input_rx = Receiver::new(InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        });
        self.written = 0;
        self.last_send = None;
        self.screen_stale = true;
        // An attach has just completed and the client sends immediately, so
        // "now" is true rather than optimistic — the same reasoning as `spawn`.
        self.last_heard = Instant::now();
        Ok(())
    }
}
```

- [ ] **Step 4: Add the select arm**

`HostWake` gains one variant. The receiver is created by the caller (Task 3)
and passed to `run`; inside the loop it is a **local**, per C1:

```rust
enum HostWake {
    Pty,
    Exit,
    Frame(Frame),
    Due,
    /// A second attach completed. Carries the whole thing, because the
    /// session needs the size as well as the link.
    Attached(crate::attach_exchange::Attached),
}
```

`run` takes the receiver and duplicates it into a local before the loop, the
way the descriptors already are:

```rust
pub async fn run_with_attaches(
    &mut self,
    attaches: &mut tokio::sync::mpsc::Receiver<crate::attach_exchange::Attached>,
) -> Result<i32> {
```

and the select gains, alongside the existing arms:

```rust
Some(a) = attaches.recv() => HostWake::Attached(a),
```

with the match arm:

```rust
HostWake::Attached(a) => {
    self.adopt(a.link, a.client_size)
        .context("adopting a second attach")?;
}
```

Keep `run()` as a thin wrapper that calls `run_with_attaches` with a receiver
whose sender was dropped, so every existing caller and test is unchanged.
**A closed mpsc receiver yields `None` immediately and `Some(a) = ...` makes
that arm disabled rather than hot** — this is why the arm uses the `Some(..)`
pattern and not a bare binding, and it is the same reason there is no
`conn.closed()` arm.

- [ ] **Step 5: Run the tests**

Run: `make check`
Expected: PASS, all suites.

- [ ] **Step 6: Injection — prove both guards can fail**

1. Change `TAKEN_OVER` to `b""` at the `close` call site only. Run the takeover
   test: it must FAIL naming the wrong reason. Restore.
2. Delete `self.screen_stale = true;` from `adopt`. Run the full-state test: it
   must FAIL with a non-zero `from_state`. Restore.

If either still passes, the test is not a guard — fix the test before going on.

- [ ] **Step 7: Commit**

```bash
git add src/session.rs
git commit -m "feat(session): adopt a second attach, and say so to the first

The swap is design spec 8.5 in code: a new generation restarts both sync
channels at sequence 1, and screen_stale forces the full state that the
newcomer must have, since it has never seen the base any diff would be
computed against.

The displaced connection is closed with a reason naming the takeover. Merely
dropping it makes the displaced client report that the host stopped answering,
which is the one thing that did not happen: it is answering somebody else.

Both assertions were checked by injection."
```

---

### Task 3: Bind the socket, accept, and feed the session

**Files:**
- Create: `src/listener.rs`
- Modify: `src/serve.rs` (start the listener after R13; call `run_with_attaches`), `src/main.rs` (`mod listener;`)
- Test: `src/listener.rs`

**Interfaces:**
- Consumes: `Attached` and `run_attach_exchange` (Task 1),
  `HostSession::run_with_attaches` (Task 2), `RegistryGuard::{socket_path,
  update}`, `oxutrm_host::check_socket_path_length`.
- Produces:

```rust
/// Serve second attaches for the life of the session.
///
/// Never returns on its own: it is spawned, and dropped when the session ends.
pub(crate) async fn serve_attaches(
    listener: tokio::net::UnixListener,
    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
    cfg: NetConfig,
    tx: tokio::sync::mpsc::Sender<Attached>,
);
```

`RegistryGuard` becomes an `Arc` in `serve()` because the listener task and
`serve()` both need it and its `Drop` removes the session directory — the
directory must outlive both, and an `Arc` says exactly that.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A failed attach attempt must not disturb the running session.
    ///
    /// The arm only ever fires on a COMPLETED link, so a client that connects
    /// and hangs up costs the session nothing — no message on the channel, and
    /// the listener still accepting afterwards.
    #[tokio::test]
    async fn an_abandoned_attempt_sends_nothing_and_leaves_the_door_open() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(fresh_meta("door")));
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &*meta.blocking_lock())
                .expect("register"),
        );

        let task = tokio::spawn(serve_attaches(
            listener,
            guard,
            std::sync::Arc::clone(&meta),
            NetConfig::default(),
            tx,
        ));

        // Connect and hang up without saying anything.
        drop(tokio::net::UnixStream::connect(&sock).await.expect("connect"));

        // Nothing reaches the session.
        assert!(
            rx.try_recv().is_err(),
            "an attempt that never completed handed a link to the session"
        );

        // And the door is still open: a second connect is accepted.
        let again = tokio::net::UnixStream::connect(&sock).await;
        assert!(
            again.is_ok(),
            "the listener died with the first failed attempt: {:?}",
            again.err()
        );
        task.abort();
    }

    /// A failed attempt must not advertise a generation that never served.
    ///
    /// `begin_attach` bumps `attach_id` at R4, before the exchange can fail,
    /// so the in-memory record moves even when nothing comes of it. The
    /// registry is only written on success — otherwise `--list` and a
    /// reconnecting client would name a generation that no link ever ran as.
    #[tokio::test]
    async fn a_failed_attempt_does_not_write_its_generation_to_the_registry() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("gen");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            std::sync::Arc::clone(&guard),
            std::sync::Arc::clone(&meta),
            NetConfig::default(),
            tx,
        ));

        // Connect and hang up: R4 runs, R7 fails.
        drop(tokio::net::UnixStream::connect(&sock).await.expect("connect"));
        // Let the listener finish handling it before reading the file.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let on_disk: SessionMeta = serde_json::from_slice(
            &std::fs::read(guard.meta_path()).expect("meta.json is there"),
        )
        .expect("meta.json parses");
        assert_eq!(
            on_disk.attach_id, 0,
            "the registry advertises generation {} but no link ever ran as it; \
             a client told to expect it would be waiting for a host that is \
             not there",
            on_disk.attach_id
        );
        task.abort();
    }
}
```

**On the full end-to-end test:** it is not written here, on purpose. Driving a
second attach all the way through needs the *client* half of the exchange as a
reusable function, and extracting `connect()`'s L4–L10 is B2's first task, not
this plan's. Writing a throwaway signalling client here would mean
re-implementing the client to test the host — a harness that shares the
implementation's assumptions cannot refute them. **Until B2, B1's end-to-end
evidence is the hand test in Task 5, and the plan says so rather than
pretending otherwise.**

- [ ] **Step 2: Run it and watch it fail**

Run: `cargo test --workspace --jobs 4 listener -- --test-threads 4`
Expected: FAIL — `serve_attaches` does not exist.

- [ ] **Step 3: Implement the listener**

```rust
pub(crate) async fn serve_attaches(
    listener: tokio::net::UnixListener,
    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
    cfg: NetConfig,
    tx: tokio::sync::mpsc::Sender<Attached>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            // A failed accept is not a reason to stop answering the door.
            Err(_) => continue,
        };
        let (r, w) = stream.into_split();

        // One attach at a time, deliberately. Two concurrent exchanges would
        // both bump the generation and race to hand the session a link, and
        // the loser's shell would be a live connection nobody owns.
        let mut m = meta.lock().await;
        let attached = match crate::attach_exchange::run_attach_exchange(
            tokio::io::BufReader::new(r),
            w,
            &mut m,
            &cfg,
        )
        .await
        {
            Ok(a) => a,
            // The attempt failed. The running session is untouched: it never
            // heard about this, which is the whole point of the arm firing
            // only on a completed link.
            Err(_) => continue,
        };
        // `update`'s own doc: "after every attach, because attach_id moves".
        let _ = guard.update(&m);
        drop(m);

        if tx.send(attached).await.is_err() {
            // The session is gone; so is the reason to keep listening.
            return;
        }
    }
}
```

- [ ] **Step 4: Wire it into `serve()`**

After R13's `register_in`, and before R14's `HostSession::spawn`:

```rust
let guard = std::sync::Arc::new(guard);
let meta = std::sync::Arc::new(tokio::sync::Mutex::new(meta));

// The socket path has always been computed and registered. Nothing has ever
// bound it until now.
let sock = guard.socket_path();
oxutrm_host::check_socket_path_length(&sock)?;
let listener = tokio::net::UnixListener::bind(&sock)
    .with_context(|| format!("binding the session socket at {}", sock.display()))?;

let (attach_tx, mut attach_rx) = tokio::sync::mpsc::channel(1);
let listening = tokio::spawn(crate::listener::serve_attaches(
    listener,
    std::sync::Arc::clone(&guard),
    std::sync::Arc::clone(&meta),
    cfg,
    attach_tx,
));
```

and R15 becomes `let code = session.run_with_attaches(&mut attach_rx).await;`
followed by `listening.abort();` before the guard is dropped.

**Rung 4 does not get a listener.** `permit` is `None` for rung 4, whose
traffic runs inside the ssh connection and which `settle_detachability` records
as not detachable. Bind only in the `Some(permit)` arm; a non-detachable
session that advertised a socket would accept an attach it cannot survive.

- [ ] **Step 5: Run**

Run: `make check`
Expected: PASS.

- [ ] **Step 6: Injection**

Move the `bind` into the `None` (rung 4) arm as well and confirm a rung-4
session now advertises a socket — assert it via
`oxutrm host --list` showing `NOT detachable` beside a bound socket, or by a
direct `UnixStream::connect` succeeding where it should be refused. Restore.

- [ ] **Step 7: Commit**

```bash
git add src/listener.rs src/serve.rs src/main.rs
git commit -m "feat(host): bind the session socket that was never bound

Registry::socket_path has been computed, registered and advertised since the
registry existed, and nothing has ever bound it. This binds it, accepts on it,
and runs the same R4-R10 exchange the ssh pipes run.

One attach at a time, under the meta lock: two concurrent exchanges would both
bump the generation and race to hand the session a link, and the loser would
be a live connection nobody owns. A failed attempt costs the running session
nothing, because the session's arm only ever sees a completed link.

Rung 4 gets no listener. It cannot outlive its ssh, so a socket would offer an
attach that cannot survive."
```

---

### Task 4: `oxutrm host --attach <id>`

**Files:**
- Modify: `src/main.rs:76-81` (the error arm), and add `run_host_attach`
- Test: `crates/oxutrm-host/tests/attach.rs` (extend), plus an end-to-end test

**Interfaces:**
- Consumes: `oxutrm_host::attach::{connect_to_session, relay_signals}` — both
  already exist and are already tested.

- [ ] **Step 1: Write the failing tests**

Two things are automatable at this boundary. The third — a second client
actually receiving the shell — is the hand test in Task 5, for the reason
given at the end of Task 3.

In `crates/oxutrm-host/tests/attach.rs`, which already has the fixtures:

```rust
/// The id is what a user types, and mistyping it is the common case.
///
/// `--attach` must reach `connect_to_session`'s error, which lists what does
/// exist — that listing is the whole value of the error.
#[tokio::test]
async fn attaching_to_an_unknown_id_lists_what_is_actually_there() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let mut m = a_session("realone");
    let _guard = RegistryGuard::register_in(dir.path(), &m).expect("register");
    let _listener = tokio::net::UnixListener::bind(
        Registry::socket_path_in(dir.path(), "realone"),
    )
    .expect("bind");

    let err = oxutrm_host::attach::connect_to_session(dir.path(), "typo")
        .await
        .expect_err("no session called typo");

    let text = err.to_string();
    assert!(
        text.contains("realone"),
        "the error does not name the session that IS there, so the user has \
         nothing to correct their typo against: {text}"
    );
}
```

and in `src/main.rs`'s test module:

```rust
/// `--attach` with no id is a usage error that points at `--list`, not a
/// panic on `args[1]`.
#[test]
fn attach_without_an_id_says_where_to_find_one() {
    let err = run_host(&["--attach".to_owned()]).expect_err("no id given");
    let text = err.to_string();
    assert!(
        text.contains("--list"),
        "the error does not tell the user how to find a session id: {text}"
    );
}
```

- [ ] **Step 2: Run them and watch them fail**

Run: `cargo test --workspace --jobs 4 attaching_to_an_unknown_id attach_without_an_id -- --test-threads 4`
Expected: FAIL — the second because `--attach` still returns the "not wired up
yet" error, which says nothing about `--list`.

- [ ] **Step 3: Implement `run_host_attach`**

```rust
fn run_host_attach(id: &str) -> anyhow::Result<()> {
    let root = oxutrm_host::choose_registry_root()?;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let stream = oxutrm_host::attach::connect_to_session(
                &oxutrm_host::Registry::dir_at(&root.base),
                id,
            )
            .await?;
            let (sr, mut sw) = stream.into_split();
            let mut sr = tokio::io::BufReader::new(sr);
            let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
            let mut stdout = tokio::io::stdout();

            // Both directions, concurrently, until either side finishes.
            // `relay_signals` decodes and re-encodes on purpose: garbage
            // cannot be relayed into a running session.
            tokio::select! {
                r = oxutrm_host::attach::relay_signals(&mut stdin, &mut sw) => { r?; }
                r = oxutrm_host::attach::relay_signals(&mut sr, &mut stdout) => { r?; }
            }
            Ok(())
        })
}
```

and `src/main.rs:76`'s arm becomes:

```rust
Some("--attach") => match args.get(1) {
    Some(id) => run_host_attach(id),
    None => Err(anyhow::anyhow!(
        "`oxutrm host --attach` needs a session id. \
         `oxutrm host --list` shows them."
    )),
},
```

- [ ] **Step 4: Run**

Run: `make check`
Expected: PASS.

- [ ] **Step 5: Injection**

Change the unknown-id error to drop its `available` list and confirm
`attaching_to_an_unknown_id_lists_what_is_actually_there` FAILS. Restore.

Then change the no-id arm's message to omit `--list` and confirm
`attach_without_an_id_says_where_to_find_one` FAILS. Restore.

- [ ] **Step 6: Commit**

```bash
git add src/main.rs src/serve.rs
git commit -m \"feat(host): --attach relays a second client into a running session

main.rs has said '--attach is not wired up yet' since the registry existed.
It is wired up now: connect_to_session and relay_signals both already existed
and were already tested; what was missing was the listener at the other end,
which Task 3 bound.

The two things automatable at this boundary are covered: a mistyped id still
lists what is actually there, and a missing id points at --list. A second
client actually receiving the shell is the hand test, and stays the hand test
until B2 makes the client half of the exchange reusable.\"
```

---

### Task 5: Changelog, and hand-test the thing a user types

**Files:**
- Modify: `CHANGES.md`
- Create: `docs/superpowers/notes/2026-09-03-tier-b1-hand-test.md`

- [ ] **Step 1: Write the changelog entry**

Under `## Unreleased` → `### New`, written for someone who hit the symptom:

```markdown
- **A running session can be picked up from another terminal.** `oxutrm host
  --list` has always shown live sessions and `--attach` has always refused to
  do anything about them, because the socket every session registers was never
  bound. It is bound now: attaching hands the same shell to the new terminal,
  with the screen it has right now, and tells the terminal that had it that it
  was taken over rather than leaving it to report silence.
```

- [ ] **Step 2: Hand-test it, and write down what actually happened**

Per the standing rule — **check the thing a user types**, and **verify by
RUNNING, not grepping**. The recipe that works is in
`docs/superpowers/notes/2026-08-30-tier-a-hand-test.md`; this note follows it.

Fill in every blank from a real run:

1. `oxutrm <target>`, run something that leaves a recognisable screen, detach.
2. `oxutrm host --list` in a second terminal — note the id and `attach` number.
3. `oxutrm host --attach <id>` from a **third** terminal. Record: does the
   screen arrive complete? How long did it take? What did the second terminal
   show at the moment it was displaced?
4. `--list` again: did `attach` increment in the registry?
5. Attach a fourth time. Does the generation keep moving?
6. **Cleanup**: `pgrep -a -f oxutrm`, and kill what is left. Every hand test
   leaves a session behind and they cannot be reattached — budget this in.

Record anomalies as anomalies. **Something seen once and not reproduced is not
a known defect, and its non-reproduction is not a fix.**

- [ ] **Step 3: Commit**

```bash
git add CHANGES.md docs/superpowers/notes/2026-09-03-tier-b1-hand-test.md
git commit -m "docs(host): the changelog entry, and the B1 hand test as run"
```

---

## Done when

- `make check` is green: fmt, clippy `-D warnings`, and the full suite with
  **zero failures**, re-run on the tree being pushed rather than inherited.
- Every new assertion above has been **seen to fail** against its injected bug.
- The hand test note has no blanks left in it.
- `oxutrm host --attach <id>` moves a live shell between terminals.

**The honest limit of B1's automated coverage:** no test in this plan drives a
second attach all the way to a newcomer painting a screen. That needs the
client half of the exchange, which B2 extracts. B1's evidence for the headline
behaviour is one hand test, run once, on one machine — the same standing of
evidence the PTO fix has, and worth the same scepticism. **B2's first task
should close this**, and the plan for B2 should say so in its own §1.

## What this plan deliberately does not do

- **No client rebuild loop, and no `REBUILD_AFTER`.** That is B2. A client
  whose path dies still sits there; what changes is that a *new* client can now
  take the session, which is what B2 will automate.
- **No `askpass`.** B3. A rebuild that needs an ssh passphrase is B2/B3's
  problem; `--attach` run by hand has an ordinary terminal and prompts on it.
- **No `Displaced` state or take-it-back key.** B4. B1 closes the old link with
  a reason; rendering that reason as its own state is B4's.
- **No session picker.** It stays unspecced, and B1 does not change that.

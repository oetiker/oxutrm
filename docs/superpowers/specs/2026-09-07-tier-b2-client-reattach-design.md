# oxutrm — Tier B2: client-side reattach

Status: approved 2026-09-07
Supersedes nothing. Implements phase 3's client half from
`docs/superpowers/specs/2026-08-29-session-recovery-design.md` §5.3, and adds
the session offer that spec left unspecced.

---

## 1. Purpose

Tier B1 gave the host everything reattach needs: a serving session binds its
registered Unix socket, and `oxutrm host --attach <id>` relays a second client
into it. Nothing on the client can ask for that.

Two consequences, both observed on `thinlinc` on 2026-09-07:

- A user who kills their client and reconnects gets a **new session**. The old
  one is still parked, holding their shell and scrollback, and nothing in the
  ordinary flow leads back to it.
- The reattach path has **no automatic test**. Exercising it needs a shim on
  the target that rewrites the remote command, so it has been hand-run once.

B2 fixes both with one building block: the client's L4–L10 exchange becomes a
reusable function, and the remote command becomes one that can serve *or*
attach depending on what the client chooses.

### 1.1 Goals

- A fresh `oxutrm <target>` finds the user's live sessions and can resume one.
- A running client whose link dies rebuilds it by itself, without the user
  doing anything.
- The reattach composition — attach, adopt, a newcomer painting — is testable
  without ssh, without a shim, and without a network.

### 1.2 Non-goals

- **`askpass`** — B3. A rebuild's ssh runs `BatchMode=yes` (§5.4).
- **The `Displaced` state and the take-it-back key** — B4.
- Speculative echo, session names or labels, and attaching across different
  ssh targets that happen to resolve to one host.

### 1.3 The rule this obeys

`crates/oxutrm-host/src/attach.rs:3`, unchanged and not negotiable:

> Reattachment is not a second code path.

Everything B2 adds happens **before** the R4–R10 exchange. Once a choice is
made, the exchange that runs is byte for byte the one `--serve` and `--attach`
already run, so nothing can work on reattach that an ordinary connect does not
exercise.

---

## 2. The session offer

### 2.1 The remote command

`REMOTE_SERVE` (`crates/oxutrm-host/src/ssh.rs:43`) becomes
`["oxutrm", "host", "--connect"]`. `--serve` and `--attach <id>` keep their
current behaviour for hand use and for the tests that drive them directly.

`oxutrm host --connect` resolves the registry root, lists live sessions, and
writes a `Sessions` signal as its **first** line — before any hello. It then
reads one `Choose` and dispatches:

- `Choose::New` → today's `run_host_serve()` path, R1–R13, unchanged.
- `Choose::Attach { id }` → today's `run_host_attach(id)` relay, unchanged:
  `connect_to_session` then two `relay_signals` between stdio and the session
  socket.

The host process therefore does exactly one of the two things it already knew
how to do. `--connect` is a dispatcher, not a third path.

### 2.2 The two new signals

Added to `oxutrm_proto::signal::Signal`:

```rust
Sessions { sessions: Vec<SessionSummary> },
Choose(Choice),
```

`SessionSummary` is derived from `SessionMeta`, carrying `session_id`,
`created_unix`, `shell`, `size`, `detachable` and `attach_id`. It is a separate
type on purpose: `SessionMeta` also carries `pid` and `boot`, which are local
bookkeeping and have no meaning to a client on another machine.

`Choice` is `Attach { id: String }` or `New`.

Non-detachable sessions **are listed**, marked as such. A rung-4 session dies
with its ssh and cannot be attached; showing it and saying why is better than
a list that silently disagrees with what `--list` on the host reports.

`sessions` may be empty. That is the ordinary first-connect case and the client
answers `New` without asking anything.

### 2.3 Compatibility

A new client against a host binary that predates `--connect` gets ssh exiting
with an unknown-option error. `SshChannel::diagnose` already turns a dead
channel with stderr into a readable message; the failure surfaces as a remote
binary that needs upgrading, which is true.

**No `PROTO_VERSION` bump.** The version field lives in the hellos, and this
exchange completes before them; a mismatch here is detected as a missing or
unparseable first line, not as a version disagreement.

---

## 3. The client's reusable exchange

`connect()`'s L4–L10 (`src/connect.rs:63-161`) move into:

```rust
pub(crate) async fn establish<R, W>(
    reader: R, writer: W, size: TermSize, cfg: &NetConfig,
) -> anyhow::Result<Established>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
```

returning `Established { link: Link, path: PathDescription, session_id: String,
attach_id: u64 }`.

The bounds mirror `run_attach_exchange` (`src/attach_exchange.rs:47`) for the
same reason: the candidate pumps are spawned. `SshChannel::halves()` already
yields a `BufReader<ChildStdout>` and a `ChildStdin`, so the ssh caller passes
what it has.

**`host_facts` stops discarding the session identity.** Today it matches
`HostHello { .. }` and drops `session_id` and `attach_id` (`connect.rs:223`).
The rebuild loop cannot exist without the session id, so `Established` carries
it and `ClientSession` holds it.

This function is what makes the reattach path testable: paired with
`run_attach_exchange` over a `tokio::io::duplex` pair, a test runs a real
client exchange against a real host session with no ssh and no network (§6).

---

## 4. Choosing a session

### 4.1 The CLI

```
oxutrm [--attach <id>] [--new] <ssh-target>
```

Both flags are optional and mutually exclusive. The usage text, and the test
that pins it (`src/main.rs:445`), change: reattach is implemented now.

### 4.2 The decision, as a pure function

Input is the offered list plus the flags; output is a `Choice` or an error.
No I/O, so it is table-driven in tests.

| Flags | Sessions offered | Result |
|---|---|---|
| `--attach <id>` | contains a unique match by full id or prefix | `Attach { id }` |
| `--attach <id>` | no match | error naming the id, listing what is live |
| `--attach <id>` | ambiguous prefix | error naming the candidates |
| `--attach <id>` | match is not detachable | error saying it dies with its ssh |
| `--new` | anything | `New` |
| none | empty | `New` |
| none | exactly one, detachable | `Attach` on it, with one line saying so |
| none | anything else | ask (§4.3) |

Prefix matching is on the id's hex text, minimum four characters.

### 4.3 The picker

Only when the decision is `ask`. It prints the offered sessions in the format
`oxutrm host --list` already produces (`attach::format_session_list`), numbered,
then reads **one line from stdin**:

- a number → attach that one
- `n` → new session
- `q` → quit without connecting, exit 0
- anything else → say so and read again; EOF is treated as `q`

It runs **before raw mode** (L11), like every other thing that may need the
terminal, so it is ordinary line I/O and involves no overlay. The overlay
version belongs with B3's askpass work, when there is somewhere for a prompt
to go during a rebuild.

---

## 5. The rebuild loop

### 5.1 The state

`Phase::Recovering { attempt: u32, next_try: Instant }` joins `Live`, `Silent`
and `Confirming` in `src/linkstate.rs`. Entered after `REBUILD_AFTER` of
`Silent`; the constant lands in source at last, at the spec's 20 s.

Backoff 1, 2, 4, 8 s, then every 8 s indefinitely. **The client never stops on
its own** — the quit key is how a user stops it.

The notice reads "waiting for the network", the attempt number, the countdown,
and, when the last attempt produced any, ssh's first stderr line. The existing
`Ctrl-\ q` key is unchanged.

### 5.2 An attempt

A spawned task that opens a **fresh** `SshChannel` to the same target with
`BatchMode=yes`, expects `Sessions`, answers `Attach { id }` with the session
id kept from the first hello, and runs `establish`. Its result reaches the
session loop over an mpsc receiver **held as a local and selected on**, exactly
as `HostSession::run_with_attaches` does — the loop's rule that its arms borrow
locals and never `self` is untouched.

### 5.3 The swap

**The old link is held throughout.** A rebuild is an additional attempt, not a
replacement (§5.3 of the session-recovery spec).

- A frame arriving on the old link returns the phase to `Live` and **drops the
  in-flight attempt**, which kills its ssh.
- The attempt landing first swaps: the old link is closed with a local reason,
  sync state starts over expecting the host's full state as the first datagram
  (design spec §8.5, as at every attach), and the phase becomes `Live` on the
  first frame, or `Confirming` if input was held.

The client sends its **current** terminal size in the new `ClientHello`, so the
host adopts at the right geometry with no separate resize afterwards.

**`TAKEN_OVER` on the old link is expected during a swap.** The host closes the
displaced link when it adopts the new one; while a rebuild is in flight or has
just landed, that close is ignored rather than reported. With no rebuild in
flight the current behaviour stands — an exit naming the takeover — until B4
makes it `Displaced`.

### 5.4 Failure, split two ways

**Definite answers end the loop.** The client leaves raw mode, prints the
reason and exits non-zero:

- the host's `Failed` saying the chosen session is gone, or that its socket did
  not answer within `ATTACH_TIMEOUT`;
- the remote binary rejecting `--connect` (§2.3).

**Everything else retries:** ssh dying, no reply, ICE failing, the QUIC
handshake failing.

**Authentication refused under `BatchMode` is the middle case.** It cannot be
fixed without a terminal until B3, so the notice says reconnecting needs a
terminal to authenticate, and the loop keeps its 8 s cadence — an ssh agent
coming back is a real way this resolves.

---

## 6. Testing

The centrepiece: **`establish` paired with `run_attach_exchange` over a
`tokio::io::duplex` pair.** A real client exchange against a real host session,
no ssh, no shim, no network. This is the composition test the reattach path has
never had; the `adopt`-must-`resize` regression that only a reading review
caught on B1 becomes a test that fails.

Around it:

- **The offer round trip.** Host writes `Sessions`, client answers `Choose`,
  both parse. Table-driven over none, one, several, non-detachable.
- **Selection (§4.2) as a pure function.** Every row of that table, including
  the ambiguous prefix and the missing id.
- **The picker**, on scripted stdin: a number, `n`, `q`, garbage then a number,
  EOF.
- **The rebuild loop**, with the launcher pointed at a fake that speaks the
  protocol on stdio: an attempt landing; the old link reviving first and the
  attempt being abandoned; a definite `Failed` ending the loop; the backoff
  schedule asserted as observed wake-ups, never as elapsed sleeps.
- **A subprocess test** of the new flags, spawning `env!("CARGO_BIN_EXE_oxutrm")`
  — `#![forbid(unsafe_code)]` in `src/main.rs` blocks setting env vars in
  process.

All tests use the STUN-free `NetConfig`. The help-text tests change with the
usage string.

---

## 7. What changes where

| File | Change |
|---|---|
| `crates/oxutrm-proto/src/signal.rs` | `Sessions`, `Choose`; `SessionSummary`, `Choice` |
| `crates/oxutrm-host/src/ssh.rs` | `REMOTE_SERVE` → `--connect` |
| `crates/oxutrm-host/src/attach.rs` | `SessionSummary` from `SessionMeta`; numbered list rendering |
| `src/main.rs` | `host --connect` dispatcher; `--attach`/`--new` on the client; usage text |
| `src/connect.rs` | L4–L10 → `establish`; the offer, the decision, the picker |
| `src/session.rs` | session id held; the rebuild receiver as a local; the swap; `TAKEN_OVER` during a swap |
| `src/linkstate.rs` | `Recovering`, `REBUILD_AFTER`, backoff |
| `crates/oxutrm-client/src/notice.rs` | the `Recovering` notice |
| `CHANGES.md` | New + Compatibility entries |

---

## 8. Risks and open questions

- **`REBUILD_AFTER` is still a guess** (20 s). Revisit against a real bad
  network, not by reasoning.
- **`BatchMode=yes` makes a rebuild fail on any host needing interaction.**
  Deliberate until B3, and it must degrade with a legible notice rather than by
  fighting the renderer for the screen.
- **The picker is line I/O before raw mode.** Correct for a first version, and
  it is the piece most likely to move into the overlay once B3 exists.
- **A prefix match is a convenience with a sharp edge.** Four hex characters of
  a 32-character id is not a lot; the ambiguity error is what keeps it honest.
- **`--connect` changes what oxutrm asks of a remote binary**, so mixed
  versions fail at connect time. Acceptable for a tool whose two ends are
  deployed together; the compatibility note says so plainly.

---

## 9. Inherited constraints this design must not break

`oxutrm-host` MUST NOT depend on `oxutrm-net`. `src/main.rs` is
`forbid(unsafe_code)`. The host loop's arms borrow locals, never `self`. A
rejected frame must never disconnect, and a send failure must never end a
session. Both ends reset sequence counters at every attach and the host's first
datagram is a full state. Never a bare `cargo test` — `make check` only.
A changelog entry is part of the work.

---

## Correction, 2026-09-08 (recorded during Task 6)

§6 says of the duplex-paired `establish` / `run_attach_exchange` test that "the
`adopt`-must-`resize` regression that only a reading review caught on B1
becomes a test that fails". **It does not and cannot.** That test's size
assertion reads `attached.client_size`, which `run_attach_exchange` produces
when it parses `ClientHello` — strictly upstream of `adopt`, and reached
whether or not `adopt` is ever called. Breaking `adopt` cannot move it.

No coverage is missing: `session::tests::adopting_a_link_resizes_the_shell_to_the_newcomers_terminal`
catches that regression, and catches it on the authoritative screen the host
actually ships rather than on a field. Only the rationale was overstated. The
composition test is still worth what the rest of §6 claims for it — it is the
first thing to exercise both halves of the handshake against each other with no
ssh and no shim.

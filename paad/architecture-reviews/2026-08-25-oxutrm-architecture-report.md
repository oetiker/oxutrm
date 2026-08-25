# Architecture Report — oxutrm

**Date:** 2026-08-25
**Commit reviewed:** d9a1cb912a7ef1273dbdd6e1dbd5a0b55754a3c9
**Languages:** Rust (edition 2024, MSRV 1.85)
**Key directories:** `src/`, `crates/oxutrm-{proto,sync,term,net,host,client}/`, `docs/superpowers/`
**Scope:** full repository, 74 source files

## Repo Overview

oxutrm is a remote terminal that survives bad networks, IP changes and NAT on both ends —
"Mosh rebuilt in Rust with a real terminal emulator at both ends". The host owns an
authoritative screen and ships state diffs, never a byte stream, so a lost datagram costs
nothing: the next diff is computed against the same acknowledged base and therefore
contains what was lost. SSH only starts or reattaches sessions and carries ICE candidates;
it is never the data path except on rung 4 of a five-rung connectivity ladder.

## Method

Five specialists in parallel (structure, coupling, integration/data, error-handling and
observability, security and code quality), then a single independent verifier that
re-read the code at every cited line, dropped what it could not confirm, corrected impact
levels, and deduplicated across specialists. The verifier was explicitly instructed to
refute the controller's guiding hypothesis if the evidence warranted — and it corrected it
twice.

- Raw findings: 31 · Verified: 21 flaws + 14 strengths · Dropped: 10
- By impact: 5 High, 12 Medium, 4 Low

## Reading order

Section 5 first — it corrects the framing that produced this review. Then the High flaws.
Section 2's strengths matter as much: they say what must not be broken while fixing the rest.

---

## 1. Verified flaws, ranked by impact

### HIGH

---

#### V1 · Protocol defect · The sender keeps 32 states; the receiver keeps 1. Every frame sent inside one round trip is rejected.

`crates/oxutrm-sync/src/channel.rs:113` · `crates/oxutrm-sync/src/screen.rs:218-224` ·
`crates/oxutrm-sync/src/input.rs:135`
**Impact: High · Found by: INT (1). Root-caused independently by OBS (F1) from the
degradation-signal side.**

`Sender::make_frame` diffs against `peer_saw`, the last state the peer *acknowledged*.
`Receiver` holds exactly one state and `apply` demands an exact base match.

```rust
// channel.rs:113
let base = self.ring.iter().find(|s| s.seq() == self.peer_saw);

// screen.rs:219-224 (input.rs:135-140 is identical in shape)
if base != 0 && base != self.seq {
    return Err(ApplyError::BaseMismatch { base, current: self.seq });
}
```

Traced and confirmed: host at `peer_saw = N` sends diff `N -> M`; the client applies and
holds `M`; the ack for `M` needs ~1 RTT plus up to one client pacing interval to return.
The host's next pacing tick still has `peer_saw == N`, so it emits `from_state = N,
my_state = M+2`. The staleness gate at `channel.rs:214` passes it (`M+2 > M`), `apply`
then rejects it on the base. Every frame between a successful apply and the returning ack
is wasted work.

Pacing is `clamp(rtt/2, 8ms, 100ms)` (`src/link.rs:181`, verified verbatim), so above
~16 ms RTT the steady state is roughly one useful frame per round trip against one to two
rejected ones.

**Why the suite is green.** Verified by grep: every convergence property test and both
loopback paths teleport the ack in-process — `crates/oxutrm-sync/tests/convergence.rs`
and `src/loopback.rs:128,161` all call `on_ack(rx.ack())`. The proptest models frame
`Drop`, `Twice` and `Delay`; it gives the **ack** zero latency, which is the one property
that produces base drift. `src/session.rs`'s real-link test runs on loopback (~0.2 ms),
far below the 8 ms pacing floor, so the ack always beats the next frame home.

**Note for the lead:** the `diag-basedrift` agent is working this same area. This is
almost certainly the same defect reached from the data-flow side, and matches the reported
symptom `diff base N does not match current state N+2`.

---

#### V2 · Security · Unbounded allocation from a peer-supplied resize

`crates/oxutrm-sync/src/screen.rs:235` · **Impact: High · Found by: SEC (30.1)**
**Pre-confirmed by the lead; re-read and agreed.**

```rust
if let Some(size) = d.resize {
    self.rows = size.rows;
    self.cols = size.cols;
    self.cells = vec![Cell::blank(); size.rows as usize * size.cols as usize];
}
```

Allocation happens **before** anything validates the size, and `ScreenState::validate`
checks `cells.len() == rows * cols`, which is satisfied *after* the fact. No ceiling on
`rows`/`cols` exists in `oxutrm-proto` or `oxutrm-sync`. The payload that triggers it is
about ten bytes, so `MAX_FRAME` (8 MiB, `src/link.rs:53`) and `MAX_DECOMPRESSED` (64 MiB,
`channel.rs:22`) are both irrelevant.

What makes this a finding rather than an omission is verified one file over —
`channel.rs:16-22` argues explicitly for capping the zstd bomb *because* "'the peer is
authenticated' is exactly the assumption that fails first". The resize bomb is cheaper to
send and more expensive to receive, and was not given the same treatment.

SEC also correctly notes the contract's I1–I6 place no ceiling on `rows`/`cols` either.

---

#### V3 · Protocol defect · Under sustained output above ~85 ms RTT, the ring is exhausted and every frame becomes a full state

`crates/oxutrm-sync/src/lib.rs:40` · `src/session.rs:58`, `:236` ·
`crates/oxutrm-sync/src/channel.rs:115`
**Impact: High · Found by: INT (2). The silent-fallback half also found by OBS (F1).**

`STATE_RING = 32` (verified, `lib.rs:40`, with its derivation comment). `HostSession::run`
turns every `IDLE_POLL = 4ms` (verified, `session.rs:58`), and `term.poll()` returns true
on every turn under continuous output, so the ring holds roughly 128 ms of history. With
`peer_saw` ~1.5 RTT stale (V1), at RTT ≳ 85 ms the acknowledged base has already been
evicted on arrival, `find` returns `None`, and `make_frame` takes the `full_diff()` branch
permanently.

I re-did the arithmetic and it holds, **conditional on two things** the finding states but
does not flag: `term.poll()` returning true on essentially every 4 ms turn (true only
under sustained output), and V1 remaining unfixed. V3 is largely a corollary of V1.

The full state then exceeds the datagram limit and takes the stream path
(`src/link.rs:116`), head-of-line blocked and reset-on-supersede. 80–200 ms is the
ordinary RTT of the mobile and transatlantic links this project exists to survive.

Separately verified and worth keeping: **the fallback is silent.** `make_frame` returns
`Result<Option<Frame>, ApplyError>`; nothing distinguishes "diffing normally" from
"shipping whole screens every pacing interval". No counter, no flag. This is live code —
`loopback.rs:162` calls it.

---

#### V4 · Missing implementation · Reattach cannot work: the documented `seq` reset has no implementation and no API to implement it with

`crates/oxutrm-host/src/keys.rs:113-133` · `crates/oxutrm-sync/src/channel.rs:45-97` ·
`src/session.rs:91-240`
**Impact: High · Found by: INT (4)**

`begin_attach`'s doc states it as fact (verified verbatim at `keys.rs:120-123`):

> Both `seq` counters reset to 1 at every attach, so without it a host already serving a
> session could not tell a second `--attach` from the current one, and stale datagrams
> from the previous generation would look perfectly valid.

Nothing resets them. Verified by enumerating the API surface:

- `Sender`'s complete public surface is `new`, `update`, `on_ack`, `current`, `make_frame`
  (`channel.rs:45,64,78,82,97`). `ring`, `peer_saw` and `last_ack_sent` are private with
  no setter. The only route back to seq 1 is `Sender::new`.
- `HostSession`'s complete public surface is `spawn`, `turn`, `resize`, `run`, `screen`
  (`session.rs:91,124,220,230,240`). No reattach method, no way to swap `link`.

So reaching seq 1 requires constructing a new `HostSession`, which restarts the shell —
the exact thing reattach must not do.

**Related, verified:** `Frame` carries `my_state`, `from_state`, `ack_state`, `flags`,
`payload` and nothing else (confirmed from its construction at `channel.rs:130-140`). The
generation tag lives only in `Signal::HostHello`. The stated replay protection is
therefore delivered by the fresh cert/PSK making it a different QUIC connection, not by
the mechanism the doc names.

**Framing correction:** this is unbuilt work, not a regression. What raises it above
ordinary unfinished work is that **the doc asserts the mechanism as existing fact**, which
is a trap for whoever wires reattach.

---

#### V5 · Composition gap · The PSK never reaches the ICE credentials, in production or in any test — and no base64 decoder exists

`crates/oxutrm-host/src/keys.rs:58,69` · `crates/oxutrm-net/src/stunmsg.rs:117`
**Impact: High · Found by: SEC (32.2)**

Verified by exhaustive grep for base64 use across the tree:

```
crates/oxutrm-host/src/keys.rs:59:  base64::engine::general_purpose::STANDARD.encode(self.psk)
crates/oxutrm-host/src/keys.rs:70:  base64::engine::general_purpose::STANDARD.encode(self.cert_spki_sha256)
```

Those are the only two `base64` call sites in non-test code, and both are **encode**.
There is no decoder anywhere — nothing turns `HostHello.psk` back into `[u8; 32]`, and
nothing turns the emitted SPKI fingerprint back into the `[u8; 32]` that
`quic_client` consumes. `begin_attach` and `AttachKeys` are called only from
`crates/oxutrm-host/tests/attach.rs` (verified by grep; no other caller in the tree).

Both ends of the key schedule are well tested in isolation and have never been joined. A
base64 alphabet, length or ordering mismatch between mint and derive would not be caught
by anything currently green. This is the single most consequential untested seam in the
tree.

---

### MEDIUM

---

#### V6 · Duplication · Two `status_line` functions, two crates, different output, both tested

`crates/oxutrm-host/src/ladder.rs:250` vs `crates/oxutrm-client/src/status.rs:44`
**Impact: Medium · Found by: STR (F-2), CPL, OBS (F7), SEC (31.2) — four of five specialists**

Read both in full. They genuinely disagree for identical input:

| Rung | host `ladder.rs` | client `status.rs` |
|---|---|---|
| `PortMapped` | `IPv4 punched (router mapping)` — **hard-codes IPv4** | `{family} punched (port mapped)`, derived from `path.remote` |
| `StunPunch` | `IPv4 punched (STUN)` — **hard-codes IPv4** | `{family} punched` |
| `Birthday` | falls through to the general line, carries `mtu` | `… ms · {nat} NAT`, no mtu |
| `SshTunnel` | `no UDP path available … cannot roam and cannot be reattached` | `no UDP path, not detachable` |

Each has its own exact-string test suite (`crates/oxutrm-host/tests/ladder.rs:289-312` vs
`status.rs:93-206`), so the suite is green while the two disagree. Only the client copy
has a production caller — verified: `src/session.rs:47` does
`use oxutrm_client::{Renderer, status_line}`, and `src/session.rs:300` calls it. The host
copy has none.

**Contract contradiction, verified by grep:** `status_line` appears **exactly once** in
`docs/superpowers/plans/2026-08-25-oxutrm-contract.md`, at line 827, inside the
`oxutrm-client` block. The host copy is un-contracted.

The host copy is the one that would misreport an IPv6 path as IPv4.

---

#### V7 · Duplication · Two contradictory rules for the datagram size limit

`crates/oxutrm-host/src/transport.rs:22-25, 102-104` vs `src/link.rs:101-104`
**Impact: Medium · Found by: CPL (Flaw 3/6) and STR (F-1)**

Both read in full. The policies are stated in writing, in opposite directions:

`transport.rs:22-25` — "So `Path` exposes `Path::max_payload` returning a plain `usize`,
**decided once when the path is built**. There is no `Option` at the send site."
`transport.rs:103` treats `max_datagram_size() == None` as a hard error
(`.ok_or(PathError::DatagramsDisabled)?`).

`src/link.rs:101-104` — "`max_datagram_size` is None when the peer disabled datagrams …
**Asking every time rather than caching** is what keeps this correct across a migration",
and treats `None` as "fall through to a stream".

Both rules are individually defensible. Having both, in two crates, with the live one
being the negation of the documented one, is the finding. `transport.rs` has 14 tests
(counted) and zero production callers (verified: every reference outside the file is
`crates/oxutrm-host/tests/transport.rs` or the re-export at `lib.rs:56`).

**Impact corrected down from High (STR) to Medium** — see §4.

---

#### V8 · Resource leak · `ClientSession` never calls `InputState::consume`; typed input accumulates for the life of the session

`src/session.rs:324,392` vs `src/loopback.rs:145` · `crates/oxutrm-sync/src/input.rs:46`
**Impact: Medium · Found by: STR (F-3b) and INT (3)**

Verified by exhaustive grep for `.consume(`: the only non-test caller in the entire tree
is `src/loopback.rs:145`. `src/session.rs` calls `append` at 324 and 392 and `consume`
nowhere.

`consume` (`input.rs:46`) is the only mechanism that drops executed keystrokes from
`pending`. Three consequences follow directly from code I read:

- `pending` grows monotonically for the life of the session.
- `Sender::update` clones each state into a ring of 32 (`channel.rs:64-70`), so the memory
  cost is 32× everything ever typed.
- `InputState::diff_from` (`input.rs:102-118`) is a reverse-scan overlap search that
  compares slices — worst case O(n²) in `pending` — run at every pacing tick.
- `full_diff` (`input.rs:120-127`) re-sends that whole history in one frame on a ring
  miss, which is exactly the post-loss recovery path.

**No correctness bug today.** `HostSession::drain_input`'s `written` offset
(`session.rs:179-194`) keeps step with the un-trimmed `pending`, so nothing is written to
the PTY twice. Its guard `if self.written > pending.len()` (line 183) anticipates a trim
no caller ever issues. The mechanism exists on both sides; one caller is missing. This is
a resource and latency leak on the path that most needs to be cheap, on a product whose
headline use case is a week-long session.

---

#### V9 · Placement · `RungRunner`'s only possible production implementor lives in a crate `oxutrm-host` cannot see

`crates/oxutrm-host/src/ladder.rs:65` · `crates/oxutrm-host/Cargo.toml`
**Impact: Medium · Found by: STR (F-4), CPL (Flaw 4/7), SEC (31.2), OBS (F9)**
**The manifest independence was pre-confirmed by the lead.**

Verified by grep: `impl RungRunner` appears exactly once in the tree —
`crates/oxutrm-host/tests/ladder.rs:47`, the `Scripted` mock. Every rung mechanism
(`IceAgent`, `PortMapping`, `birthday_blast`, `stun_discover`) lives in `oxutrm-net`,
which `oxutrm-host` does not depend on.

So no `impl RungRunner` can ever be written inside `oxutrm-host`. It must land in the root
binary, or the ladder must move. This is not a scheduling matter: the current placement is
what guarantees the trait's only implementation is a test mock.

**Impact corrected from High (STR) to Medium** — see §4.

**Related, verified:** `oxutrm-host`'s package description claims "PTY supervision". The
crate cannot see `oxutrm-term` and supervises no PTY.

---

#### V10 · Reach · `oxutrm-net` has exactly one production call site

**Impact: Medium · Found by: STR (F-5)**

Verified by grep for `oxutrm_net::` / `use oxutrm_net` across `src/` and all crates except
`oxutrm-net` itself. Two hits, total:

```
src/link.rs:296:     let (demux, _stun) = oxutrm_net::StunDemuxSocket::new(&socket)?;
src/session.rs:417:  use oxutrm_net::{generate_cert, quic_client, quic_server};
```

`src/session.rs:414` is `#[cfg(test)]`, so line 417 is inside the test module. That leaves
**one** production reference — and `src/link.rs` itself has no caller (V12). `IceAgent`,
`birthday_blast`, `stun_discover` and `StunResponder` are integrated only in
`crates/oxutrm-net/src/bin/oxutrm-netns-peer.rs`, a test fixture binary.

**Impact corrected from High (STR) to Medium** — see §4.

---

#### V11 · Vacuous test · `no_keys_on_disk`'s headline test cannot fail

`crates/oxutrm-host/tests/no_keys_on_disk.rs:56-124` · **Impact: Medium · Found by: SEC (32.1)**

Read the whole test. The module header claims it "creates a session **while holding
secrets**, then reads every byte of every file under the registry root". It does not.

```rust
let psk: Vec<u8> = PSK.to_vec();
let cert_key = PRIVATE_KEY_PEM.to_string();

let mut meta = SessionMeta { session_id: …, attach_id: 7, … };
let guard = RegistryGuard::register_in(&root, &meta).expect("register");
```

`psk` and `cert_key` appear nowhere afterwards except as the *needles* in the assertions
at lines 96-120. They are never passed to `register_in`, to `update`, or to anything else.
The only writer exercised is the registry, handed a `SessionMeta` that structurally cannot
contain a secret. `AttachKeys` and `begin_attach` do not appear in the file at all — so
the test passes for every possible implementation of `keys.rs`, including one that writes
the PSK to a file of its own.

**SEC's mitigation is accurate and I confirm it:**
`!text.to_ascii_lowercase().contains("psk")` would catch a field *named* for one, and the
second test in the file (`meta_json_holds_exactly_the_seven_fields_it_is_allowed_to`,
line 131) is genuinely load-bearing. That second test is doing the work the first one
claims.

Real risk is false confidence, not an actual leak.

---

#### V12 · Reach · The whole session and link layer has no production caller

`src/main.rs:21-25` · **Impact: Medium · Found by: SEC (31.1), OBS (F13), INT, STR, CPL — all five**

Verified verbatim:

```rust
#[allow(dead_code)]
mod link;
mod loopback;
#[allow(dead_code)]
mod session;
```

and `--serve` (`main.rs:69`), `--attach` (`main.rs:76`) and `run_connect`
(`main.rs:190`) all return honest "not wired up yet" errors, each naming what exists and
what to run instead.

The one consequence worth naming beyond the fact itself: **the module-level `allow` also
suppresses dead-code warnings inside those files**, so further dead code there is
invisible to the compiler.

**Impact corrected from High (SEC) to Medium, and the line count corrected** — see §4.

---

#### V13 · Signals with no readers · Degradation outcomes are computed, allocated and discarded

`src/link.rs:95,98,111,131` · `src/session.rs:135,143,338,345`
**Impact: Medium · Found by: OBS (verdict + F2), INT**

Verified by exhaustive grep for every reader:

| Signal | Written at | Read by |
|---|---|---|
| `SendOutcome::Dropped(String)` | `link.rs:95, 98, 111, 131` | **nobody — not one match arm, not even a test** |
| `SendOutcome::Stream { bytes, superseded }` | `link.rs:167` | one test matches `Stream { .. }` (`session.rs:786`); neither field is read |
| `Turn.rejected` | `session.rs:143, 345` | **nobody** |
| `Turn.applied` | `session.rs:135, 338` | **nobody** |
| `Turn.sent` | `session.rs:167, 358` | two tests (`session.rs:720, 786`) |
| ring-miss → full-state fallback | `channel.rs:115` | nobody — not even counted |

`Dropped` formats a reason string at four sites and every one is discarded. `Turn.rejected`
carries a doc comment naming the deadlock it represents and has zero readers.

Also verified, and part of the same pattern: both `offer_frame` implementations
(`session.rs:205`, `:369`) collapse "nothing to send" and "the diff could not be built"
into one value:

```rust
Ok(None) | Err(_) => return None,
```

The nine-variant `ApplyError` taxonomy is bound to `_` and dropped. A host whose encode
fails transmits nothing forever and reports the same value as a healthy idle session.

**OBS's own qualification is correct and I keep it:** these have no *reader* largely
because they have no *caller* (V12). The ring-miss fallback (V3) is the exception — that
one is live.

---

#### V14 · Observability · After severing from ssh, every diagnostic goes to `/dev/null`, and no logging facility exists

`crates/oxutrm-host/src/daemon.rs:244-266` · **Impact: Medium · Found by: OBS (F4, F5)**

Confirmed by grep across every `Cargo.toml`: no `tracing`, `log`, `env_logger`, `slog`,
`syslog` or `metrics` dependency anywhere. OBS's inventory of the entire production
diagnostic surface — five `eprintln!` sites plus one `Option<String>` warning field — is
accurate; I spot-checked four of the five.

`reopen_standard_descriptors` points 0/1/2 at `/dev/null` permanently, so a detached
session — the state the project exists to support — has no destination for an intentional
diagnostic. `SessionMeta` records nothing about health, so `--list` cannot report
degradation either.

OBS's observation that the code answers the wrong half of the hazard is fair and verified
at `daemon.rs:241-243`: the design makes *stray* writes harmless and never gives
*intentional* ones a destination.

**Impact corrected from High to Medium** — see §4.

---

#### V15 · Terminal corruption · The client logs into the screen it is painting, without invalidating the renderer

`src/session.rs:344-348` (and the host's copy at `:142-146`) · **Impact: Medium · Found by: OBS (F3) and INT (10)**

```rust
Err(e) => {
    turn.rejected += 1;
    eprintln!("oxutrm: client dropped an unapplicable screen frame: {e}");
}
```

This runs in the client loop, which owns a terminal in raw mode with a `Renderer` holding
a byte-exact model of what is painted. Two problems, both verified:

1. `eprintln!` emits `\n`, not `\r\n` — in raw mode the message staircases.
2. It writes outside the renderer's model and does **not** invalidate it.

`ClientSession::announce`, 30 lines earlier in the same file, gets this exactly right —
`session.rs:311-317` writes one line, states the reason ("that model is now wrong by one
row"), and calls `self.renderer.invalidate()`. The rejection arm omits it.

So the condition that fires this log — the per-RTT storm from V1 — corrupts the display
and never repairs it.

**Context that matters, verified in `git log`:** the `eprintln!` is deliberate and recent,
added in `bbec42b` because "silence there hid this for a day". The finding is not "remove
the log"; it is that the log has nowhere safe to go.

---

#### V16 · Rule violation · The only shipping path uses `?` on `on_frame`

`src/loopback.rs:129, 131, 162, 173` · **Impact: Medium · Found by: OBS (F8) and INT (5)**

Verified exact lines:

```
129:  if let Some(frame) = self.input_tx.make_frame(self.input_rx.ack())? {
131:      if self.input_rx.on_frame(&round_tripped)? {
162:  if let Some(frame) = self.screen_tx.make_frame(self.screen_rx.ack())? {
173:      if self.screen_rx.on_frame(&round_tripped)? {
```

`tick()` returns `Result`, `src/main.rs:169` does `session.tick(&buf[..n], &mut stdout)?`,
and `main` returns it — so an `ApplyError` terminates the process and kills the user's
shell. That contradicts the normative contract, verified verbatim at
`2026-08-25-oxutrm-contract.md:554-555`: *"A rejected frame NEVER disconnects the session;
the host and client loops log it and go on."*

`session.rs` obeys the rule; `loopback.rs` does not, and `loopback.rs` is what runs today.
The identical call is **fatal** in one file and **silently swallowed** in the other (V13).

**Impact corrected from High (OBS) to Medium** — I verified the trigger is structurally
unreachable in loopback: `on_ack(rx.ack())` runs immediately before `make_frame` at
`loopback.rs:128` and `:161`, so `peer_saw` always equals the receiver's current seq and
base drift cannot occur. It is a latent trap and a rule violation, not a live crash.

---

#### V17 · Persistence · `SessionMeta` has two copies and nothing forces them to agree

`crates/oxutrm-host/src/keys.rs:128` · `crates/oxutrm-host/src/registry.rs:56`
**Impact: Medium · Found by: INT (8)**

`begin_attach(&mut meta, …)` bumps `meta.attach_id` in memory and returns (verified at
`keys.rs:128-134` — it does exactly two things: `saturating_add(1)` and mint keys).
`set_detachable` does the same for `detachable`. Neither persists; the caller must
remember `guard.update(&meta)`.

INT's framing is what makes this worth keeping, and it is accurate: the same crate solves
the same class of problem structurally right next door — `DetachPermit` cannot be
constructed except by `settle_detachability`, so "sever before the rung is known" is
impossible to write. The ordering that matters here, mutate-then-persist, is left to
memory. If it is missed, `--list` and `--attach` in another process read a stale
`attach_id` and a stale `detachable`.

No production caller exists to get it wrong yet.

---

### LOW

---

#### V18 · Unused wire vocabulary and unreachable components

**Impact: Low · Found by: STR (F-6, F-7), CPL, SEC (31.4)**

All verified by grep across the whole tree:

- **`Probe`** (`crates/oxutrm-net/src/discover.rs:72`) is **never constructed anywhere**,
  including inside its own file. The only three occurrences are the definition, a doc-table
  header at `discover.rs:14`, and the re-export at `lib.rs:57`. `classify` takes five bare
  parameters instead of the type built to carry exactly that.
- **`ControlMsg` / `ScrollbackReq`** (`crates/oxutrm-proto/src/stream.rs:11,26`) are
  exported at `lib.rs:92` and referenced only by their own `#[cfg(test)]` module
  (`stream.rs:47-118`). Those tests exercise postcard, not the system.
- **`HostTerm::scrollback`** (`crates/oxutrm-term/src/host.rs:204`) is implemented and has
  three tests; every caller is inside its own test module (`host.rs:603-619`).
- **`FrameSource::recv`** (`src/link.rs:252`) — `session.rs:130,333` use `try_recv`
  exclusively.

---

#### V19 · Unused dependencies, one contradicting the contract

`crates/oxutrm-net/Cargo.toml:26,27,36` · `crates/oxutrm-term/Cargo.toml:14`
**Impact: Low · Found by: SEC (31.5)**

Verified by grep across every `.rs` and `.toml`:

- `sha1` and `hmac` are declared by `oxutrm-net`. Neither appears in any Rust code — the
  only hits are two doc comments (`oxutrm-net/src/lib.rs:32`, `stunserver.rs:14`).
  `stun_codec` performs MESSAGE-INTEGRITY internally.
- `unicode-width` is declared by `oxutrm-term`. Zero uses of `unicode_width` or
  `UnicodeWidth` anywhere.
- `crates/oxutrm-net/Cargo.toml:36` declares `testkit = []`. No `#[cfg(feature =
  "testkit")]` exists and no crate enables it.

**Contract contradiction, verified:** `contract.md:65,67` assigns `hmac` and `sha1` to
`oxutrm-net` for STUN MESSAGE-INTEGRITY. The contract describes a hand-rolled integrity
layer the code correctly declined to build. This is a contract-is-wrong finding, not a
code finding.

---

#### V20 · Weak assertions

**Impact: Low · Found by: SEC (32.3, 32.4)**

- **`crates/oxutrm-host/tests/ladder.rs:30,40`** — `Scripted::delay_ms` carries the doc
  "Milliseconds each attempt takes, so a race has something to race", and
  `Scripted::new` hardcodes `delay_ms: 0`. It is the only constructor and no test
  overrides it, so `if self.delay_ms > 0 { sleep }` never runs. Consequently
  `the_first_validated_path_wins_the_race` (line 155) asserts only
  `plan.raced.contains(&path.rung)` — true for any of the three raced rungs. Latency-ordered
  nomination is untested. **SEC's refinement is right and I endorse it: the rest of the
  file is not vacuous.** Its other three tests make claims a wrong `nominate` would fail.
- **`crates/oxutrm-net/src/stunmsg.rs:579-587`** — `transaction_ids_are_not_predictable`
  inserts 1,000 ids into a `HashSet` and asserts no collision. That tests *uniqueness*; a
  monotonic counter passes it. Unpredictability is the property that matters.

---

#### V21 · Small correctness and hygiene items

**Impact: Low**

- **Comment/code mismatch on the receive channel** (`src/link.rs:202-204`, OBS F11 / INT 6).
  Verified: the comment says "if the consumer stalls, dropping frames is right", and
  `tokio::sync::mpsc::Sender::send(...).await` at `:216` and `:238` **waits** for capacity.
  A reader of this file would believe the drop decision is made here. **The elaborated
  mechanism in both reports is wrong — see §3.**
- **`daemonize()` is a token-free public route to `sever()`** (`daemon.rs:196-199`, CPL).
  Verified: it takes no arguments and calls the private `sever()` directly, bypassing
  `DetachPermit`. It is `pub use`d at `lib.rs:47`. **Impact corrected from Medium to Low —
  see §4.**
- **`SUN_PATH_MAX` and its own doc disagree** (`registry.rs:465-476`, OBS F12). Verified:
  the doc says "`sockaddr_un::sun_path` holds 108 bytes"; the constant is 100; the
  user-facing error states "cannot exceed 100" as fact. Unexplained 7-byte margin.
- **`meta.json` is rewritten non-atomically** (`registry.rs:311-325`, INT 9). Verified:
  `write_private_file` opens with `.truncate(true)`. Other processes read the same file
  with no locking. Mitigated in practice — `list_in` skips a file that fails to parse, so
  the failure mode is a missing row, not a crash.
- **Unbounded line read on the signalling channel** (`crates/oxutrm-proto/src/signal.rs:118-135`,
  SEC 30.3). Verified: `read_line` into a fresh `String` with no cap, inside a loop that
  discards non-signal lines and keeps reading. Self-DoS from a user-chosen remote.
- **`ssh` target is not separated from options** (`crates/oxutrm-host/src/ssh.rs:182-189`,
  SEC 30.5). Verified: `cmd.args(&launcher.args); … cmd.arg(target);` with no `--` and no
  leading-dash rejection. No shell is involved and `target` comes from the user's own
  argv, so this is theoretical today.
- **`ClientSession::rebind` and `announce` are two halves of one event** (`session.rs:398`
  vs `:290`, CPL). Verified: `rebind` repaints nothing by design; `announce` is what calls
  `invalidate()`. A roam requires both, in that order, and nothing says so.
- **The pacing clamp is inline literals** (`src/link.rs:181`, OBS F10). Verified: `8` and
  `100` as bare literals, restated in the doc above, again in `session.rs:28`, and again
  in the test at `session.rs:1052`. Every other timing value in the tree is a named `const`
  with a derivation comment.
- **The QUIC data path has no client authentication** (`crates/oxutrm-net/src/quic.rs:63`,
  SEC 30.2). Verified: `.with_no_client_auth()`. Pinning is one-directional by design; the
  PSK gates ICE path nomination, not the QUIC handshake. **Impact corrected from Medium to
  Low for today** — the whole path is unwired and the attacker must know the punched
  `IP:port`. Worth deciding before wiring, not a live exposure.

---

## 2. Verified strengths — what to protect

---

#### S1 · The no-I/O boundary is enforced by machine, on the transitive closure, with guards on the guards

`crates/oxutrm-sync/tests/no_io.rs` · **Found by: STR (S-A) and CPL (S5) — both rated it
the strongest thing in the tree**

Read the file. It is an **allowlist**, not a denylist (`ALLOWED` at line 21, with a
comment stating exactly why: "A denylist only catches the I/O crates someone thought to
name; this fails on anything unrecognised, which is the only version that still works in
two years"). Plus a `NEVER` list of specifically named regressions. Plus meta-tests that
fail if the manifest parser returns nothing, if comment-stripping ate the file, or if the
closure walk found nothing — each of which would otherwise make every real test in the
file pass vacuously. `include_str!` reads the manifest at compile time, so the test does
no I/O of its own.

Commit `c8ed9ad` later added the **transitive** guard, closing the hole its own doc names:
add `reqwest` to `oxutrm-proto` tomorrow and every manifest-level test still passes.

This is the mechanism that keeps the project's highest-risk property — convergence under
loss — testable without a network. Its loss would be the hardest to notice.

---

#### S2 · The TLS pinning verifier does not have the hole

`crates/oxutrm-net/src/tls.rs:138-175` · **Found by: SEC (S10.1)**

Read all three methods. The circulating copy-paste trap is **absent**:
`verify_tls12_signature` and `verify_tls13_signature` both delegate to
`rustls::crypto::verify_tls1{2,3}_signature` with the provider's real algorithm set, and
`supported_verify_schemes()` returns
`self.provider.signature_verification_algorithms.supported_schemes()` rather than an empty
`vec![]`. The doc above `verify_tls12_signature` names the trap: "Returning `Ok` here
unconditionally is the common shortcut and it throws away proof of key possession."

SEC's assessment that this is the single most valuable thing in the tree to protect is
one I endorse.

---

#### S3 · The crate graph is a clean DAG, and the placement decision behind it is load-bearing

`Cargo.toml` (workspace) · `crates/oxutrm-proto/src/lib.rs:9-27`
**Found by: CPL (S4) and STR (S-B)**

`ScreenState` lives in the wire crate, not the emulator crate, because
`alacritty_terminal` "drags in a PTY, `polling` and `signal-hook` with no feature flag to
exclude them" — which would put a PTY in `oxutrm-sync`'s tree and destroy S1. The
reasoning is recorded in the crate that benefits, the crate that gives it up, **and** the
manifest. Commit `bf41e32` was a deliberate corrective refactor to reach this shape.

CPL verified zero cycles at crate level and zero at module level across all six crates by
mapping every `use crate::` / `use super::`. I did not re-derive the module-level claim;
I did confirm the crate-level shape and the placement reasoning.

---

#### S4 · The reject/ack contract is exact and self-consistent

`crates/oxutrm-sync/src/channel.rs:164-255` · **Found by: INT (S6)**

Read in full. Apply-to-clone-then-commit means a rejection provably leaves both `state`
and `ack()` untouched, closing the "I told you I hold N while holding N-1" failure the doc
describes at `channel.rs:170-174`. `peer_ack` is absorbed from *every* frame including
stale ones (`channel.rs:186`), and monotonically. The comment explains both halves as a
matched pair, and each traces to a commit recording a measured regression (`a365981`,
`5218b9c`).

**This is the code that pays for the settled rule "a rejected frame never disconnects".**

---

#### S5 · A send failure genuinely cannot end a session, and "never queue" is implemented rather than asserted

`src/link.rs:88-171` · **Found by: INT (S12, two entries)**

Read in full. `send` returns `SendOutcome`, never `Result`: encode failure, an absurd
size, a full datagram buffer and a peer that disabled datagrams all become
`Dropped(String)`. At most one stream is in flight; a newer state drops the cancel
`oneshot`, which makes the writer task call `reset(SUPERSEDED)` rather than let the
`SendStream` destructor `finish()` a truncated frame — the comment at `link.rs:26-29`
states exactly why that distinction matters. A stream already carrying an equal-or-newer
state causes the *new* send to be dropped instead of a second stream opening
(`link.rs:129-132`). Both directions of the invariant are handled.

This directly implements two of the lead's settled rules. Protect it.

---

#### S6 · A bounded decompressor with the right threat model

`crates/oxutrm-sync/src/channel.rs:16-22` · **Found by: INT (S12) and SEC (S10.6)**

`MAX_DECOMPRESSED` caps the work regardless of what the zstd header claims, and the
rationale explicitly refuses to lean on peer authentication: *"'the peer is authenticated'
is exactly the assumption that fails first."* INT verified the test asserts not just the
rejection but that `ack()` did not move.

This is the reasoning V2 needs applied one file over.

---

#### S7 · Ordering enforced by types, not comments

`crates/oxutrm-host/src/keys.rs:136-175` · `crates/oxutrm-host/src/daemon.rs:33-160`
**Found by: CPL (Hypothesis B) and SEC (S10.5)**

`Detached` and `DetachPermit` are unconstructable-except-by tokens, taken **by value**,
gating descriptor closure on (a) the double fork having happened and (b) the rung being
nominated. `sever_from_ssh(detached, permit)` cannot be written out of order, and a
rung-4 session simply has no token to offer.

CPL's argument for why this earns its complexity is the part I most want preserved, and I
verified the constraints it names: fork must precede any thread, the rung cannot be known
without a runtime, the runtime's threads do not survive a fork, and the ssh pipes must stay
open in between. Welded together those have no solution; split, the ladder runs between
them. This is temporal coupling that cannot be designed away, converted from a comment
into a compile error.

`close_inherited_descriptors` (`daemon.rs:213`) enumerates `/proc/self/fd` with no
keep-list and the doc says it must never grow one.

---

#### S8 · The renderer commits its model only on a successful write

`crates/oxutrm-client/src/renderer.rs:108-128` · **Found by: OBS (S8)**

Verified verbatim: on `Ok(())` it stores `Painted`; on `Err(e)` it sets `self.painted =
None` to force a full repaint. The comment states the invariant — "Committing it before
the bytes are out would leave every later diff computed against a screen that was never
painted, with no way back — and a rejected frame must cost a repaint, never a session."

OBS is right that this is the one place in the codebase where a failed I/O operation
correctly **drives a recovery action** rather than being counted and forgotten. It is the
template V13, V15 and V14 are missing.

---

#### S9 · Direction-labelled ICE credentials

`crates/oxutrm-net/src/stunmsg.rs:95-163` · **Found by: SEC (S10.2)**

HKDF-SHA256 splits the PSK into `c2h` and `h2c` so a reflected copy of one's own check is
signed with the wrong credential and fails, closing "nominate a path to yourself".
`a_reflected_copy_of_our_own_request_is_rejected` tests it from both roles, and
`a_check_request_has_exactly_the_bytes_we_expect` pins the wire layout byte-for-byte
including "nothing may follow MESSAGE-INTEGRITY".

I did not re-read the HKDF derivation itself; I confirmed the tests exist and the module
docs record the two silent failure modes.

---

#### S10 · Key material is designed out, not guarded

`crates/oxutrm-host/src/keys.rs:28-104` · **Found by: SEC (S10.3)**

Hand-written redacting `Debug` (line 74). A `Drop` that zeroes the PSK and is **honest
about what it does not guarantee** — base64 may already have copied it, no
`write_volatile`, "the fence makes elision unlikely rather than impossible". SEC's
description of this as an unusually accurate piece of security writing is fair. The same
redaction discipline appears pre-emptively on `SshChannel`.

---

#### S11 · Injection of the *command*, not a mock trait

`crates/oxutrm-host/src/ssh.rs:47-58, 200-218` · **Found by: CPL (S3)**

`SshLauncher` substitutes the program `ssh`, so tests drive a real subprocess over real
pipes. The stated reason is the point: "The pipe handling is a large part of what can go
wrong here — a deadlock on an undrained stderr, a message stuck in a buffer — and a
trait-shaped mock would bypass precisely the code that has those bugs." `SshChannel::open`
then spawns a continuous stderr drain for that exact deadlock, and the fixture is a real
binary that emits a banner and motd.

---

#### S12 · Tests that spawn real processes to observe process death

`crates/oxutrm-client/src/guard.rs:296-458` · **Found by: SEC (S11.3)**

`RawGuard::enter()` cannot run under `cargo test` (no controlling terminal), and the cases
that matter are the process *dying*. So the parent opens a pty, re-invokes the test binary
on an `#[ignore]`d helper with that pty as stdin, and watches the line discipline from its
own descriptor. It asserts both that the terminal was restored **and** that the child died
*of* the signal it was sent. The panic-hook test `std::mem::forget`s the guard so `Drop`
cannot be what restores.

The structural opposite of a mock that makes its assertion true by construction.

---

#### S13 · Global mutable state is present, minimal, and correct

`crates/oxutrm-client/src/guard.rs:38-43` · **Found by: STR (S-E)**

Verified: four statics — `ORIGINAL`, `RESTORED`, `HOOK_INSTALLED`, `SIGNALS_INSTALLED`.
They are unavoidable, because a panic hook and a signal handler cannot borrow a guard, and
the comment says so. `RESTORED` starts `true` with its reason stated: with no guard
installed there is nothing to undo, so a restore from an unrelated panic is correctly a
no-op. `FATAL_SIGNALS` documents that `SIGKILL`/`SIGSTOP` are absent "because the kernel
does not allow them to be caught, not because they were overlooked."

STR went looking for the god-object/global-state flaw and found its counter-example. I
confirm the negative result.

---

#### S14 · No dumping grounds, and no god objects

**Found by: STR (S-F, and the negative result at the end of its report)**

Verified: no `util.rs`, `common.rs`, `helpers.rs` or `misc.rs` in any of the seven source
trees. STR's negative finding on god objects is one I re-checked where it matters most:
`src/session.rs` is 1,081 lines but `#[cfg(test)]` begins at line 414, so it is ~413 lines
of production code holding two clearly separated types. `crates/oxutrm-host/src/registry.rs`
is a size outlier (479 lines) and cohesive — all of it serves "where sessions are recorded,
and which entries are still real".

**This is exactly the check the lead asked for: line counts prove nothing, and here they
prove nothing.**

---

## 3. Dropped findings, with reasons

---

**D1 · "The live code was written *after* the seam already existed, did not adopt it, and
re-derived the decision differently."** (CPL, Hypothesis A; same causal story in STR's
hypothesis section.)

**Dropped: the inference is not supported by the evidence.** I pulled the timestamps:

```
6d5d536  08-25 19:44  feat: the session loops - a real remote terminal over QUIC   (src/link.rs)
7232d9a  08-25 19:43  feat(host): rung 4, the swappable path seam, and the ladder  (transport.rs)
```

**Sixty seconds apart, in a parallel-agent build.** A one-minute gap in commit order is not
evidence that the author of `link.rs` had seen `transport.rs`. The far more likely reading
— and the one STR's own history section supports — is that two agents built the same seam
simultaneously and neither reconciled afterwards.

The **duplication is real and stays** (V7). The **"seam built then bypassed" mechanism is
dropped**, because it points at a different remedy than the evidence supports. See §5.

---

**D2 · "The two commits that do span components — `9aaa066` loopback (19:26) and
`6d5d536` session loops (19:44) — each wrote a fresh integration instead of using the
seams already built."** (STR)

**Dropped for `9aaa066`.** `transport.rs` landed at 19:43, seventeen minutes *after*
loopback. The seam did not exist to be used. The `6d5d536` half falls to D1.

---

**D3 · "`Path` has 14 tests locking in a policy the contract forbids."** (STR, F-1)

**Dropped as overreach.** I read `Path::check_payload` (`transport.rs:157-167`) and the
contract. `check_payload` reports whether a payload fits a path; it does not send anything
and does not forbid a caller from choosing a stream on the strength of its answer. The
`Path::Tunnel` half is unambiguously correct — rung 4 has no QUIC streams to escape to,
as STR itself concedes.

The genuine, verified contradiction is narrower and survives as V7: `None` handling, and
decide-once versus ask-every-time.

---

**D4 · "quinn's own buffer absorbs and then discards the newest arrivals … 64
already-superseded frames are preserved at the cost of the current one … at the price of
the queueing latency and decompression."** (INT 6, and the same mechanism in OBS F11.)

**Dropped: factually wrong on two counts.** I read `quinn-proto-0.11.14`,
`src/connection/datagrams.rs:132-139`:

```rust
let was_empty = self.recv_buffered == 0;
while datagram.data.len() + self.recv_buffered > window {
    debug!("dropping stale datagram");
    self.recv();          // pops the FRONT — the OLDEST
}
```

quinn evicts the **oldest** to make room for the newest. The current frame is not lost.

Second: stale frames cost almost nothing to reject. `Receiver::on_frame` runs the
staleness gate at `channel.rs:214` and returns `Ok(false)` **before** the decompress at
`:220`, so the "price of decompression" does not apply to the stale case either.

**What survives is the plain comment/code mismatch** (kept as V21): the comment says
"dropping frames is right" and `tx.send(f).await` waits. The "backpressure pointed the
wrong way" elaboration is dropped.

---

**D5 · `SendOutcome::DatagramsDisabled`.** Two specialists correctly reported it absent.
Per the lead's scoping caveat, this is a brief that predates this tree, not a specialist
error. Not recorded as a finding in either direction.

---

**D6 · Every contradiction against the M1–M4 plan documents.** (OBS F13 partial, OBS F14,
CPL steering-doc item 4, SEC steering-doc item 4.)

**Dropped: the M1–M4 plans are known stale by project rule.** Specifically dropped:
`migration_line` in `status.rs`; the M4 file table naming
`crates/oxutrm-net/src/link.rs`; M4's prescribed `eprintln!` on the send-failure arm.

OBS F14 additionally frames the last one as "the plan says surface it; the code discards
it — this is the origin of the `Dropped(String)`-with-no-reader anomaly". The plan half is
dropped; the underlying observation stands independently as V13.

CPL's item 4 already correctly identified `src/link.rs`'s placement as a *documented*
deviation explained in the root `Cargo.toml`. Good catch, but not a finding.

---

**D7 · "Contract vs code — signature drift on `ScreenState::blank`"** (SEC item 3):
contract says `-> ScreenState`, code returns `Result<…, ApplyError>`.
**Dropped as not a flaw.** The code is *stronger* than the contract, and validating at
construction is a documented strength (STR S-C). Contract needs updating; nothing is wrong
with the code.

---

**D8 · "`make_frame` is specified as `&self` but is `&mut self`"** (INT 7, sub-item).
**Dropped as not a flaw** for the same reason: it must be `&mut self` to update
`last_ack_sent`, which is the fix from `bbec42b`. The contract is behind the code.

---

**D9 · "The `&& !self.applied_any` narrowing is a contract drift."** (INT 7)
**Dropped by settled project rule.** The lead's rules state that a full state applies
regardless of sequence number and that the seq-1 collision fix (`3bad4d3`) is settled. INT
was careful here — it explicitly called the narrowing "deliberate and well argued" and
raised only that the contract text was not updated. That is a documentation task, not an
architecture finding, and it sits close enough to a settled rule that recording it as a
flaw invites exactly the regression the rule warns against.

---

**D10 · "The whole connectivity sequence in `oxutrm-net` is convention-only, and the
contract says MUST."** (CPL, Flaw 27)

**Dropped: below the evidence bar.** CPL states plainly in its own "What I did NOT examine"
that this is "inferred from the contract and `lib.rs` prose, **not** from tracing
`IceAgent::run`". I did not trace it either. The contract quote is real
(`contract.md:679-682`, "Nomination MUST complete BEFORE QUIC starts") and matches the
lead's settled rule that ICE nominates before QUIC starts — but whether the ordering is
enforced by construction inside `ice.rs`/`quic.rs` is an open question neither of us
answered. Listed in §6 as unverified rather than dropped outright.

---

**D11 · `crates/oxutrm-host/src/lib.rs:50-55` — "generic OS helpers in the host crate's
public root"** (STR F-8, self-rated confidence 65).

**Dropped.** `now_unix`, `pid_alive` and `process_start_unix` are cohesive inside
`registry.rs` (all three serve staleness detection), and STR itself rated this "marginal;
reported for completeness, not urgency". Below the bar for an architecture report.

---

## 4. Impact and category corrections

| # | Finding | Was | Now | Reason |
|---|---|---|---|---|
| 1 | **SEC's "~1,850 lines with no production caller"** | 1,850 | **~1,190** | Arithmetic error. SEC's table sums *total file lines*. `src/session.rs` is 1,081 lines but `#[cfg(test)]` begins at line 414, so ~413 are production, not 1,081. Corrected: session 413 + link 301 + ladder 274 + transport 202 ≈ 1,190, a large share of which is doc comments. **STR got this right and SEC did not.** The direction of the claim is unaffected; the magnitude is overstated by ~55%. |
| 2 | V12 (session/link layer has no caller) | High (SEC) | **Medium** | This is documented, deliberate and honestly labelled. It is a statement of project phase, not a defect. Its High-impact *consequences* are captured individually (V1, V4, V13). |
| 3 | V10 (`oxutrm-net` has one call site) | High (STR) | **Medium** | Same reason. It is the strongest single piece of evidence for the seam hypothesis, but "not yet connected" is not itself a High-impact architectural flaw. |
| 4 | V9 (`RungRunner` placement) | High (STR) | **Medium** | Real and structural, but it is a design decision to take before wiring, with an obvious set of options. CPL's Medium was the better call. |
| 5 | V7 (two datagram-limit rules) | High (STR) | **Medium** | Neither implementation has a production caller yet, so nothing misbehaves today; the cost is reconciliation work before wiring. CPL's Medium was the better call. |
| 6 | V16 (loopback `?`) | High (OBS) | **Medium** | I verified the trigger is structurally unreachable in loopback: `on_ack(rx.ack())` immediately precedes `make_frame` at `loopback.rs:128,161`, so base drift cannot occur there. A latent trap and a settled-rule violation — not a live crash. INT's Medium was the better call. |
| 7 | V14 (no logging facility) | High (OBS) | **Medium** | Accurate and well-evidenced, but the program it would instrument does not run yet (V12). High is the right rating the day `--serve` is wired; it is not the right rating today. |
| 8 | V21 (`daemonize()` bypasses the permit) | Medium (CPL) | **Low** | Verified real. But it has no production caller, `tests/daemonize.rs` needs it, and its doc (`daemon.rs:184-195`) explicitly argues why it is unusable for a session. Real consequence today: zero. Worth watching when `--serve` is wired. |
| 9 | V21 (no QUIC client auth) | Medium (SEC) | **Low** | SEC's own text says "practical exposure today is nil". I agree with SEC's *recommendation* to decide this before wiring; the impact rating should reflect the consequence, which is currently none. |

**Category corrections:**

- **V8** was filed by STR under "duplicated session loop" (Flaw 9/11) and by INT under
  data integration (Cat 26). The verified content — one required call site is missing — is
  neither duplication nor integration style. Recategorised as **resource leak / missing
  call site**.
- **INT's "distributed monolith not applicable"** verdict is correct and I endorse it: two
  processes, one deliberately narrow interface, enforced crate boundaries.
- **SEC's flaw 33 (hard-coded secrets): REFUTED** — I endorse the refutation. Every literal
  resembling key material is a labelled fixture, and real key material comes from `OsRng`
  (`keys.rs:42`) or `rcgen` (`tls.rs:82`).
- **CPL's flaw 5 (circular dependencies) and flaw 8 (premature optimisation): not found** —
  I endorse. The optimisation-shaped decisions are evidence-backed in-line, including the
  fragmentation arithmetic at `link.rs:11-18` that the lead's settled rules confirm.

---

## 5. Verdict on the controller's seam hypothesis

> *"This codebase builds and tests components thoroughly while leaving the connections
> between them unbuilt and untested."*

### The facts are confirmed. The framing needs one correction, and the causal story needs another.

**Confirmed, and this is not agreeable-agent confirmation — I re-derived each one:**

- `oxutrm-net` (~4,300 lines, five rungs, ICE, STUN, birthday blast, port mapping) has
  **exactly one production call site** in the entire tree (`src/link.rs:296`), and that
  file itself has no caller.
- `ladder.rs` (274 lines) and `transport.rs` (202 lines) have **zero** non-test callers.
- `src/session.rs` and `src/link.rs` sit behind `#[allow(dead_code)]`; all three CLI entry
  points return "not wired up yet".
- The PSK is minted at one end and consumed at the other, and **no base64 decoder exists
  anywhere in the tree** to join them. Neither production code nor any test crosses that
  seam.
- Component-level testing is genuinely excellent (S1–S12).
  Composition-level testing is essentially absent.
- `git log` confirms the mechanism: the entire product was built on 2026-08-25 between
  18:01 and ~23:18 by parallel agents, one commit per component. **Not one commit in that
  sequence wires component A to component B.**

### Correction 1 — this is unfinished work, not an architectural flaw, in most places.

The lead asked to be told plainly if that is the honest verdict, and for the bulk of the
tree it is. Category by category:

- **Most of it is "not built yet, and buildable."** `ControlMsg`, `ScrollbackReq`,
  `HostTerm::scrollback`, `Probe`, `oxutrm host --serve`. Nothing blocks them. The tree is
  five hours of construction old and `main.rs:15-20` refuses to fabricate a call site,
  saying so in writing: *"inventing a use to satisfy the linter hides exactly the fact
  worth knowing, which is that this code has no caller yet."* **That honesty is why this
  is diagnosable at all rather than buried, and it is itself a strength.**
- **A smaller, genuinely architectural part is "built twice, differently."** Two
  `status_line` (V6), two datagram-limit policies (V7), two session loops that have already
  drifted (V8, V16). This is the part that needs reconciliation *before* wiring, because
  wiring one as-is installs the wrong one.
- **One part is "built into a shape that forbids the wiring."** `RungRunner` in a crate
  that cannot see the rungs (V9). Not a wiring task — a crate-graph decision.
- **Three parts are live or first-to-run defects**, and these do not depend on the
  hypothesis being true at all: base drift (V1), the resize allocation (V2), the loopback
  `?` (V16).

### Correction 2 — the "seam was built, then bypassed" story is not supported.

Two specialists built a causal narrative on top of the duplication: `transport.rs` landed
first, `link.rs` landed later and bypassed it. I checked the timestamps. **`7232d9a` at
19:43 and `6d5d536` at 19:44 — sixty seconds apart, in a parallel-agent build.** That is
not evidence that one author saw the other's work. `9aaa066` (loopback, 19:26) predates
`transport.rs` by seventeen minutes, so it could not have bypassed a seam that did not
exist.

This matters because it changes the remedy. "Unused abstractions get bypassed" argues for
building fewer abstractions. **The evidence argues for something different: two agents
built the same thing at the same time and nothing reconciled them afterwards.** That is a
coordination gap in the build process, not an abstraction habit — and CPL's own strongest
counter-evidence supports the same reading: **two traits in 74 files**, with the codebase
repeatedly arguing itself *out* of abstractions in writing (`ssh.rs:47-58`,
`transport.rs:27-31`). I verified both counts. **CPL's refutation of the over-abstraction
framing is correct and should be kept.**

### Bottom line

The lead's hypothesis is a correct description of the tree's state and a useful lens. It
is **not** a diagnosis of poor architecture. The architecture is, on the evidence, better
than average — a clean DAG, an enforced I/O boundary, ordering encoded in types, a pinning
verifier without the standard hole, and a documented refusal to fake call sites. What it
has is four hours of parallel construction and no reconciliation pass.

The four commits the lead cited — `2da21c9`, `2294d3d`, `bbec42b`, `e67ee82` — are the
team already doing that reconciliation pass, one seam at a time, and naming it accurately
in the subject lines.

---

## 6. What I could not verify

Stated plainly, since it bounds everything above.

- **`crates/oxutrm-net` internals.** `ice.rs` (824 L), `birthday.rs`, `mapping.rs`,
  `candidates.rs`, `demux.rs`, `demuxsock.rs`, `stunserver.rs`, `socketfam.rs` — I read
  only the specific lines cited by findings (`quic.rs:63`, `discover.rs:14,72`,
  `stunmsg.rs:575-590`, `tls.rs:138-175`). **In particular I did not trace `IceAgent::run`,
  so CPL's convention-only-ordering claim (D10) is neither confirmed nor refuted.** If the
  team wants it settled, that is where to look.
- **`crates/oxutrm-term` and `crates/oxutrm-client` renderers.** `renderer.rs` (1,020 L)
  read only at 108-128; `color.rs`, `blink.rs`, `palette.rs`, `grid.rs`, `pty.rs` unread.
  **Escape-sequence emission driven by peer-controlled screen state is an injection
  surface that no specialist examined and I did not either.** That is the largest
  unexamined risk area across all six reports.
- **INT's findings V1 and V3 are derived, not measured.** They are supported by the state
  machine, the commit history, the structure of the test harness, and the reported symptom
  — not by a run. INT's own suggestion is the right next step and I endorse it: one
  instrumented session at ~100 ms simulated RTT, counting `Turn.rejected` and frames with
  `from_state == 0`, confirms or kills both in minutes. **V3's arithmetic additionally
  assumes `term.poll()` returns true on essentially every 4 ms turn**, which holds only
  under sustained output.
- **Strengths S9 and S12** were confirmed structurally (the tests exist, the mechanism is
  as described) but I did not re-derive the HKDF construction in `stunmsg.rs` or read the
  pty-harness test bodies in `guard.rs` line by line.
- **Test-suite quality beyond the files named.** `crates/oxutrm-host/tests/*` (10 files),
  `oxutrm-sync/tests/{convergence,faults}.rs`, `oxutrm-proto/tests/invariants.rs` were
  reached by targeted grep, not read. SEC's warning that further vacuous tests may hide
  there is fair and untested.
- **CPL's zero-module-cycles claim** across all six crates. I confirmed the crate-level DAG
  and the placement reasoning; I did not re-map every `use crate::` / `use super::`.
- **`crates/oxutrm-net/tests/netns.rs`** (SEC 32.5). The file's own skip-is-not-pass
  discipline is real, but **whether CI runners actually execute it is a question no static
  reading can answer**, and it decides whether rungs 1–3 have any honest test at all.
- **No build, no test run, per instruction.** Every "no production caller" claim above
  rests on grep over source across direct-name forms. Re-exports and trait-method dispatch
  can hide a caller from grep. The claims most worth a compiler's confirmation are V10 and
  V12.

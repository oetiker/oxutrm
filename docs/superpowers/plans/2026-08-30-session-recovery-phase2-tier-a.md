# Session Recovery Phase 2 — Tier A Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop a short outage or a local address change from costing a session.
The QUIC connection outlives silence instead of being killed at thirty seconds,
and a client whose machine changed network route follows it to the new local
address without dropping the connection.

**Architecture:** Two independent changes that only make sense together. First,
`max_idle_timeout` goes to `None`, so the transport stops adjudicating liveness
and the client's own `LinkState` becomes the only thing that decides the host has
stopped answering — which is the point of the design's §1.1. Second, a **route
probe**: while `Silent`, bind a throwaway UDP socket, `connect` it to the peer,
and read its `local_addr()`. That is the source address the kernel would use for
this peer right now, obtained without sending a packet. Different from the one
that was true when the link last worked → the route moved → swap the session
socket underneath the live connection with the already-built `Link::rebind`.

Removing the transport's timeout takes a guarantee away from the **host** as well
as the client, because both ends share one `transport_config()`. Task 3 gives the
host that guarantee back from its own clock. Skipping it trades a client-side
hang for a host-side CPU leak on every abandoned session.

**Tech Stack:** Rust edition 2024, `quinn` 0.11.11 / `quinn-proto` 0.11.17,
`tokio`, `rustix`, existing `oxutrm-net` socket helpers.

**Spec:** `docs/superpowers/specs/2026-08-29-session-recovery-design.md` — §4 is
this plan's source. §1.1 is the principle it serves; §3 is the state machine
phase 1 built and this phase leans on.

---

## Global Constraints

Copied verbatim from the spec, the contract, and the project's settled rules.
Every task's requirements implicitly include this section.

- **MSRV is 1.96** and must not exceed the maintainer's default toolchain. Keep
  `workspace.package.rust-version` and CI's `dtolnay/rust-toolchain@` in step.
- **Edition 2024.** Cap build and test parallelism at **4** (`-j4`).
- **`oxutrm-client` is `deny(unsafe_code)`; `src/main.rs` is `forbid`.**
- **`oxutrm-host` MUST NOT depend on `oxutrm-net`.** The probe is client-side and
  lives in the binary crate, not in `oxutrm-host`.
- **Candidate lists, not `cfg`.** Both platforms walk the same code. The probe
  has no `#[cfg(target_os)]` in it — that rule is why `FD_DIRS`, `open_keyboard`
  and `second_loopback_ip` look the way they do.
- **Do not assert platform rules in tests** — assert on outcomes.
- **A send failure must never end a session, and a rejected frame must never
  disconnect.** A *failed probe or rebind* must never end one either.
- **Do not add `#[allow(dead_code)]` to quiet something not wired up yet.** This
  phase's job is partly to *remove* three of them; do not add more.
- **Do not adopt crossterm, a ratatui backend, or a `Terminal`.**
- **Do not add a `conn.closed()` arm to the host loop.** A closed connection is
  permanently ready and an arm watching one spins.
- **`IDLE_POLL` survives only as the bounded re-check after an unconfirmed exit
  hint** and must not be reintroduced as a pace. The route probe is gated to
  `ROUTE_PROBE_EVERY` and runs **only while `Silent`**, so a healthy session
  makes no probe syscalls at all.
- **The notice may state only what the client can observe.** No "safe", no
  "retry", no "reconnect", no assertion about the host's state.
  `assert_claims_nothing_it_cannot_see` enforces this and stays.
- **The two dev boxes disagree about formatting.** Mac has rustc 1.97.1,
  thinlinc 1.96.0, both rustfmt 1.9.0. CI's fmt job runs on `stable` only —
  there is no 1.96 fmt gate. Format so both accept it.
- **Inject the fault before believing the test.** A test that passes against the
  injected bug is not a guard. This has cost this project real time twice.
- **A changelog entry is part of the work**, not an afterthought — see Task 7.
  `CHANGES.md`'s `## Unreleased` block is what the release workflow folds into
  the release notes, and it was empty when `v0.1.0` was cut.

### Phase 2 scope boundary

Tier A holds **one** connection open across an outage. It does not rebuild one.
So:

- **No new `LinkState` phase.** `Recovering` and `Displaced` belong to phases 3
  and 4. The states remain `Live`, `Silent`, `Confirming`.
- **No new notice, and no new notice copy.** The user-visible change in this
  phase is that the `Silent` box *stops appearing* for outages that used to be
  fatal, and that the client no longer dies at ~33 s. Nothing new is claimed.
- **`REBUILD_AFTER` is not implemented here.** Spec §4.2 ends "if the rebind does
  not restore contact, `REBUILD_AFTER` arrives and §6 re-punches properly" —
  that is phase 3. In this phase, a rebind that does not restore contact leaves
  the `Silent` notice up until the user presses `Ctrl-\ q`. **Say so in the
  commit message; do not invent a countdown to a thing that does not exist.**

---

## Three defects in the spec's own §4, found by checking the code

The prior phase shipped five plan defects and its self-review caught none. All
five were the same shape: *a statement that was true when written and was never
re-checked against the code beneath it*. §4 has three more. Each is written up
in the task that fixes it; they are collected here because an implementer who
reads only the spec will get all three wrong, and every one of them fails
**silently** — the change compiles, the suite stays green, and the feature does
not work.

1. **"`max_idle_timeout` is removed from `transport_config()`" is a no-op as
   written.** quinn's default is `Some(VarInt(30_000))`
   (`quinn-proto-0.11.17/src/config/transport.rs:369`). Deleting the line
   restores exactly the thirty seconds it was setting. It must be changed to
   `max_idle_timeout(None)`, explicitly. → Task 2.

2. **"along with the constant in `src/accept.rs:53` that exists specifically to
   match it" would reintroduce a session leak.** Line 53 is not the constant; it
   is a sentence *inside `ACCEPT_TIMEOUT`'s doc comment* that justifies the
   number by reference to `max_idle_timeout`. The constant itself is at line 64
   and its own doc says why it must stay: *"Not a tuning knob — the alternative
   to it is a leak."* It bounds the case quinn cannot see at all — no handshake
   to time out, because no peer ever spoke — and `max_idle_timeout` never
   covered that case. Delete it and `--serve` parks on `Endpoint::accept()`
   forever, holding a registered session, a punched socket and no shell. Sessions
   that cannot be reattached are already accumulating on the test host; do not
   add a source of more. **Keep the constant, rewrite the justification.** → Task 2.

3. **"Differs from the socket we hold → the route moved" compares against a
   value that carries no route information.** The session socket is bound
   wildcard (`oxutrm_net::bind_socket` prefers dual-stack `[::]`), so its
   `local_addr()` is `[::]:port`. Measured on the dev Mac:

   ```
   held session socket local_addr : ('::', 54214)
   probe connected to ::ffff:8.8.8.8      -> local_addr ('::ffff:10.46.18.101', 49974)
   probe connected to ::ffff:192.168.0.11 -> local_addr ('::ffff:192.168.17.5', 57050)
   ```

   `::` never equals a concrete source IP, so the comparison is true on the
   *first* probe of a perfectly healthy link. And §4.2 itself says a rebind
   invalidates a punched NAT hole — so the check as written would break the path
   it exists to repair, every time, on every link. The comparison must be
   **probe against the previous probe** (a baseline captured while the link was
   known good), and on the **IP only**: the probe socket's ephemeral port is its
   own and means nothing (note 49974 vs 57050 above). → Task 4.

Note also what that measurement shows about the machine this will be tested on:
the source address genuinely differs per peer (a VPN address for the internet,
a LAN address for the LAN host). The probe is therefore only meaningful when
connected to **the actual QUIC peer**, which is what §4.2 says and what Task 4
does.

---

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/oxutrm-net/src/quic.rs` | Transport settings for both ends | Modify: `max_idle_timeout(None)`, and say why keep-alive is not redundant |
| `src/accept.rs` | The host's one-connection accept deadline | Modify: `ACCEPT_TIMEOUT` **kept**, its justification rewritten |
| `src/roam.rs` | **New.** Which local address the kernel would use for a peer, and whether it moved | Create |
| `src/main.rs` | Module list | Modify: `mod roam;` |
| `src/link.rs` | `Link::rebind` | Modify: drop two `#[allow(dead_code)]`, rewrite the "no caller" docs |
| `src/session.rs` | Both session loops | Modify: client probe/rebind wiring, `note_heard` on every applied frame, host detach clock, the `TimedOut` message |
| `CHANGES.md` | Release notes source | Modify: `## Unreleased` |

`src/roam.rs` is a new file rather than a private helper in `session.rs` for the
same reason `linkstate.rs` is one: the decision ("did the route move?") is pure
and belongs in something testable without a socket, and the one impure part (the
probe) is four lines that want their own test against a real kernel.
`session.rs` is already 3400+ lines; this does not go in it.

---

## Task 1: A frame heard is a frame heard, wherever it is applied

**Files:**
- Modify: `src/session.rs` (`ClientSession::take_frames`, `Wake::Frame` arm)

**Interfaces:**
- Consumes: `ClientSession::note_heard(&mut self, now: Instant)` (exists)
- Produces: no new signatures. `take_frames` calls `note_heard` for every frame
  it applies.

**Background.** `LinkState::evaluate` returns early when the phase is already
`Silent`, so the *only* exit from `Silent` is `LinkState::heard`, and the only
caller of `note_heard` is the loop's `Wake::Frame` arm. A frame scavenged out of
the channel by `take_frames` on a pacing, keyboard or resize lap therefore
applies to the screen — the user watches the picture come back to life —
underneath a box that still says "no reply from host". The same gap makes the
`silent for Ns` counter read higher than the truth, which `notice_at`'s comment
documents at length as a known overstatement.

Today that self-heals within `HEARTBEAT_IDLE`, because the client heartbeats and
the answer wakes the loop through the frame arm. **This task comes first because
Task 2 makes the stale case worse in kind**: with no idle timeout, the client no
longer dies at thirty seconds, so every cosmetic staleness gets an unbounded
amount of time to be looked at.

**Watch the interaction with `prefix_pending`.** `LinkState::heard` clears it, on
the argument that a `Ctrl-\` whose letter never arrived belongs to the box that
was up when it was typed. Calling `heard` more often clears it more often, and
`Ctrl-\` and its letter can genuinely arrive in two separate reads. Step 1's
second test pins the behaviour that must survive.

- [ ] **Step 1: Write the failing tests**

Add to `src/session.rs`'s `mod tests`:

```rust
    /// A frame that arrives on a pacing lap rather than through the frame arm
    /// still counts as hearing from the host. Without this the picture comes
    /// back to life underneath a box saying nobody is answering.
    #[tokio::test]
    async fn a_scavenged_frame_clears_the_notice() {
        let (mut host, mut session) = with_notice().await;
        let mut out = Vec::new();

        // The host answers. The frame lands in the channel, but nothing wakes
        // the loop's frame arm -- this is the pacing lap that scavenges it.
        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        assert!(
            matches!(session.link_state.phase(), Phase::Live),
            "a frame was applied and the client still believes the host is silent: {:?}",
            session.link_state.phase()
        );
    }

    /// The counter is built from `last_heard`, so a scavenged frame must move
    /// it or the box overstates the outage for as long as the box is up.
    #[tokio::test]
    async fn a_scavenged_frame_takes_the_notice_down() {
        let (mut host, mut session) = with_notice().await;
        let mut out = Vec::new();

        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        assert!(
            session.notice_at(Instant::now()).is_none(),
            "the notice survived a frame that was applied"
        );
    }

    /// `heard` clears a half-typed prefix, and this task makes `heard` run far
    /// more often. A `Ctrl-\` and its letter genuinely arrive in two reads;
    /// a frame landing between them must not eat the command.
    #[tokio::test]
    async fn a_frame_between_the_prefix_and_its_letter_does_not_eat_the_command() {
        let (mut host, mut session) = with_confirming_notice(b"echo hi\r").await;
        let mut out = Vec::new();

        // The prefix arrives at the end of one read...
        session
            .route_keys(&[CTRL_BACKSLASH], &mut out)
            .expect("the prefix is held");
        // ...a frame is applied between the two reads...
        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");
        // ...and the letter arrives in the next.
        session.route_keys(b"d", &mut out).expect("the letter lands");

        assert!(
            session.link_state.held().is_empty(),
            "Ctrl-\\ d did not drop the held buffer: the frame ate the prefix"
        );
    }
```

Add this helper next to `with_notice` in `mod tests`, if it is not already
there — the other tests in this file wait for a frame by polling the source:

```rust
    /// Wait until a frame is sitting in the client's source, without applying
    /// it. `try_recv` in the code under test is what must pick it up.
    async fn wait_for_frame(session: &mut ClientSession) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !session.link.source.has_frame() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the host's frame never arrived");
    }
```

If `FrameSource` has no `has_frame`, do **not** add one: use the shape the
neighbouring tests already use to await a frame and hand it back via
`turn_with(&[], Some(frame), &mut out)` — but then this test is testing the
frame arm, not the scavenging path, so instead sleep briefly and rely on
`try_recv` inside `turn`:

```rust
    async fn wait_for_frame(_session: &mut ClientSession) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
```

Check which of the two `FrameSource` supports before writing the test, and use
the first form if it exists. The second is a timing proxy, and this project has
recorded that *a timing proxy becomes a race when the thing it proxied moves* —
so if you use it, say so in the commit message.

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -j4 --bin oxutrm a_scavenged_frame -- --nocapture
cargo test -j4 --bin oxutrm a_frame_between_the_prefix -- --nocapture
```

Expected: the two `a_scavenged_frame_*` tests FAIL — the phase is still
`Silent` and the notice is still up. The `prefix` test should **pass** already;
it is a guard against the change, not a driver of it. If it fails now, stop:
that is a pre-existing bug and it needs its own commit first.

- [ ] **Step 3: Make it pass**

In `ClientSession::take_frames`, inside the `Ok(true)` arm of the `on_frame`
match — the arm that already sets `turn.applied += 1` and `painted = true`:

```rust
                Ok(true) => {
                    turn.applied += 1;
                    painted = true;
                    // Wherever a frame is applied, the host was heard. The
                    // loop's `Wake::Frame` arm is not the only path here:
                    // `try_recv` below scavenges frames on pacing, keyboard
                    // and resize laps, and a frame applied on one of those
                    // used to repaint the screen underneath a box still
                    // saying nobody was answering. It also moves `last_heard`,
                    // which is what the `silent for Ns` counter is built from.
                    self.note_heard(Instant::now());
                }
```

Leave the `Wake::Frame` arm's `note_heard` where it is. It is now redundant on
the happy path but not always: `turn_with` appends input **before** it takes
frames, and a frame that `on_frame` answers with `Ok(false)` — an older sequence
number — is still evidence that the host is alive while not being an applied
frame. Removing it would trade this fix for a narrower version of the same bug.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -j4 --bin oxutrm a_scavenged_frame -- --nocapture
cargo test -j4 --bin oxutrm a_frame_between_the_prefix -- --nocapture
cargo test -j4
```

Expected: all PASS, and the full suite stays at its current count plus 3.

- [ ] **Step 5: Verify the guard by injecting the fault**

Comment out the `self.note_heard(Instant::now());` line just added and re-run
the two `a_scavenged_frame_*` tests. Both must fail. Restore the line. **A test
that passes against the injected bug is not a guard**, and this project has paid
for that lesson twice.

- [ ] **Step 6: Update the comment that this makes untrue**

`notice_at`'s long comment about the counter overstating the silence names
`take_frames` as the reason: *"applies a frame the pacing tick scavenged out of
the channel without telling this module, so `last_heard` does not move for it"*.
That is now false. Rewrite that paragraph to say the counter's remaining
overstatement is bounded by `HEARTBEAT_IDLE` and drop the `take_frames`
exception. **Do not delete the surrounding explanation** of why `last_heard` and
not `owed_since` is the display clock — that part is still true and is load-bearing.

- [ ] **Step 7: Commit**

```bash
git add src/session.rs
git commit -m "fix(client): a frame applied on a pacing lap is a frame heard

take_frames scavenges frames out of the channel on pacing, keyboard and
resize laps, and did not tell LinkState. Since evaluate returns early once
the phase is Silent, and heard is the only exit from it, the screen came
back to life underneath a box still saying the host was not answering, and
the counter it showed was built from a last_heard that had not moved.

It self-healed within HEARTBEAT_IDLE. Phase 2 removes the idle timeout, so
the client stops dying at 30s and the stale window stops being bounded by
anything -- which is why this goes first."
```

---

## Task 2: The idle timeout goes — explicitly, not by deletion

**Files:**
- Modify: `crates/oxutrm-net/src/quic.rs` (`transport_config`)
- Modify: `src/accept.rs` (`ACCEPT_TIMEOUT`'s doc comment only)
- Modify: `src/session.rs` (`exit_code`'s `TimedOut` arm)

**Interfaces:**
- Produces: no new signatures. `transport_config()` keeps its shape.

**Background — and the trap.** The spec says *"`max_idle_timeout` is removed from
`transport_config()`"*. Taken literally that is a **no-op**: quinn's default is
`Some(VarInt(30_000))`, thirty seconds, at
`quinn-proto-0.11.17/src/config/transport.rs:369`. Delete the call and you get
back precisely the timeout you meant to remove, the suite stays green, and the
hand test still shows the client dying at ~33 s. Pass `None`, explicitly.

`None` means an infinite timeout, and quinn's own doc carries a warning about it:
*"If a peer or its network path malfunctions or acts maliciously, an infinite
idle timeout can result in permanently hung futures!"* That is not an objection
here, it is the **design**: §1.1 says the client's own state machine owns the
liveness verdict, and phase 1 built that machine. The `Silent` notice and
`Ctrl-\ q` are what a user gets instead of a hung future, and they are strictly
more informative than a connection dying under them.

`transport_config()` is shared by `server_config` and `client_config`, so one
edit changes both ends — which is required, not incidental: the effective idle
timeout is the **minimum of the two peers'**, so a one-sided change would do
nothing. It also means a new client against an **old** host still negotiates 30 s.
That is a deployment fact to state in the changelog, not a thing to code around.

**`ACCEPT_TIMEOUT` stays.** See the second defect in the preamble. It bounds a
case `max_idle_timeout` never covered — nobody ever spoke, so there is no
connection to time out — and its own doc opens *"Not a tuning knob — the
alternative to it is a leak."* Only the paragraph justifying the *number* by
reference to `max_idle_timeout` becomes untrue.

- [ ] **Step 1: Write the failing test**

Add to `crates/oxutrm-net/src/quic.rs`'s test module (create `#[cfg(test)] mod
tests` at the end of the file if there is none):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The transport must not adjudicate liveness. quinn's DEFAULT is 30s, so
    /// deleting the setter would silently restore exactly what this removes --
    /// this asserts the negotiated value, not the absence of a line of code.
    #[test]
    fn the_transport_imposes_no_idle_timeout() {
        let cfg = transport_config();
        assert_eq!(
            format!("{cfg:?}").contains("max_idle_timeout: Some"),
            false,
            "an idle timeout is still set; the client will still die on silence: {cfg:?}"
        );
    }

    /// Keep-alive is NOT redundant with the client's heartbeat: it is what
    /// holds a punched NAT binding open, which rungs 2 and 3 depend on for the
    /// life of the connection. Removing the idle timeout must not take it too.
    #[test]
    fn keep_alive_survives_the_idle_timeout_going() {
        let cfg = transport_config();
        assert!(
            format!("{cfg:?}").contains("keep_alive_interval: Some"),
            "keep-alive went with the idle timeout; punched NAT bindings will lapse: {cfg:?}"
        );
    }
}
```

**Check `TransportConfig`'s `Debug` output before relying on it.** Run
`cargo test -j4 -p oxutrm-net the_transport_imposes_no_idle_timeout -- --nocapture`
once and read the printed struct. If `Debug` does not render those field names,
do not fight it — assert through behaviour instead by building two endpoints and
observing that a connection survives longer than 30 s of silence. That is Task 6's
job, and in that case say so here and delete these two tests rather than shipping
an assertion that cannot fail.

- [ ] **Step 2: Run the test to verify it fails**

```bash
cargo test -j4 -p oxutrm-net the_transport_imposes_no_idle_timeout -- --nocapture
```

Expected: FAIL, reporting `max_idle_timeout: Some(...)`.

- [ ] **Step 3: Make it pass**

In `crates/oxutrm-net/src/quic.rs`, replace the `max_idle_timeout` call:

```rust
    // No idle timeout, deliberately, and `None` rather than a deleted line:
    // quinn's DEFAULT is 30s, so removing the setter restores exactly what
    // this is here to remove.
    //
    // The transport must not adjudicate liveness. It has no idea what the
    // user is looking at, so all it can do about silence is kill a connection
    // that was about to recover -- which is what it did, at ~33s, for the
    // whole of phase 1. `LinkState` owns that verdict now: it raises a notice
    // at 2s, holds what is typed blind, and offers `Ctrl-\ q`. quinn warns
    // that an infinite timeout can hang a future for ever; the notice is the
    // answer to that, and it says more than a dead connection ever did.
    //
    // Consequence, stated because it is easy to miss: `conn.closed()` now
    // fires only on an explicit close or a transport error, never on silence.
    // Nothing may be built on it firing for a quiet peer -- see
    // `HostSession::attached`, which used to rely on exactly that.
    t.max_idle_timeout(None);
    // NOT redundant with the client's 0.2 Hz heartbeat, and it does not go
    // with the timeout above. The heartbeat exists so an answer is *owed*, on
    // the QUIC stream, where `LinkState` can see it. Keep-alive is what holds
    // a punched NAT binding open, which rungs 2 and 3 depend on for the whole
    // life of the connection -- and with no idle timeout, a connection can now
    // outlive a binding by a very long way.
    t.keep_alive_interval(Some(Duration::from_secs(10)));
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -j4 -p oxutrm-net
```

Expected: PASS.

- [ ] **Step 5: Rewrite `ACCEPT_TIMEOUT`'s justification — and keep the constant**

In `src/accept.rs`, the paragraph beginning *"Thirty seconds, matching the
`max_idle_timeout` `oxutrm_net` already sets on the transport"* is now false in
every clause. Replace that paragraph — and **only** that paragraph — with:

```rust
/// Thirty seconds, and the number now stands on its own. It used to be
/// justified by matching the transport's `max_idle_timeout`: a handshake still
/// unfinished after thirty idle seconds was one quinn was about to abandon
/// anyway, so the deadline cut nothing short. Phase 2 set that timeout to
/// `None`, so there is no longer anything to match and quinn will abandon
/// nothing.
///
/// That makes this deadline MORE load-bearing, not less. It is now the only
/// bound on the case it always described -- no peer ever spoke, so there is no
/// connection and nothing for a transport timeout to fire on. Without it
/// `--serve` parks on `Endpoint::accept()` for ever, holding a registered
/// session, a punched socket and a shell nobody can reach; and reattach does
/// not exist yet, so such a session cannot be reclaimed, only killed by PID.
///
/// It stays generous for the case that matters. ICE has already completed
/// connectivity checks over this very path by the time anything here runs, so
/// the client is known reachable and its `ClientHello` is one round trip away;
/// the address-validation `retry()` below costs one more. Thirty seconds is
/// tens of round trips on a link bad enough to be worth keeping.
```

Leave the two paragraphs above it — *"Not a tuning knob — the alternative to it
is a leak"* and the one explaining what parks — exactly as they are.

- [ ] **Step 6: Fix the two other comments this makes untrue**

Both are single sentences that name the thirty seconds:

1. `ClientSession::exit_code`'s `quinn::ConnectionError::TimedOut` arm says
   *"the host stopped answering and the link timed out after 30s"*. Silence no
   longer reaches this arm at all. Keep the arm — a handshake timeout can still
   produce it, and an unreachable arm is cheaper than a wrong one — and change
   the message to describe what is left, without naming a duration that no
   longer exists anywhere:

```rust
        quinn::ConnectionError::TimedOut => Err(anyhow::anyhow!(
            "the link to the host timed out. Silence alone no longer ends a \
             session, so this is the transport giving up rather than the host \
             going quiet."
        )),
```

   Note it drops the sentence *"Reattaching is not implemented yet"* only
   because it also drops the one it qualified. Do not add a reconnection claim
   in its place: nothing reconnects until phase 3.

2. `notice_at`'s `Phase::Confirming` arm explains that nothing reconnected
   because *"the QUIC connection never dropped, it went quiet and recovered
   inside the idle timeout"*. There is no idle timeout to recover inside now.
   Change "inside the idle timeout" to "and came back", and leave the rest of
   that comment — its point, that the headline must not say "reconnected", is
   still exactly right and still enforced by
   `assert_claims_nothing_it_cannot_see`.

- [ ] **Step 7: Grep for anything else that inherited the number**

```bash
grep -rn "idle timeout\|max_idle_timeout\|after 30s\|30 s" --include=*.rs src crates
```

Every surviving hit must be either (a) a test's own `tokio::time::timeout`
bound, which is unrelated, or (b) a sentence that is still true. Fix or leave
each deliberately; do not skip one because it is "just a comment". **The defect
class this project shipped five of last phase is a comment that outlives its
truth.**

- [ ] **Step 8: Commit**

```bash
git add crates/oxutrm-net/src/quic.rs src/accept.rs src/session.rs
git commit -m "feat(net): silence no longer ends a session

max_idle_timeout goes to None. Explicitly None and not a deleted line:
quinn's default is Some(30s), so removing the setter would have restored
exactly the timeout it was meant to remove, greenly and silently.

The transport cannot adjudicate liveness -- it does not know what the user
is looking at, so all it can do about silence is kill a connection that was
about to recover. LinkState owns that verdict: a notice at 2s, blind typing
held, Ctrl-\\ q offered. quinn warns an infinite timeout can hang a future;
the notice is the answer, and it says more than a dead connection did.

ACCEPT_TIMEOUT is KEPT, against the spec's wording. Spec 4.1 calls it 'the
constant that exists specifically to match' the idle timeout, but line 53 is
the sentence justifying the number, not the constant, and the constant bounds
a case the idle timeout never covered: nobody ever spoke, so there is no
connection to time out. Removing it parks --serve on accept() for ever
holding a registered session and a punched socket. Its doc is rewritten; the
number now stands on its own.

Both ends change together because transport_config() is shared, which is
required rather than incidental -- the effective timeout is the minimum of
the two peers, so a one-sided change would do nothing. A new client against
an old host therefore still negotiates 30s."
```

---

## Task 3: The host learns to detach without quinn's help

**Files:**
- Modify: `src/session.rs` (`HostSession`: struct, `spawn`, `turn`, `turn_with`)

**Interfaces:**
- Produces:
  - `pub const DETACH_AFTER: Duration`
  - `HostSession::turn_at(&mut self, now: Instant, first: Option<Frame>) -> Result<Turn>`
  - `HostSession::turn(&mut self) -> Result<Turn>` — unchanged signature, now
    `self.turn_at(Instant::now(), None)`
  - `HostSession::turn_with(&mut self, first: Option<Frame>) -> Result<Turn>` —
    unchanged signature, now `self.turn_at(Instant::now(), first)`

**Background — this is the task that stops Task 2 being a regression.** The host
decides whether anyone is listening like this, at `src/session.rs:230`:

```rust
let attached = self.link.sink.connection().close_reason().is_none();
```

and its comment ends: *"This turns off only once quinn has given the connection
up."* Task 2 makes quinn **never** give the connection up. So `attached` is
`true` for ever, `turn.detached` is never set, and the optimisation that comment
is guarding evaporates — silently, with every test still green, because no test
runs a host for thirty seconds with a vanished client.

What it is guarding, from the same comment, measured: *"a detached session whose
child was writing five lines a second burned 17-20% of a core"* building
snapshots and offering frames for a screen nobody would ever see. After Task 2
that becomes permanent, for every abandoned session, on a machine where
abandoned sessions already accumulate because reattach does not exist. **Tier A
without this task trades a client-side hang for a host-side CPU leak.**

The fix is the signal the old comment explicitly rejected — "did a frame arrive
recently" — and it is worth being clear about why that rejection expired rather
than was wrong. It was rejected *because* `close_reason` was strictly better
while a finite timeout existed: it never guessed. With the timeout gone,
`close_reason` no longer answers the question at all, and a generous recency
window answers it exactly as well as the timeout did. Set `DETACH_AFTER` to the
thirty seconds quinn used to enforce and the host's behaviour is **unchanged**,
by construction.

The objection in that comment still stands and is still honoured: *"during a
network blip the connection is still open and we WANT the work to continue, so
the session resumes instantly when the peer comes back."* Thirty seconds is six
times `HEARTBEAT_IDLE`, so an attached-but-quiet client is nowhere near it; and
detaching does not close anything, so a peer that returns is heard, sets
`last_heard`, flips `attached` back, and `screen_stale` forces the fresh snapshot
that already exists for exactly this.

**Clock as a parameter**, following the rule `note_heard`'s doc already states
for this file: *"The clock is a parameter so the loop's behaviour can be tested
without sleeping, exactly as `LinkState` is."* A threshold tested by sleeping
thirty seconds is a threshold nobody will test.

- [ ] **Step 1: Write the failing tests**

Add to `src/session.rs`'s `mod tests`:

```rust
    /// The property Task 2 takes away from quinn and this task gives back.
    /// A host whose client stopped speaking must stop building frames for it,
    /// or an abandoned session burns a core for ever on a screen nobody will
    /// see. Measured at 17-20% of a core for a child writing five lines a
    /// second, which is why this is not a micro-optimisation.
    #[tokio::test]
    async fn a_host_whose_client_went_quiet_detaches_on_its_own_clock() {
        let (mut host, _client) = pair("/bin/sh").await;
        let t = Instant::now();

        let turn = host.turn_at(t, None).expect("a turn while attached");
        assert!(!turn.detached, "detached while the client was still speaking");

        let turn = host
            .turn_at(t + DETACH_AFTER + Duration::from_secs(1), None)
            .expect("a turn after the client went quiet");
        assert!(
            turn.detached,
            "the host is still building frames for a client that stopped \
             answering {DETACH_AFTER:?} ago"
        );
    }

    /// Detaching must not be a one-way door: the whole point of holding the
    /// connection open is that a peer coming back is heard instantly.
    #[tokio::test]
    async fn a_returning_client_reattaches_the_host() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let t = Instant::now();
        let late = t + DETACH_AFTER + Duration::from_secs(1);

        assert!(host.turn_at(late, None).expect("a turn").detached);

        // The client speaks again. Any frame is evidence of a peer.
        let mut out = Vec::new();
        client.turn(b"x", &mut out).expect("the client types");
        let frame = client_frame(&mut host).await;

        let turn = host
            .turn_at(late + Duration::from_millis(1), Some(frame))
            .expect("a turn with the client back");
        assert!(
            !turn.detached,
            "a client that came back was not heard; the screen would stay frozen"
        );
    }

    /// A blip is not a departure. HEARTBEAT_IDLE is 5s and DETACH_AFTER is 30s,
    /// and the gap between them is what stops an ordinary quiet moment from
    /// freezing the emulator behind the child.
    #[tokio::test]
    async fn an_ordinary_quiet_moment_does_not_detach_the_host() {
        let (mut host, _client) = pair("/bin/sh").await;
        let t = Instant::now();

        let turn = host
            .turn_at(t + crate::linkstate::HEARTBEAT_IDLE * 2, None)
            .expect("a turn a couple of heartbeats in");
        assert!(
            !turn.detached,
            "detached after two heartbeats; a quiet session is not an absent one"
        );
    }
```

`client_frame` is a helper for taking one frame off the host's source. If the
neighbouring tests already have one under another name, use theirs; otherwise:

```rust
    /// One frame from the client, as the host's select would receive it.
    async fn client_frame(host: &mut HostSession) -> Frame {
        tokio::time::timeout(Duration::from_secs(5), host.link.source.recv())
            .await
            .expect("the client's frame never arrived")
            .expect("the source closed")
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -j4 --bin oxutrm detaches_on_its_own_clock -- --nocapture
```

Expected: FAIL to **compile** — `turn_at` does not exist. That is a compile
error, not a behavioural RED, so Step 5 below verifies the guards properly
afterwards. This is the same situation the Task 8 implementer met last phase.

- [ ] **Step 3: Add the clock and the constant**

In `src/session.rs`, near the other session constants:

```rust
/// How long the host keeps building frames for a client it has not heard from.
///
/// **This is the guarantee quinn used to provide and no longer does.** Until
/// phase 2 the host asked `close_reason()`, which answered once the transport's
/// 30 s idle timeout had fired. `max_idle_timeout` is `None` now, so
/// `close_reason()` stays `None` for ever on a silent peer and the question has
/// to be answered from a clock of our own.
///
/// Thirty seconds, so the behaviour is unchanged by construction: it is exactly
/// what quinn enforced before. Six times `HEARTBEAT_IDLE`, so an attached
/// client that is merely quiet is nowhere near it -- it heartbeats at 0.2 Hz and
/// every heartbeat is a frame.
///
/// Detaching closes nothing. It stops snapshotting and stops offering frames;
/// the pty is still drained and the emulator still fed, because the screen being
/// current on reattach is the whole reason a detached session keeps emulating.
/// A peer that comes back is heard on its first frame and `screen_stale` forces
/// the snapshot.
pub const DETACH_AFTER: Duration = Duration::from_secs(30);
```

Add the field to `HostSession`:

```rust
    /// The last time anything arrived from the client. The host's own liveness
    /// clock, because `close_reason()` stopped being one when the transport's
    /// idle timeout went. See [`DETACH_AFTER`].
    last_heard: Instant,
```

and initialise it in `spawn`, alongside `screen_stale: false`:

```rust
            // An attach has just completed and R5 obliges the client to send
            // immediately, so "now" is true rather than optimistic.
            last_heard: Instant::now(),
```

- [ ] **Step 4: Thread the clock through `turn`**

Rename the body of `turn_with` to `turn_at`, taking `now` first, and leave the
two existing entry points as one-liners so no call site changes:

```rust
    /// One turn: apply whatever arrived, drain the PTY, offer a frame.
    pub fn turn(&mut self) -> Result<Turn> {
        self.turn_at(Instant::now(), None)
    }

    /// [`HostSession::turn`], plus a frame the caller has already taken off
    /// the source.
    ///
    /// `run`'s select has to *receive* a frame to know one arrived, so it
    /// arrives holding one; `try_recv` below would never see it and the
    /// keystrokes in it would be silently dropped.
    pub fn turn_with(&mut self, first: Option<Frame>) -> Result<Turn> {
        self.turn_at(Instant::now(), first)
    }

    /// [`HostSession::turn_with`], with the clock injected.
    ///
    /// The clock is a parameter for the same reason it is one throughout
    /// `LinkState` and `ClientSession::note_heard`: [`DETACH_AFTER`] is thirty
    /// seconds, and a threshold that can only be tested by sleeping thirty
    /// seconds is a threshold nobody tests.
    pub fn turn_at(&mut self, now: Instant, mut first: Option<Frame>) -> Result<Turn> {
        // ... the existing body of turn_with ...
    }
```

In the inbound loop at the top of that body, record the arrival. **Every frame,
not only applied ones** — a frame rejected for its sequence number is still
proof there is a peer:

```rust
        while let Some(frame) = first.take().or_else(|| self.link.source.try_recv()) {
            // Any frame at all is evidence of a peer, including one `on_frame`
            // rejects: a stale sequence number says the client is behind, not
            // that it is gone.
            self.last_heard = now;
            match self.input_rx.on_frame(&frame) {
                // ... unchanged ...
            }
        }
```

- [ ] **Step 5: Replace the `attached` test**

```rust
        // Two questions, and since phase 2 they have different answers.
        // `close_reason` still catches a peer that closed properly or a
        // transport error -- both are immediate and certain. What it no longer
        // catches is silence: `max_idle_timeout` is `None`, so quinn will hold
        // a connection to a peer that vanished for ever, and this used to read
        // "turns off only once quinn has given the connection up".
        //
        // So the recency window is what answers it now. Generous on purpose:
        // during a blip the connection is open and we WANT the work to
        // continue, so the session resumes instantly when the peer comes back.
        // `DETACH_AFTER` is six times the client's heartbeat interval.
        let closed = self.link.sink.connection().close_reason().is_some();
        let quiet_too_long = now.duration_since(self.last_heard) >= DETACH_AFTER;
        let attached = !closed && !quiet_too_long;
        turn.detached = !attached;
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test -j4 --bin oxutrm detach -- --nocapture
cargo test -j4 --bin oxutrm reattaches -- --nocapture
cargo test -j4
```

Expected: all PASS.

- [ ] **Step 7: Verify the guards against mutations, not against a compile error**

Step 2's RED was a missing symbol, which proves nothing. Verify each guard by
mutating the **shipped logic** and confirming the matching test fails:

1. `let attached = !closed;` — drop the recency term.
   → `a_host_whose_client_went_quiet_detaches_on_its_own_clock` must FAIL.
2. `let quiet_too_long = false;`
   → the same test must FAIL.
3. Remove `self.last_heard = now;` from the inbound loop.
   → `a_returning_client_reattaches_the_host` must FAIL.
4. `DETACH_AFTER = Duration::from_secs(1)`.
   → `an_ordinary_quiet_moment_does_not_detach_the_host` must FAIL.

Restore after each. A mutation that leaves the suite green means the test is
measuring the wrong thing — fix the test, not the count.

- [ ] **Step 8: Commit**

```bash
git add src/session.rs
git commit -m "fix(host): detach from a vanished client on our own clock

The host asked close_reason() whether anyone was listening, and its comment
ended 'this turns off only once quinn has given the connection up'. The
previous commit makes quinn never give a connection up, so attached would
have been true for ever and turn.detached never set.

That is not cosmetic. The comment guards a measured 17-20% of a core, burned
by a detached session whose child was writing five lines a second, building
snapshots and offering frames for a screen nobody would ever see. After the
idle timeout goes it would burn permanently, for every abandoned session, on
a host where abandoned sessions already pile up because reattach does not
exist yet. Tier A without this trades a client hang for a host CPU leak.

DETACH_AFTER is 30s -- exactly what quinn enforced -- so the behaviour is
unchanged by construction, and it is 6x the client's heartbeat so a merely
quiet client is nowhere near it. The old comment rejected 'did a frame
arrive recently' because close_reason was strictly better while a finite
timeout existed; with the timeout gone close_reason no longer answers the
question at all.

turn_at takes the clock, per the rule this file already states for
note_heard: a 30s threshold testable only by sleeping is untested."
```

---

## Task 4: The route probe

**Files:**
- Create: `src/roam.rs`
- Modify: `src/main.rs` (add `mod roam;`)

**Interfaces:**
- Produces:
  - `pub fn route_source(peer: SocketAddr) -> std::io::Result<IpAddr>`
  - `pub struct RouteWatch` with:
    - `pub fn new(baseline: Option<IpAddr>) -> RouteWatch`
    - `pub fn moved(&self, seen: IpAddr) -> bool`
    - `pub fn settle(&mut self, seen: IpAddr)`
  - `pub const ROUTE_PROBE_EVERY: Duration`

**Background.** §4.2: *"Bind a scratch UDP socket, connect it to the peer's
address, and read its `local_addr()`: that is the address the kernel would use
for this peer right now, obtained without sending a packet."* `connect` on a UDP
socket sends nothing; it fixes a default destination and performs the route
lookup, and `getsockname` then reports the source the kernel chose. No netlink,
no route sockets, no `cfg` — both platforms walk the same code, the rule that
`FD_DIRS`, `open_keyboard` and `second_loopback_ip` already follow.

**What the probe is compared against is where §4.2 is wrong.** It says *"Differs
from the socket we hold → the route moved"*. The socket we hold is bound
wildcard, so its `local_addr()` is `[::]:port` and carries no route information:
measured on the dev Mac, `('::', 54214)` for the session socket against
`('::ffff:192.168.17.5', 57050)` for a probe. Those never compare equal, so the
check would fire on the first probe of a healthy link — and §4.2 says in the very
next paragraph that a rebind invalidates a punched NAT hole. It would break the
path it exists to repair.

So the comparison is **probe against the previous probe**: a baseline taken while
the link was known good, replaced whenever a rebind succeeds. That is what
`RouteWatch` holds, and keeping it in its own type is what lets the decision be
tested with no sockets at all.

**IP only, never the port.** The probe socket's ephemeral port is its own and
says nothing about the session socket — 49974 and 57050 in the measurement above,
for two probes seconds apart on an unchanged machine. Comparing ports would make
every probe a route change.

**Addresses are unmapped on both sides of the comparison.** The session socket is
dual-stack where possible, so quinn reports a v4 peer as `::ffff:a.b.c.d`;
`oxutrm_net::unmap` and `unmap_ip` exist for exactly this and the crate's own doc
warns that forgetting either *"produces a socket that silently talks to nobody"*.
Unmapping first also lets the probe bind a socket in the peer's own family, so no
mapping is needed on the `connect` at all.

- [ ] **Step 1: Write the failing tests**

Create `src/roam.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("a literal address")
    }

    /// The whole point: a baseline taken while the link worked, compared with
    /// what the kernel would do now.
    #[test]
    fn a_changed_source_address_is_a_moved_route() {
        let w = RouteWatch::new(Some(ip("192.168.17.5")));
        assert!(w.moved(ip("10.46.18.101")));
        assert!(!w.moved(ip("192.168.17.5")));
    }

    /// Guards the defect in spec 4.2. The session socket is bound wildcard, so
    /// comparing against ITS local_addr would compare `::` with a real source
    /// address and call every healthy link a moved route -- then rebind, which
    /// invalidates a punched NAT hole. The unspecified address is not a
    /// baseline and must never be treated as one.
    #[test]
    fn the_wildcard_address_is_not_a_baseline() {
        let w = RouteWatch::new(Some(ip("::")));
        assert!(
            !w.moved(ip("192.168.17.5")),
            "`::` was treated as a real source address; every probe would rebind"
        );
    }

    /// With nothing known yet there is nothing to compare against, and a
    /// rebind on no evidence is strictly worse than doing nothing.
    #[test]
    fn no_baseline_is_not_a_moved_route() {
        let w = RouteWatch::new(None);
        assert!(!w.moved(ip("192.168.17.5")));
    }

    /// A rebind is once per actual change, not once per probe. After settling
    /// on the new address the same reading must stop asking to move.
    #[test]
    fn settling_stops_the_same_change_asking_twice() {
        let mut w = RouteWatch::new(Some(ip("192.168.17.5")));
        assert!(w.moved(ip("10.46.18.101")));
        w.settle(ip("10.46.18.101"));
        assert!(
            !w.moved(ip("10.46.18.101")),
            "the same route change asked for a second rebind"
        );
    }

    /// v4-mapped v6 and plain v4 are the same address. The session socket is
    /// dual-stack, so a baseline can be learned in one form and a probe read
    /// back in the other; treating them as different would rebind for ever.
    #[test]
    fn a_mapped_address_equals_its_plain_form() {
        let w = RouteWatch::new(Some(ip("::ffff:192.168.17.5")));
        assert!(
            !w.moved(IpAddr::V4(Ipv4Addr::new(192, 168, 17, 5))),
            "a v4-mapped baseline did not match its own plain v4 probe"
        );
    }

    /// Against a real kernel: the source address for a peer is a concrete
    /// address, never the wildcard the session socket reports. This is the
    /// measurement the whole trigger rests on, so it is asserted rather than
    /// assumed. Loopback, so it needs no network.
    #[test]
    fn probing_a_peer_yields_a_concrete_source_address() {
        let peer = "127.0.0.1:9".parse().expect("a literal address");
        let seen = route_source(peer).expect("loopback is always routable");

        assert!(!seen.is_unspecified(), "the probe returned the wildcard: {seen}");
        assert_eq!(seen, ip("127.0.0.1"), "the route to loopback is not loopback");
    }

    /// A probe that cannot answer must be silent, not fatal. An unroutable
    /// peer makes `connect` fail with ENETUNREACH, and that is a normal thing
    /// for a machine mid-outage -- which is exactly when this code runs.
    #[test]
    fn an_unroutable_peer_is_an_error_and_not_a_panic() {
        // Documentation range, guaranteed never routed anywhere real.
        let peer = "192.0.2.1:9".parse().expect("a literal address");
        // Either answer is legitimate: some stacks route it to a default
        // gateway and some refuse. Neither may panic, and neither may return
        // the wildcard.
        if let Ok(seen) = route_source(peer) {
            assert!(!seen.is_unspecified(), "the probe returned the wildcard: {seen}");
        }
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -j4 --bin oxutrm roam:: -- --nocapture
```

Expected: FAIL to compile — nothing above the test module exists yet.

- [ ] **Step 3: Write the module**

Put this above the test module in `src/roam.rs`:

```rust
//! Which local address this machine would use to reach the host, and whether
//! it changed.
//!
//! QUIC identifies a connection by connection IDs rather than by addresses, so
//! a client that changes its own local address is survivable by design --
//! `Link::rebind` performs the swap. What was missing was anything that
//! noticed the change, and this is it.
//!
//! **The trigger is evidence, not a platform API.** Bind a throwaway UDP
//! socket, `connect` it to the peer, and read its `local_addr()`. `connect` on
//! a UDP socket sends nothing: it fixes a default destination and runs the
//! route lookup, and `getsockname` then reports the source address the kernel
//! chose. So this asks the routing table the only question that matters --
//! "what would you do for THIS peer, right now" -- without a packet, without
//! netlink, without route sockets and without a `cfg`. Both platforms walk the
//! same code, which is the rule `FD_DIRS`, `open_keyboard` and
//! `second_loopback_ip` already follow.
//!
//! # What it is compared against, and what it must not be
//!
//! Not the session socket. `oxutrm_net::bind_socket` binds wildcard --
//! dual-stack `[::]` where it can -- so the session socket's `local_addr()` is
//! `[::]:port` and says nothing about any route. Measured on the dev Mac:
//! the session socket reports `('::', 54214)` while a probe to a LAN peer
//! reports `('::ffff:192.168.17.5', 57050)`. Those can never be equal, so a
//! check against the held socket -- which the design spec's 4.2 describes --
//! would call every healthy link a moved route and rebind, and a rebind moves
//! our source port and invalidates a punched NAT hole. It would break the path
//! it exists to repair, on every probe.
//!
//! So [`RouteWatch`] compares a probe against the PREVIOUS probe: a baseline
//! taken while the link was known good, replaced whenever a rebind succeeds.
//!
//! **The IP only, never the port.** The throwaway socket's ephemeral port is
//! its own; two probes seconds apart on an unchanged machine gave 49974 and
//! 57050. Comparing ports would make every probe a route change.

use std::net::{IpAddr, SocketAddr, UdpSocket};
use std::time::Duration;

/// How often the route is probed while `Silent`.
///
/// The client's loop wakes every `pacing_interval` -- 8 ms to 100 ms -- so a
/// probe on every lap would be up to 125 bind/connect pairs a second. Once a
/// second is far faster than any human notices and slower than anything that
/// could matter.
///
/// It is a floor on probing while `Silent` and nothing else. A healthy session
/// never reaches it: probing runs only in `Silent`, so a working link makes no
/// probe syscalls at all. This is emphatically not `IDLE_POLL` coming back as
/// a pace.
pub const ROUTE_PROBE_EVERY: Duration = Duration::from_secs(1);

/// The local address the kernel would use to reach `peer`, right now.
///
/// Sends nothing. Fails when the peer is unroutable, which is an ordinary
/// thing for a machine in the middle of the outage this runs during -- the
/// caller must treat an error as "no answer this time" and never as fatal.
pub fn route_source(peer: SocketAddr) -> std::io::Result<IpAddr> {
    // Unmapped first, so the throwaway socket can be bound in the peer's own
    // family and `connect` needs no mapping of its own. `oxutrm_net`'s own doc
    // warns that getting this wrong "produces a socket that silently talks to
    // nobody"; here it would produce EINVAL instead, which is at least loud.
    let peer = oxutrm_net::unmap(peer);
    let bind: SocketAddr = match peer {
        SocketAddr::V4(_) => SocketAddr::from(([0, 0, 0, 0], 0)),
        SocketAddr::V6(_) => SocketAddr::from(([0u16; 8], 0)),
    };

    let probe = UdpSocket::bind(bind)?;
    probe.connect(peer)?;
    Ok(oxutrm_net::unmap_ip(probe.local_addr()?.ip()))
}

/// The source address that was true when the link last worked.
pub struct RouteWatch {
    baseline: Option<IpAddr>,
}

impl RouteWatch {
    /// `baseline` is what a probe said while the link was known good. `None`
    /// when no probe has succeeded yet.
    pub fn new(baseline: Option<IpAddr>) -> RouteWatch {
        RouteWatch {
            baseline: baseline.map(Self::normalise).filter(|ip| !ip.is_unspecified()),
        }
    }

    /// Whether `seen` says this machine's route to the peer has moved.
    ///
    /// False with no baseline: a rebind on no evidence is strictly worse than
    /// doing nothing, because it costs a punched NAT hole to learn nothing.
    pub fn moved(&self, seen: IpAddr) -> bool {
        let seen = Self::normalise(seen);
        if seen.is_unspecified() {
            return false;
        }
        self.baseline.is_some_and(|base| base != seen)
    }

    /// Adopt `seen` as the address the link now works from.
    ///
    /// Called after a successful rebind, so one route change asks for one
    /// rebind rather than one per probe for the rest of the session.
    pub fn settle(&mut self, seen: IpAddr) {
        let seen = Self::normalise(seen);
        if !seen.is_unspecified() {
            self.baseline = Some(seen);
        }
    }

    /// v4-mapped v6 and plain v4 are the same address. The session socket is
    /// dual-stack, so a baseline can be learned in one form and a probe read
    /// back in the other; treating them as different would rebind for ever.
    fn normalise(ip: IpAddr) -> IpAddr {
        oxutrm_net::unmap_ip(ip)
    }
}
```

Add to `src/main.rs`, in the module list:

```rust
mod roam;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test -j4 --bin oxutrm roam:: -- --nocapture
```

Expected: all seven PASS.

- [ ] **Step 5: Verify the guards by injecting the faults**

Each mutation must fail the test named beside it. Restore after each.

1. `moved`: drop the `seen.is_unspecified()` guard **and** `new`'s `.filter(...)`
   → `the_wildcard_address_is_not_a_baseline` must FAIL.
2. `moved`: `self.baseline.is_none_or(|base| base != seen)`
   → `no_baseline_is_not_a_moved_route` must FAIL.
3. `normalise`: return `ip` unchanged
   → `a_mapped_address_equals_its_plain_form` must FAIL.
4. `settle`: make it a no-op
   → `settling_stops_the_same_change_asking_twice` must FAIL.
5. `route_source`: return `probe.local_addr()?.ip()` from a socket that was
   never `connect`ed
   → `probing_a_peer_yields_a_concrete_source_address` must FAIL, reporting the
   wildcard. **This is the most important of the five** — it is the mutation
   that turns the probe back into the thing spec §4.2 describes.

- [ ] **Step 6: Commit**

```bash
git add src/roam.rs src/main.rs
git commit -m "feat(client): ask the routing table which source address it would use

Bind a throwaway UDP socket, connect it to the peer, read local_addr. connect
sends nothing -- it fixes a destination and runs the route lookup -- so this
asks the only question that matters, 'what would you do for THIS peer right
now', without a packet, netlink, route sockets or a cfg.

Against spec 4.2, the reading is NOT compared with the session socket's own
local_addr. That socket is bound wildcard, so it reports `::` and carries no
route information: measured, `('::', 54214)` for the session socket against
`('::ffff:192.168.17.5', 57050)` for a probe. They can never be equal, so
that check would call every healthy link a moved route -- and 4.2 says in its
next paragraph that a rebind invalidates a punched NAT hole. It would break
the path it exists to repair, on every probe.

RouteWatch compares a probe with the previous probe instead: a baseline from
when the link worked, replaced when a rebind succeeds. IP only; the probe
socket's ephemeral port is its own and moves on every probe."
```

---

## Task 5: Wiring the rebind into the client loop

**Files:**
- Modify: `src/session.rs` (`ClientSession`: struct, `new`, `run_on`, `rebind`)
- Modify: `src/link.rs` (`Link::rebind` and the `endpoint` field: drop
  `#[allow(dead_code)]`, rewrite the "no caller" docs)

**Interfaces:**
- Consumes: `crate::roam::{ROUTE_PROBE_EVERY, RouteWatch, route_source}`,
  `crate::ladder::adopt`, `oxutrm_net::bind_socket`, `Link::rebind`
- Produces:
  - `ClientSession::follow_route(&mut self, now: Instant) -> bool` — probes if
    due, rebinds if the route moved, reports whether a rebind happened
  - `ClientSession::rebind` loses its `#[allow(dead_code)]`

**Background.** Three `#[allow(dead_code)]` attributes come off in this task:
`Link::rebind`, `Link::endpoint` and `ClientSession::rebind`. The project's rule
is *do not add `#[allow(dead_code)]` to quiet a module that is not wired up yet —
it outlives the wiring*; these three are the proof, and removing them is part of
the deliverable rather than tidying.

**Only while `Silent`**, per §4.2: a rebind moves our source port and invalidates
a punched NAT hole, so doing it to a working path breaks the path in order to
test it. `Silent` means a reply has been owed for `SILENT_AFTER` and none came —
the path is already not working, so there is nothing left to break.

**The baseline is taken on the first probe and after every successful rebind**,
never while `Live`. Probing a healthy link would cost a syscall pair per second
for the life of every session to detect something that, by construction, cannot
have broken anything yet. The consequence is that the very first probe of an
outage has no baseline and cannot rebind; the one a second later can. One second
of a `Silent` box, against never touching a healthy session, is the right trade.

**A failed probe or rebind must never end the session.** The machine is mid-outage
— `connect` failing with `ENETUNREACH` is an ordinary reading, not an error
condition, and this is the project's "a send failure must never end a session"
rule applied to the thing most likely to fail.

**Honest about what is not yet known.** Whether a wildcard-bound socket needs the
rebind at all on every platform is unproven: the kernel sources packets from the
new address automatically when the route changes, so some route moves may recover
with no help. What `rebind_abstract` adds is a fresh socket and quinn's own path
validation from it. Task 6 is where this gets measured rather than asserted, and
the handoff already flags *"the route probe is unproven under a VPN"*. Do not
write a comment claiming the rebind is what saved the session until it has been
watched saving one.

- [ ] **Step 1: Write the failing tests**

Add to `src/session.rs`'s `mod tests`:

```rust
    /// The rule from spec 4.2, and the one that costs something to get wrong:
    /// a rebind moves our source port and invalidates a punched NAT hole, so
    /// doing it to a link that is working breaks the path in order to test it.
    #[tokio::test]
    async fn a_healthy_session_never_probes_the_route() {
        let (_host, mut session) = pair("/bin/sh").await;
        let t = Instant::now();
        session.note_heard(t);

        assert!(
            !session.follow_route(t),
            "probed a healthy link; a rebind on a working path breaks it"
        );
        assert!(
            !session.follow_route(t + ROUTE_PROBE_EVERY * 5),
            "probed a healthy link after several intervals"
        );
    }

    /// Probing is gated even inside `Silent`: the loop wakes every 8-100ms and
    /// a bind/connect pair on every lap is up to 125 a second.
    #[tokio::test]
    async fn probing_is_paced_while_silent() {
        let (_host, mut session) = with_notice().await;
        let t = Instant::now();

        session.follow_route(t);
        let after_first = session.probed_at;
        assert!(after_first.is_some(), "the first probe in Silent did not run");

        session.follow_route(t + ROUTE_PROBE_EVERY / 2);
        assert_eq!(
            session.probed_at, after_first,
            "probed twice inside one ROUTE_PROBE_EVERY"
        );

        session.follow_route(t + ROUTE_PROBE_EVERY * 2);
        assert_ne!(
            session.probed_at, after_first,
            "the probe never resumed after its interval"
        );
    }

    /// A machine mid-outage is exactly when `connect` fails with ENETUNREACH,
    /// and that is the moment this code runs. It must be a reading, not a
    /// failure: the session survives it and the notice stays up.
    #[tokio::test]
    async fn an_unroutable_peer_does_not_end_the_session() {
        let (_host, mut session) = with_notice().await;
        let t = Instant::now();

        // No panic, no error out of the loop, and no rebind on no evidence.
        assert!(!session.follow_route(t));
        assert!(
            session.notice_at(t).is_some(),
            "a failed probe took the notice down"
        );
    }

    /// The first probe of an outage has no baseline and must not rebind on it.
    /// A rebind costs a punched NAT hole; spending one to learn nothing is
    /// strictly worse than waiting a second for a second reading.
    #[tokio::test]
    async fn the_first_probe_of_an_outage_does_not_rebind() {
        let (_host, mut session) = with_notice().await;
        let t = Instant::now();
        let before = session.link.socket.local_addr().expect("a bound socket");

        assert!(!session.follow_route(t), "rebound with no baseline to compare");
        assert_eq!(
            session.link.socket.local_addr().expect("a bound socket"),
            before,
            "the session socket was swapped on the first probe"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cargo test -j4 --bin oxutrm follow_route -- --nocapture
cargo test -j4 --bin oxutrm probing_is_paced -- --nocapture
```

Expected: FAIL to compile — `follow_route` and `probed_at` do not exist. Step 6
verifies the guards properly by mutation, because a compile error is not a RED.

- [ ] **Step 3: Add the state**

Add to `ClientSession`:

```rust
    /// The source address the link was working from, so a moved route can be
    /// spotted. See `crate::roam`.
    route: crate::roam::RouteWatch,
    /// When the route was last probed, so `Silent` does not probe on every
    /// lap of a loop that wakes up to 125 times a second.
    probed_at: Option<Instant>,
```

and to `ClientSession::new`, in the struct literal:

```rust
            // No baseline: the first probe of an outage takes one and the
            // second can act on it. Probing while healthy would cost a syscall
            // pair a second for the life of every session, to detect something
            // that cannot have broken anything yet.
            route: crate::roam::RouteWatch::new(None),
            probed_at: None,
```

- [ ] **Step 4: Write `follow_route`**

Add to `impl ClientSession`, next to `rebind`:

```rust
    /// Follow this machine's route to the host, if it moved.
    ///
    /// Returns whether the session socket was actually swapped.
    ///
    /// **Only while `Silent`**, per design spec 4.2: a rebind moves our source
    /// port, which invalidates a punched NAT hole, so doing it to a working
    /// path breaks the path in order to test it. `Silent` means a reply has
    /// been owed for `SILENT_AFTER` with none arriving -- the path is already
    /// not working, so there is nothing left to break.
    ///
    /// Nothing here may end the session. A machine in the middle of an outage
    /// is exactly where `connect` fails with `ENETUNREACH` and where binding a
    /// fresh socket fails, and this runs during precisely that. Every failure
    /// is "no answer this time"; the next probe asks again. Same rule as "a
    /// send failure must never end a session", applied to the thing most
    /// likely to fail.
    fn follow_route(&mut self, now: Instant) -> bool {
        if !matches!(self.link_state.phase_now(), Phase::Silent { .. }) {
            return false;
        }
        if self
            .probed_at
            .is_some_and(|last| now.duration_since(last) < crate::roam::ROUTE_PROBE_EVERY)
        {
            return false;
        }
        self.probed_at = Some(now);

        let peer = self.link.sink.connection().remote_address();
        let Ok(seen) = crate::roam::route_source(peer) else {
            // Unroutable right now, which is an ordinary reading mid-outage
            // and not a fault. The baseline is left alone: a route we cannot
            // see is not a route that moved.
            return false;
        };

        if !self.route.moved(seen) {
            // Either nothing changed, or this is the first reading of the
            // outage and there is nothing to compare it with. Adopting it as
            // the baseline is what makes the NEXT probe able to act.
            self.route.settle(seen);
            return false;
        }

        // The route moved. Bind a fresh socket the same way the ladder bound
        // the first one -- wildcard, preferring 443 -- and hand it to the live
        // connection. QUIC is identified by connection IDs, not addresses, so
        // the connection itself does not notice.
        let cfg = oxutrm_net::NetConfig::default();
        let Ok(bound) = oxutrm_net::bind_socket(&cfg) else {
            return false;
        };
        let Ok(socket) = crate::ladder::adopt(bound) else {
            return false;
        };
        if self.rebind(socket).is_err() {
            // The old socket is still in place and still the one quinn holds:
            // `Link::rebind` only assigns after `rebind_abstract` succeeded.
            return false;
        }

        // Only now, so a failed rebind leaves the old baseline and the next
        // probe tries again rather than believing it has already moved.
        self.route.settle(seen);
        true
    }
```

`phase_now` does not exist yet: `LinkState::phase` is `#[cfg(test)]`, and its
doc says why — *"a later phase that genuinely needs to read the state without
advancing it can lift the attribute back off"*. This is that phase. In
`src/linkstate.rs`, replace the attribute and rewrite the doc:

```rust
    /// The phase, without advancing anything.
    ///
    /// `evaluate` returns the phase it decided, so the loop reads it from
    /// there rather than asking twice. This exists for the callers that need
    /// to know what is already true without deciding anything: phase 2's route
    /// probe runs only while `Silent`, and asking `evaluate` would both
    /// require a `reply_owed` it has no business computing and advance a state
    /// machine it only wants to read.
    pub fn phase_now(&self) -> Phase {
        self.phase
    }
```

Update `linkstate.rs`'s own tests to call `phase_now`.

- [ ] **Step 5: Call it from the loop**

In `run_on`, in the block after the `match wake` that already computes `now` and
rebuilds the notice — immediately **after** the notice block and **before**
`self.heartbeat(now)`:

```rust
            // Follow the route if it moved. Inside the loop rather than on a
            // timer of its own: `follow_route` is gated on `Silent` and paced
            // by `ROUTE_PROBE_EVERY`, so a healthy session reaches this line
            // ten times a second and does nothing but one `matches!`.
            //
            // After the notice, so the box describing the silence is already
            // on the screen before anything is done about it -- and the user
            // is told nothing about the rebind, because a rebind that has not
            // restored contact yet is not something the client can honestly
            // report.
            let _ = self.follow_route(now);
```

The `let _ =` is deliberate and not laziness: the loop has nothing to do
differently either way. A rebind that works shows up as a frame arriving, which
takes the notice down through the ordinary path Task 1 fixed.

- [ ] **Step 6: Remove the three `#[allow(dead_code)]` and fix their docs**

In `src/link.rs`, the `endpoint` field:

```rust
    /// The endpoint quinn owns. Rebound while roaming, so a local address
    /// change does not cost the connection.
    pub endpoint: quinn::Endpoint,
```

and `Link::rebind` — keep the first paragraph about QUIC's one kind of
migration, and replace the "No caller" paragraph:

```rust
    /// Called by `ClientSession::follow_route` when the route probe says this
    /// machine's source address for the peer has changed. Only ever while
    /// `Silent`: this moves our source port and invalidates a punched NAT
    /// hole, so doing it to a working path would break the path in order to
    /// test it.
```

In `src/session.rs`, `ClientSession::rebind` — same treatment:

```rust
    /// Move to a new local socket without dropping the connection.
    ///
    /// Called by [`ClientSession::follow_route`]. See [`Link::rebind`].
    pub fn rebind(&mut self, socket: Arc<tokio::net::UdpSocket>) -> Result<()> {
```

Then confirm nothing else needed the attribute:

```bash
cargo build -j4 2>&1 | grep -i "never used\|dead_code"
```

Expected: no hits for `rebind` or `endpoint`. Leave any other warnings alone —
they belong to their own wiring.

- [ ] **Step 7: Run the tests to verify they pass**

```bash
cargo test -j4 --bin oxutrm follow_route -- --nocapture
cargo test -j4 --bin oxutrm probing_is_paced -- --nocapture
cargo test -j4 --bin oxutrm first_probe -- --nocapture
cargo test -j4
```

Expected: all PASS.

- [ ] **Step 8: Verify the guards against mutations of the shipped logic**

Each must fail the test named beside it; restore after each.

1. Drop the `Phase::Silent` gate from `follow_route`.
   → `a_healthy_session_never_probes_the_route` must FAIL. **This is the one
   that matters most**: without the gate, a healthy session probes and
   eventually rebinds a working path.
2. Drop the `ROUTE_PROBE_EVERY` gate.
   → `probing_is_paced_while_silent` must FAIL.
3. Make the `route_source` error arm `panic!()` instead of `return false`.
   → `an_unroutable_peer_does_not_end_the_session` must FAIL.
4. In `RouteWatch::moved`, treat `None` as moved.
   → `the_first_probe_of_an_outage_does_not_rebind` must FAIL.

- [ ] **Step 9: Commit**

```bash
git add src/session.rs src/link.rs src/linkstate.rs
git commit -m "feat(client): follow the route when the local address moves

Wires Link::rebind, which has been built and tested with no caller since M3.
Three #[allow(dead_code)] come off with it -- Link::rebind, Link::endpoint
and ClientSession::rebind -- which is why the project's rule is not to add
one for something that is not wired up yet: they outlive the wiring.

Only while Silent, per spec 4.2. A rebind moves our source port and
invalidates a punched NAT hole, so doing it to a working path would break
the path in order to test it; Silent means a reply has been owed for
SILENT_AFTER with none arriving, so there is nothing left to break.

The baseline is taken on the first probe of an outage and after every
successful rebind, never while healthy: probing a working link would cost a
syscall pair a second for the life of every session to detect something that
cannot have broken anything yet. So the first probe of an outage cannot act
and the second can. One second of a box already on the screen, against never
touching a healthy session.

Nothing here can end the session. A machine mid-outage is exactly where
connect returns ENETUNREACH, and that is when this runs.

LinkState::phase loses its #[cfg(test)] and becomes phase_now, which its own
doc anticipated: 'a later phase that genuinely needs to read the state
without advancing it can lift the attribute back off'."
```

---

## Task 6: The composed test, and the measurement

**Files:**
- Modify: `src/session.rs` (`mod tests`)
- Create: `docs/superpowers/notes/2026-08-30-tier-a-hand-test.md`

**Interfaces:**
- Produces: no source signatures. One composed test and one written measurement.

**Background — budget one of these per phase.** Phase 1's Critical bug survived
eight per-task reviews because **every per-task test holds the clock still**, and
the bug lived across the seam between two tasks. This phase has the same shape:
Task 2 removes a timeout, Task 3 replaces the guarantee it provided, Task 5 acts
only in a state Task 1 governs the exit from. No per-task test spans that. The
one thing that finds this class is *a composed test that lets the real loop run
past the longest timer in the system*.

The longest timer here is `DETACH_AFTER`, at 30 s. A 30 s wall-clock test is too
slow for the suite — and the existing 12 s
`a_healthy_session_paints_no_notice_across_several_heartbeats` is already flagged
in the handoff as something that could flake on a loaded runner. So the composed
test runs against the **client's** longest real timer instead, and `DETACH_AFTER`
is covered by Task 3's injected clock plus the hand test below.

- [ ] **Step 1: Write the composed test**

```rust
    /// The composed test for phase 2, and the reason there is one: phase 1's
    /// Critical bug lived across a seam no single task owned, and every
    /// per-task test holds the clock still. This one lets the real loop run.
    ///
    /// It asserts the phase's whole user-visible claim in one place: silence
    /// raises a box, the session does NOT die under it -- which is the entire
    /// change -- and the box comes down by itself when the host speaks again.
    /// Before phase 2 the client exited at ~33 s with an error instead.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_session_outlives_a_silence_that_used_to_kill_it() {
        let (mut host, mut client) = pair("/bin/sh").await;
        let mut out = Vec::new();
        let t = Instant::now();

        client.note_heard(t);
        client.note_sent(t);

        // Long enough to have been fatal: the old max_idle_timeout was 30s and
        // the client died at ~33s in the hand test. Real elapsed time, because
        // the thing under test is a real quinn connection's own timers, and an
        // injected clock cannot reach those.
        let outage = Duration::from_secs(35);
        let deadline = tokio::time::Instant::now() + outage;
        while tokio::time::Instant::now() < deadline {
            // The loop's own laps, with the host saying nothing at all.
            client.turn(&[], &mut out).expect("the session survives the lap");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            client.notice_at(Instant::now()).is_some(),
            "no notice after {outage:?} of silence"
        );
        assert!(
            client.link.sink.connection().close_reason().is_none(),
            "the connection died under the notice: {:?}",
            client.link.sink.connection().close_reason()
        );

        // The host comes back. The notice must come down on its own -- through
        // the scavenging path Task 1 fixed, since nothing here wakes a frame arm.
        host.turn().expect("the host answers at last");
        tokio::time::sleep(Duration::from_millis(200)).await;
        client.turn(&[], &mut out).expect("a lap that scavenges the frame");

        assert!(
            client.notice_at(Instant::now()).is_none(),
            "the host answered and the notice stayed up"
        );
    }
```

- [ ] **Step 2: Run it**

```bash
cargo test -j4 --bin oxutrm outlives_a_silence -- --nocapture --ignored --test-threads=1
cargo test -j4 --bin oxutrm outlives_a_silence -- --nocapture
```

Expected: PASS, taking a little over 35 s.

**If it is too slow to keep in the default suite**, mark it `#[ignore]` with a
doc line saying which command runs it, and add that command to `CHANGES.md`'s
entry and to the hand-test note. Do **not** shorten the outage below 31 s: the
whole assertion is that it outlives the timeout that used to kill it, and a
25-second version would pass against the unfixed code. **Verify that**: check
out `transport_config` from before Task 2, run this test, and confirm it fails.
A test that passes against the injected bug is not a guard.

- [ ] **Step 3: Hand-test it on a real host**

Tests are not the artefact; a person looking at a terminal is. Use the recipe
that worked last phase, verbatim:

```bash
# On the Mac. The trailing sleep is ESSENTIAL -- without it the tmux server
# dies with the client and the exit message is lost.
tmux new-session -d -s ox -x 100 -y 30 \
  'target/release/oxutrm thinlinc 2>/tmp/err.txt; echo "EXITED rc=$?" >> /tmp/err.txt; sleep 900'
```

- Induce silence by sending **`SIGSTOP` to the host-side session process**.
  Deterministic, instant, reversible with `SIGCONT`, no firewall rules. Get the
  PID from `oxutrm host --list` — **not** from `pgrep host --serve`, which
  detaches and re-execs so its PID is transient and already gone.
- Observe with `tmux capture-pane -p`.
- **Check liveness from the process or `/tmp/err.txt`, never from pane content.**
  A pane keeps showing the last painted screen after the process exits; that
  cost a wrong "still alive at 40 s" report last phase.
- **Timestamp both ends with `date +%T` before calling any number a defect.** A
  claimed 6 s counter overstatement was 14 s actual against 15 s displayed.

**The host binary on thinlinc must be rebuilt for this phase.** The handoff
records that it is host-equivalent to `main` only because phase 1 changed no host
code. Task 3 changes host code, so that equivalence is dead. Rebuild, and **keep
the `~/.local/bin/oxutrm` → `/scratch/oetiker/cargo-target-oxutrm-main/release/oxutrm`
symlink**.

What to establish, in order:

1. **`SIGSTOP` for 60 s.** The box appears at ~2 s, the counter climbs past 33 s,
   and **the client is still alive at 60 s**. That single observation is the
   phase. `SIGCONT`; the box goes; the shell still works.
2. **The host detaches and comes back.** With the client `SIGSTOP`ped instead
   for 40 s, confirm from `top`/`ps` `TIME` **deltas** — not `%CPU`, which is a
   lifetime average — that the host process is not burning a core, and that the
   screen is correct when the client resumes. Run something that writes
   steadily (`while true; do date; sleep 0.2; done`) before stopping it, or the
   measurement is of an idle session and proves nothing about the case Task 3
   is for. **Measure the case the complaint was about.**
3. **A real route change.** Drop the Mac's wifi and bring it back on a different
   network, or connect/disconnect the VPN, while the session is up. Record what
   the probe saw, whether a rebind fired, and whether the session recovered.
   This is the one that is genuinely unproven — the handoff flags *"the route
   probe is unproven under a VPN"* — and the measured source addresses on this
   machine differ per peer (VPN `10.46.18.101` for the internet,
   `192.168.17.5` for the LAN host), so a VPN toggle should move it.

- [ ] **Step 4: Write down what was measured**

Create `docs/superpowers/notes/2026-08-30-tier-a-hand-test.md` with the actual
numbers, both ends timestamped, and — for anything that did not work — what was
observed rather than what was expected. **A prediction in a handoff is a
hypothesis, not a finding**, and the three open questions this phase inherits
(`SILENT_AFTER` = 2 s, `REBUILD_AFTER` = 20 s, the probe under a VPN) are all
waiting on exactly this note.

Be explicit if the rebind turns out not to be what recovered the session. A
wildcard-bound socket may follow a route change on its own, because the kernel
picks the source address per packet; if that is what happened, say so, because
it changes what phase 3 needs to build.

- [ ] **Step 5: Commit**

```bash
git add src/session.rs docs/superpowers/notes/2026-08-30-tier-a-hand-test.md
git commit -m "test: a session outlives the silence that used to kill it

The composed test for this phase, and the reason to budget one per phase:
phase 1's Critical bug survived eight per-task reviews because every
per-task test holds the clock still and the bug lived across a seam no
single task owned. Phase 2 has the same shape -- one task removes a timeout,
another replaces the guarantee it provided, a third acts only in a state a
fourth governs the exit from.

Real elapsed time, past the timer that used to fire, because the thing under
test is a real quinn connection's own timers and an injected clock cannot
reach those.

Plus the hand-test note: what a person watching a terminal actually saw."
```

---

## Task 7: Say what shipped

**Files:**
- Modify: `CHANGES.md`
- Modify: `docs/superpowers/specs/2026-08-29-session-recovery-design.md` (§4 only)

**Background.** `CHANGES.md`'s `## Unreleased` block is what the release workflow
folds into the release notes, and it was **empty** when `v0.1.0` was cut — so the
release shipped phase 1 and described none of it, and the notes had to be
repaired afterwards with `gh release edit`. The changelog is part of the work.

The spec edit is the other half of the same rule. §4 contains three statements
that are wrong against the code, and this plan's preamble documents them. A spec
is normative in this repo — the handoff's standing instruction is *read the specs
before designing here, they were written by someone who thought further ahead
than the code got* — so leaving known-false sentences in a normative document
sets up the next reader to implement them.

- [ ] **Step 1: Write the changelog entry**

Under `## Unreleased` in `CHANGES.md`:

```markdown
### Added

- **The session survives a network outage instead of dying at thirty seconds.**
  The QUIC transport no longer imposes an idle timeout, so silence stops ending
  a session: the client's own state machine decides the host has gone quiet,
  raises the notice at two seconds, holds what is typed blind, and keeps the
  connection for whenever the network comes back.
- **The client follows a local address change.** While the link is silent, a
  route probe asks the routing table which source address it would now use for
  the host — a throwaway UDP socket, `connect`ed, no packet sent. If it moved,
  the session socket is swapped underneath the live connection, which QUIC
  allows because it identifies a connection by connection IDs rather than by
  addresses.

### Changed

- The host now decides for itself when a client has gone away, after thirty
  seconds without a frame, rather than waiting for the transport to give the
  connection up — which it no longer does. Behaviour is unchanged; a session
  whose client vanished still stops building screens nobody will see.

### Compatibility

- **Both ends must be on this version** for the idle timeout to be gone. QUIC
  negotiates the effective timeout as the minimum of the two peers', so a new
  client against an `0.1.0` host still dies at thirty seconds of silence.
- Reattaching a session you were disconnected from is still not implemented;
  `oxutrm host --attach` says so. That is the next phase.
```

- [ ] **Step 2: Correct §4 of the spec**

Three edits, each replacing a false statement with the true one and a sentence
saying what was wrong. Do not silently rewrite history — the reason each was
wrong is the useful part.

1. §4.1, *"`max_idle_timeout` is removed from `transport_config()` ... along with
   the constant in `src/accept.rs:53` that exists specifically to match it"*:

```markdown
`max_idle_timeout` is set to **`None`** in `transport_config()`
(`crates/oxutrm-net/src/quic.rs`). Explicitly `None`, not a deleted line: quinn's
default is `Some(30s)` (`quinn-proto` `TransportConfig::default`), so removing
the setter restores exactly the timeout this removes. Both ends change together
because `transport_config()` is shared, which is required rather than
incidental — the effective timeout is the minimum of the two peers', so a
one-sided change would do nothing.

`ACCEPT_TIMEOUT` in `src/accept.rs` **stays.** An earlier draft of this section
called for removing it as "the constant that exists specifically to match" the
idle timeout; that was wrong twice over. `src/accept.rs:53` is a sentence inside
the constant's doc comment justifying its *value*, not the constant. And the
constant bounds a case `max_idle_timeout` never covered: no peer ever spoke, so
there is no connection for a transport timeout to fire on. Removing it parks
`--serve` on `Endpoint::accept()` for ever, holding a registered session and a
punched socket. Its justification is rewritten; the number stands on its own.

**The host needs its own detach clock.** `HostSession::turn` decided whether
anyone was listening from `close_reason()`, which answered once the idle timeout
had fired. With no idle timeout it never answers, so a session whose client
vanished would build screens for it for ever — a measured 17-20% of a core for a
child writing five lines a second. `DETACH_AFTER`, thirty seconds without a
frame, restores the previous behaviour exactly.
```

2. §4.2, *"Differs from the socket we hold → the route moved → rebind"*:

```markdown
Compare it against **the previous probe**, not against the socket we hold. An
earlier draft said "differs from the socket we hold"; the session socket is bound
wildcard, so its `local_addr()` is `[::]:port` and carries no route information
at all — it can never equal a concrete source address, so that check would call
every healthy link a moved route and rebind it, invalidating a punched NAT hole
to repair a path that was working. The baseline is taken on the first probe of an
outage and replaced after each successful rebind, and only the **IP** is
compared: the probe socket's ephemeral port is its own and changes on every probe.
```

3. §4.2's closing line about `REBUILD_AFTER` — add one sentence, since Tier A
   ships before §6 exists:

```markdown
Until phase 3 builds it, a rebind that does not restore contact leaves the
`Silent` notice up until the user presses `Ctrl-\ q`. The client no longer dies
on its own, which is the point, and it does not pretend to be retrying.
```

- [ ] **Step 3: Run everything**

```bash
cargo fmt --all -- --check
cargo clippy -j4 --all-targets -- -D warnings
cargo test -j4
```

Expected: clean, and the suite green. Record the actual pass count — **one green
run is not evidence**, so run the suite twice and confirm the count both times,
particularly for the 12 s and 35 s wall-clock tests.

- [ ] **Step 4: Commit**

```bash
git add CHANGES.md docs/superpowers/specs/2026-08-29-session-recovery-design.md
git commit -m "docs: say what Tier A shipped, and correct the spec it came from

CHANGES.md's Unreleased block is what the release workflow folds into the
notes, and it was empty when v0.1.0 was cut -- the release shipped phase 1
and described none of it. A changelog entry is part of the work.

Spec 4 carried three statements that were wrong against the code, each of
which fails silently: 'remove max_idle_timeout' is a no-op because quinn
defaults to 30s; removing ACCEPT_TIMEOUT reintroduces a session leak it was
written to prevent; and comparing the probe against the held socket compares
a concrete address with the wildcard, so it fires on every healthy link. The
spec is normative here, so leaving them in sets up the next reader to
implement them. Each is corrected with the reason it was wrong."
```

---

## Self-Review

Run against the spec with fresh eyes, per the skill. Phase 1's self-review passed
a plan with five defects in it, so this section names what it actually checked
rather than asserting the plan is fine.

**1. Spec coverage.**

| §4 requirement | Task |
|---|---|
| `max_idle_timeout` removed from `transport_config()` | Task 2 — **as `None`**, since deletion is a no-op |
| the constant in `accept.rs` that matches it | Task 2 — **kept**, doc rewritten; removing it is a leak |
| both ends change | Task 2 — `transport_config()` is shared; noted in the changelog for version skew |
| quinn's keep-alive stays at 10 s | Task 2 — asserted by a test, since "stays" is exactly what a careless edit breaks |
| `conn.closed()` fires only on close or error | Task 3 — the consequence, and the host's detach clock that answers it |
| bind a scratch socket, connect, read `local_addr()` | Task 4 — `route_source` |
| no netlink, no route sockets, no `cfg` | Task 4 — one code path; in Global Constraints |
| differs → rebind | Task 4 — **against the previous probe**, IP only; the spec's comparison is wrong |
| only ever attempted while `Silent` | Task 5 — the gate, and the mutation test that proves the gate |
| `Link::rebind` wired | Task 5 — plus the three `#[allow(dead_code)]` |
| `REBUILD_AFTER` / §6 re-punch | **Out of scope**, stated in the scope boundary and in the spec edit |

Two things in this plan are not in §4, and both are here because §4's changes
break something otherwise: Task 3 (the host's detach clock) and Task 1 (the
scavenged-frame `note_heard`, which §7 of the handoff proposed and which Task 2
promotes from cosmetic to unbounded).

**2. Placeholder scan.** No "TBD", no "add appropriate error handling", no
"similar to Task N". Every code step carries the code. Two places deliberately
tell the implementer to **check before writing** rather than giving one answer —
`FrameSource::has_frame` in Task 1 Step 1, and `TransportConfig`'s `Debug` output
in Task 2 Step 1 — because in both cases the plan cannot know the answer without
running the code, and guessing is how the last plan shipped a test that called a
two-argument helper with no arguments. Each says what to do in either case.

**3. Type consistency.** `route_source(SocketAddr) -> io::Result<IpAddr>`,
`RouteWatch::{new, moved, settle}`, `ROUTE_PROBE_EVERY`, `DETACH_AFTER`,
`HostSession::turn_at(Instant, Option<Frame>)`, `ClientSession::follow_route(Instant) -> bool`,
`LinkState::phase_now() -> Phase`, `ClientSession::{route, probed_at}` — each is
defined in exactly one task and used with the same name and arity everywhere
after. `phase_now` is a rename of the `#[cfg(test)]` `phase`, so Task 5 Step 4
says to update `linkstate.rs`'s own tests.

**4. What this plan is most likely to have got wrong.** Named rather than
smoothed over, because the previous plan's defects all read fine:

- **Whether the rebind is what recovers a roamed session at all.** A
  wildcard-bound socket lets the kernel choose a source address per packet, so a
  route change may recover with no help from us. Task 5's background says not to
  claim otherwise and Task 6 Step 4 asks for the observation either way. If it
  turns out the rebind is unnecessary, the probe is still the right trigger and
  phase 3 needs to know.
- **`DETACH_AFTER` interacting with a host loop that may not be awake.** A
  detached host with a quiet child sleeps on descriptors and never re-evaluates
  `attached`. That is harmless — nothing is being burned while nothing is
  happening, and the next pty write wakes it and recomputes — but it means
  `turn.detached` can read stale for an arbitrary time on an idle session. No
  test asserts a bound on that, deliberately; asserting one would be asserting a
  scheduler.
- **The 35 s composed test on a loaded runner.** The handoff already flags the
  existing 12 s negative assertion as a flake risk. This one is longer and
  positive, which is safer, but Task 6 Step 2 offers `#[ignore]` and says what
  must not be traded away to make it faster.

# P1 — Standby link — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** While a session's link is up, the client builds a second, warm QUIC connection to the same session over a path that leaves the machine through a different local address. When the primary goes silent and the standby still answers, the client switches to the standby in about three seconds, without ssh.

**Architecture:** After `Established`, each end serves a *control stream* on every link: a QUIC bidirectional stream carrying ordinary `Signal` lines. `StandbyRequest` on the primary's control stream runs the host's unchanged `run_attach_exchange` over that stream, fed through the same serial door the Unix-socket listener uses. The result is an `Attached` with `role: Standby`, which the host parks instead of adopting. The client's search runs the unchanged `establish`, with one new input: a filter that forbids ICE from nominating any remote the kernel would reach from the primary's source address. Failover uses `Probe`/`ProbeAck` on the standby's own control stream, then the existing `swap_in`. The host adopts the standby when its first sync frame arrives.

**Tech Stack:** Rust, edition 2024, MSRV 1.96. `tokio`, `quinn` (bidirectional streams), `serde` (signal JSON), `anyhow`.

**Spec:** `docs/superpowers/specs/2026-09-25-standby-and-rendezvous-design.md`, §1–§3, §6–§10 (P1 only; §4–§5 are P2 and not in this plan). Read it before Task 1. Where the plan and the spec disagree, the spec wins, except for the deviations listed at the end of this header. Background: `docs/superpowers/specs/2026-08-29-session-recovery-design.md` §2, §5, §6, and `docs/superpowers/specs/2026-09-07-tier-b2-client-reattach-design.md` §5.

**Baseline:** `make check` on `main` at `6049aee`. It was not run while this plan was written. Run it first; if it fails, stop and report before Task 1.

**Deviations from the spec, decided while planning:**

1. **The egress test compares source addresses, not interface names** (spec §3.3). `crate::roam::route_source` already asks the kernel which local address it would use for a peer, sending nothing, so this needs no `netdev` lookup. Different source addresses do not guarantee different interfaces: a dual-stack or multi-address NIC yields two source addresses over the same wire, so a standby may share the primary's interface (ruling S6; named as a known limit in `CHANGES.md` and the README).
2. **The failover clock** (spec §3.5). The probe is sent on the first lap in `Silent` (2 s after a reply became owed). Failover happens `FAILOVER_GRACE` (1 s) after the probe was *sent*, provided it was answered. That is 3 s after the reply became owed, the spec's `FAILOVER_AFTER`, without a second clock.
3. **The netns end-to-end test of spec §7 is not in this plan.** The netns harness (`crates/oxutrm-net/tests/netns.rs`) lives in `oxutrm-net` and cannot reach the root crate's session code. A two-uplink topology driving the real binary is its own piece of work. This plan proves the failover with the in-process `Relay` fixture, which really drops packets, plus a hand test on `thinlinc` (Task 10). The netns test is recorded as follow-up work in `CHANGES.md`.

**Deviations accepted during execution (2026-10-03):**

(a) **One stream per control conversation, not spec §2's single control
stream.** `request_standby` and `probe` (`src/control.rs`) each `open_bi()`
their own fresh bidirectional stream rather than sharing one persistent
control stream per link — simpler than multiplexing conversations onto one
stream, and QUIC streams are cheap.

(b) **The remote filter is enforced at nomination, not before `add_remote`.**
`IceAgent::add_remote` (`crates/oxutrm-net/src/ice.rs:148`) admits every
candidate unfiltered; `remote_filter` is applied only in `best_validated`
(`ice.rs:357`), when a candidate is about to be nominated — so a filtered
candidate is still tracked and validated, just never chosen.

(c) **Probe retry, and failover while `Recovering` without a rebuild attempt in
flight, beyond the spec's `Silent`-only failover (§3.5).** `Standby::step`
(`src/standby.rs`) re-probes after `PROBE_RETRY` on a failed probe rather than
asking once, and `failover_due` (`src/linkstate.rs`) fires on
`Phase::is_outage()` — `Silent` or `Recovering` — gated by the caller on no
rebuild attempt being in flight (ruling B1), rather than `Silent` alone.

(d) **The host promotes the standby through `promote_standby`, adopting and
feeding the first frame in one call.** `ClientSession`'s host-side counterpart
(`src/session.rs:723`) resets the sequence counters and processes the
triggering frame together, rather than as two separately observable steps.

(e) **A superseded parked standby closes with `SUPERSEDED`, not `SWITCHED`.**
`SWITCHED` (`src/session.rs:837`) means a client's own promoted standby;
`SUPERSEDED` (`src/session.rs:841`) means a newer standby replaced a parked
one that was never used — the host tells the two apart (`session.rs:706`) so a
displaced standby's connection is not misread as a failover that happened.

## Global Constraints

Every task's requirements include these.

- **Reattachment is not a second code path** (`crates/oxutrm-host/src/attach.rs:3`). The standby runs `run_attach_exchange` and `establish` unchanged. Only the pipes differ.
- **`oxutrm-host` MUST NOT depend on `oxutrm-net`.**
- **`src/main.rs` is `#![forbid(unsafe_code)]`.**
- **The loops' `select!` arms borrow locals, never `self`** (constraint C1). Every new receiver or watched connection in `run_with_attaches` and `run_on` is a local.
- **A rejected frame must never disconnect, and a send failure must never end a session.** Standby failures are never fatal to the session.
- **Both ends reset sequence counters at every attach, and the host's first datagram is a full state** (design spec §8.5). Failover is an attach and follows this rule.
- **Keys are fresh per attach and never on disk.** The standby gets its own `begin_attach` generation, PSK and certificate.
- **Idle CPU (`19cc001`):** no new timer that fires while nothing is happening. New waits are channel or connection events. The standby step runs on laps the loop already takes.
- **Never a bare `cargo test`.** `make check` is the gate: fmt check, clippy with `-D warnings`, tests, parallelism capped at 4. For a single test: `cargo test --workspace --jobs 4 <name> -- --test-threads 4`.
- **A changelog entry is part of the work** (`CHANGES.md`, under `## Unreleased`).
- **No `#[allow(dead_code)]` for something not wired up yet.** A task that adds an item used only by a later task marks it `#[cfg_attr(not(test), allow(dead_code))]` **with a comment naming the task that wires it**, and that task removes the attribute. A test-only helper is `#[cfg(test)]`.
- **Tests use a STUN-free `NetConfig`** (`stun_servers: vec![]`, `enable_port_mapping: false`, `enable_birthday: false`).
- English for all comments, identifiers and documentation.

## Test discipline — read before writing any test

The project's two recurring defects are **a guard that cannot fail** and **a comment that outlives its truth**. So:

- **Every task ends with an injection step:** break the thing deliberately, watch the new test fail, then put it back. A test that has never been seen to fail is not a guard.
- When a test asserts a return value, ask what else produces that value, and assert the side effect instead.
- Prefer a completed observation (a message read off a channel, a close reason) over a `sleep`.

## Review Focus

Five inputs the spec implies but that are easy to leave untested. Each has a test in the task named.

1. **A standby whose only validated pair is the primary's own path** (no VPN, a plain direct connection): the search must fail with "no standby path", not build a second connection over the same wire. *Task 3: `a_filtered_remote_is_never_nominated_even_when_it_validates`, which also covers the same address coming back as a peer-reflexive candidate. Task 8: `a_standby_search_honours_the_filter`.*
2. **The primary recovers while the probe is in flight**, a blip shorter than the grace: no failover, the probe state is cleared, and the standby stays parked. *Task 7: `a_frame_before_the_grace_ends_cancels_the_failover`. Task 8: `step_never_fails_over_on_an_unanswered_probe`.*
3. **An ssh attach by another client while a standby is parked:** the host must drop the standby too, or the displaced client could take the session back through it silently. *Task 6: `a_primary_attach_drops_the_parked_standby`.*
4. **The standby connection dies on its own** (NAT mapping expired): the client forgets it and searches again after backoff, and the next outage does not try to fail over onto a corpse. *Task 8: `a_standby_that_closes_is_forgotten_and_searched_for_again`.*
5. **An old host without the feature:** the client must never open a control stream and must behave exactly as B2. *Task 1: `a_hello_without_features_parses_as_none`. Task 8: `no_standby_is_searched_for_when_the_host_did_not_offer_one`.*

## File Structure

| File | Responsibility |
|---|---|
| `crates/oxutrm-proto/src/signal.rs` | Modified. `features` on both hellos; `StandbyRequest`, `Probe`, `ProbeAck`; `FEATURE_*` constants. |
| `crates/oxutrm-net/src/ice.rs` | Modified. `RemoteFilter`, `IceAgent::set_remote_filter`, enforced in `best_validated`. |
| `crates/oxutrm-net/src/lib.rs` | Modified. Re-export `RemoteFilter`. |
| `src/ladder.rs` | Modified. `Ladder::admit_remote`; installed on the agent; checked once more on every nomination (covers the birthday blast). |
| `src/egress.rs` | **New.** `avoiding_source`: the filter that keeps a standby off the primary's path. |
| `src/control.rs` | **New.** `Role`, `DoorRequest`, `serve_control` (the host's control-stream server), and the client's `request_standby` / `probe`. |
| `src/listener.rs` | Modified. The door takes Unix connections *and* control-stream requests, serially; each resulting link gets a control server. |
| `src/attach_exchange.rs` | Modified. `Attached::role`; features in the hello; a `#[cfg(test)] pub(crate) mod fixtures`. |
| `src/connect.rs` | Modified. `establish` takes `admit_remote`; `Established::host_features`; the session is given a standby when the host offers one. |
| `src/rebuild.rs` | Modified. Passes `None` for `admit_remote`. |
| `src/serve.rs` | Modified. Creates the door channel, and a control server for the first link. |
| `src/linkstate.rs` | Modified. `ProbeState`, `FAILOVER_GRACE`, `PROBE_TIMEOUT`, `PROBE_RETRY`, `STANDBY_DELAY`, `standby_backoff`, `failover_due`. |
| `src/standby.rs` | **New.** `Standby`, the client's standby state and its step function. Separate from `session.rs`, which is 5,700 lines and owns a different concern. |
| `src/session.rs` | Modified. Host: parked standby, adopt on first frame, `SWITCHED`. Client: standby arms, failover, announcement. |
| `src/main.rs` | Modified. `mod control; mod egress; mod standby;`. |
| `README.md`, `CHANGES.md` | Modified. |

---

### Task 1: The protocol additions

**Files:**
- Modify: `crates/oxutrm-proto/src/signal.rs` (the `Signal` enum at :74; tests at the bottom)
- Modify: every construction site of `Signal::HostHello` / `Signal::ClientHello`. Today these are `src/attach_exchange.rs:261` and `src/connect.rs:~300` in production code, plus test literals in `signal.rs`, `src/attach_exchange.rs` and `src/connect.rs`. Find them with `grep -rn "Signal::HostHello {\|Signal::ClientHello {" src crates`.

**Interfaces:**
- Produces:
  - `Signal::HostHello { .., features: Vec<String> }` and `Signal::ClientHello { .., features: Vec<String> }`, both `#[serde(default)]`.
  - `Signal::StandbyRequest`, `Signal::Probe { nonce: u64 }` and `Signal::ProbeAck { nonce: u64 }`.
  - `pub const FEATURE_CONTROL: &str = "control"; pub const FEATURE_STANDBY: &str = "standby";`, re-exported from `oxutrm_proto`.

- [ ] **Step 1: Write the failing tests** in `signal.rs`'s test module:

```rust
#[test]
fn a_hello_without_features_parses_as_none() {
    // A host from before P1 sends no `features` key at all. Built by removing
    // the key from a current hello, so the test cannot drift from the real
    // field set.
    let hello = Signal::HostHello {
        proto: PROTO_VERSION,
        session_id: "00112233445566778899aabbccddeeff".to_string(),
        attach_id: 1,
        cert_spki_sha256: HostSpki::new([7; 32]),
        psk: Psk::new([9; 32]),
        candidates: vec![],
        nat_type: NatType::Unknown,
        bound_port: 443,
        detachable: true,
        features: vec![FEATURE_STANDBY.to_string()],
    };
    let mut value = serde_json::to_value(&hello).unwrap();
    value.as_object_mut().unwrap().remove("features").expect("the field is on the wire");
    let old: Signal = serde_json::from_value(value).unwrap();
    match old {
        Signal::HostHello { features, .. } => assert!(features.is_empty()),
        other => panic!("parsed as {other:?}"),
    }
}

#[test]
fn the_standby_signals_round_trip() {
    for s in [
        Signal::StandbyRequest,
        Signal::Probe { nonce: 42 },
        Signal::ProbeAck { nonce: 42 },
    ] {
        let mut line = Vec::new();
        write_signal(&mut line, &s).unwrap();
        let back = read_signal(&mut &line[..]).unwrap();
        assert_eq!(format!("{back:?}"), format!("{s:?}"));
    }
}
```

Use the constructors the existing tests in this file use for `HostSpki`, `Psk` and `NatType`. If `HostSpki::new` takes a different argument, copy the construction from the test at `signal.rs:287`. The same goes for `write_signal`/`read_signal` (`:197`/`:218`): match their real signatures.

- [ ] **Step 2: Run the tests and confirm they fail to compile.** Run `cargo test --workspace --jobs 4 -p oxutrm-proto signal -- --test-threads 4`. Expected: compile errors about the unknown field `features` and variant `StandbyRequest`.

- [ ] **Step 3: Implement.** In the `Signal` enum:

```rust
    HostHello {
        // ...existing fields unchanged...
        /// What this host can do beyond the base exchange. Absent on a host
        /// from before P1, which is what `default` is for: an old peer omits
        /// it, a new peer reads that as "nothing", and no `PROTO_VERSION`
        /// bump is needed (spec §2.1).
        #[serde(default)]
        features: Vec<String>,
    },
    // ...
    ClientHello {
        // ...existing fields unchanged...
        #[serde(default)]
        features: Vec<String>,
    },
    // ...
    /// client -> host, first line on a control stream: run an attach exchange
    /// over this stream and park the result as a standby (spec §3.2).
    StandbyRequest,
    /// client -> host on a standby's control stream. Answered without
    /// adopting anything: a sync frame is what adopts (spec §3.4).
    Probe { nonce: u64 },
    ProbeAck { nonce: u64 },
```

Add near `PROTO_VERSION`:

```rust
/// The host serves a control stream on every link (spec §2).
pub const FEATURE_CONTROL: &str = "control";
/// The host parks a standby link on request (spec §3).
pub const FEATURE_STANDBY: &str = "standby";
```

Fix every construction site. Production hellos:
- `host_hello` in `src/attach_exchange.rs` gets `features: vec![FEATURE_CONTROL.to_string(), FEATURE_STANDBY.to_string()]`.
- The `ClientHello` in `establish` gets `features: vec![]`. The client offers nothing; the host decides.
- Test literals get `features: vec![]`.

- [ ] **Step 4: Run the proto tests again and confirm they pass.** Then run `make check`; expected: exit 0.

- [ ] **Step 5: Injection.** Remove `#[serde(default)]` from `HostHello::features` and run `a_hello_without_features_parses_as_none`. It must FAIL with a missing-field error. Restore it.

- [ ] **Step 6: Commit**

```bash
git add crates/oxutrm-proto/src/signal.rs crates/oxutrm-proto/src/lib.rs src/attach_exchange.rs src/connect.rs
git commit -m "feat(proto): hellos say what they can do, and three signals for a standby"
```

---

### Task 2: The client learns what the host offers

**Files:**
- Modify: `src/connect.rs` (`HostFacts` :458, `host_facts` :495, `Established` :245, `establish` :272, tests)

**Interfaces:**
- Consumes: `Signal::HostHello::features` (Task 1).
- Produces: `Established { .., host_features: Vec<String> }`.

- [ ] **Step 1: Write the failing test** in `connect.rs`'s tests, next to `the_offer_yields_the_key_material_and_the_peers_candidates` (:784):

```rust
#[test]
fn the_offer_carries_the_hosts_features_through() {
    let mut hello = a_host_hello();
    if let Signal::HostHello { features, .. } = &mut hello {
        *features = vec![oxutrm_proto::FEATURE_STANDBY.to_string()];
    }
    let facts = host_facts(hello).unwrap();
    assert_eq!(facts.features, vec![oxutrm_proto::FEATURE_STANDBY.to_string()]);
}
```

Then extend the existing duplex test `a_client_and_a_host_complete_an_exchange_over_a_pipe` (:655) with one assertion after it gets its `Established`:

```rust
assert!(
    client.host_features.iter().any(|f| f == oxutrm_proto::FEATURE_STANDBY),
    "a current host offers a standby: {:?}",
    client.host_features
);
```

(Use whatever the test names its `Established` binding.)

- [ ] **Step 2: Run the two tests and confirm they fail** with `no field features on HostFacts` / `no field host_features`.

- [ ] **Step 3: Implement.** Add `features: Vec<String>` to `HostFacts` and include it in its hand-written `Debug`; it is not secret. Destructure it in `host_facts`. Add `pub host_features: Vec<String>` to `Established`, with a doc comment: "What the host said it can do. The session searches for a standby only when this contains `FEATURE_STANDBY`." Fill it from `host.features` in `establish`.

- [ ] **Step 4: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 5: Injection.** In `establish`, set `host_features: vec![]`. The duplex test must fail. Restore it.

- [ ] **Step 6: Commit**

```bash
git add src/connect.rs
git commit -m "feat(client): keep what the host said it can do"
```

---

### Task 3: ICE can be told which remotes it may not nominate

**Files:**
- Modify: `crates/oxutrm-net/src/ice.rs` (`IceAgent` :84, `best_validated` :332, tests)
- Modify: `crates/oxutrm-net/src/lib.rs` (re-export)
- Modify: `src/ladder.rs` (`Ladder` :231, `race` ~:405, `nominate` :289)
- Modify: `src/connect.rs` (`establish` signature), `src/attach_exchange.rs` (Ladder literal), `src/rebuild.rs` (the `establish` call ~:188), and test call sites of `establish` / `Ladder { .. }`

**Interfaces:**
- Produces:
  - `pub type RemoteFilter = std::sync::Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;` in `oxutrm_net`.
  - `IceAgent::set_remote_filter(&mut self, f: RemoteFilter)`.
  - `Ladder { .., pub admit_remote: Option<RemoteFilter> }`.
  - `establish(reader, writer, size, cfg, admit_remote: Option<RemoteFilter>)`.

- [ ] **Step 1: Write the failing tests** in `ice.rs`'s test module. They reuse `sock`, `cfg`, `host_candidate`, `PSK` and `nominated` from that module.

```rust
/// Runs one controlling agent with `filter` against a plain controlled one,
/// and returns what the controlling side reported.
async fn run_filtered(filter: RemoteFilter) -> (Vec<IceEvent>, SocketAddr) {
    let cs = sock().await;
    let hs = sock().await;
    let ca = cs.local_addr().unwrap();
    let ha = hs.local_addr().unwrap();

    let mut client = IceAgent::new(&PSK, IceRole::Controlling, cfg(1500));
    client.add_local(host_candidate(ca));
    client.add_remote(host_candidate(ha));
    client.set_remote_filter(filter);

    let mut host = IceAgent::new(&PSK, IceRole::Controlled, cfg(1500));
    host.add_local(host_candidate(ha));
    host.add_remote(host_candidate(ca));

    let c = tokio::spawn(async move {
        let mut out = Vec::new();
        for _ in 0..8 {
            let ev = client.run(cs.clone()).await;
            let stop = matches!(ev, IceEvent::Nominated { .. } | IceEvent::Failed(_));
            out.push(ev);
            if stop {
                break;
            }
        }
        out
    });
    // The host keeps checking us the whole time, so `ha` also arrives on the
    // client as a peer-reflexive source. The filter must hold against that
    // route in, not only against the offered candidate.
    let h = tokio::spawn(async move {
        for _ in 0..8 {
            if matches!(host.run(hs.clone()).await, IceEvent::Nominated { .. } | IceEvent::Failed(_)) {
                break;
            }
        }
    });
    let out = c.await.unwrap();
    h.abort();
    (out, ha)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_filtered_remote_is_never_nominated_even_when_it_validates() {
    let (events, ha) = {
        // The filter needs `ha`, which does not exist until the sockets do,
        // so it forbids every address; there is only one.
        run_filtered(Arc::new(|_| false)).await
    };
    assert!(
        nominated(&events).is_none(),
        "nominated a forbidden remote {ha}: {events:?}"
    );
    assert!(
        matches!(events.last(), Some(IceEvent::Failed(_))),
        "must fail within the budget: {events:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admitted_remote_is_nominated_as_before() {
    let (events, ha) = run_filtered(Arc::new(|_| true)).await;
    let (_, remote, _, _) = nominated(&events).expect("nominated");
    assert_eq!(remote, ha);
}
```

- [ ] **Step 2: Run the tests and confirm they fail to compile** (`set_remote_filter` not found).

- [ ] **Step 3: Implement** in `ice.rs`:

```rust
/// A veto over which remote addresses may be nominated.
///
/// Checked at the one place a nomination is decided, `best_validated`, and
/// nowhere else. That covers offered, updated and peer-reflexive remotes
/// alike, and it deliberately leaves checks and responses alone: the peer
/// still needs our answers to validate ITS side, and a pair we will not
/// nominate costs a few probes and nothing more.
pub type RemoteFilter = std::sync::Arc<dyn Fn(SocketAddr) -> bool + Send + Sync>;
```

Add a field `remote_filter: Option<RemoteFilter>` to `IceAgent`, initialised to `None` in `new`, and a setter:

```rust
    /// Forbid nominating any remote for which `f` returns false.
    pub fn set_remote_filter(&mut self, f: RemoteFilter) {
        self.remote_filter = Some(f);
    }
```

In `best_validated`, add `.filter(|(a, _)| self.remote_filter.as_ref().is_none_or(|f| f(**a)))` after the `validated()` filter.

Re-export `RemoteFilter` from `crates/oxutrm-net/src/lib.rs`, next to `IceAgent`.

In `src/ladder.rs`, add to `Ladder`:

```rust
    /// Remotes this ladder may not nominate, or `None` for any. Only a
    /// standby search sets it (spec §3.3).
    pub admit_remote: Option<oxutrm_net::RemoteFilter>,
```

In `race`, after the `add_remote` loop, add `if let Some(f) = &ladder.admit_remote { agent.set_remote_filter(f.clone()); }`.

The birthday rung nominates without the agent's `best_validated`, so `nominate` also needs a second check. Wherever it has produced a `Nomination` and is about to return it `Ok`, including the blast, first check:

```rust
if let Some(f) = &ladder.admit_remote
    && !f(n.remote)
{
    report.set(rung, Verdict::Failed(format!(
        "{} is reached the same way as the link this is standing by for",
        n.remote
    )));
    // fall through to the next rung, exactly as a failed rung does
}
```

Adapt this to however `nominate` records a failed rung. Read `nominate`'s body and use its own `Verdict` for a tried-and-failed rung. The structure is: a forbidden nomination is a failed rung, not an `Err` return.

Add the parameter `admit_remote: Option<oxutrm_net::RemoteFilter>` as the last argument of `establish` and pass it into its `Ladder`. Pass `None` in:
- the `Ladder` literal in `run_attach_exchange` (the host never filters: the client is Controlling and the only side that nominates);
- `connect`'s call;
- `rebuild.rs`'s call;
- every test call site.

- [ ] **Step 4: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 5: Injection.** Remove the `.filter(...)` line from `best_validated`. `a_filtered_remote_is_never_nominated_even_when_it_validates` must FAIL. Restore it.

- [ ] **Step 6: Commit**

```bash
git add crates/oxutrm-net/src/ice.rs crates/oxutrm-net/src/lib.rs src/ladder.rs src/connect.rs src/attach_exchange.rs src/rebuild.rs
git commit -m "feat(net): a ladder can be told which remotes it may not nominate"
```

---

### Task 4: The egress filter

**Files:**
- Create: `src/egress.rs`
- Modify: `src/main.rs` (`mod egress;`)

**Interfaces:**
- Consumes: `crate::roam::route_source(peer: SocketAddr) -> std::io::Result<IpAddr>` (`src/roam.rs:70`); `oxutrm_net::RemoteFilter` (Task 3).
- Produces: `pub(crate) fn avoiding_source(primary_remote: SocketAddr) -> Option<RemoteFilter>` and the pure `fn admits(baseline: IpAddr, seen: std::io::Result<IpAddr>) -> bool`.

- [ ] **Step 1: Write the failing tests** in `src/egress.rs`:

```rust
//! Keeping a standby off the primary's path (spec §3.3).
//!
//! A standby exists to survive what kills the primary, so a standby that
//! leaves this machine the same way the primary does is worthless: it is the
//! same wire with a second connection on it. "The same way" is decided by
//! the kernel's own answer to "which local address would you send this from",
//! which `route_source` asks without sending anything. Two routes leave
//! through different interfaces exactly when that answer differs, so no
//! interface table is consulted.

use std::net::{IpAddr, SocketAddr};

use oxutrm_net::RemoteFilter;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_reached_from_the_primarys_source_is_refused() {
        let primary: IpAddr = "10.8.0.2".parse().unwrap();
        assert!(!admits(primary, Ok(primary)));
    }

    #[test]
    fn a_remote_reached_from_another_source_is_admitted() {
        let primary: IpAddr = "10.8.0.2".parse().unwrap();
        let other: IpAddr = "192.168.1.20".parse().unwrap();
        assert!(admits(primary, Ok(other)));
    }

    #[test]
    fn an_unroutable_remote_is_admitted_because_it_cannot_validate_anyway() {
        let primary: IpAddr = "10.8.0.2".parse().unwrap();
        let err = std::io::Error::from(std::io::ErrorKind::NetworkUnreachable);
        assert!(admits(primary, Err(err)));
    }

    /// The whole function on this machine's real routing table: loopback is
    /// reached from loopback, so a loopback primary refuses a loopback remote.
    #[test]
    fn on_loopback_the_filter_refuses_loopback() {
        let f = avoiding_source("127.0.0.1:4433".parse().unwrap()).expect("loopback routes");
        assert!(!f("127.0.0.1:9".parse().unwrap()));
    }
}
```

- [ ] **Step 2: Run the tests and confirm they fail to compile.** Run `cargo test --workspace --jobs 4 egress -- --test-threads 4`.

- [ ] **Step 3: Implement** above the tests:

```rust
/// A filter admitting only remotes this machine would reach from a different
/// local address than it reaches `primary_remote` from.
///
/// `None` when the primary's own route cannot be read. A standby search is
/// then pointless rather than dangerous (it could end up on the primary's own
/// path), so the caller skips it and says there is no standby.
pub(crate) fn avoiding_source(primary_remote: SocketAddr) -> Option<RemoteFilter> {
    let baseline = crate::roam::route_source(primary_remote).ok()?;
    Some(std::sync::Arc::new(move |remote| {
        admits(baseline, crate::roam::route_source(remote))
    }))
}

/// The decision, without the kernel.
///
/// An unroutable remote is admitted. ICE will not validate it, so admitting
/// it costs a few probes, while refusing it would need a second opinion on
/// routes the kernel has already declined to give.
fn admits(baseline: IpAddr, seen: std::io::Result<IpAddr>) -> bool {
    seen.map_or(true, |ip| ip != baseline)
}
```

Add `mod egress;` to `src/main.rs`. `avoiding_source` is unused until Task 8: mark it `#[cfg_attr(not(test), allow(dead_code))] // wired by Task 8`.

- [ ] **Step 4: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 5: Injection.** Change `ip != baseline` to `ip == baseline`. The first two tests must fail. Restore it.

- [ ] **Step 6: Commit**

```bash
git add src/egress.rs src/main.rs
git commit -m "feat(client): tell whether a remote is reached the primary's way"
```

---

### Task 5: The host's control stream and the shared door

**Files:**
- Create: `src/control.rs`
- Modify: `src/listener.rs` (`serve_attaches` :48, its tests)
- Modify: `src/attach_exchange.rs` (`Attached` :33; add a test-fixtures module)
- Modify: `src/serve.rs` (:136–:175)
- Modify: `src/main.rs` (`mod control;`)

**Interfaces:**
- Consumes: `run_attach_exchange` (unchanged); `Signal::StandbyRequest` / `Probe` / `ProbeAck` (Task 1); `read_signal_async` / `write_signal_async` (`oxutrm_host::signalling`).
- Produces:
  - `pub(crate) enum Role { Primary, Standby }` (Copy, Eq, Debug).
  - `pub(crate) struct DoorRequest { pub reader: Box<dyn AsyncBufRead + Unpin + Send>, pub writer: Box<dyn AsyncWrite + Unpin + Send>, pub role: Role }`.
  - `pub(crate) fn serve_control(conn: quinn::Connection, door: mpsc::Sender<DoorRequest>) -> tokio::task::JoinHandle<()>`.
  - `Attached { .., pub role: Role }`.
  - `serve_attaches(listener, guard, meta, cfg, attach_timeout, doors: mpsc::Receiver<DoorRequest>, door_tx: mpsc::Sender<DoorRequest>, tx)`.
  - The client half, `request_standby` and `probe`, is added to this file in Task 8.

- [ ] **Step 1: Move the exchange fixtures where other modules can use them.** In `src/attach_exchange.rs`, move `fresh_meta` (:329) and `stun_free` (:350) out of `mod tests` into:

```rust
/// Fixtures more than one module's tests need. The listener has its own
/// copies from before this existed; they are left alone.
#[cfg(test)]
pub(crate) mod fixtures {
    // `fresh_meta` and `stun_free`, moved here verbatim and made `pub(crate)`.
}
```

Add `use super::fixtures::*;` in `mod tests`. Run `make check`; expected: exit 0, no behaviour change.

- [ ] **Step 2: Write the failing tests** in a `#[cfg(test)] mod tests` of `src/control.rs`. A loopback QUIC pair is built the way `session.rs`'s `pair_on` builds one (`session.rs:2140`). Copy that construction into a local `async fn quic_pair() -> (quinn::Connection, quinn::Connection, Vec<quinn::Endpoint>)` returning (host conn, client conn, endpoints to keep alive).

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_probe_is_answered_with_its_own_nonce() {
    let (host, client, _eps) = quic_pair().await;
    let (door_tx, _door_rx) = tokio::sync::mpsc::channel(1);
    let _server = serve_control(host, door_tx);

    let (mut send, recv) = client.open_bi().await.unwrap();
    write_signal_async(&mut send, &Signal::Probe { nonce: 7 }).await.unwrap();
    let mut recv = tokio::io::BufReader::new(recv);
    let back = read_signal_async(&mut recv).await.unwrap();
    assert!(matches!(back, Signal::ProbeAck { nonce: 7 }), "{back:?}");

    // The same stream keeps answering, so one probe stream can serve an outage.
    write_signal_async(&mut send, &Signal::Probe { nonce: 8 }).await.unwrap();
    let back = read_signal_async(&mut recv).await.unwrap();
    assert!(matches!(back, Signal::ProbeAck { nonce: 8 }), "{back:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_request_is_handed_to_the_door_with_its_stream() {
    let (host, client, _eps) = quic_pair().await;
    let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
    let _server = serve_control(host, door_tx);

    let (mut send, _recv) = client.open_bi().await.unwrap();
    write_signal_async(&mut send, &Signal::StandbyRequest).await.unwrap();
    // Something after the request must reach whoever the door hands the
    // stream to: the exchange reads the client's hello from it.
    write_signal_async(&mut send, &Signal::Probe { nonce: 1 }).await.unwrap();

    let mut req = door_rx.recv().await.expect("the door was knocked on");
    assert_eq!(req.role, Role::Standby);
    let next = read_signal_async(&mut req.reader).await.unwrap();
    assert!(matches!(next, Signal::Probe { nonce: 1 }), "{next:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn anything_else_first_is_dropped_without_reaching_the_door() {
    let (host, client, _eps) = quic_pair().await;
    let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
    let _server = serve_control(host, door_tx);

    let (mut send, _recv) = client.open_bi().await.unwrap();
    write_signal_async(&mut send, &Signal::Choose { choice: oxutrm_proto::Choice::New })
        .await
        .unwrap();
    send.finish().unwrap();
    let knocked = tokio::time::timeout(std::time::Duration::from_millis(500), door_rx.recv()).await;
    assert!(knocked.is_err(), "the door was knocked on: a stray line can start an attach");
}
```

In `src/listener.rs` tests, add the door counterpart. Use the listener's own fixtures (`fresh_meta`, `stun_free`) and follow how the existing tests there start `serve_attaches` (read `an_abandoned_attempt_sends_nothing_and_leaves_the_door_open`, :224):

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_door_request_runs_the_same_exchange_and_carries_its_role() {
    // Start serve_attaches exactly as the existing tests do, but keep
    // `door_tx` and hand it a request built from a tokio duplex pair.
    let (client_side, host_side) = tokio::io::duplex(64 * 1024);
    let (hr, hw) = tokio::io::split(host_side);
    door_tx
        .send(DoorRequest {
            reader: Box::new(tokio::io::BufReader::new(hr)),
            writer: Box::new(hw),
            role: Role::Standby,
        })
        .await
        .unwrap();
    // The host speaks first over the door exactly as over the Unix socket.
    let (cr, _cw) = tokio::io::split(client_side);
    let mut cr = tokio::io::BufReader::new(cr);
    let hello = read_signal_async(&mut cr).await.unwrap();
    assert!(matches!(hello, oxutrm_proto::Signal::HostHello { .. }), "{hello:?}");
}
```

The full role round trip (an `Attached` with `role: Standby` coming out of `tx`) is asserted end to end in Task 8, where a real client drives the exchange to completion.

- [ ] **Step 3: Run the tests and confirm they fail to compile.**

- [ ] **Step 4: Implement `src/control.rs`:**

```rust
//! The control stream: a QUIC bidirectional stream on a live link, carrying
//! ordinary `Signal` lines (spec §2).
//!
//! After `Established`, ssh is gone (`serve.rs`'s sever), and this is the only
//! channel between the two ends. It is at least as well authenticated as ssh:
//! both ends of the connection it rides on are pinned by SPKI.
//!
//! Each stream is one conversation, and its first line says which:
//! `StandbyRequest` hands the stream to the session's door, where the ordinary
//! attach exchange runs over it; `Probe` is answered in place, for as long as
//! the stream stays open. Anything else is dropped.

use oxutrm_host::signalling::{read_signal_async, write_signal_async};
use oxutrm_proto::Signal;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// What a completed attach is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Adopt now; newest attach wins (recovery spec §6).
    Primary,
    /// Park until the client's first sync frame on it (spec §3.5).
    Standby,
}

/// A pair of pipes for the door, and what the attach that runs over them is for.
pub(crate) struct DoorRequest {
    pub reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub role: Role,
}

/// Serve control streams on `conn` until it closes.
///
/// Ends by itself when the connection does, because `accept_bi` then fails.
/// Nothing needs to stop it: every link the host drops is closed first.
pub(crate) fn serve_control(
    conn: quinn::Connection,
    door: tokio::sync::mpsc::Sender<DoorRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((send, recv)) = conn.accept_bi().await {
            tokio::spawn(one_stream(send, recv, door.clone()));
        }
    })
}

async fn one_stream(
    mut send: quinn::SendStream,
    recv: quinn::RecvStream,
    door: tokio::sync::mpsc::Sender<DoorRequest>,
) {
    let mut reader = tokio::io::BufReader::new(recv);
    let Ok(first) = read_signal_async(&mut reader).await else {
        return;
    };
    match first {
        Signal::StandbyRequest => {
            // A full door, or no door at all (a session that never bound its
            // socket), drops the stream. The client's search then fails and
            // backs off, which is the whole of the damage.
            let _ = door
                .send(DoorRequest {
                    reader: Box::new(reader),
                    writer: Box::new(send),
                    role: Role::Standby,
                })
                .await;
        }
        Signal::Probe { nonce } => {
            let mut nonce = nonce;
            loop {
                if write_signal_async(&mut send, &Signal::ProbeAck { nonce }).await.is_err() {
                    return;
                }
                match read_signal_async(&mut reader).await {
                    Ok(Signal::Probe { nonce: next }) => nonce = next,
                    _ => return,
                }
            }
        }
        _ => {}
    }
}
```

Match the real module path of `read_signal_async`/`write_signal_async`; they live in `crates/oxutrm-host/src/signalling.rs:42,88`. Check how `attach_exchange.rs` imports them and do the same.

Add `pub role: Role` to `Attached`, set to `Role::Primary` at the end of `run_attach_exchange`. The listener then overwrites it with the request's role. `run_attach_exchange` does not know its caller's intent and must not need to.

Change `serve_attaches` to take two more arguments, `doors: tokio::sync::mpsc::Receiver<DoorRequest>` and `door_tx: tokio::sync::mpsc::Sender<DoorRequest>`, and replace its accept with:

```rust
        // Two ways in, one door. The Unix socket brings ssh-relayed attaches
        // (primary: newest attach wins); a control stream brings a standby.
        // Both are served here, serially, under the one meta lock, for the
        // reason the comment below gives: two concurrent exchanges would both
        // bump the generation and race to hand the session a link.
        let (reader, writer, role): (
            Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
            Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
            Role,
        ) = tokio::select! {
            r = listener.accept() => match r {
                Ok((s, _)) => {
                    let (r, w) = s.into_split();
                    (Box::new(tokio::io::BufReader::new(r)), Box::new(w), Role::Primary)
                }
                Err(_) => continue,
            },
            Some(d) = doors.recv() => (d.reader, d.writer, d.role),
        };
```

Pass `reader`/`writer` to `run_attach_exchange` in place of the old pair. After a successful exchange, before `tx.send`:

```rust
        let mut attached = attached;
        attached.role = role;
        // Every link the session is handed can carry the next standby
        // request and answer probes. The server dies with the connection.
        crate::control::serve_control(attached.link.sink.connection().clone(), door_tx.clone());
```

In `src/serve.rs`, create `let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);` beside `attach_tx`. Pass both into `serve_attaches`. Before `HostSession::spawn`, when `sock` is true, start `crate::control::serve_control(attached.link.sink.connection().clone(), door_tx.clone())` for the first link. When `sock` is false there is no listener and no control server, and a standby request on such a link finds no door, which Task 5's `one_stream` already tolerates.

Update the existing listener tests' `serve_attaches` calls with a fresh `mpsc::channel(1)` pair.

- [ ] **Step 5: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 6: Injection.** In `one_stream`, send `role: Role::Primary`. `a_standby_request_is_handed_to_the_door_with_its_stream` must fail. Restore it. Then delete the `Some(d) = doors.recv()` arm (with a `let _ = &doors;` to keep it compiling). `a_door_request_runs_the_same_exchange_and_carries_its_role` must hang or fail; give it an outer `tokio::time::timeout` of 10 s so it fails rather than hangs. Restore it.

- [ ] **Step 7: Commit**

```bash
git add src/control.rs src/listener.rs src/attach_exchange.rs src/serve.rs src/main.rs
git commit -m "feat(host): a control stream on every link, and one door for both kinds of attach"
```

---

### Task 6: The host parks a standby and adopts it on its first frame

**Files:**
- Modify: `src/session.rs`: host half (`run_with_attaches` :424, `adopt` :602, `HostWake` :682, the constants near `TAKEN_OVER` :730), and tests

**Interfaces:**
- Consumes: `Attached::role` (Task 5).
- Produces:
  - `pub const SWITCHED: &[u8] = b"switched to the standby link";`
  - `HostSession::adopt_as(&mut self, link: Link, size: TermSize, reason: &'static [u8]) -> Result<()>`, with `adopt` delegating to it with `TAKEN_OVER`.
  - `HostSession::on_attached(&mut self, a: Attached, standby: &mut Option<Link>) -> Result<()>`.

- [ ] **Step 1: Extract a link-pair fixture.** In `session.rs`'s tests, split `pair_on`'s QUIC construction into:

```rust
/// Two ends of one loopback QUIC connection, as `Link`s: (host, client).
async fn link_pair(client_bind: &str) -> (Link, Link) {
    // exactly the cert / quic_server / quic_client / accept code from pair_on,
    // returning Link::new(host_conn, host_ep, host_sock) and
    // Link::new(client_conn, client_ep, client_sock)
}
```

`pair_on` then calls it. Run `make check`; expected: exit 0.

- [ ] **Step 2: Write the failing tests:**

```rust
fn standby_attached(link: Link) -> crate::attach_exchange::Attached {
    crate::attach_exchange::Attached {
        link,
        path: path_of(Rung::StunPunch, 30, 1400, 4, NatType::Unknown),
        client_size: size(),
        role: crate::control::Role::Standby,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_is_parked_and_the_primary_is_left_alone() {
    let (mut host, _client) = pair("").await;
    let (standby_host, _standby_client) = link_pair("127.0.0.1:0").await;
    let mut slot = None;

    host.on_attached(standby_attached(standby_host), &mut slot).unwrap();

    assert!(slot.is_some(), "the standby was not parked");
    assert!(
        host.link.sink.connection().close_reason().is_none(),
        "parking a standby closed the primary"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_primary_attach_drops_the_parked_standby() {
    let (mut host, _client) = pair("").await;
    let (standby_host, standby_client) = link_pair("127.0.0.1:0").await;
    let (newcomer_host, _newcomer_client) = link_pair("127.0.0.1:0").await;
    let mut slot = None;
    host.on_attached(standby_attached(standby_host), &mut slot).unwrap();

    let mut newcomer = standby_attached(newcomer_host);
    newcomer.role = crate::control::Role::Primary;
    host.on_attached(newcomer, &mut slot).unwrap();

    assert!(slot.is_none(), "a takeover kept the displaced client's standby");
    let reason = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        standby_client.sink.connection().closed(),
    )
    .await
    .expect("the standby was closed");
    assert!(is_takeover(&reason), "closed as {reason:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn promoting_the_standby_closes_the_primary_as_switched() {
    let (mut host, client) = pair("").await;
    let (standby_host, _standby_client) = link_pair("127.0.0.1:0").await;

    host.adopt_as(standby_host, size(), SWITCHED).unwrap();

    let reason = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        client.link.sink.connection().closed(),
    )
    .await
    .expect("the old primary was closed");
    assert!(
        matches!(&reason, quinn::ConnectionError::ApplicationClosed(c) if c.reason.as_ref() == SWITCHED),
        "closed as {reason:?}"
    );
}
```

Also write one end-to-end test that drives the loop. Model it on `a_landed_rebuild_swaps_the_link_and_starts_the_sync_state_over` (:4616) and reuse that test's way of running host and client turns:

```rust
/// The host adopts a parked standby when the client's first frame arrives on
/// it, and that frame is not lost to the reset (seq 1 after the reset is seq 1
/// the host must apply).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_first_frame_on_a_standby_adopts_it_and_is_applied() {
    // 1. pair("") ; link_pair for the standby; park it through the host's
    //    attaches channel with role Standby, running host.run_with_attaches in
    //    a task exactly as the rebuild test runs the host.
    // 2. client.swap_in_as(standby_client, Instant::now(), SWITCHED).
    // 3. client types "echo standby-ok\n" and turns until its screen contains
    //    "standby-ok", with the same bounded loop the rebuild test uses.
    // 4. assert the old primary's close reason is SWITCHED.
}
```

`swap_in_as` is added in Task 8. Write this test in Task 8, not now: it needs both halves. Step 2 of Task 8 carries it.

- [ ] **Step 3: Run the three tests and confirm they fail to compile.**

- [ ] **Step 4: Implement.** Near `TAKEN_OVER`:

```rust
/// The close reason for a primary replaced by its own client's standby
/// (spec §3.5). Not `TAKEN_OVER`: nobody else took anything, and the client
/// must not read its own failover as a displacement.
pub const SWITCHED: &[u8] = b"switched to the standby link";
```

Rename the body of `adopt` to `adopt_as(&mut self, link: Link, size: TermSize, reason: &'static [u8])`, using `reason` in the close. `adopt(link, size)` becomes `self.adopt_as(link, size, TAKEN_OVER)`. Then:

```rust
    /// One completed attach, by role. `standby` is the loop's local slot.
    pub(crate) fn on_attached(
        &mut self,
        a: crate::attach_exchange::Attached,
        standby: &mut Option<Link>,
    ) -> Result<()> {
        match a.role {
            crate::control::Role::Primary => {
                // A takeover. The standby belongs to the displaced client, and
                // leaving it parked would let that client take the session back
                // by failing over onto it, without going through ssh.
                if let Some(old) = standby.take() {
                    old.sink.connection().close(quinn::VarInt::from_u32(0), TAKEN_OVER);
                }
                self.adopt(a.link, a.client_size)
                    .context("adopting a second attach")
            }
            crate::control::Role::Standby => {
                // One slot. A newer standby supersedes the older one.
                if let Some(old) = standby.replace(a.link) {
                    old.sink.connection().close(quinn::VarInt::from_u32(0), SWITCHED);
                }
                Ok(())
            }
        }
    }
```

In `run_with_attaches`, add a local `let mut standby: Option<Link> = None;` before the loop. Add to `HostWake`:

```rust
    /// A frame arrived on the parked standby: the client has failed over.
    StandbyFrame(Frame),
    /// The parked standby's connection is gone.
    StandbyGone,
```

Add a select arm borrowing the local (C1):

```rust
                f = async { standby.as_mut().expect("armed").source.recv().await },
                    if standby.is_some() => match f {
                        Some(frame) => HostWake::StandbyFrame(frame),
                        None => HostWake::StandbyGone,
                    },
```

Handle the new wakes:

```rust
                HostWake::Attached(a) => self.on_attached(a, &mut standby)?,
                HostWake::StandbyFrame(frame) => {
                    let link = standby.take().expect("the arm was armed");
                    let size = self.size;
                    // Reset first, THEN feed the frame. `adopt_as` restarts the
                    // input receiver at seq 1, and the client's first frame
                    // after its own reset is seq 1. The other order would apply
                    // it against the old generation and throw it away.
                    self.adopt_as(link, size, SWITCHED)
                        .context("switching to the standby")?;
                    pending = Some(frame);
                }
                HostWake::StandbyGone => standby = None,
```

The size passed to `adopt_as` is the session's current one. The client's frame carries its real size in `InputState`, and `turn_at` already reconciles it.

- [ ] **Step 5: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 6: Injection.** In `on_attached`'s `Primary` arm, delete the `standby.take()` block. `a_primary_attach_drops_the_parked_standby` must fail. Restore it.

- [ ] **Step 7: Commit**

```bash
git add src/session.rs
git commit -m "feat(host): park a standby, and switch to it on its first frame"
```

---

### Task 7: The failover decision, as pure functions

**Files:**
- Modify: `src/linkstate.rs`

**Interfaces:**
- Produces:

```rust
pub const FAILOVER_GRACE: Duration;   // 1 s
pub const PROBE_TIMEOUT: Duration;    // 2 s
pub const PROBE_RETRY: Duration;      // 5 s
pub const STANDBY_DELAY: Duration;    // 5 s
pub fn standby_backoff(failures: u32) -> Duration; // 30, 60, 120, then 300 s
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeState { Idle, Pending { sent: Instant }, Answered { sent: Instant }, Failed { at: Instant } }
pub fn failover_due(phase: Phase, probe: ProbeState, now: Instant) -> bool;
```

- [ ] **Step 1: Write the failing tests** in `linkstate.rs`'s tests:

```rust
#[test]
fn failover_waits_for_the_grace_after_an_answered_probe() {
    let t0 = Instant::now();
    let silent = Phase::Silent { since: t0 };
    let answered = ProbeState::Answered { sent: t0 };
    assert!(!failover_due(silent, answered, t0 + FAILOVER_GRACE - Duration::from_millis(1)));
    assert!(failover_due(silent, answered, t0 + FAILOVER_GRACE));
}

#[test]
fn failover_needs_an_answer() {
    let t0 = Instant::now();
    let silent = Phase::Silent { since: t0 };
    let late = t0 + Duration::from_secs(60);
    for p in [ProbeState::Idle, ProbeState::Pending { sent: t0 }, ProbeState::Failed { at: t0 }] {
        assert!(!failover_due(silent, p, late), "{p:?}");
    }
}

#[test]
fn a_frame_before_the_grace_ends_cancels_the_failover() {
    // `Live` is what a frame on the primary produces. However the probe went,
    // a primary that answered is not failed over from.
    let t0 = Instant::now();
    assert!(!failover_due(Phase::Live, ProbeState::Answered { sent: t0 }, t0 + FAILOVER_GRACE));
}

#[test]
fn recovering_still_fails_over() {
    // An answer that lands after the 20 s escalation is still an answer.
    let t0 = Instant::now();
    let rec = Phase::Recovering { attempt: 0, next_try: t0 };
    assert!(failover_due(rec, ProbeState::Answered { sent: t0 }, t0 + FAILOVER_GRACE));
}

#[test]
fn the_standby_backoff_climbs_and_then_holds() {
    let s: Vec<u64> = (0..6).map(|n| standby_backoff(n).as_secs()).collect();
    assert_eq!(s, vec![30, 60, 120, 300, 300, 300]);
}
```

- [ ] **Step 2: Run the tests and confirm they fail to compile.**

- [ ] **Step 3: Implement:**

```rust
/// How long after sending an answered probe the client fails over.
///
/// The probe goes out on the first lap in `Silent`, which is `SILENT_AFTER`
/// after a reply became owed, so failover lands at `SILENT_AFTER` plus this:
/// three seconds, the spec's `FAILOVER_AFTER` (§3.5). The second is there so a
/// primary that was only blipping gets to answer first, since failing over
/// costs a full-state snapshot and a new standby search.
pub const FAILOVER_GRACE: Duration = Duration::from_secs(1);

/// How long a probe may take before the standby counts as not answering.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long after a failed probe the next one may go, within one outage.
pub const PROBE_RETRY: Duration = Duration::from_secs(5);

/// How long after a link is up before a standby is searched for, so the
/// search never competes with the first paint (spec §3.1).
pub const STANDBY_DELAY: Duration = Duration::from_secs(5);

/// The wait before standby search number `failures + 1`.
pub fn standby_backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 => 30,
        1 => 60,
        2 => 120,
        _ => 300,
    })
}

/// Where this outage's probe of the standby stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeState {
    Idle,
    Pending { sent: Instant },
    Answered { sent: Instant },
    Failed { at: Instant },
}

/// Whether to fail over onto the standby now.
///
/// Only while the primary is not answering, and only on an answer from the
/// standby, never on the absence of one: failing over onto a standby that
/// is also dead would trade a connection that might revive for one that has
/// already been seen not to.
pub fn failover_due(phase: Phase, probe: ProbeState, now: Instant) -> bool {
    let outage = matches!(phase, Phase::Silent { .. } | Phase::Recovering { .. });
    match probe {
        ProbeState::Answered { sent } => outage && now.duration_since(sent) >= FAILOVER_GRACE,
        _ => false,
    }
}
```

The constants are consumed from Task 8 on. Mark each consumer-less item `#[cfg_attr(not(test), allow(dead_code))] // wired by Task 8`.

- [ ] **Step 4: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 5: Injection.** Drop `outage &&` from `failover_due`. `a_frame_before_the_grace_ends_cancels_the_failover` must fail. Restore it.

- [ ] **Step 6: Commit**

```bash
git add src/linkstate.rs
git commit -m "feat(client): when to fail over, decided without a clock of its own"
```

---

### Task 8: The client's standby: search, probe, failover

**Files:**
- Modify: `src/control.rs` (client half: `request_standby`, `probe`)
- Create: `src/standby.rs`
- Modify: `src/session.rs`: client half (`ClientSession` fields ~:760, `swap_in` :1370, `run_on` :1531, `Wake` :653), and tests
- Modify: `src/main.rs` (`mod standby;`)
- Remove the Task 4 and Task 7 `allow(dead_code)` attributes.

**Interfaces:**
- Consumes: `establish(.., Some(filter))` (Task 3), `avoiding_source` (Task 4), `failover_due`, `ProbeState` and the constants (Task 7), `SWITCHED` (Task 6).
- Produces:
  - `control::request_standby(primary: quinn::Connection, size: TermSize, cfg: NetConfig, admit: RemoteFilter) -> anyhow::Result<Established>`.
  - `control::probe(conn: quinn::Connection, nonce: u64) -> bool`.
  - `standby::Standby::new(cfg: NetConfig, now: Instant) -> Standby`.
  - `standby::StandbyEvent { Found(Box<Established>), NotFound(String), Probed { answered: bool } }`.
  - `ClientSession::with_standby(self, s: Standby) -> ClientSession`.
  - `ClientSession::swap_in_as(&mut self, link: Link, now: Instant, reason: &'static [u8]) -> Result<()>`, with `swap_in` delegating with `REBUILT`.

- [ ] **Step 1: The client half of the control stream.** Add to `src/control.rs`:

```rust
/// Longest a whole standby search may take. Every step inside `establish`
/// has its own budget; this is the outer wall, as `ATTACH_TIMEOUT` is for
/// the host.
pub(crate) const SEARCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Ask the host, over the primary, for a standby, and run the ordinary client
/// exchange over the same stream (spec §3.2).
pub(crate) async fn request_standby(
    primary: quinn::Connection,
    size: oxutrm_proto::TermSize,
    cfg: oxutrm_net::NetConfig,
    admit: oxutrm_net::RemoteFilter,
) -> anyhow::Result<crate::connect::Established> {
    use anyhow::Context;
    let (mut send, recv) = primary.open_bi().await.context("opening a control stream")?;
    write_signal_async(&mut send, &Signal::StandbyRequest)
        .await
        .context("asking for a standby")?;
    tokio::time::timeout(
        SEARCH_DEADLINE,
        crate::connect::establish(tokio::io::BufReader::new(recv), send, size, &cfg, Some(admit)),
    )
    .await
    .context("the standby search ran out of time")?
}

/// Does the far end of `conn` answer right now?
///
/// A fresh stream per probe. Opening one on a dead path succeeds locally,
/// since stream ids are ours to allocate, and it is the timeout that decides.
pub(crate) async fn probe(conn: quinn::Connection, nonce: u64) -> bool {
    let attempt = async {
        let (mut send, recv) = conn.open_bi().await.ok()?;
        write_signal_async(&mut send, &Signal::Probe { nonce }).await.ok()?;
        let mut reader = tokio::io::BufReader::new(recv);
        match read_signal_async(&mut reader).await.ok()? {
            Signal::ProbeAck { nonce: n } if n == nonce => Some(()),
            _ => None,
        }
    };
    matches!(
        tokio::time::timeout(crate::linkstate::PROBE_TIMEOUT, attempt).await,
        Ok(Some(()))
    )
}
```

- [ ] **Step 2: Write the failing tests.** In `src/control.rs` tests, add an end-to-end search against a real host door, using `attach_exchange::fixtures`:

```rust
/// A host side that serves control streams and runs every standby request
/// through the real exchange, returning what it produced.
fn host_door(
    host_conn: quinn::Connection,
) -> tokio::sync::oneshot::Receiver<crate::attach_exchange::Attached> {
    let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
    serve_control(host_conn, door_tx);
    let (done_tx, done_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let req = door_rx.recv().await.expect("a request");
        let mut meta = crate::attach_exchange::fixtures::fresh_meta("00112233445566778899aabbccddeeff");
        let cfg = crate::attach_exchange::fixtures::stun_free();
        if let Ok(mut a) = crate::attach_exchange::run_attach_exchange(req.reader, req.writer, &mut meta, &cfg).await {
            a.role = req.role;
            let _ = done_tx.send(a);
        }
    });
    done_rx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_search_lands_a_second_connection_to_the_same_host() {
    let (host, client, _eps) = quic_pair().await;
    let done = host_door(host);
    let cfg = crate::attach_exchange::fixtures::stun_free();

    let est = request_standby(client.clone(), TermSize { cols: 40, rows: 10 }, cfg, std::sync::Arc::new(|_| true))
        .await
        .expect("the standby landed");
    let attached = done.await.expect("the host completed its side");

    assert_eq!(attached.role, Role::Standby);
    assert_ne!(
        est.link.sink.connection().stable_id(),
        client.stable_id(),
        "the standby is the primary"
    );
    assert!(client.close_reason().is_none(), "the search disturbed the primary");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_standby_search_honours_the_filter() {
    let (host, client, _eps) = quic_pair().await;
    let _done = host_door(host);
    let mut cfg = crate::attach_exchange::fixtures::stun_free();
    cfg.gather_timeout = std::time::Duration::from_millis(800);

    let r = request_standby(client, TermSize { cols: 40, rows: 10 }, cfg, std::sync::Arc::new(|_| false)).await;
    assert!(r.is_err(), "landed a standby on a forbidden path");
}
```

The two `RemoteFilter`s here are explicit (`|_| true` / `|_| false`) because loopback is the primary's own path. `avoiding_source` would correctly refuse every candidate this test has.

Then, in `session.rs` tests, add the end-to-end test Task 6 deferred, `the_first_frame_on_a_standby_adopts_it_and_is_applied`, now with `swap_in_as` available. Follow the outline in Task 6 Step 2 and the rebuild test at :4616 for driving turns. The three numbered checks in that outline are the assertions.

Add the failover test using the `Relay` fixture (`pair_through_relay`, :2084), which really drops packets:

```rust
/// Spec §3.5 end to end: the primary goes dark, the standby answers its
/// probe, and the client is on the standby within FAILOVER_GRACE after the
/// probe, with no ssh anywhere in the test.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_primary_fails_over_onto_an_answering_standby() {
    // 1. (host, client, relay) = pair_through_relay("").
    // 2. A standby link_pair; serve_control on its host end (door unused);
    //    park the host end on the host via on_attached(role Standby); give the
    //    client end to the client via client.standby_mut().adopt_found(..)
    //    (the test hook defined in standby.rs, Step 4).
    // 3. relay.blackhole(true); client types "echo after-failover\n".
    // 4. Drive client.run_on-equivalent laps with the rebuild test's helper
    //    until the screen shows "after-failover", bounded by
    //    SILENT_AFTER + FAILOVER_GRACE + PROBE_TIMEOUT + 3 s.
    // 5. Assert the old primary's close reason is SWITCHED and the client's
    //    current link's stable_id is the standby's.
}
```

Add to `src/standby.rs` tests:

```rust
#[test]
fn no_standby_is_searched_for_when_the_host_did_not_offer_one() {
    // The gate lives in connect.rs: `with_standby` is only called when
    // host_features contains FEATURE_STANDBY. Test that helper:
    assert!(!crate::connect::offers_standby(&[]));
    assert!(!crate::connect::offers_standby(&["control".to_string()]));
    assert!(crate::connect::offers_standby(&[
        "control".to_string(),
        "standby".to_string()
    ]));
}

#[test]
fn a_standby_that_closes_is_forgotten_and_searched_for_again() {
    let t0 = Instant::now();
    let mut s = Standby::new(stun_free_cfg(), t0);
    s.found_for_test(t0); // parks a placeholder; see Step 4
    s.lost(t0);
    assert!(!s.has_link());
    assert_eq!(s.next_search(), t0 + standby_backoff(0));
}
```

- [ ] **Step 3: Run the tests and confirm they fail to compile.**

- [ ] **Step 4: Implement `src/standby.rs`:**

```rust
//! The client's standby: whether it has one, when to look for one, and where
//! this outage's probe of it stands (spec §3).
//!
//! State only. The work (searching, probing) runs in spawned tasks that
//! report back through a channel the session loop holds as a local (C1), and
//! the loop calls `step` on the laps it already takes. So nothing here holds a
//! timer: an idle session with a healthy standby costs no wakeups (19cc001).

use std::time::Instant;

use oxutrm_net::NetConfig;
use oxutrm_proto::PathDescription;

use crate::connect::Established;
use crate::link::Link;
use crate::linkstate::{
    PROBE_RETRY, Phase, ProbeState, STANDBY_DELAY, failover_due, standby_backoff,
};

pub(crate) enum StandbyEvent {
    Found(Box<Established>),
    NotFound(String),
    Probed { answered: bool },
}

/// What `step` asks the loop to do.
pub(crate) enum StandbyAction {
    Nothing,
    /// Spawn `control::request_standby` against the primary, reporting
    /// `Found`/`NotFound`.
    Search,
    /// Spawn `control::probe` against the standby, reporting `Probed`.
    Probe { nonce: u64 },
    /// Swap the standby in now.
    FailOver,
}

pub(crate) struct Standby {
    pub(crate) cfg: NetConfig,
    link: Option<(Link, PathDescription)>,
    searching: bool,
    failures: u32,
    next_search: Instant,
    probe: ProbeState,
    probing: bool,
    nonce: u64,
    /// Whether "no standby path" has been said for the current dry spell, so
    /// it is said once, not after every failed search.
    told_none: bool,
}

impl Standby {
    pub(crate) fn new(cfg: NetConfig, now: Instant) -> Standby {
        Standby {
            cfg,
            link: None,
            searching: false,
            failures: 0,
            next_search: now + STANDBY_DELAY,
            probe: ProbeState::Idle,
            probing: false,
            nonce: 0,
            told_none: false,
        }
    }

    pub(crate) fn has_link(&self) -> bool {
        self.link.is_some()
    }

    pub(crate) fn next_search(&self) -> Instant {
        self.next_search
    }

    /// The standby's connection, for the loop's `closed()` arm.
    pub(crate) fn connection(&self) -> Option<quinn::Connection> {
        self.link.as_ref().map(|(l, _)| l.sink.connection().clone())
    }

    /// One decision per lap. `phase` is the lap's own phase.
    pub(crate) fn step(&mut self, phase: Phase, now: Instant) -> StandbyAction {
        let outage = matches!(phase, Phase::Silent { .. } | Phase::Recovering { .. });
        if !outage {
            // The outage (if there was one) is over, and so is its probe.
            self.probe = ProbeState::Idle;
            if self.link.is_none() && !self.searching && now >= self.next_search {
                self.searching = true;
                return StandbyAction::Search;
            }
            return StandbyAction::Nothing;
        }
        if self.link.is_none() {
            return StandbyAction::Nothing;
        }
        if failover_due(phase, self.probe, now) {
            return StandbyAction::FailOver;
        }
        let may_probe = match self.probe {
            ProbeState::Idle => true,
            ProbeState::Failed { at } => now.duration_since(at) >= PROBE_RETRY,
            _ => false,
        };
        if may_probe && !self.probing {
            self.probing = true;
            self.nonce = self.nonce.wrapping_add(1);
            self.probe = ProbeState::Pending { sent: now };
            return StandbyAction::Probe { nonce: self.nonce };
        }
        StandbyAction::Nothing
    }

    /// A search finished with a link.
    pub(crate) fn found(&mut self, e: Established) {
        self.searching = false;
        self.failures = 0;
        self.told_none = false;
        self.link = Some((e.link, e.path));
    }

    /// A search finished without one. Returns whether to tell the user.
    pub(crate) fn not_found(&mut self, now: Instant) -> bool {
        self.searching = false;
        self.next_search = now + standby_backoff(self.failures);
        self.failures = self.failures.saturating_add(1);
        !std::mem::replace(&mut self.told_none, true)
    }

    pub(crate) fn probed(&mut self, answered: bool, now: Instant) {
        self.probing = false;
        if let ProbeState::Pending { sent } = self.probe {
            self.probe = if answered {
                ProbeState::Answered { sent }
            } else {
                ProbeState::Failed { at: now }
            };
        }
        // A result for a probe whose outage already ended is dropped: `step`
        // reset `probe` to `Idle`, and `Idle` is not `Pending`.
    }

    /// The standby's connection closed on its own.
    pub(crate) fn lost(&mut self, now: Instant) {
        self.link = None;
        self.probe = ProbeState::Idle;
        self.next_search = now + standby_backoff(self.failures);
        self.failures = self.failures.saturating_add(1);
    }

    /// Hand the standby over for a failover. After it, a fresh search is due
    /// once the new primary has settled.
    pub(crate) fn take_for_failover(&mut self, now: Instant) -> Option<(Link, PathDescription)> {
        self.probe = ProbeState::Idle;
        self.failures = 0;
        self.next_search = now + STANDBY_DELAY;
        self.link.take()
    }

    /// A rebuild over ssh landed. The host dropped our standby when it adopted
    /// the rebuild (Task 6), so ours is a corpse.
    pub(crate) fn forget(&mut self, now: Instant) {
        self.link = None;
        self.probe = ProbeState::Idle;
        self.next_search = now + STANDBY_DELAY;
    }
}
```

Add `#[cfg(test)]` helpers in the same file: `fn stun_free_cfg() -> NetConfig`, and `found_for_test(&mut self, now)`, which calls `lost`'s inverse by putting the state into "has a link" without a real `Link`. If a real `Link` is required, build one with a loopback QUIC pair as in `control.rs`'s tests and make the test `#[tokio::test]`. Also add `pub(crate) fn adopt_found(&mut self, e: Established)` as a `#[cfg(test)]` alias of `found` for the session test.

In `src/connect.rs`, add:

```rust
/// Whether a host's hello offered a standby (spec §2.1).
pub(crate) fn offers_standby(features: &[String]) -> bool {
    features.iter().any(|f| f == oxutrm_proto::FEATURE_STANDBY)
}
```

and at `ClientSession::new` (:198): if `offers_standby(&established.host_features)`, chain `.with_standby(Standby::new(cfg.clone(), Instant::now()))`, where `cfg` is the `NetConfig` `connect` already uses.

In `ClientSession`, add the field `standby: Option<crate::standby::Standby>` (`None` in `new`), plus:

```rust
    /// Search for, and fail over onto, a standby (spec §3). Only for a host
    /// that offered one; see `connect::offers_standby`.
    pub(crate) fn with_standby(mut self, s: crate::standby::Standby) -> ClientSession {
        self.standby = Some(s);
        self
    }
```

Rename `swap_in`'s body to `swap_in_as(&mut self, link, now, reason: &'static [u8])`, using `reason` in the close. `swap_in` delegates with `REBUILT`.

In `run_on`, add locals beside `outcomes`:

```rust
        // Where standby searches and probes report back. A local (C1).
        let (standby_tx, mut standby_rx) = tokio::sync::mpsc::channel::<crate::standby::StandbyEvent>(2);
        // The standby's connection, watched for closing. A local for the same
        // reason `conn` is.
        let mut standby_conn: Option<quinn::Connection> = None;
```

Add `Wake` variants `Standby(crate::standby::StandbyEvent)` and `StandbyClosed`, and select arms:

```rust
                Some(ev) = standby_rx.recv() => Wake::Standby(ev),
                _ = async { standby_conn.as_ref().expect("armed").closed().await },
                    if standby_conn.is_some() => Wake::StandbyClosed,
```

Handle them:

```rust
                Wake::Standby(crate::standby::StandbyEvent::Found(e)) => {
                    let path = e.path.clone();
                    if let Some(s) = self.standby.as_mut() {
                        s.found(*e);
                        standby_conn = s.connection();
                    }
                    self.announce_standby(Some(&path), out)?;
                }
                Wake::Standby(crate::standby::StandbyEvent::NotFound(_why)) => {
                    let tell = self.standby.as_mut().is_some_and(|s| s.not_found(Instant::now()));
                    if tell {
                        self.announce_standby(None, out)?;
                    }
                }
                Wake::Standby(crate::standby::StandbyEvent::Probed { answered }) => {
                    if let Some(s) = self.standby.as_mut() {
                        s.probed(answered, Instant::now());
                    }
                }
                Wake::StandbyClosed => {
                    standby_conn = None;
                    if let Some(s) = self.standby.as_mut() {
                        s.lost(Instant::now());
                    }
                }
```

In the `Wake::Rebuilt(Landed)` arm, after the swap: `if let Some(s) = self.standby.as_mut() { s.forget(Instant::now()); } standby_conn = None;`.

After the existing per-lap `rebuild_step` call, add the standby step. It returns what to do, and the loop does it, so spawning never borrows `self` inside a select:

```rust
            let now = Instant::now();
            let phase = self.link_state.phase_now();
            let action = self
                .standby
                .as_mut()
                .map_or(crate::standby::StandbyAction::Nothing, |s| s.step(phase, now));
            match action {
                crate::standby::StandbyAction::Nothing => {}
                crate::standby::StandbyAction::Search => {
                    let primary = self.link.sink.connection().clone();
                    let size = self.size;
                    let cfg = self.standby.as_ref().expect("stepped").cfg.clone();
                    let tx = standby_tx.clone();
                    tokio::spawn(async move {
                        let ev = match crate::egress::avoiding_source(primary.remote_address()) {
                            None => crate::standby::StandbyEvent::NotFound(
                                "cannot tell which way the link leaves this machine".into(),
                            ),
                            Some(admit) => match crate::control::request_standby(primary, size, cfg, admit).await {
                                Ok(e) => crate::standby::StandbyEvent::Found(Box::new(e)),
                                Err(e) => crate::standby::StandbyEvent::NotFound(format!("{e:#}")),
                            },
                        };
                        let _ = tx.send(ev).await;
                    });
                }
                crate::standby::StandbyAction::Probe { nonce } => {
                    if let Some(conn) = self.standby.as_ref().and_then(|s| s.connection()) {
                        let tx = standby_tx.clone();
                        tokio::spawn(async move {
                            let answered = crate::control::probe(conn, nonce).await;
                            let _ = tx.send(crate::standby::StandbyEvent::Probed { answered }).await;
                        });
                    }
                }
                crate::standby::StandbyAction::FailOver => {
                    if let Some((link, path)) = self.standby.as_mut().and_then(|s| s.take_for_failover(now)) {
                        self.swap_in_as(link, now, crate::session::SWITCHED)
                            .context("failing over to the standby")?;
                        conn = self.link.sink.connection().clone();
                        standby_conn = None;
                        takeover_expected = false;
                        self.announce_failover(&path, out)?;
                        // The host adopts on our first frame, so send one now.
                        deadline = tokio::time::Instant::now();
                    }
                }
            }
```

Place this block where it runs on every lap after the phase has been evaluated. The loop already computes `now` for the notice; reuse that `now` rather than taking a second one. An aborted search task needs no handle. The search runs over the primary, so if the primary dies mid-search, the search fails on its own, and a late `Found` after a failover is still a valid standby: the host parked it against the same session.

- [ ] **Step 5: The two announcement lines.** Next to `announce`:

```rust
    /// Say once that a standby exists, or that none could be found (spec §3.7).
    fn announce_standby<W: Write>(&mut self, path: Option<&PathDescription>, out: &mut W) -> Result<()> {
        let line = match path {
            Some(p) => format!(
                "oxutrm  standby: {}  \u{b7}  {} ms",
                oxutrm_client::rung_label(p),
                p.rtt_ms
            ),
            None => "oxutrm  no standby path".to_string(),
        };
        writeln!(out, "{line}").context("announcing the standby")?;
        out.flush().context("flushing the terminal")?;
        self.renderer.invalidate();
        Ok(())
    }

    fn announce_failover<W: Write>(&mut self, path: &PathDescription, out: &mut W) -> Result<()> {
        writeln!(
            out,
            "oxutrm  switched to standby ({})  \u{b7}  {} ms",
            oxutrm_client::rung_label(path),
            path.rtt_ms
        )
        .context("announcing the failover")?;
        out.flush().context("flushing the terminal")?;
        self.renderer.invalidate();
        self.announced = Some(path.clone());
        Ok(())
    }
```

Add a test for each line, modelled on the existing `announce` tests in this file: find them with `grep -n "fn .*announce" src/session.rs`. Before writing the assertion, check what the output held *before* the change: the injection must be able to fail it.

- [ ] **Step 6: Run the tests and confirm they pass.** Run `make check`.

- [ ] **Step 7: Injections**, one at a time, restoring each:
  - In `Standby::step`, return `StandbyAction::FailOver` whenever `outage && self.link.is_some()`, ignoring the probe. `a_frame_before_the_grace_ends_cancels_the_failover` still passes, because it is pure, but a new test `step_never_fails_over_on_an_unanswered_probe` must fail. Write that test first: `found_for_test`, then `step(Silent)` gives `Probe`, then `probed(false)`, then `step` at +10 s must not be `FailOver`.
  - In the host's `StandbyFrame` arm, swap the order: set `pending` before `adopt_as`. `the_first_frame_on_a_standby_adopts_it_and_is_applied` must fail or time out.
  - In `a_dead_primary_fails_over_onto_an_answering_standby`, remove the `relay.blackhole(true)`. The test must fail, because no failover happens, which proves the test watches the failover and not merely the echo.

- [ ] **Step 8: Commit**

```bash
git add src/control.rs src/standby.rs src/session.rs src/connect.rs src/main.rs src/egress.rs src/linkstate.rs
git commit -m "feat(client): find a standby, and fail over to it when the primary goes dark"
```

---

### Task 9: Documentation and changelog

**Files:**
- Modify: `README.md` (the reconnect paragraph at ~:91, "How it connects" ~:117, "What does not work yet" ~:145)
- Modify: `CHANGES.md` (`## Unreleased`)

- [ ] **Step 1: README.** After the paragraph that starts "A client whose network dies reconnects by itself", add:

```markdown
While a link is up, the client also looks for a **standby**: a second
connection to the same session over a path that leaves this machine another
way, for instance the open internet next to a split-tunnel VPN. It is kept
warm and costs a keep-alive every ten seconds. When the link goes quiet and
the standby answers, the client switches within about three seconds, with no
ssh. The status line says whether a standby exists:

    oxutrm  IPv4 direct  ·  11 ms
    oxutrm  standby: IPv4 punched  ·  38 ms

"no standby path" means the session has no fallback, and the next outage
waits for the twenty-second rebuild.
```

In "What does not work yet", replace the bullet "A better path found after connect is lost until the next attach" with:

```markdown
- **A better path found after connect is used only as a standby.** The client
  switches to it when the link fails, never because it is faster.
```

- [ ] **Step 2: CHANGES.md**, under `## Unreleased`:

```markdown
### Added
- Standby link: while connected, the client builds a second connection to the
  same session over a path that leaves the machine through a different local
  address, keeps it warm, and switches to it about three seconds after the
  primary stops answering, without ssh. Hosts advertise it with
  `features: ["control", "standby"]` in their hello; older hosts are
  unaffected.

### Not yet
- The two-uplink network-namespace test from the spec (§7) is not written; the
  failover is covered by the in-process relay test and a hand test.
```

- [ ] **Step 3:** Run `make check`. Then commit:

```bash
git add README.md CHANGES.md
git commit -m "docs: the standby link"
```

---

### Task 10: Hand test on `thinlinc`

This task is manual and needs the user. The `thinlinc` binary must be rebuilt from this branch first (see the memory note on building on `thinlinc`).

- [ ] **Step 1:** Build on `thinlinc` and on the Mac from the branch head.
- [ ] **Step 2:** With the VPN up, run `/Users/oetiker/checkouts/oxutrm/target/release/oxutrm thinlinc`. Expected: a status line for the primary; about five seconds later, `oxutrm  standby: IPv4 punched ...` (or another rung that is not the tunnel).
- [ ] **Step 3:** Start `while :; do date; sleep 1; done` in the session. Disconnect the VPN.
- [ ] **Step 4:** Expected: the clock stalls for about three seconds, then `oxutrm  switched to standby (...)` appears and the clock continues. Nothing about ssh appears.
- [ ] **Step 5:** Reconnect the VPN. Expected: nothing visible changes; the session stays on the public path (no fail-back, spec §1.2). About five seconds later a new standby line may appear.
- [ ] **Step 6:** Record the outcome, including the observed stall time, in `CHANGES.md` under the Unreleased entry, and commit.

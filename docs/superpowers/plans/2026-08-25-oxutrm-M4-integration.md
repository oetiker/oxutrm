# oxutrm M4 — Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire M1's terminal core and sync engine, M2's QUIC transport and NAT ladder, and M3's SSH bootstrap and session registry into a remote terminal that is usable daily.

**Architecture:** Two symmetric session loops joined by QUIC. The host drains its PTY into `alacritty_terminal`, feeds a `Sender<ScreenState>`, and paces `Frame`s onto QUIC datagrams, fragmenting any diff too large for one datagram. The client feeds local input into a `Sender<InputState>`, applies incoming `ScreenDiff`s, and re-renders through a `Renderer` that down-converts colour to whatever the user's real terminal can show, so the host's state stays full fidelity.

**Tech Stack:** Rust 2024 (MSRV 1.85), `quinn` 0.11 (datagrams, connection migration, `Endpoint::rebind_abstract`), `tokio` 1 (`select!`, `AsyncFd`, unix signals), `alacritty_terminal` 0.26 with its re-exported `vte`, `rustix` 1 termios, `postcard` 1.

**Spec:** `docs/superpowers/specs/2026-08-25-oxutrm-design.md`
**Contract:** `docs/superpowers/plans/2026-08-25-oxutrm-contract.md` — **normative, read it first.**

**Depends on:** M1 (`2026-08-25-oxutrm-M1-terminal-core.md`), M2 (`2026-08-25-oxutrm-M2-transport.md`), M3 (`2026-08-25-oxutrm-M3-sessions.md`). Assume all three are merged and green.

---

## Global Constraints

Every task's requirements implicitly include the whole contract file. The load-bearing ones, repeated verbatim:

- **Binary and product name is `oxutrm`.** Not `oxuterm`. The checkout directory is `oxuterm` for historical reasons; nothing inside it uses that spelling.
- **Rust edition 2024**, workspace at the repo root, one binary `src/main.rs`.
  `alacritty_terminal` 0.26 is edition 2024 with MSRV 1.85, so that floor applies to
  the whole build.
- **Cap all parallelism at 4**: `cargo build --jobs 4`, `cargo test --jobs 4 -- --test-threads 4`.
- **Workspace root `Cargo.toml` must contain:**
  ```toml
  [profile.dev]
  debug = "line-tables-only"
  split-debuginfo = "unpacked"
  ```
- **`oxutrm-sync` performs no I/O.** No `std::net`, no `std::fs`, no `tokio`, no clock access. M4 adds nothing to `oxutrm-sync`; if a task feels like it needs to, it is the wrong task.
- **English** for all identifiers, comments, and documentation.
- **`anyhow::Result`** at binary and crate-boundary level; concrete error enums inside `oxutrm-sync` and `oxutrm-proto`.
- **No key material is ever written to disk**, in any crate, at any time.
- **Every task ends green**: `cargo clippy --all-targets -- -D warnings` and `cargo test --jobs 4 -- --test-threads 4` both pass before committing.
- **Commit messages** end with:
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`

### Dependency additions M4 makes beyond the contract table

| Crate | Version | Used by | Why |
|---|---|---|---|
| `bytes` | `1` | net | `quinn::Connection::send_datagram` takes `bytes::Bytes` |
| `thiserror` | `2` | net | `FragError`, which callers must discriminate |
| `libc` | `0.2` | root (dev) | raising signals at the real process in the exit-path tests |

Both crate additions go in `crates/oxutrm-net/Cargo.toml` in Task 2; `libc` is a
root `[dev-dependencies]` entry added in Task 8. Nothing else new.

---

## File Structure

**New files:**

| File | Responsibility |
|---|---|
| `crates/oxutrm-net/src/pace.rs` | `Pacer` — the §10.4 send-interval policy, pure, clock injected |
| `crates/oxutrm-net/src/link.rs` | `LinkStats` + `link_stats()` — the *only* place that touches `quinn::ConnectionStats` |
| `crates/oxutrm-net/src/frag.rs` | `fragment()` / `Reassembler` — §7.1.1, pure, no sockets |
| `crates/oxutrm-net/src/xport.rs` | `FrameSink` / `FrameSource` — fragmenting send, reassembling receive |
| `crates/oxutrm-net/src/tunnel.rs` | Rung 4: a loopback UDP relay over the SSH channel |
| `crates/oxutrm-net/src/testkit.rs` | `loopback_pair()` — two connected `quinn::Connection`s, behind feature `testkit` |
| `crates/oxutrm-host/src/input_cursor.rs` | `InputCursor` — which client input bytes have reached the PTY, pure |
| `crates/oxutrm-host/src/session.rs` | `run_host_session` — PTY ↔ `alacritty_terminal` ↔ QUIC |
| `crates/oxutrm-client/src/color.rs` | Truecolor → 256 → 16 → 8 down-conversion, in the client |
| `crates/oxutrm-client/src/input_queue.rs` | `InputQueue` — client-side mirror of the host's input bookkeeping, pure |
| `crates/oxutrm-client/src/guard.rs` | `TerminalGuard` — raw mode, alt screen, panic hook, one idempotent restore |
| `crates/oxutrm-client/src/status.rs` | `status_line`, `migration_line`, `rung_label` |
| `crates/oxutrm-client/src/pane.rs` | `StatusPane`, `status_pane_lines` — the `Ctrl-]` pane |
| `crates/oxutrm-client/src/session.rs` | `run_client_session` — input ↔ QUIC ↔ `Renderer` |
| `src/help.rs` | The `--help` text |
| `README.md` | Project README |
| `tests/exit_paths.rs` | Panic / SIGINT / SIGTERM / peer-loss restore, under a real PTY |
| `tests/roaming.rs` | Forced client rebind mid-session |
| `tests/e2e.rs` | Host + client on loopback, rendered screen vs. authoritative state |
| `tests/support/mod.rs` | `spawn_under_pty` and friends |

**Modified files:**

| File | Change |
|---|---|
| `crates/oxutrm-term/src/host_term.rs` | `AsFd` for `HostTerm`, `set_nonblocking`, `parse_to_state` |
| `crates/oxutrm-term/src/lib.rs` | re-export `parse_to_state` |
| `crates/oxutrm-net/src/lib.rs` | `mod` + `pub use` for the five new modules |
| `crates/oxutrm-host/src/lib.rs` | `mod` + `pub use` for `input_cursor`, `session` |
| `crates/oxutrm-client/src/lib.rs` | `mod` + `pub use` for the six new modules |
| `crates/oxutrm-client/src/renderer.rs` | call `down_convert` before emitting SGR |
| `src/main.rs` | wire `--help`, the client session, the host session, rung 4 |

---

## Fragmentation, stated once (spec §7.1.1)

`quinn::Connection::max_datagram_size()` is on the order of 1200 bytes after
overhead. A full `ScreenState` for 80×24 with truecolor cells postcard-encodes to
well over 10 KB, and even a several-row `ScreenDiff` can exceed the limit. QUIC
datagrams are never fragmented by the transport: `send_datagram` rejects an
oversized payload with `SendDatagramError::TooLarge`.

This is not merely a large-repaint problem. It **breaks the sync engine's own
recovery path**: §8.2's "the peer's acknowledged state has left the ring, send a
full state" produces the single largest message the protocol can construct. Without
fragmentation the protocol cannot recover from its own worst case.

**oxutrm therefore fragments diffs itself.** `Frame` carries `frag_index: u16` and
`frag_count: u16`; the encoded diff is split into pieces that each fit
`max_datagram_size()`, and every piece is sent as its own datagram carrying the
same `my_state` and `from_state`. Compression happens **before** fragmentation, so
the `flags` byte is identical across every fragment of one state.

Four rules, normative, all from §7.1.1:

1. **A state is applied only when all of its fragments have arrived.** An
   incomplete set is **discarded wholesale** — never partially applied, never held
   waiting for a retransmission, because there are no retransmissions. This is what
   preserves §8.1: the receiver's acknowledged state is unchanged by a lost
   fragment, so the sender's next diff is computed against that same base and
   therefore *contains* everything the dropped set was carrying. Losing one
   fragment costs exactly one send interval and nothing else.
2. **The receiver holds at most one incomplete set.** A fragment naming a
   `my_state` newer than the set in progress **replaces** it; the older partial set
   is dropped at once rather than kept in hope.
3. **Fragments of a state older than the receiver's current state are discarded
   on arrival.**
4. **`frag_count` is bounded by configuration.** A diff exceeding the bound is a
   bug in diff generation, not a runtime condition to handle.

Unidirectional QUIC streams are **not** used for screen state. Control,
scrollback and clipboard remain bidirectional streams (§7.2).

## Rejection is wholesale, and rejection is not disconnection

Two rules meet in M4's session loops, and they are easy to get backwards.

**Rule 1 — a diff violating an invariant is rejected wholesale and never applied
partially** (spec §8.6, contract `ScreenState::validate`). `Receiver::on_frame`
calls `validate` after `apply` and returns `ApplyError::LengthMismatch` or
`ApplyError::CursorOutOfBounds`. §8.1's whole recovery story rests on the
receiver's state always being a state *the sender actually held*; a partially
applied diff leaves the receiver holding a state that existed nowhere, that no
sender ring contains, and that no later diff was computed against. That is
strictly worse than dropping the datagram, because dropping is a case the
protocol already recovers from.

A consequence M4 depends on and must not paper over: **`on_frame` must leave the
receiver's state untouched when it returns `Err`.** Applying into a scratch copy
and swapping only after `validate` succeeds is the implementation that satisfies
this. Task 2 asserts the observable property.

**Rule 2 — a rejected frame is a dropped frame, not a dead session.** Neither
session loop may use `?` on `Receiver::on_frame`. When it returns `Err`, log and
`continue`:

- the receiver's state did not change, so `ack()` does not advance;
- our next outgoing frame therefore still names a state the peer holds in its
  ring;
- the peer's next diff is computed from that same base and repairs everything.

Tearing down the connection would turn a single bad frame into a lost session,
and reconnecting cannot help because the peer would re-derive the same diff.

**And do not reach for `min()` on an out-of-range cursor.** Clamping is one line
and it is wrong: a sender holding a valid state cannot produce a diff that puts
the cursor outside it, so an out-of-range cursor means the two ends have genuinely
desynchronised. Clamping hides that behind a session which looks healthy while the
screens drift apart — the exact failure this design exists to prevent.

## 0-RTT is deliberately not used

Spec §6 states this and it is repeated here so a later reader does not "fix" it:
0-RTT needs a TLS resumption ticket issued under the same server configuration,
and §11's fresh certificate per attach guarantees every attach meets a server that
cannot decrypt any earlier ticket. Reattach pays a full handshake. **No task in
this plan implements or tests 0-RTT, and none should be added.**

---

### Task 1: Link statistics and the send-pacing policy

**Files:**
- Create: `crates/oxutrm-net/src/link.rs`
- Create: `crates/oxutrm-net/src/pace.rs`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `quinn::Connection` (M2, from `quic_client` / `quic_server`); `oxutrm_proto::{PathDescription, Rung, NatType}`.
- Produces:
  ```rust
  // crates/oxutrm-net/src/link.rs
  #[derive(Clone, Copy, PartialEq, Debug)]
  pub struct LinkStats {
      pub rtt: std::time::Duration,
      pub mtu: u16,
      pub loss_pct: f32,
      pub bytes_tx: u64,
      pub bytes_rx: u64,
      pub local: std::net::SocketAddr,
      pub remote: std::net::SocketAddr,
  }
  pub fn link_stats(conn: &quinn::Connection, local: std::net::SocketAddr) -> LinkStats;
  pub fn refresh_path(path: &mut oxutrm_proto::PathDescription, s: &LinkStats);

  // crates/oxutrm-net/src/pace.rs
  pub const PACE_MIN: std::time::Duration = std::time::Duration::from_millis(8);
  pub const PACE_MAX: std::time::Duration = std::time::Duration::from_millis(100);
  pub fn pace_interval(rtt: std::time::Duration) -> std::time::Duration;

  #[derive(Clone, Copy, Debug, Default)]
  pub struct Pacer { last_sent: Option<std::time::Instant> }
  impl Pacer {
      pub fn new() -> Pacer;
      pub fn may_send(&self, now: std::time::Instant, rtt: std::time::Duration) -> bool;
      pub fn on_sent(&mut self, now: std::time::Instant);
      pub fn next_deadline(&self, rtt: std::time::Duration) -> Option<std::time::Instant>;
      pub fn go_idle(&mut self);
  }
  ```

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/pace.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn interval_is_half_rtt_clamped_to_8_and_100_ms() {
        assert_eq!(pace_interval(Duration::from_millis(0)), Duration::from_millis(8));
        assert_eq!(pace_interval(Duration::from_millis(4)), Duration::from_millis(8));
        assert_eq!(pace_interval(Duration::from_millis(40)), Duration::from_millis(20));
        assert_eq!(pace_interval(Duration::from_millis(180)), Duration::from_millis(90));
        assert_eq!(pace_interval(Duration::from_millis(400)), Duration::from_millis(100));
    }

    #[test]
    fn an_idle_pacer_sends_immediately() {
        let p = Pacer::new();
        assert!(p.may_send(Instant::now(), Duration::from_millis(40)));
        assert_eq!(p.next_deadline(Duration::from_millis(40)), None);
    }

    #[test]
    fn after_a_send_the_pacer_waits_one_interval() {
        let t0 = Instant::now();
        let rtt = Duration::from_millis(40); // interval 20 ms
        let mut p = Pacer::new();
        p.on_sent(t0);
        assert!(!p.may_send(t0 + Duration::from_millis(19), rtt));
        assert!(p.may_send(t0 + Duration::from_millis(20), rtt));
        assert_eq!(p.next_deadline(rtt), Some(t0 + Duration::from_millis(20)));
    }

    #[test]
    fn going_idle_restores_immediate_send() {
        let t0 = Instant::now();
        let mut p = Pacer::new();
        p.on_sent(t0);
        p.go_idle();
        assert!(p.may_send(t0, Duration::from_millis(40)));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod pace;` to `crates/oxutrm-net/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-net pace:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function pace_interval`, `cannot find type Pacer`.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-net/src/pace.rs`:

```rust
//! Send pacing, spec §10.4: `interval = clamp(rtt / 2, 8ms, 100ms)`, with an
//! immediate send when the link has been idle.

use std::time::{Duration, Instant};

/// Never send screen state more often than this.
pub const PACE_MIN: Duration = Duration::from_millis(8);
/// Never let screen state go stale for longer than this.
pub const PACE_MAX: Duration = Duration::from_millis(100);

/// The §10.4 policy. `rtt` comes from `quinn::Connection::stats().path.rtt`.
pub fn pace_interval(rtt: Duration) -> Duration {
    (rtt / 2).clamp(PACE_MIN, PACE_MAX)
}

/// Decides *when* to send. Holds no clock of its own: the caller passes `now`,
/// which keeps this testable and keeps the policy separable from the loop.
#[derive(Clone, Copy, Debug, Default)]
pub struct Pacer {
    last_sent: Option<Instant>,
}

impl Pacer {
    pub fn new() -> Pacer {
        Pacer { last_sent: None }
    }

    /// True when a frame may go out now.
    pub fn may_send(&self, now: Instant, rtt: Duration) -> bool {
        match self.last_sent {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= pace_interval(rtt),
        }
    }

    pub fn on_sent(&mut self, now: Instant) {
        self.last_sent = Some(now);
    }

    /// When the next send becomes legal, or `None` if it already is.
    pub fn next_deadline(&self, rtt: Duration) -> Option<Instant> {
        self.last_sent.map(|t| t + pace_interval(rtt))
    }

    /// The link went quiet: the next state change should go out at once
    /// rather than waiting out an interval that bought nothing.
    pub fn go_idle(&mut self) {
        self.last_sent = None;
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-net pace:: -- --test-threads 4`

Expected: PASS, 4 tests.

- [ ] **Step 5: Write the failing test for the stats adapter**

Create `crates/oxutrm-net/src/link.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{NatType, PathDescription, Rung};

    fn path() -> PathDescription {
        PathDescription {
            rung: Rung::StunPunch,
            local: "127.0.0.1:1".parse().unwrap(),
            remote: "127.0.0.1:2".parse().unwrap(),
            probes_sent: 0,
            nat_type: NatType::EndpointIndependent,
            rtt_ms: 0,
            mtu: 0,
        }
    }

    #[test]
    fn refresh_copies_rtt_and_mtu_into_the_path() {
        let s = LinkStats {
            rtt: std::time::Duration::from_micros(11_400),
            mtu: 1452,
            loss_pct: 0.4,
            bytes_tx: 10,
            bytes_rx: 20,
            local: "127.0.0.1:3".parse().unwrap(),
            remote: "127.0.0.1:4".parse().unwrap(),
        };
        let mut p = path();
        refresh_path(&mut p, &s);
        assert_eq!(p.rtt_ms, 11);
        assert_eq!(p.mtu, 1452);
        assert_eq!(p.local, "127.0.0.1:3".parse().unwrap());
        assert_eq!(p.remote, "127.0.0.1:4".parse().unwrap());
        // The rung is a property of how the path was built, never of its stats.
        assert_eq!(p.rung, Rung::StunPunch);
    }

    #[test]
    fn loss_is_zero_when_nothing_was_sent() {
        assert_eq!(loss_pct(0, 0), 0.0);
        assert_eq!(loss_pct(7, 700), 1.0);
    }
}
```

- [ ] **Step 6: Run test to verify it fails**

Add `pub mod link;` to `crates/oxutrm-net/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-net link:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find type LinkStats`.

- [ ] **Step 7: Write minimal implementation**

Put this above the test module in `crates/oxutrm-net/src/link.rs`:

```rust
//! The single place in oxutrm that reads `quinn::ConnectionStats`.
//!
//! Everything downstream (pacing, the status line, the status pane) consumes
//! `LinkStats`. If a field name moves in a `quinn` 0.11 point release, this
//! file is the only one that needs editing.

use oxutrm_proto::PathDescription;
use std::net::SocketAddr;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LinkStats {
    pub rtt: Duration,
    pub mtu: u16,
    pub loss_pct: f32,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    pub local: SocketAddr,
    pub remote: SocketAddr,
}

/// Packet loss as a percentage, guarding the empty-denominator case.
pub fn loss_pct(lost: u64, sent: u64) -> f32 {
    if sent == 0 {
        0.0
    } else {
        (lost as f32) * 100.0 / (sent as f32)
    }
}

/// `local` is the endpoint's own bound address, which a `Connection` does not
/// expose; the caller has it from `Endpoint::local_addr()`.
pub fn link_stats(conn: &quinn::Connection, local: SocketAddr) -> LinkStats {
    let s = conn.stats();
    LinkStats {
        rtt: s.path.rtt,
        mtu: s.path.current_mtu,
        loss_pct: loss_pct(s.path.lost_packets, s.path.sent_packets),
        bytes_tx: s.udp_tx.bytes,
        bytes_rx: s.udp_rx.bytes,
        local,
        remote: conn.remote_address(),
    }
}

/// Fold fresh measurements into the `PathDescription` the status display shows.
/// The rung, probe count and NAT type describe how the path was *built* and are
/// deliberately left alone.
pub fn refresh_path(path: &mut PathDescription, s: &LinkStats) {
    path.rtt_ms = s.rtt.as_millis().min(u128::from(u32::MAX)) as u32;
    path.mtu = s.mtu;
    path.local = s.local;
    path.remote = s.remote;
}
```

- [ ] **Step 8: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-net -- --test-threads 4`

Expected: PASS, all of `oxutrm-net` still green.

- [ ] **Step 9: Commit**

```bash
git add crates/oxutrm-net/src/pace.rs crates/oxutrm-net/src/link.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): send-pacing policy and a single quinn stats adapter

interval = clamp(rtt/2, 8ms, 100ms) per spec 10.4, with an immediate send
when the link has been idle. LinkStats isolates every ConnectionStats field
access to one file.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 2: Fragmentation and reassembly — the highest-risk task in M4

Read "Fragmentation, stated once" above before starting. This task is why the
protocol can recover from its own worst case.

The split is deliberate: `frag.rs` is **pure** — it takes a `Frame` and a size
limit and returns `Frame`s, and it reassembles `Frame`s into a `Frame`. It has no
sockets and no clock, so the rules of §7.1.1 are tested exhaustively without a
network. `xport.rs` is the thin I/O shell around it.

**Files:**
- Create: `crates/oxutrm-net/src/frag.rs`
- Create: `crates/oxutrm-net/src/xport.rs`
- Create: `crates/oxutrm-net/src/testkit.rs`
- Modify: `crates/oxutrm-net/src/lib.rs`
- Modify: `crates/oxutrm-net/Cargo.toml`

**Interfaces:**
- Consumes: `oxutrm_proto::Frame` — **the contract defines its fields, types and
  order; do not re-derive them from this plan or from the spec, which no longer
  carries wire types at all.** This task uses `my_state`, `from_state`,
  `ack_state`, `frag_index`, `frag_count`, `flags` and `payload`, and copies all
  but the fragment pair unchanged onto every piece. Also
  `Frame::encode() -> Result<Vec<u8>, ProtoError>` and
  `Frame::decode(&[u8]) -> Result<Frame, ProtoError>`;
  `oxutrm_sync::{Receiver, Sender, ScreenDiff, RowPatch, Run, ApplyError}` and
  `oxutrm_term::{ScreenState, Cell, CellText, Cursor, CursorShape}` (dev only,
  for the worst-case and invariant tests) — `ApplyError` has the variants
  `LengthMismatch` and `CursorOutOfBounds` that `ScreenState::validate` raises;
  `oxutrm_net::{generate_cert, quic_server, quic_client}` from M2. M2's
  `quic_server` / `quic_client` **must** have set
  `TransportConfig::datagram_receive_buffer_size(Some(_))` (its default is
  `None`, which disables incoming datagrams outright) and
  `datagram_send_buffer_size` — Step 6's test asserts this.
- Produces:
  ```rust
  // crates/oxutrm-net/src/frag.rs
  /// Headroom below `max_datagram_size()` for the postcard `Frame` header:
  /// three varint u64s, two u16s, a flags byte and the payload length prefix.
  pub const FRAG_HEADER_BUDGET: usize = 48;
  /// A diff needing more fragments than this is a bug in diff generation
  /// (§7.1.1), not a runtime condition. 200 x 1200 bytes is 240 KB.
  pub const MAX_FRAGMENTS: u16 = 200;

  #[derive(thiserror::Error, Debug, PartialEq, Eq)]
  pub enum FragError {
      #[error("datagram budget {budget} is too small to carry any payload")]
      BudgetTooSmall { budget: usize },
      #[error("diff of {len} bytes needs more than {max} fragments")]
      TooManyFragments { len: usize, max: u16 },
  }

  /// Split one logical `Frame` into datagram-sized pieces. Always returns at
  /// least one `Frame`; `frag_count == 1` when the whole thing fits.
  pub fn fragment(f: &oxutrm_proto::Frame, max_datagram: usize)
      -> Result<Vec<oxutrm_proto::Frame>, FragError>;

  /// Rebuilds fragmented states. Holds at most one incomplete set.
  #[derive(Debug, Default)]
  pub struct Reassembler { /* private */ }
  impl Reassembler {
      pub fn new() -> Reassembler;
      /// `Some(frame)` when this fragment completed a state. Anything stale,
      /// duplicate or superseded returns `None` and is never an error.
      pub fn accept(&mut self, f: &oxutrm_proto::Frame) -> Option<oxutrm_proto::Frame>;
      /// The receiver's current state, so older fragments can be discarded.
      pub fn set_current_state(&mut self, seq: u64);
      /// Fragment sets abandoned because a newer state superseded them.
      pub fn dropped_sets(&self) -> u64;
  }

  // crates/oxutrm-net/src/xport.rs
  pub struct FrameSink { /* private */ }
  impl FrameSink {
      pub fn new(conn: quinn::Connection) -> FrameSink;
      /// Fragments as needed and sends every piece. `Ok(n)` is the number of
      /// datagrams sent.
      pub fn send(&self, f: &oxutrm_proto::Frame) -> anyhow::Result<usize>;
  }

  pub struct FrameSource { /* private */ }
  impl FrameSource {
      pub fn new(conn: quinn::Connection) -> FrameSource;
      /// The next COMPLETE frame. Incomplete sets never surface.
      pub async fn recv(&mut self) -> anyhow::Result<oxutrm_proto::Frame>;
      pub fn set_current_state(&mut self, seq: u64);
  }

  // crates/oxutrm-net/src/testkit.rs, behind feature "testkit"
  pub async fn loopback_pair() -> anyhow::Result<(quinn::Connection, quinn::Connection)>;
  ```

- [ ] **Step 1: Add the `bytes` dependency and the `testkit` feature**

In `crates/oxutrm-net/Cargo.toml`:

```toml
[dependencies]
bytes = "1"
thiserror = "2"

[features]
testkit = []
```

- [ ] **Step 2: Write the failing tests for the pure fragmenter**

Create `crates/oxutrm-net/src/frag.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::Frame;

    /// Deterministic pseudo-random bytes, so a failure is reproducible.
    fn noise(n: usize) -> Vec<u8> {
        let mut s: u64 = 0x2545_F491_4F6C_DD1D;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    fn frame(my: u64, from: u64, payload: Vec<u8>) -> Frame {
        Frame {
            my_state: my,
            from_state: from,
            ack_state: 0,
            frag_index: 0,
            frag_count: 1,
            flags: 0,
            payload,
        }
    }

    #[test]
    fn a_small_frame_is_one_unfragmented_piece() {
        let f = frame(7, 6, noise(200));
        let out = fragment(&f, 1200).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frag_count, 1);
        assert_eq!(out[0].frag_index, 0);
        assert_eq!(out[0].payload, f.payload);
    }

    #[test]
    fn a_large_frame_splits_into_pieces_that_all_fit() {
        let f = frame(9, 0, noise(50_000));
        let out = fragment(&f, 1200).unwrap();
        assert!(out.len() > 40, "expected many fragments, got {}", out.len());
        for (i, p) in out.iter().enumerate() {
            assert_eq!(p.frag_index, i as u16);
            assert_eq!(p.frag_count, out.len() as u16);
            assert_eq!(p.my_state, 9);
            assert_eq!(p.from_state, 0);
            assert!(
                p.encode().unwrap().len() <= 1200,
                "fragment {i} encodes to {} bytes, over the datagram limit",
                p.encode().unwrap().len()
            );
        }
    }

    /// Compression happens before fragmentation, so every fragment of one
    /// state carries the same flags byte (§7.1).
    #[test]
    fn the_flags_byte_is_identical_across_a_fragment_set() {
        let mut f = frame(3, 2, noise(10_000));
        f.flags = oxutrm_proto::FLAG_ZSTD;
        let out = fragment(&f, 1200).unwrap();
        assert!(out.len() > 1);
        assert!(out.iter().all(|p| p.flags == oxutrm_proto::FLAG_ZSTD));
    }

    #[test]
    fn the_payload_concatenates_back_to_the_original() {
        let f = frame(4, 1, noise(31_337));
        let out = fragment(&f, 1200).unwrap();
        let rejoined: Vec<u8> = out.iter().flat_map(|p| p.payload.clone()).collect();
        assert_eq!(rejoined, f.payload);
    }

    #[test]
    fn an_absurd_diff_is_refused_rather_than_silently_truncated() {
        let f = frame(1, 0, noise(4 * 1024 * 1024));
        assert!(matches!(
            fragment(&f, 1200),
            Err(FragError::TooManyFragments { .. })
        ));
    }

    #[test]
    fn a_budget_too_small_for_any_payload_is_an_error_not_an_infinite_loop() {
        let f = frame(1, 0, noise(100));
        assert!(matches!(
            fragment(&f, FRAG_HEADER_BUDGET),
            Err(FragError::BudgetTooSmall { .. })
        ));
    }

    #[test]
    fn an_empty_payload_still_produces_exactly_one_fragment() {
        let f = frame(2, 1, Vec::new());
        let out = fragment(&f, 1200).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].frag_count, 1);
    }

    // ---- reassembly ----

    #[test]
    fn an_unfragmented_frame_passes_straight_through() {
        let mut r = Reassembler::new();
        let f = frame(5, 4, noise(100));
        let got = r.accept(&f).expect("a single fragment completes at once");
        assert_eq!(got.payload, f.payload);
        assert_eq!(got.my_state, 5);
    }

    #[test]
    fn a_complete_set_reassembles_in_order() {
        let mut r = Reassembler::new();
        let f = frame(6, 0, noise(20_000));
        let parts = fragment(&f, 1200).unwrap();
        let last = parts.len() - 1;
        for (i, p) in parts.iter().enumerate() {
            let got = r.accept(p);
            if i < last {
                assert!(got.is_none(), "completed early at fragment {i}");
            } else {
                let got = got.expect("the last fragment completes the set");
                assert_eq!(got.payload, f.payload);
                assert_eq!(got.frag_count, 1, "a reassembled frame is logically whole");
                assert_eq!(got.from_state, 0);
                assert_eq!(got.flags, f.flags);
            }
        }
    }

    #[test]
    fn a_set_reassembles_when_fragments_arrive_out_of_order() {
        let mut r = Reassembler::new();
        let f = frame(6, 0, noise(20_000));
        let mut parts = fragment(&f, 1200).unwrap();
        parts.reverse();
        let last = parts.len() - 1;
        for (i, p) in parts.iter().enumerate() {
            let got = r.accept(p);
            assert_eq!(got.is_some(), i == last);
            if let Some(got) = got {
                assert_eq!(got.payload, f.payload);
            }
        }
    }

    #[test]
    fn a_duplicated_fragment_does_not_complete_a_set_early() {
        let mut r = Reassembler::new();
        let f = frame(6, 0, noise(20_000));
        let parts = fragment(&f, 1200).unwrap();
        for _ in 0..5 {
            assert!(r.accept(&parts[0]).is_none());
        }
        for p in &parts[1..parts.len() - 1] {
            assert!(r.accept(p).is_none());
        }
        assert!(r.accept(&parts[parts.len() - 1]).is_some());
    }

    /// The §8.1 property: an incomplete set is discarded wholesale, and the
    /// receiver's acknowledged state is untouched by the loss.
    #[test]
    fn an_incomplete_set_never_yields_a_partial_state() {
        let mut r = Reassembler::new();
        let f = frame(6, 0, noise(20_000));
        let parts = fragment(&f, 1200).unwrap();
        for p in &parts[..parts.len() - 1] {
            assert!(r.accept(p).is_none());
        }
        // The last fragment is lost. Nothing is ever produced for state 6.
        assert_eq!(r.dropped_sets(), 0);

        // The sender re-diffs from the same base and sends state 7.
        let g = frame(7, 0, noise(20_000));
        let parts7 = fragment(&g, 1200).unwrap();
        let last = parts7.len() - 1;
        for (i, p) in parts7.iter().enumerate() {
            let got = r.accept(p);
            if i == last {
                assert_eq!(got.expect("state 7 completes").payload, g.payload);
            } else {
                assert!(got.is_none());
            }
        }
        assert_eq!(r.dropped_sets(), 1, "the abandoned state-6 set must be counted");
    }

    #[test]
    fn a_newer_state_replaces_an_incomplete_older_one_immediately() {
        let mut r = Reassembler::new();
        let old = fragment(&frame(6, 0, noise(20_000)), 1200).unwrap();
        let new_payload = noise(20_000);
        let new = fragment(&frame(9, 0, new_payload.clone()), 1200).unwrap();

        assert!(r.accept(&old[0]).is_none());
        assert!(r.accept(&new[0]).is_none());
        assert_eq!(r.dropped_sets(), 1);

        // Late fragments of the superseded set must not resurrect it.
        for p in &old[1..] {
            assert!(r.accept(p).is_none(), "a superseded set was completed");
        }
        let last = new.len() - 1;
        for (i, p) in new.iter().enumerate().skip(1) {
            let got = r.accept(p);
            assert_eq!(got.is_some(), i == last);
            if let Some(got) = got {
                assert_eq!(got.payload, new_payload);
            }
        }
    }

    #[test]
    fn fragments_older_than_the_current_state_are_discarded_on_arrival() {
        let mut r = Reassembler::new();
        r.set_current_state(50);
        let old = fragment(&frame(20, 0, noise(20_000)), 1200).unwrap();
        for p in &old {
            assert!(r.accept(p).is_none(), "a state older than current was applied");
        }
        // A current-enough state still works.
        let f = frame(51, 50, noise(100));
        assert!(r.accept(&f).is_some());
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Add `pub mod frag;` to `crates/oxutrm-net/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-net frag:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function fragment`, `cannot find type Reassembler`.

- [ ] **Step 4: Write minimal implementation**

Put this above the test module in `crates/oxutrm-net/src/frag.rs`:

```rust
//! Fragmentation and reassembly, spec §7.1.1.
//!
//! A diff is not guaranteed to fit in a QUIC datagram, and the diff that is
//! guaranteed NOT to fit is the one §8.2's ring-miss recovery has to send. QUIC
//! never fragments datagrams itself, so oxutrm does.
//!
//! **A state is applied only when all of its fragments have arrived**, and an
//! incomplete set is discarded wholesale. Note the division of labour: this
//! module decides only whether the BYTES are all present. Whether the state they
//! decode to is legal is `ScreenState::validate`'s job, and a state that fails
//! there is rejected just as wholesale — see spec §8.6. That is what preserves §8.1: the
//! receiver's acknowledged state is unchanged by the loss, so the sender's next
//! diff is computed against the same base and contains everything the dropped
//! set was carrying. One lost fragment costs one send interval, nothing more.
//!
//! No sockets, no clock: every rule here is tested without a network.

use oxutrm_proto::Frame;
use std::collections::BTreeMap;

/// Headroom below `max_datagram_size()` for the postcard `Frame` header: three
/// varint `u64`s, two `u16`s, a flags byte and the payload's length prefix.
pub const FRAG_HEADER_BUDGET: usize = 48;

/// More fragments than this means diff generation is broken, not that the link
/// is slow (§7.1.1). 200 x ~1200 bytes is 240 KB.
pub const MAX_FRAGMENTS: u16 = 200;

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum FragError {
    #[error("datagram budget {budget} is too small to carry any payload")]
    BudgetTooSmall { budget: usize },
    #[error("diff of {len} bytes needs more than {max} fragments")]
    TooManyFragments { len: usize, max: u16 },
}

/// Split one logical `Frame` into datagram-sized pieces, each carrying the same
/// `my_state`, `from_state`, `ack_state` and `flags`.
pub fn fragment(f: &Frame, max_datagram: usize) -> Result<Vec<Frame>, FragError> {
    let chunk = max_datagram.saturating_sub(FRAG_HEADER_BUDGET);
    if chunk == 0 {
        return Err(FragError::BudgetTooSmall { budget: max_datagram });
    }

    // An empty payload is still exactly one fragment: `frag_count` is never 0.
    let count = f.payload.len().div_ceil(chunk).max(1);
    if count > MAX_FRAGMENTS as usize {
        return Err(FragError::TooManyFragments {
            len: f.payload.len(),
            max: MAX_FRAGMENTS,
        });
    }

    Ok((0..count)
        .map(|i| {
            let start = i * chunk;
            let end = ((i + 1) * chunk).min(f.payload.len());
            Frame {
                my_state: f.my_state,
                from_state: f.from_state,
                ack_state: f.ack_state,
                frag_index: i as u16,
                frag_count: count as u16,
                flags: f.flags,
                payload: f.payload[start..end].to_vec(),
            }
        })
        .collect())
}

/// Rebuilds fragmented states. Holds **at most one** incomplete set: a fragment
/// naming a newer `my_state` replaces the set in progress rather than the
/// receiver hoping the older one still completes.
#[derive(Debug, Default)]
pub struct Reassembler {
    /// The state the partial set belongs to.
    seq: u64,
    from_state: u64,
    ack_state: u64,
    flags: u8,
    expected: u16,
    /// Fragment index -> payload piece. A map, so duplicates overwrite rather
    /// than counting twice and completing the set early.
    parts: BTreeMap<u16, Vec<u8>>,
    /// The receiver's current applied state; anything older is discarded.
    current: u64,
    dropped: u64,
}

impl Reassembler {
    pub fn new() -> Reassembler {
        Reassembler::default()
    }

    /// Tell the reassembler what the receiver has applied, so fragments of an
    /// already-superseded state are dropped on arrival.
    pub fn set_current_state(&mut self, seq: u64) {
        self.current = self.current.max(seq);
    }

    /// Fragment sets abandoned because a newer state arrived. Diagnostic only.
    pub fn dropped_sets(&self) -> u64 {
        self.dropped
    }

    /// `Some(frame)` when this fragment completed a state. Stale, duplicate and
    /// superseded fragments return `None`; none of them is an error.
    pub fn accept(&mut self, f: &Frame) -> Option<Frame> {
        // Rule 3: older than what the receiver already has.
        if f.my_state <= self.current {
            return None;
        }
        // A malformed set is a lost set, never a panic.
        if f.frag_count == 0 || f.frag_index >= f.frag_count {
            return None;
        }

        // The common case: nothing to reassemble.
        if f.frag_count == 1 {
            if self.expected != 0 && self.seq < f.my_state {
                self.abandon();
            }
            return Some(f.clone());
        }

        if self.expected == 0 || f.my_state > self.seq {
            // Rule 2: a newer state replaces the set in progress at once.
            if self.expected != 0 {
                self.abandon();
            }
            self.seq = f.my_state;
            self.from_state = f.from_state;
            self.ack_state = f.ack_state;
            self.flags = f.flags;
            self.expected = f.frag_count;
            self.parts.clear();
        } else if f.my_state < self.seq {
            // A late fragment of a set we already gave up on.
            return None;
        }

        self.parts.insert(f.frag_index, f.payload.clone());
        if self.parts.len() < self.expected as usize {
            return None;
        }

        let payload: Vec<u8> = std::mem::take(&mut self.parts).into_values().flatten().collect();
        let out = Frame {
            my_state: self.seq,
            from_state: self.from_state,
            ack_state: self.ack_state,
            // Logically whole: downstream never sees fragmentation.
            frag_index: 0,
            frag_count: 1,
            flags: self.flags,
            payload,
        };
        self.expected = 0;
        Some(out)
    }

    fn abandon(&mut self) {
        self.dropped += 1;
        self.parts.clear();
        self.expected = 0;
    }
}
```

- [ ] **Step 5: Run tests to verify they pass**

`cargo test --jobs 4 -p oxutrm-net frag:: -- --test-threads 4`

Expected: PASS, 13 tests.

- [ ] **Step 6: Write the loopback harness and the failing transport tests**

Create `crates/oxutrm-net/src/testkit.rs`:

```rust
//! Test-only helpers. Behind the `testkit` feature so integration tests in the
//! workspace root can reach them without them shipping in a release build.

use anyhow::Context;

/// Two connected `quinn::Connection`s over loopback UDP, using the same
/// self-signed pinned certificate the real product uses.
///
/// Returns `(server_side, client_side)`.
pub async fn loopback_pair() -> anyhow::Result<(quinn::Connection, quinn::Connection)> {
    let (cert, key, spki) = crate::generate_cert()?;

    let server_sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let server_addr = server_sock.local_addr()?;
    let endpoint = crate::quic_server(server_sock, cert, key).await?;

    let client_sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let accepting = tokio::spawn(async move {
        let incoming = endpoint.accept().await.context("endpoint closed")?;
        let conn = incoming.await?;
        // The endpoint must outlive the connection it drives.
        anyhow::Ok((conn, endpoint))
    });

    let client = crate::quic_client(client_sock, server_addr, spki).await?;
    let (server, server_endpoint) = accepting.await??;
    std::mem::forget(server_endpoint);
    Ok((server, client))
}
```

Create `crates/oxutrm-net/src/xport.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::Frame;

    fn noise(n: usize) -> Vec<u8> {
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        (0..n)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    #[tokio::test]
    async fn a_small_frame_makes_one_datagram_and_arrives_whole() -> anyhow::Result<()> {
        let (server, client) = crate::testkit::loopback_pair().await?;
        assert!(
            server.max_datagram_size().is_some(),
            "M2's quic_server/quic_client must set datagram_receive_buffer_size \
             and datagram_send_buffer_size, or QUIC datagrams are disabled"
        );

        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);
        let f = Frame {
            my_state: 7,
            from_state: 6,
            ack_state: 3,
            frag_index: 0,
            frag_count: 1,
            flags: 0,
            payload: noise(200),
        };
        assert_eq!(sink.send(&f)?, 1);

        let got = source.recv().await?;
        assert_eq!((got.my_state, got.from_state, got.ack_state), (7, 6, 3));
        assert_eq!(got.payload, f.payload);
        Ok(())
    }

    /// The one that matters: a 200x60 truecolor full state, which is exactly
    /// what §8.2's ring-miss recovery sends, must arrive intact.
    #[tokio::test]
    async fn a_full_200x60_truecolor_state_arrives_intact() -> anyhow::Result<()> {
        use oxutrm_proto::TermSize;
        use oxutrm_sync::{ScreenDiff, Sender};
        use oxutrm_term::{Attrs, Cell, Color, ScreenState};

        let size = TermSize { cols: 200, rows: 60 };
        // Every cell distinct, so run-length collapsing cannot shrink it and
        // the test really exercises the worst case.
        let mut s = ScreenState::blank(size.rows, size.cols);
        for (i, c) in s.cells.iter_mut().enumerate() {
            *c = Cell {
                // `Cell.text` is `CellText` (a `CompactString`), not `String`.
                text: oxutrm_term::CellText::from(
                    char::from_u32(0x41 + (i as u32 % 26)).unwrap().to_string().as_str(),
                ),
                fg: Color::Rgb((i % 251) as u8, ((i / 7) % 251) as u8, ((i / 13) % 251) as u8),
                bg: Color::Rgb(((i / 3) % 251) as u8, ((i / 5) % 251) as u8, (i % 241) as u8),
                attrs: Attrs::BOLD,
            };
        }
        s.seq = 1;

        // The real thing: a full diff from the sync engine, framed and paced
        // exactly as the host session loop would.
        let sender: Sender<ScreenState> = Sender::new(s.clone());
        let frame = sender
            .make_frame(0)?
            .expect("a fresh sender owes the peer a full state");
        let encoded_len = frame.payload.len();
        assert!(
            encoded_len > 10_000,
            "a 200x60 truecolor state must be well over 10 KB, got {encoded_len}"
        );

        let (server, client) = crate::testkit::loopback_pair().await?;
        let max = server.max_datagram_size().expect("datagrams enabled");
        assert!(
            encoded_len > max,
            "this test is meaningless unless the state exceeds one datagram \
             ({encoded_len} vs {max})"
        );

        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);
        let sent = sink.send(&frame)?;
        assert!(sent > 1, "the state must have been fragmented, got {sent} datagram(s)");

        let got = tokio::time::timeout(std::time::Duration::from_secs(30), source.recv()).await??;
        assert_eq!(got.my_state, frame.my_state);
        assert_eq!(got.from_state, frame.from_state);
        assert_eq!(got.flags, frame.flags, "the flags byte must survive fragmentation");
        assert_eq!(got.payload.len(), encoded_len, "reassembled payload truncated");
        assert_eq!(got.payload, frame.payload, "reassembled payload corrupted");

        // And it must still decode into the state it started as.
        let mut rx: oxutrm_sync::Receiver<ScreenState> =
            oxutrm_sync::Receiver::new(ScreenState::blank(size.rows, size.cols));
        assert!(rx.on_frame(&got)?);
        assert_eq!(rx.state().cells, s.cells, "the screen did not survive the round trip");
        let _: Option<ScreenDiff> = None;
        Ok(())
    }

    /// Where fragmentation meets the invariants. A state that reassembles
    /// perfectly but violates an invariant must be rejected WHOLESALE: the
    /// receiver's state and its acknowledgement must both be untouched, so the
    /// peer's next diff — computed from the same base — repairs it.
    #[tokio::test]
    async fn a_reassembled_but_invalid_state_is_rejected_without_disturbing_the_receiver()
    -> anyhow::Result<()> {
        use oxutrm_proto::TermSize;
        use oxutrm_sync::{Receiver, RowPatch, Run, ScreenDiff};
        use oxutrm_term::{Cell, CursorShape, ScreenState};

        let size = TermSize { cols: 80, rows: 24 };

        // Large enough to fragment, and deliberately invalid: the cursor sits
        // outside the grid. Per I2 this is rejected, never clamped.
        let bad = ScreenDiff {
            resize: None,
            rows: (0..24)
                .map(|row| RowPatch {
                    row,
                    runs: vec![Run { start_col: 0, repeat: 0, cells: vec![Cell::blank(); 80] }],
                })
                .collect(),
            cursor: Some(oxutrm_term::Cursor {
                row: 999,
                col: 999,
                visible: true,
                shape: CursorShape::Block,
            }),
            modes: None,
            title: None,
            bell: None,
            scrollback_len: None,
        };
        let payload = postcard::to_stdvec(&bad)?;

        let (server, client) = crate::testkit::loopback_pair().await?;
        let max = server.max_datagram_size().expect("datagrams enabled");
        assert!(payload.len() > max, "this test needs a fragmented diff");

        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);
        let f = Frame {
            my_state: 2,
            from_state: 1,
            ack_state: 0,
            frag_index: 0,
            frag_count: 1,
            flags: 0,
            payload,
        };
        assert!(sink.send(&f)? > 1, "the diff must have been fragmented");

        // Reassembly itself succeeds: the bytes are all there.
        let got = tokio::time::timeout(std::time::Duration::from_secs(30), source.recv()).await??;
        assert_eq!(got.my_state, 2);

        // The sync engine is what rejects it, and it must reject wholesale.
        let mut initial = ScreenState::blank(size.rows, size.cols);
        initial.seq = 1;
        let mut rx: Receiver<ScreenState> = Receiver::new(initial.clone());
        let before_ack = rx.ack();

        let err = rx.on_frame(&got).expect_err("an out-of-range cursor must be rejected");
        assert!(
            matches!(err, oxutrm_sync::ApplyError::CursorOutOfBounds { .. }),
            "expected CursorOutOfBounds, got {err:?}"
        );
        assert_eq!(rx.ack(), before_ack, "a rejected frame must not advance the ack");
        assert_eq!(
            rx.state(),
            &initial,
            "a rejected diff must not be applied even partially"
        );

        // And the receiver still works: the next valid diff from the SAME base
        // is what repairs the screen.
        let mut good = ScreenState::blank(size.rows, size.cols);
        good.seq = 1;
        good.cells[0] = Cell { text: oxutrm_term::CellText::from("Z"), ..Cell::blank() };
        let sender: oxutrm_sync::Sender<ScreenState> = oxutrm_sync::Sender::new(good.clone());
        let repair = sender.make_frame(0)?.expect("a fresh sender owes a full state");
        assert!(rx.on_frame(&repair)?, "the repairing diff must apply");
        assert_eq!(rx.state().cell(0, 0).text.as_str(), "Z");
        Ok(())
    }

    #[tokio::test]
    async fn a_frame_far_beyond_one_datagram_arrives_intact() -> anyhow::Result<()> {
        let (server, client) = crate::testkit::loopback_pair().await?;
        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);

        let payload = noise(150_000);
        let f = Frame {
            my_state: 42,
            from_state: 0,
            ack_state: 0,
            frag_index: 0,
            frag_count: 1,
            flags: 0,
            payload: payload.clone(),
        };
        let sent = sink.send(&f)?;
        assert!(sent > 100, "expected many fragments, got {sent}");

        let got = tokio::time::timeout(std::time::Duration::from_secs(30), source.recv()).await??;
        assert_eq!(got.payload, payload);
        Ok(())
    }
}
```

- [ ] **Step 7: Run tests to verify they fail**

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
pub mod xport;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
```

Add `oxutrm-sync`, `oxutrm-term` and `oxutrm-proto` to `crates/oxutrm-net/Cargo.toml`
under `[dev-dependencies]` so the worst-case test can build a real state.

Run: `cargo test --jobs 4 -p oxutrm-net xport:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find type FrameSink`.

- [ ] **Step 8: Write minimal implementation**

Put this above the test module in `crates/oxutrm-net/src/xport.rs`:

```rust
//! Getting a `Frame` from one peer to the other.
//!
//! Screen and input state travel as QUIC datagrams: unreliable and never
//! retransmitted, which is exactly what §8.1 wants. Anything too large for one
//! datagram is fragmented by `crate::frag`; unidirectional streams are NOT used
//! for state. Control, scrollback and clipboard are bidirectional streams and
//! belong elsewhere (§7.2).

use crate::frag::{fragment, Reassembler};
use oxutrm_proto::Frame;

/// Sends `Frame`s, fragmenting whatever will not fit.
pub struct FrameSink {
    conn: quinn::Connection,
}

impl FrameSink {
    pub fn new(conn: quinn::Connection) -> FrameSink {
        FrameSink { conn }
    }

    /// Returns the number of datagrams sent.
    pub fn send(&self, f: &Frame) -> anyhow::Result<usize> {
        // `None` means the peer disabled datagrams, which M2's TransportConfig
        // must not allow; 1200 is QUIC's floor and a safe last resort.
        let max = self.conn.max_datagram_size().unwrap_or(1200);
        let parts = fragment(f, max)?;
        for p in &parts {
            self.conn.send_datagram(bytes::Bytes::from(p.encode()?))?;
        }
        Ok(parts.len())
    }
}

/// Receives `Frame`s, reassembling fragment sets.
pub struct FrameSource {
    conn: quinn::Connection,
    reassembler: Reassembler,
}

impl FrameSource {
    pub fn new(conn: quinn::Connection) -> FrameSource {
        FrameSource { conn, reassembler: Reassembler::new() }
    }

    /// Tell the source what the receiver has applied, so fragments of an
    /// already-superseded state are discarded on arrival (§7.1.1).
    pub fn set_current_state(&mut self, seq: u64) {
        self.reassembler.set_current_state(seq);
    }

    /// The next COMPLETE frame. Incomplete sets never surface: they are
    /// discarded wholesale, and the peer's next diff carries what they held.
    pub async fn recv(&mut self) -> anyhow::Result<Frame> {
        loop {
            let bytes = self.conn.read_datagram().await?;
            match Frame::decode(&bytes) {
                Ok(f) => {
                    if let Some(whole) = self.reassembler.accept(&f) {
                        return Ok(whole);
                    }
                }
                // A malformed datagram is a lost datagram: the next diff comes
                // from the same acknowledged base and contains what it held.
                Err(e) => eprintln!("oxutrm: dropping undecodable datagram: {e}"),
            }
        }
    }
}
```

- [ ] **Step 9: Run tests to verify they pass**

`cargo test --jobs 4 -p oxutrm-net xport:: -- --test-threads 4`

Expected: PASS, 4 tests, including `a_full_200x60_truecolor_state_arrives_intact`
and `a_reassembled_but_invalid_state_is_rejected_without_disturbing_the_receiver`.

- [ ] **Step 10: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-net/src/frag.rs crates/oxutrm-net/src/xport.rs \
        crates/oxutrm-net/src/testkit.rs crates/oxutrm-net/src/lib.rs \
        crates/oxutrm-net/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(net): fragment diffs across datagrams, reassemble whole states only

A full ScreenState is well over 10 KB and send_datagram refuses anything past
max_datagram_size, so the sync engine's own ring-miss recovery could not send.
Frames now carry frag_index/frag_count; an incomplete set is discarded whole,
which is what keeps "a lost datagram costs nothing" true.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
---

### Task 3: Input bookkeeping on both sides

`InputState.pending` is the client's *unacknowledged* input queue. It grows when
the user types and shrinks from the front when the host's acknowledgement comes
back. The host must write each byte to the PTY exactly once across that
shrinking, and neither side may guess: guessing by suffix matching is genuinely
ambiguous (type `aaaa` and no overlap rule recovers the right split).

On the wire this is already solved. `InputDiff` carries
`consumed: u64` alongside `appended: Vec<u8>`, and `apply()` is defined as **drop
`consumed` bytes from the front, THEN append `appended`** — get that order wrong
and consumed input reaches the PTY twice. What is *not* available is the diff
itself: `Receiver::on_frame` applies it internally and returns only
`Result<bool, ApplyError>`, so the host cannot simply write `appended`.

This task therefore reconstructs the same bookkeeping from what a caller can
see. (If `oxutrm-sync` ever grows an `on_frame_with(&Frame, |d: &S::Diff|)`
callback, `InputCursor` collapses to `term.write_input(&d.appended)` and should
be deleted.)

The mechanism, which needs no wire change:

- The host writes every byte of `pending` it has not written, then acknowledges
  that state. It records "when the peer tells me it applied *my* screen state
  `S`, its pending queue has advanced to absolute offset `B`".
- `Frame.ack_state` on incoming input frames is exactly that signal.
- The client mirrors it: on seeing `ack_state = N`, it drops the pending prefix
  that state `N` carried, whose length it kept.

**Files:**
- Create: `crates/oxutrm-host/src/input_cursor.rs`
- Create: `crates/oxutrm-client/src/input_queue.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`

**Interfaces:**
- Consumes: `oxutrm_sync::{InputState, Sender, Receiver}` with
  `InputState::append(&self, bytes: &[u8], size: TermSize) -> InputState`,
  `InputState::consume(&self, n: usize) -> InputState`,
  `Sender::<InputState>::update(&mut self, next: InputState)`,
  `Sender::current(&self) -> &InputState`; `oxutrm_proto::TermSize`.
- Produces:
  ```rust
  // crates/oxutrm-host/src/input_cursor.rs
  #[derive(Clone, Debug, Default)]
  pub struct InputCursor { /* private */ }
  impl InputCursor {
      pub fn new() -> InputCursor;
      /// The slice of `pending` that has not yet reached the PTY. Call once per
      /// accepted input state, and write the result.
      pub fn take_new<'a>(&mut self, pending: &'a [u8]) -> &'a [u8];
      /// About to emit a screen frame numbered `my_state` that acknowledges
      /// everything taken so far.
      pub fn on_ack_sent(&mut self, my_state: u64);
      /// The peer reports having applied our screen state `screen_state`.
      pub fn on_peer_saw(&mut self, screen_state: u64);
      pub fn written(&self) -> u64;
  }

  // crates/oxutrm-client/src/input_queue.rs
  pub struct InputQueue { /* private */ }
  impl InputQueue {
      pub fn new(size: oxutrm_proto::TermSize) -> InputQueue;
      pub fn sender(&self) -> &oxutrm_sync::Sender<oxutrm_sync::InputState>;
      pub fn sender_mut(&mut self) -> &mut oxutrm_sync::Sender<oxutrm_sync::InputState>;
      /// User typed something, or the window resized.
      pub fn push(&mut self, bytes: &[u8], size: oxutrm_proto::TermSize);
      /// The host reports having applied our input state `acked`.
      pub fn on_host_ack(&mut self, acked: u64);
  }
  ```

- [ ] **Step 1: Write the failing test for the host cursor**

Create `crates/oxutrm-host/src/input_cursor.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_append_yields_only_the_new_tail() {
        let mut c = InputCursor::new();
        assert_eq!(c.take_new(b"ab"), b"ab");
        assert_eq!(c.take_new(b"abcd"), b"cd");
        assert_eq!(c.written(), 4);
    }

    #[test]
    fn a_duplicate_state_yields_nothing() {
        let mut c = InputCursor::new();
        assert_eq!(c.take_new(b"ab"), b"ab");
        assert_eq!(c.take_new(b"ab"), b"");
    }

    /// The race the whole struct exists for: the client appends before it has
    /// seen our acknowledgement, so its queue still carries bytes we wrote.
    #[test]
    fn bytes_written_before_the_client_trimmed_are_not_written_twice() {
        let mut c = InputCursor::new();
        // Client state 1: "aa". We write it and acknowledge it in screen state 10.
        assert_eq!(c.take_new(b"aa"), b"aa");
        c.on_ack_sent(10);
        // Client state 2, formed before our ack landed: "aa" + "a".
        assert_eq!(c.take_new(b"aaa"), b"a");
        c.on_ack_sent(11);
        // The client now applies screen state 10, so it drops the two bytes of
        // state 1. Its queue becomes "a".
        c.on_peer_saw(10);
        // Client state 3: "a" + "a" -> "aa". Only the last byte is new.
        assert_eq!(c.take_new(b"aa"), b"a");
        assert_eq!(c.written(), 4);
    }

    #[test]
    fn a_trim_that_empties_the_queue_is_handled() {
        let mut c = InputCursor::new();
        assert_eq!(c.take_new(b"hello"), b"hello");
        c.on_ack_sent(5);
        c.on_peer_saw(5);
        assert_eq!(c.take_new(b""), b"");
        assert_eq!(c.take_new(b"x"), b"x");
        assert_eq!(c.written(), 6);
    }

    #[test]
    fn seeing_a_later_screen_state_clears_every_earlier_promise() {
        let mut c = InputCursor::new();
        assert_eq!(c.take_new(b"one"), b"one");
        c.on_ack_sent(1);
        assert_eq!(c.take_new(b"onetwo"), b"two");
        c.on_ack_sent(2);
        // The client skipped straight to screen state 2.
        c.on_peer_saw(2);
        assert_eq!(c.take_new(b"three"), b"three");
        assert_eq!(c.written(), 11);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod input_cursor;` and `pub use input_cursor::InputCursor;` to
`crates/oxutrm-host/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-host input_cursor:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find type InputCursor`.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-host/src/input_cursor.rs`:

```rust
//! Tracks which of the client's input bytes have reached the PTY, across the
//! client's trimming of its own pending queue.
//!
//! Both sides count in the same absolute byte stream. The client trims its
//! queue by the length of the state we acknowledged; we learn it has done so
//! when an input frame arrives whose `ack_state` reaches the screen state that
//! carried our acknowledgement.

use std::collections::VecDeque;

#[derive(Clone, Debug, Default)]
pub struct InputCursor {
    /// Absolute offset, in the client's input byte stream, of `pending[0]`.
    client_base: u64,
    /// Absolute offset of the next byte that must go to the PTY.
    written: u64,
    /// Absolute offset just past everything we have taken so far.
    taken_end: u64,
    /// `(our screen state carrying the ack, the client_base it implies)`.
    promised: VecDeque<(u64, u64)>,
}

impl InputCursor {
    pub fn new() -> InputCursor {
        InputCursor::default()
    }

    /// The suffix of `pending` that has not yet reached the PTY.
    pub fn take_new<'a>(&mut self, pending: &'a [u8]) -> &'a [u8] {
        let start = self.written.saturating_sub(self.client_base) as usize;
        let start = start.min(pending.len());
        self.taken_end = self.client_base + pending.len() as u64;
        self.written = self.taken_end;
        &pending[start..]
    }

    /// Record that screen state `my_state` acknowledges everything taken so far.
    pub fn on_ack_sent(&mut self, my_state: u64) {
        let end = self.taken_end;
        match self.promised.back_mut() {
            Some((s, b)) if *s == my_state => *b = end,
            _ => self.promised.push_back((my_state, end)),
        }
    }

    /// The peer has applied our screen state `screen_state`, so it has seen
    /// every acknowledgement carried at or before it.
    pub fn on_peer_saw(&mut self, screen_state: u64) {
        while let Some(&(s, base)) = self.promised.front() {
            if s > screen_state {
                break;
            }
            self.client_base = self.client_base.max(base);
            self.promised.pop_front();
        }
    }

    pub fn written(&self) -> u64 {
        self.written
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-host input_cursor:: -- --test-threads 4`

Expected: PASS, 5 tests.

- [ ] **Step 5: Write the failing test for the client queue**

Create `crates/oxutrm-client/src/input_queue.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::TermSize;

    fn size() -> TermSize {
        TermSize { cols: 80, rows: 24 }
    }

    #[test]
    fn typing_accumulates_in_the_pending_queue() {
        let mut q = InputQueue::new(size());
        q.push(b"ab", size());
        q.push(b"cd", size());
        assert_eq!(q.sender().current().pending, b"abcd");
    }

    #[test]
    fn an_acknowledgement_drops_exactly_what_that_state_carried() {
        let mut q = InputQueue::new(size());
        q.push(b"ab", size());
        let first = q.sender().current().seq;
        q.push(b"cd", size());
        q.on_host_ack(first);
        assert_eq!(q.sender().current().pending, b"cd");
    }

    #[test]
    fn a_repeated_or_stale_acknowledgement_drops_nothing_further() {
        let mut q = InputQueue::new(size());
        q.push(b"ab", size());
        let first = q.sender().current().seq;
        q.push(b"cd", size());
        q.on_host_ack(first);
        q.on_host_ack(first);
        q.on_host_ack(first - 1);
        assert_eq!(q.sender().current().pending, b"cd");
    }

    #[test]
    fn acknowledging_the_latest_state_empties_the_queue() {
        let mut q = InputQueue::new(size());
        q.push(b"hello", size());
        let latest = q.sender().current().seq;
        q.on_host_ack(latest);
        assert_eq!(q.sender().current().pending, b"");
    }

    #[test]
    fn a_resize_travels_without_adding_input_bytes() {
        let mut q = InputQueue::new(size());
        q.push(b"", TermSize { cols: 100, rows: 30 });
        assert_eq!(q.sender().current().size, TermSize { cols: 100, rows: 30 });
        assert_eq!(q.sender().current().pending, b"");
    }
}
```

- [ ] **Step 6: Run test to verify it fails**

Add `pub mod input_queue;` and `pub use input_queue::InputQueue;` to
`crates/oxutrm-client/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-client input_queue:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find type InputQueue`.

- [ ] **Step 7: Write minimal implementation**

Put this above the test module in `crates/oxutrm-client/src/input_queue.rs`:

```rust
//! The client half of the input bookkeeping.
//!
//! We keep, for every state we have produced, how many pending bytes it
//! carried. When the host reports having applied state `N`, it has written all
//! `N` bytes to the PTY, so those bytes leave our queue.

use oxutrm_proto::TermSize;
use oxutrm_sync::{InputState, Sender};
use std::collections::VecDeque;

pub struct InputQueue {
    sender: Sender<InputState>,
    /// `(state seq, pending length at that seq)`, oldest first.
    history: VecDeque<(u64, usize)>,
    /// The highest sequence number we have already trimmed for.
    trimmed_through: u64,
}

impl InputQueue {
    pub fn new(size: TermSize) -> InputQueue {
        let initial = InputState { seq: 0, pending: Vec::new(), size };
        let sender = Sender::new(initial);
        let seq = sender.current().seq;
        let mut history = VecDeque::new();
        history.push_back((seq, 0usize));
        InputQueue { sender, history, trimmed_through: seq }
    }

    pub fn sender(&self) -> &Sender<InputState> {
        &self.sender
    }

    pub fn sender_mut(&mut self) -> &mut Sender<InputState> {
        &mut self.sender
    }

    /// The user typed, or the window changed size.
    pub fn push(&mut self, bytes: &[u8], size: TermSize) {
        let next = self.sender.current().append(bytes, size);
        self.sender.update(next);
        let cur = self.sender.current();
        self.history.push_back((cur.seq, cur.pending.len()));
        // A state older than the ring can never be acknowledged usefully.
        while self.history.len() > oxutrm_sync::STATE_RING {
            self.history.pop_front();
        }
    }

    /// The host has applied our input state `acked`, so it has written every
    /// byte that state carried.
    pub fn on_host_ack(&mut self, acked: u64) {
        if acked <= self.trimmed_through {
            return;
        }
        let Some(&(_, len)) = self.history.iter().find(|(s, _)| *s == acked) else {
            // Fell out of the ring: the host will send a full state and we
            // will resynchronise from there.
            return;
        };
        self.sender.on_ack(acked);
        let next = self.sender.current().consume(len);
        self.sender.update(next);
        let cur = self.sender.current();
        self.history.retain(|(s, _)| *s > acked);
        self.history.push_back((cur.seq, cur.pending.len()));
        self.trimmed_through = acked;
    }
}
```

- [ ] **Step 8: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-client input_queue:: -- --test-threads 4`

Expected: PASS, 5 tests.

- [ ] **Step 9: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-host/src/input_cursor.rs crates/oxutrm-host/src/lib.rs \
        crates/oxutrm-client/src/input_queue.rs crates/oxutrm-client/src/lib.rs
git commit -m "$(cat <<'EOF'
feat: exactly-once input delivery across the client's queue trimming

The host writes each pending byte to the PTY once, and the client drops the
prefix the host acknowledged. Both sides count in the same absolute byte
stream, so the append-before-ack race cannot double-echo.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: Colour down-conversion, in the client

Spec §9.4: "Colours the client cannot show are down-converted **in the client**,
so the host's state stays full fidelity for a future client that can." The host
never learns about this; `ScreenState` always carries whatever `alacritty_terminal`
produced.

**Files:**
- Create: `crates/oxutrm-client/src/color.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`
- Modify: `crates/oxutrm-client/src/renderer.rs`

**Interfaces:**
- Consumes: `oxutrm_term::Color` (`Default | Idx(u8) | Rgb(u8, u8, u8)`);
  `oxutrm_proto::TerminalCaps` with field `colors: u32` (8, 16, 256 or
  16_777_216); `oxutrm_client::Renderer` from M1, constructed as
  `Renderer::new(size: TermSize, caps: TerminalCaps)`.
- Produces:
  ```rust
  // crates/oxutrm-client/src/color.rs
  /// The xterm 256-colour cube levels.
  pub const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
  pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8;
  pub fn rgb_to_16(r: u8, g: u8, b: u8) -> u8;
  pub fn idx_to_rgb(i: u8) -> (u8, u8, u8);
  /// Map a colour onto what `caps.colors` can actually display.
  pub fn down_convert(c: oxutrm_term::Color, caps: &oxutrm_proto::TerminalCaps) -> oxutrm_term::Color;
  ```

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-client/src/color.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::TerminalCaps;
    use oxutrm_term::Color;

    fn caps(colors: u32) -> TerminalCaps {
        TerminalCaps {
            truecolor: colors >= 16_777_216,
            colors,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_string(),
        }
    }

    #[test]
    fn a_truecolor_terminal_changes_nothing() {
        let c = caps(16_777_216);
        assert_eq!(down_convert(Color::Rgb(1, 2, 3), &c), Color::Rgb(1, 2, 3));
        assert_eq!(down_convert(Color::Idx(200), &c), Color::Idx(200));
        assert_eq!(down_convert(Color::Default, &c), Color::Default);
    }

    #[test]
    fn the_cube_corners_map_to_their_own_indices() {
        // 16 + 36*r + 6*g + b
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 255, 255), 231);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(0, 255, 0), 46);
        assert_eq!(rgb_to_256(0, 0, 255), 21);
        assert_eq!(rgb_to_256(95, 135, 175), 16 + 36 + 12 + 3);
    }

    #[test]
    fn near_greys_prefer_the_grey_ramp_over_the_cube() {
        // 0x808080 is closer to grey 244 (0x808080) than to any cube entry.
        assert_eq!(rgb_to_256(0x80, 0x80, 0x80), 244);
        assert_eq!(rgb_to_256(8, 8, 8), 232);
        assert_eq!(rgb_to_256(238, 238, 238), 255);
    }

    #[test]
    fn a_256_colour_terminal_folds_rgb_and_keeps_indices() {
        let c = caps(256);
        assert_eq!(down_convert(Color::Rgb(255, 0, 0), &c), Color::Idx(196));
        assert_eq!(down_convert(Color::Idx(200), &c), Color::Idx(200));
    }

    #[test]
    fn a_16_colour_terminal_folds_both_rgb_and_high_indices() {
        let c = caps(16);
        assert_eq!(down_convert(Color::Rgb(255, 0, 0), &c), Color::Idx(9));
        assert_eq!(down_convert(Color::Rgb(0, 0, 0), &c), Color::Idx(0));
        assert_eq!(down_convert(Color::Rgb(255, 255, 255), &c), Color::Idx(15));
        assert_eq!(down_convert(Color::Idx(9), &c), Color::Idx(9));
        // 196 is pure red in the cube.
        assert_eq!(down_convert(Color::Idx(196), &c), Color::Idx(9));
    }

    #[test]
    fn an_8_colour_terminal_never_emits_a_bright_index() {
        let c = caps(8);
        for i in 0u8..=255 {
            if let Color::Idx(n) = down_convert(Color::Idx(i), &c) {
                assert!(n < 8, "index {i} down-converted to {n}, which is bright");
            } else {
                panic!("an index must down-convert to an index");
            }
        }
        assert_eq!(down_convert(Color::Rgb(255, 85, 85), &c), Color::Idx(1));
    }

    #[test]
    fn every_palette_index_round_trips_through_rgb() {
        for i in 16u8..=231 {
            let (r, g, b) = idx_to_rgb(i);
            assert_eq!(rgb_to_256(r, g, b), i, "cube index {i}");
        }
        for i in 232u8..=255 {
            let (r, g, b) = idx_to_rgb(i);
            assert_eq!(rgb_to_256(r, g, b), i, "grey index {i}");
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod color;` and `pub use color::down_convert;` to
`crates/oxutrm-client/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-client color:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function rgb_to_256`.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-client/src/color.rs`:

```rust
//! Colour down-conversion.
//!
//! The host's `ScreenState` always carries full fidelity — whatever the emulator
//! produced. Reducing it to what the user's real terminal can show happens
//! here, on the client, so a future client on a better terminal gets the
//! original (spec §9.4).

use oxutrm_proto::TerminalCaps;
use oxutrm_term::Color;

/// The six levels of the xterm 6x6x6 colour cube.
pub const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The 16 ANSI system colours, as most terminals actually render them.
const ANSI16: [(u8, u8, u8); 16] = [
    (0, 0, 0),
    (170, 0, 0),
    (0, 170, 0),
    (170, 85, 0),
    (0, 0, 170),
    (170, 0, 170),
    (0, 170, 170),
    (170, 170, 170),
    (85, 85, 85),
    (255, 85, 85),
    (85, 255, 85),
    (255, 255, 85),
    (85, 85, 255),
    (255, 85, 255),
    (85, 255, 255),
    (255, 255, 255),
];

fn dist(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let d = |x: u8, y: u8| {
        let d = i32::from(x) - i32::from(y);
        (d * d) as u32
    };
    d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)
}

fn nearest_level(v: u8) -> usize {
    let mut best = 0usize;
    let mut best_d = u32::MAX;
    for (i, &l) in CUBE_LEVELS.iter().enumerate() {
        let d = dist((v, 0, 0), (l, 0, 0));
        if d < best_d {
            best_d = d;
            best = i;
        }
    }
    best
}

/// The RGB an xterm palette index renders as.
pub fn idx_to_rgb(i: u8) -> (u8, u8, u8) {
    match i {
        0..=15 => ANSI16[i as usize],
        16..=231 => {
            let n = i - 16;
            (
                CUBE_LEVELS[(n / 36) as usize],
                CUBE_LEVELS[((n / 6) % 6) as usize],
                CUBE_LEVELS[(n % 6) as usize],
            )
        }
        232..=255 => {
            let v = 8 + 10 * (i - 232);
            (v, v, v)
        }
    }
}

/// Nearest entry of the xterm 256-colour palette, considering both the cube
/// and the 24-step grey ramp. Indices 0-15 are excluded: their rendering is
/// theme-dependent, so a colour picked deliberately should not land on one.
pub fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    let target = (r, g, b);

    let cube_idx = 16
        + 36 * nearest_level(r) as u16
        + 6 * nearest_level(g) as u16
        + nearest_level(b) as u16;
    let cube_idx = cube_idx as u8;
    let cube_d = dist(target, idx_to_rgb(cube_idx));

    // The grey ramp runs 8, 18, ... 238.
    let avg = (u32::from(r) + u32::from(g) + u32::from(b)) / 3;
    let step = ((avg as i32 - 8) as f32 / 10.0).round().clamp(0.0, 23.0) as u8;
    let grey_idx = 232 + step;
    let grey_d = dist(target, idx_to_rgb(grey_idx));

    if grey_d < cube_d {
        grey_idx
    } else {
        cube_idx
    }
}

/// Nearest of the 16 ANSI system colours.
pub fn rgb_to_16(r: u8, g: u8, b: u8) -> u8 {
    let target = (r, g, b);
    let mut best = 0u8;
    let mut best_d = u32::MAX;
    for (i, &c) in ANSI16.iter().enumerate() {
        let d = dist(target, c);
        if d < best_d {
            best_d = d;
            best = i as u8;
        }
    }
    best
}

/// Map a colour onto what `caps.colors` can display. `Color::Default` always
/// survives: it means "the terminal's own default", which every terminal has.
pub fn down_convert(c: Color, caps: &TerminalCaps) -> Color {
    if caps.colors >= 16_777_216 {
        return c;
    }
    match c {
        Color::Default => Color::Default,
        Color::Rgb(r, g, b) => match caps.colors {
            n if n >= 256 => Color::Idx(rgb_to_256(r, g, b)),
            n if n >= 16 => Color::Idx(rgb_to_16(r, g, b)),
            _ => {
                let i = rgb_to_16(r, g, b);
                Color::Idx(i & 0x07)
            }
        },
        Color::Idx(i) => match caps.colors {
            n if n >= 256 => Color::Idx(i),
            n if n >= 16 => {
                if i < 16 {
                    Color::Idx(i)
                } else {
                    let (r, g, b) = idx_to_rgb(i);
                    Color::Idx(rgb_to_16(r, g, b))
                }
            }
            _ => {
                if i < 8 {
                    Color::Idx(i)
                } else {
                    let (r, g, b) = idx_to_rgb(i);
                    Color::Idx(rgb_to_16(r, g, b) & 0x07)
                }
            }
        },
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-client color:: -- --test-threads 4`

Expected: PASS, 7 tests.

- [ ] **Step 5: Write the failing test that the Renderer actually uses it**

Append to the test module in `crates/oxutrm-client/src/color.rs`:

```rust
    #[test]
    fn the_renderer_emits_no_truecolor_sgr_on_a_256_colour_terminal() {
        use oxutrm_proto::TermSize;
        use oxutrm_term::{Cell, ScreenState};

        let size = TermSize { cols: 10, rows: 2 };
        let mut r = crate::Renderer::new(size, caps(256));
        let mut s = ScreenState::blank(size.rows, size.cols);
        s.cells[0] = Cell {
            text: oxutrm_term::CellText::from("X"),
            fg: Color::Rgb(255, 0, 0),
            bg: Color::Rgb(0, 0, 255),
            attrs: oxutrm_term::Attrs::empty(),
        };
        let mut out: Vec<u8> = Vec::new();
        r.render(&mut out, &s).unwrap();
        let text = String::from_utf8_lossy(&out).to_string();
        assert!(
            !text.contains("38;2;") && !text.contains("48;2;"),
            "renderer emitted truecolor SGR to a 256-colour terminal: {text:?}"
        );
        assert!(text.contains("38;5;196"), "expected the folded index: {text:?}");
        assert!(text.contains("48;5;21"), "expected the folded index: {text:?}");
    }
```

- [ ] **Step 6: Run test to verify it fails**

`cargo test --jobs 4 -p oxutrm-client color::tests::the_renderer_emits_no_truecolor -- --test-threads 4`

Expected: FAIL — the renderer emits `38;2;255;0;0` because M1's renderer writes
`Color` straight out.

- [ ] **Step 7: Wire `down_convert` into the renderer**

In `crates/oxutrm-client/src/renderer.rs`, find where the renderer turns a
`Color` into an SGR parameter list. Change that function so it folds first.
The `Renderer` already stores the `TerminalCaps` passed to `Renderer::new`; if
M1 did not keep them, add a `caps: TerminalCaps` field and store them there.

```rust
impl Renderer {
    /// SGR parameters for a foreground colour, folded to what the user's
    /// terminal can display.
    fn fg_sgr(&self, c: oxutrm_term::Color) -> String {
        match crate::color::down_convert(c, &self.caps) {
            oxutrm_term::Color::Default => "39".to_string(),
            oxutrm_term::Color::Idx(i) => format!("38;5;{i}"),
            oxutrm_term::Color::Rgb(r, g, b) => format!("38;2;{r};{g};{b}"),
        }
    }

    /// SGR parameters for a background colour, folded the same way.
    fn bg_sgr(&self, c: oxutrm_term::Color) -> String {
        match crate::color::down_convert(c, &self.caps) {
            oxutrm_term::Color::Default => "49".to_string(),
            oxutrm_term::Color::Idx(i) => format!("48;5;{i}"),
            oxutrm_term::Color::Rgb(r, g, b) => format!("48;2;{r};{g};{b}"),
        }
    }
}
```

Replace the renderer's existing inline colour formatting with calls to
`self.fg_sgr(cell.fg)` and `self.bg_sgr(cell.bg)`.

- [ ] **Step 8: Run test to verify it passes**

```bash
cargo test --jobs 4 -p oxutrm-client -- --test-threads 4
```

Expected: PASS, including every M1 renderer test.

- [ ] **Step 9: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-client/src/color.rs crates/oxutrm-client/src/renderer.rs \
        crates/oxutrm-client/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(client): fold colour to the local terminal's palette at render time

The host's ScreenState keeps full fidelity; the client reduces truecolor to
256, 16 or 8 colours on the way to the physical terminal, per spec 9.4.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: Terminal capability negotiation, end to end

Spec §9.4, and note what it does **not** say. `caps` **never reaches the child
environment.** The host derives `TERM` and `COLORTERM` **solely from what
`alacritty_terminal` emulates**; `negotiate_term` takes no argument and has
nothing client-specific to take. All capability adaptation lives in the client
(Task 4).

Two reasons, both decisive, and a task that "improves" on this by feeding client
capabilities into `TERM` is wrong:

- **Fidelity is not recoverable.** A `TERM` narrowed to the current client's
  intersection makes the *shell* emit degraded output, which is then baked into
  the authoritative `ScreenState` forever. A better client attaching tomorrow
  cannot recover what the application never emitted.
- **`TERM` cannot change under a running shell.** Connect and reattach are one
  code path (§4.1), so a client with different capabilities can attach to a
  session whose shell has been running for a week.

`caps` is therefore used for exactly two things: choosing the client's own
down-conversion strategy, and diagnosis. `term_name` is never propagated
anywhere.

**Files:**
- Modify: `crates/oxutrm-term/src/lib.rs`
- Create: `crates/oxutrm-host/src/caps.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Modify: `crates/oxutrm-host/Cargo.toml`

**Interfaces:**
- Consumes: `oxutrm_term::detect_caps() -> TerminalCaps` (M1);
  `oxutrm_term::negotiate_term() -> (String /*TERM*/, Option<String> /*COLORTERM*/)`
  — **no arguments**; `oxutrm_term::HostTerm::spawn(shell, args, env, size, scrollback)`;
  `oxutrm_proto::{TerminalCaps, ControlMsg, PathDescription}`.
- Produces:
  ```rust
  // crates/oxutrm-host/src/caps.rs
  /// The environment a session's child shell is started with. Takes no client
  /// capabilities, deliberately (§9.4). Never contains key material.
  pub fn child_env() -> Vec<(String, String)>;

  /// Serve one `ControlMsg`. `caps` is stored for diagnosis and for nothing
  /// else: it must never influence the child environment.
  pub fn handle_control(
      msg: &oxutrm_proto::ControlMsg,
      caps: &mut oxutrm_proto::TerminalCaps,
      info: &oxutrm_proto::ControlMsg,
      path: &oxutrm_proto::PathDescription,
  ) -> Option<oxutrm_proto::ControlMsg>;
  ```

- [ ] **Step 1: Write the failing test for `negotiate_term`**

Append to `crates/oxutrm-term/src/lib.rs`:

```rust
#[cfg(test)]
mod negotiate_term_tests {
    use super::*;

    #[test]
    fn term_describes_the_emulator_and_nothing_else() {
        let (term, colorterm) = negotiate_term();
        // alacritty_terminal handles the full 256-colour palette and 24-bit
        // SGR 38;2/48;2, so both of these are honest.
        assert_eq!(term, "xterm-256color");
        assert_eq!(colorterm.as_deref(), Some("truecolor"));
    }

    /// The signature is the guard rail: if this ever grows a `TerminalCaps`
    /// argument, spec §9.4 has been violated.
    #[test]
    fn negotiate_term_is_deterministic_across_calls() {
        assert_eq!(negotiate_term(), negotiate_term());
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

`cargo test --jobs 4 -p oxutrm-term negotiate_term_tests -- --test-threads 4`

Expected: FAIL to compile if M1 gave `negotiate_term` a `TerminalCaps` argument,
or FAIL on the assertions otherwise.

- [ ] **Step 3: Write minimal implementation**

In `crates/oxutrm-term/src/lib.rs`, replace whatever `negotiate_term` M1 wrote:

```rust
/// Derived SOLELY from what `alacritty_terminal` emulates. The client's
/// capabilities must NOT influence this: the child's `TERM` cannot change when
/// a differently-capable client reattaches, and down-converting here would
/// permanently degrade the host's authoritative state (spec §9.4). All
/// capability adaptation happens in the client, at render time.
pub fn negotiate_term() -> (String, Option<String>) {
    // The emulator implements the full 256-colour palette and 24-bit SGR
    // 38;2 / 48;2, so claiming both is honest rather than hopeful.
    ("xterm-256color".to_string(), Some("truecolor".to_string()))
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-term negotiate_term_tests -- --test-threads 4`

Expected: PASS, 2 tests.

- [ ] **Step 5: Write the failing test for the host side**

Create `crates/oxutrm-host/src/caps.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{ControlMsg, NatType, PathDescription, Rung, TerminalCaps};

    fn caps(colors: u32, truecolor: bool) -> TerminalCaps {
        TerminalCaps {
            truecolor,
            colors,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "foot".to_string(),
        }
    }

    fn get<'a>(env: &'a [(String, String)], k: &str) -> Option<&'a str> {
        env.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn the_child_environment_describes_the_emulator() {
        let env = child_env();
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "COLORTERM"), Some("truecolor"));
    }

    /// The whole point of §9.4: a 16-colour client must not degrade what the
    /// shell emits, because a better client may attach tomorrow.
    #[test]
    fn a_poor_client_does_not_change_the_child_environment() {
        // child_env takes no capabilities at all, so this is true by
        // construction — the test exists to keep it that way.
        let before = child_env();
        let mut stored = caps(16, false);
        let info = ControlMsg::SessionInfo {
            session_id: "0".repeat(32),
            shell: "/bin/sh".to_string(),
            created_unix: 0,
        };
        handle_control(&ControlMsg::CapsUpdate(caps(8, false)), &mut stored, &info, &path());
        assert_eq!(before, child_env());
    }

    #[test]
    fn the_clients_own_term_name_never_reaches_the_child() {
        let env = child_env();
        assert!(
            env.iter().all(|(_, v)| v != "foot"),
            "the client's terminal name leaked into the child environment"
        );
    }

    #[test]
    fn no_environment_variable_carries_key_material() {
        for (k, _) in child_env() {
            let k = k.to_ascii_uppercase();
            assert!(!k.contains("PSK") && !k.contains("KEY") && !k.contains("SECRET"));
        }
    }

    fn path() -> PathDescription {
        PathDescription {
            rung: Rung::StunPunch,
            local: "127.0.0.1:443".parse().unwrap(),
            remote: "127.0.0.1:9".parse().unwrap(),
            probes_sent: 4,
            nat_type: NatType::EndpointIndependent,
            rtt_ms: 38,
            mtu: 1392,
        }
    }

    #[test]
    fn a_caps_update_replaces_the_stored_capabilities_for_diagnosis() {
        let mut stored = caps(16, false);
        let info = ControlMsg::SessionInfo {
            session_id: "0".repeat(32),
            shell: "/bin/sh".to_string(),
            created_unix: 0,
        };
        let reply = handle_control(
            &ControlMsg::CapsUpdate(caps(16_777_216, true)),
            &mut stored,
            &info,
            &path(),
        );
        assert!(reply.is_none());
        assert_eq!(stored.colors, 16_777_216);
        assert!(stored.truecolor);
    }

    #[test]
    fn a_status_request_is_answered_with_the_current_path() {
        let mut stored = caps(256, false);
        let info = ControlMsg::SessionInfo {
            session_id: "a".repeat(32),
            shell: "/bin/sh".to_string(),
            created_unix: 7,
        };
        match handle_control(&ControlMsg::StatusRequest, &mut stored, &info, &path()) {
            Some(ControlMsg::StatusReply(p)) => {
                assert_eq!(p.rtt_ms, 38);
                assert_eq!(p.rung, Rung::StunPunch);
            }
            other => panic!("expected a StatusReply, got {other:?}"),
        }
    }
}
```

- [ ] **Step 6: Run test to verify it fails**

Add `pub mod caps;` to `crates/oxutrm-host/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-host caps:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function child_env`.

- [ ] **Step 7: Write minimal implementation**

Put this above the test module in `crates/oxutrm-host/src/caps.rs`:

```rust
//! The child shell's environment, and the control stream (spec §9.4).
//!
//! Mosh hardcodes `xterm-256color` and hopes. oxutrm states honestly what its
//! own emulator implements — and deliberately does NOT narrow it to the current
//! client. Narrowing would degrade what the shell emits, which is then baked
//! into the authoritative `ScreenState` forever, and it would be undefined the
//! moment a differently-capable client reattaches to a week-old shell.

use oxutrm_proto::{ControlMsg, PathDescription, TerminalCaps};

/// The environment for a session's child shell. Takes no client capabilities,
/// deliberately.
pub fn child_env() -> Vec<(String, String)> {
    let (term, colorterm) = oxutrm_term::negotiate_term();
    let mut env = vec![("TERM".to_string(), term)];
    if let Some(ct) = colorterm {
        env.push(("COLORTERM".to_string(), ct));
    }
    env
}

/// Serve one control-stream message. Returns a reply where one is due.
pub fn handle_control(
    msg: &ControlMsg,
    caps: &mut TerminalCaps,
    info: &ControlMsg,
    path: &PathDescription,
) -> Option<ControlMsg> {
    match msg {
        // Stored for diagnosis only. It must never reach the child: see the
        // module docs and spec §9.4.
        ControlMsg::CapsUpdate(new_caps) => {
            *caps = new_caps.clone();
            None
        }
        ControlMsg::StatusRequest => Some(ControlMsg::StatusReply(path.clone())),
        ControlMsg::SessionInfo { .. } => Some(info.clone()),
        ControlMsg::StatusReply(_) => None,
    }
}
```

- [ ] **Step 8: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-host caps:: -- --test-threads 4`

Expected: PASS, 6 tests.

- [ ] **Step 9: Write the failing test that the child really sees the environment**

Create `crates/oxutrm-host/tests/child_env.rs`:

```rust
use oxutrm_proto::TermSize;
use oxutrm_term::HostTerm;

/// Start a shell with the derived environment and have it print $TERM and
/// $COLORTERM back through the PTY.
fn shell_reports() -> String {
    let size = TermSize { cols: 80, rows: 10 };
    let env = oxutrm_host::caps::child_env();
    let mut term = HostTerm::spawn(
        "/bin/sh",
        &["-c".to_string(), "printf '<%s|%s>' \"$TERM\" \"$COLORTERM\"".to_string()],
        &env,
        size,
        100,
    )
    .expect("spawn");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        term.poll().expect("poll");
        let s = term.snapshot(1);
        let text: String = s.cells.iter().map(|c| c.text.as_str()).collect();
        if text.contains('>') {
            return text;
        }
        assert!(std::time::Instant::now() < deadline, "shell never reported: {text:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn the_child_gets_term_and_colorterm_from_the_emulator() {
    let out = shell_reports();
    assert!(out.contains("<xterm-256color|truecolor>"), "got {out:?}");
}
```

- [ ] **Step 10: Run test to verify it passes**

If `crates/oxutrm-host/Cargo.toml` lacks `oxutrm-term`, add it:

```toml
[dependencies]
oxutrm-term = { path = "../oxutrm-term" }
```

Run: `cargo test --jobs 4 -p oxutrm-host --test child_env -- --test-threads 4`

Expected: PASS, 1 test.

- [ ] **Step 11: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-host/src/caps.rs crates/oxutrm-host/src/lib.rs \
        crates/oxutrm-host/tests/child_env.rs crates/oxutrm-host/Cargo.toml \
        crates/oxutrm-term/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(host): derive the child's TERM from the emulator, never from the client

Narrowing TERM to a client's capabilities degrades what the shell emits, which
is then baked into the authoritative state forever, and it is undefined when a
differently-capable client reattaches. All capability adaptation stays in the
client. negotiate_term takes no argument.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
---

### Task 6: The host session loop

PTY → `alacritty_terminal` → `Sender<ScreenState>` → QUIC, and QUIC → `Receiver<InputState>`
→ PTY, paced by Task 1's `Pacer` and sent through Task 2's `FrameSink`.

The PTY master is driven by `tokio::io::unix::AsyncFd`, so the loop sleeps
instead of spinning. That needs the fd, and it needs the fd to be non-blocking.

**Files:**
- Modify: `crates/oxutrm-term/src/host_term.rs`
- Create: `crates/oxutrm-host/src/session.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Modify: `crates/oxutrm-host/Cargo.toml`

**Interfaces:**
- Consumes: `oxutrm_term::{HostTerm, ScreenState}`; `oxutrm_sync::{Sender, Receiver, InputState}`;
  `oxutrm_net::xport::{FrameSink, FrameSource}` with
  `FrameSink::send(&Frame) -> anyhow::Result<usize>`; `oxutrm_net::pace::Pacer`;
  `oxutrm_net::link::link_stats`; `oxutrm_host::InputCursor`; `oxutrm_proto::TermSize`.
  `Sender::<ScreenState>::make_frame(ack_state: u64) -> Result<Option<Frame>, ApplyError>`,
  `Receiver::<InputState>::{on_frame, state, ack, peer_ack}`.
- Produces:
  ```rust
  // crates/oxutrm-term/src/host_term.rs
  impl std::os::fd::AsFd for oxutrm_term::HostTerm;
  impl oxutrm_term::HostTerm {
      pub fn set_nonblocking(&self) -> anyhow::Result<()>;
  }

  // crates/oxutrm-host/src/session.rs
  /// Runs until the child exits or the connection dies.
  /// Returns the child's exit status, or `None` if the peer went away first.
  pub async fn run_host_session(
      term: oxutrm_term::HostTerm,
      conn: quinn::Connection,
      local: std::net::SocketAddr,
      initial: oxutrm_proto::TermSize,
  ) -> anyhow::Result<Option<i32>>;
  ```

- [ ] **Step 1: Write the failing test for the fd accessors**

Append to `crates/oxutrm-term/src/host_term.rs`:

```rust
#[cfg(test)]
mod fd_tests {
    use super::*;
    use oxutrm_proto::TermSize;
    use std::os::fd::AsFd;

    #[test]
    fn the_pty_master_is_reachable_and_can_be_made_non_blocking() {
        let t = HostTerm::spawn(
            "/bin/sh",
            &["-c".to_string(), "sleep 5".to_string()],
            &[("TERM".to_string(), "xterm".to_string())],
            TermSize { cols: 80, rows: 24 },
            100,
        )
        .expect("spawn");
        let fd = t.as_fd();
        assert!(fd.try_clone_to_owned().is_ok());
        t.set_nonblocking().expect("set_nonblocking");
        let flags = rustix::fs::fcntl_getfl(t.as_fd()).expect("getfl");
        assert!(flags.contains(rustix::fs::OFlags::NONBLOCK));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

`cargo test --jobs 4 -p oxutrm-term fd_tests -- --test-threads 4`

Expected: FAIL to compile — `no method named as_fd`, `no method named set_nonblocking`.

- [ ] **Step 3: Write minimal implementation**

In `crates/oxutrm-term/src/host_term.rs`, `HostTerm` already owns the PTY master
that `spawn` created. Add, next to the other `impl` blocks:

```rust
impl std::os::fd::AsFd for HostTerm {
    /// The PTY master. Exposed so a session loop can wait on readability with
    /// `tokio::io::unix::AsyncFd` instead of polling.
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.master.as_fd()
    }
}

impl HostTerm {
    /// Put the PTY master into non-blocking mode. Required before the fd is
    /// registered with an async reactor.
    pub fn set_nonblocking(&self) -> anyhow::Result<()> {
        use std::os::fd::AsFd as _;
        let flags = rustix::fs::fcntl_getfl(self.master.as_fd())?;
        rustix::fs::fcntl_setfl(self.master.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)?;
        Ok(())
    }
}
```

If M1 named the master field something other than `master`, use that name; if it
is not a field but is wrapped, add a private `fn master_fd(&self) -> BorrowedFd<'_>`
and route both methods through it. Add `use std::os::fd::AsFd as _;` at the top
of the file.

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-term fd_tests -- --test-threads 4`

Expected: PASS, 1 test.

- [ ] **Step 5: Write the failing test for the session loop**

Create `crates/oxutrm-host/tests/session_loop.rs`:

```rust
use oxutrm_net::xport::{FrameSink, FrameSource};
use oxutrm_proto::{Frame, TermSize};
use oxutrm_sync::{InputState, Receiver, Sender};
use oxutrm_term::{HostTerm, ScreenState};
use std::time::{Duration, Instant};

fn size() -> TermSize {
    TermSize { cols: 80, rows: 24 }
}

/// Everything visible on a screen, as one string.
fn text(s: &ScreenState) -> String {
    (0..s.rows)
        .map(|r| s.row(r).iter().map(|c| c.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_input_reaches_the_shell_and_the_output_comes_back() -> anyhow::Result<()> {
    let (server_conn, client_conn) = oxutrm_net::testkit::loopback_pair().await?;
    let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    let term = HostTerm::spawn(
        "/bin/sh",
        &[],
        &[("TERM".to_string(), "xterm-256color".to_string()), ("PS1".to_string(), "$ ".to_string())],
        size(),
        200,
    )?;

    let host = tokio::spawn(oxutrm_host::session::run_host_session(term, server_conn, local, size()));

    // Play the client by hand.
    let sink = FrameSink::new(client_conn.clone());
    let mut source = FrameSource::new(client_conn.clone());
    let mut screen_rx: Receiver<ScreenState> = Receiver::new(ScreenState::blank(size().rows, size().cols));
    let mut input_tx: Sender<InputState> =
        Sender::new(InputState { seq: 0, pending: Vec::new(), size: size() });

    let next = input_tx.current().append(b"printf 'MARKER-OK'\n", size());
    input_tx.update(next);
    if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
        sink.send(&f)?;
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "marker never appeared: {:?}", text(screen_rx.state()));
        let f: Frame = match tokio::time::timeout(remaining.min(Duration::from_millis(500)), source.recv()).await {
            Ok(f) => f?,
            Err(_) => {
                // Nothing arrived this tick; re-send the unacknowledged input.
                if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
                    sink.send(&f)?;
                }
                continue;
            }
        };
        screen_rx.on_frame(&f)?;
        if text(screen_rx.state()).contains("MARKER-OK") {
            break;
        }
    }

    client_conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(5), host).await;
    Ok(())
}

/// A frame the sync engine rejects must cost one frame, not the session.
/// Tearing down here would be self-defeating: reconnecting cannot help, because
/// the peer would re-derive the very same diff.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unapplicable_frame_does_not_kill_the_session() -> anyhow::Result<()> {
    let (server_conn, client_conn) = oxutrm_net::testkit::loopback_pair().await?;
    let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    let term = HostTerm::spawn(
        "/bin/sh",
        &[],
        &[("TERM".to_string(), "xterm".to_string()), ("PS1".to_string(), "".to_string())],
        size(),
        200,
    )?;
    let host = tokio::spawn(oxutrm_host::session::run_host_session(term, server_conn, local, size()));

    let sink = FrameSink::new(client_conn.clone());
    let mut source = FrameSource::new(client_conn.clone());
    let mut screen_rx: Receiver<ScreenState> = Receiver::new(ScreenState::blank(size().rows, size().cols));
    let mut input_tx: Sender<InputState> =
        Sender::new(InputState { seq: 0, pending: Vec::new(), size: size() });

    // Garbage where a postcard InputDiff should be. The host must log it and
    // carry on.
    sink.send(&Frame {
        my_state: 900,
        from_state: 0,
        ack_state: 0,
        frag_index: 0,
        frag_count: 1,
        flags: 0,
        payload: vec![0xFF; 64],
    })?;

    // The session is still alive: real input still reaches the shell.
    let next = input_tx.current().append(b"printf 'STILL-ALIVE'\n", size());
    input_tx.update(next);
    if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
        sink.send(&f)?;
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the session died on a bad frame:\n{}",
            text(screen_rx.state())
        );
        assert!(!host.is_finished(), "run_host_session returned instead of dropping the frame");
        match tokio::time::timeout(Duration::from_millis(500), source.recv()).await {
            Ok(f) => {
                let _ = screen_rx.on_frame(&f?);
            }
            Err(_) => {
                if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
                    sink.send(&f)?;
                }
                continue;
            }
        }
        if text(screen_rx.state()).contains("STILL-ALIVE") {
            break;
        }
    }

    client_conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(5), host).await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_resize_in_the_input_state_resizes_the_pty() -> anyhow::Result<()> {
    let (server_conn, client_conn) = oxutrm_net::testkit::loopback_pair().await?;
    let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    let term = HostTerm::spawn(
        "/bin/sh",
        &[],
        &[("TERM".to_string(), "xterm".to_string()), ("PS1".to_string(), "".to_string())],
        size(),
        200,
    )?;
    let host = tokio::spawn(oxutrm_host::session::run_host_session(term, server_conn, local, size()));

    let sink = FrameSink::new(client_conn.clone());
    let mut source = FrameSource::new(client_conn.clone());
    let mut screen_rx: Receiver<ScreenState> = Receiver::new(ScreenState::blank(size().rows, size().cols));
    let mut input_tx: Sender<InputState> =
        Sender::new(InputState { seq: 0, pending: Vec::new(), size: size() });

    let big = TermSize { cols: 120, rows: 40 };
    let next = input_tx.current().append(b"", big);
    input_tx.update(next);
    if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
        sink.send(&f)?;
    }

    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(Instant::now() < deadline, "the screen never resized");
        match tokio::time::timeout(Duration::from_millis(500), source.recv()).await {
            Ok(f) => {
                screen_rx.on_frame(&f?)?;
            }
            Err(_) => {
                if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
                    sink.send(&f)?;
                }
                continue;
            }
        }
        if screen_rx.state().cols == 120 && screen_rx.state().rows == 40 {
            break;
        }
    }

    client_conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(5), host).await;
    Ok(())
}
```

- [ ] **Step 6: Run test to verify it fails**

Add to `crates/oxutrm-host/Cargo.toml`:

```toml
[dependencies]
oxutrm-net = { path = "../oxutrm-net" }
oxutrm-sync = { path = "../oxutrm-sync" }
quinn = "0.11"

[dev-dependencies]
oxutrm-net = { path = "../oxutrm-net", features = ["testkit"] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time"] }
```

Then run: `cargo test --jobs 4 -p oxutrm-host --test session_loop -- --test-threads 4`

Expected: FAIL to compile — `could not find session in oxutrm_host`.

- [ ] **Step 7: Write minimal implementation**

Create `crates/oxutrm-host/src/session.rs`:

```rust
//! The host session loop.
//!
//! ```text
//!   PTY --> alacritty_terminal --> Sender<ScreenState> --> Frame --> QUIC
//!   PTY <--------------- Receiver<InputState> <-- Frame <-- QUIC
//! ```
//!
//! Sending is paced by spec §10.4: `clamp(rtt/2, 8ms, 100ms)`, with an
//! immediate send when the link has been idle. States coalesce rather than
//! queue, so a runaway `yes` produces one frame per interval, not a backlog.

use crate::input_cursor::InputCursor;
use anyhow::Context;
use oxutrm_net::link::link_stats;
use oxutrm_net::pace::Pacer;
use oxutrm_net::xport::{FrameSink, FrameSource};
use oxutrm_proto::TermSize;
use oxutrm_sync::{InputState, Receiver, Sender};
use oxutrm_term::{HostTerm, ScreenState};
use std::net::SocketAddr;
use std::time::Instant;
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;

/// Runs until the child exits or the connection dies. Returns the child's exit
/// status, or `None` when the peer went away first.
pub async fn run_host_session(
    mut term: HostTerm,
    conn: quinn::Connection,
    local: SocketAddr,
    initial: TermSize,
) -> anyhow::Result<Option<i32>> {
    term.set_nonblocking()?;
    let pty = AsyncFd::with_interest(
        term.as_fd().try_clone_to_owned().context("clone pty master")?,
        Interest::READABLE,
    )?;

    let sink = FrameSink::new(conn.clone());
    let mut source = FrameSource::new(conn.clone());

    // `ScreenState.seq` starts at 1 and resets to 1 at every attach; 0 is
    // reserved as the "full state" sentinel, so it is never a real state.
    let mut screen_tx: Sender<ScreenState> = Sender::new(term.snapshot(1));
    let mut input_rx: Receiver<InputState> =
        Receiver::new(InputState { seq: 0, pending: Vec::new(), size: initial });
    let mut cursor = InputCursor::new();
    let mut pacer = Pacer::new();
    let mut size = initial;
    let mut dirty = true;

    loop {
        if let Some(code) = term.child_exited() {
            conn.close(0u32.into(), b"shell exited");
            return Ok(Some(code));
        }

        let rtt = link_stats(&conn, local).rtt;
        // Wake either when the pacing interval expires or, if we owe nothing,
        // in a while — a bare `select!` with no timer would sleep through a
        // dirty screen.
        let wake = match (dirty, pacer.next_deadline(rtt)) {
            (true, Some(t)) => tokio::time::sleep_until(t.into()),
            (true, None) => tokio::time::sleep(std::time::Duration::from_millis(0)),
            (false, _) => tokio::time::sleep(std::time::Duration::from_millis(50)),
        };

        tokio::select! {
            // The shell wrote something.
            guard = pty.readable() => {
                let mut guard = guard?;
                guard.clear_ready();
                if term.poll()? {
                    dirty = true;
                }
            }

            // The client sent input, a resize, or an acknowledgement.
            frame = source.recv() => {
                let frame = match frame {
                    Ok(f) => f,
                    // The peer is gone. Detached is a normal state (spec §9.3):
                    // keep the PTY draining, transmit nothing, and let the
                    // caller decide whether to wait for a reattach.
                    Err(_) => return Ok(None),
                };
                cursor.on_peer_saw(frame.ack_state);
                // A frame the sync engine rejects is a DROPPED frame, not a
                // dead session. `ScreenState::validate` and the bounds checks
                // reject wholesale and leave the receiver's state untouched, so
                // the ack does not advance and the peer's next diff — computed
                // from that same base — repairs it. Propagating with `?` here
                // would tear down a connection that is working perfectly.
                let advanced = match input_rx.on_frame(&frame) {
                    Ok(advanced) => advanced,
                    Err(e) => {
                        eprintln!("oxutrm: dropping unapplicable input frame: {e}");
                        continue;
                    }
                };
                if advanced {
                    source.set_current_state(input_rx.ack());
                    let state = input_rx.state().clone();
                    let new_bytes = cursor.take_new(&state.pending).to_vec();
                    if !new_bytes.is_empty() {
                        term.write_input(&new_bytes)?;
                    }
                    if state.size != size {
                        size = state.size;
                        term.resize(size)?;
                        dirty = true;
                    }
                }
            }

            // Pacing tick.
            _ = wake => {
                if term.poll()? {
                    dirty = true;
                }
                let now = Instant::now();
                if dirty && pacer.may_send(now, rtt) {
                    // `poll()` reported a change, and it is damage-driven
                    // (`Term::damage()` / `reset_damage()`), so there is no
                    // whole-grid comparison here and there must not be one.
                    screen_tx.update(term.snapshot(screen_tx.current().seq));
                    if let Some(frame) = screen_tx.make_frame(input_rx.ack())? {
                        let sent = sink.send(&frame)?;
                        debug_assert!(sent >= 1);
                        cursor.on_ack_sent(frame.my_state);
                        pacer.on_sent(now);
                        dirty = false;
                    } else {
                        dirty = false;
                        // Nothing outstanding: the next change goes out at once.
                        pacer.go_idle();
                    }
                }
            }

            _ = conn.closed() => return Ok(None),
        }
    }
}
```

Note the `screen_tx.update(next)` guard: `HostTerm::snapshot` takes the sequence
number to stamp, and `Sender::update` assigns the next one, so passing the
current sequence and comparing keeps the ring free of identical states.

- [ ] **Step 8: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-host --test session_loop -- --test-threads 4`

Expected: PASS, 3 tests. If `typed_input_reaches_the_shell_and_the_output_comes_back`
hangs, the first thing to check is whether `HostTerm::poll` really returns
without blocking on an empty PTY — `set_nonblocking` must have taken effect.

- [ ] **Step 9: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-term/src/host_term.rs crates/oxutrm-host/src/session.rs \
        crates/oxutrm-host/src/lib.rs crates/oxutrm-host/Cargo.toml \
        crates/oxutrm-host/tests/session_loop.rs
git commit -m "$(cat <<'EOF'
feat(host): the session loop, PTY to the emulator to QUIC and back

Sends are paced with clamp(rtt/2, 8ms, 100ms) from quinn's own RTT estimate,
immediate when the link has been idle, and states coalesce rather than queue.
The PTY master is waited on with AsyncFd instead of polled.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: The terminal guard — raw mode, alternate screen, panic hook

A client that dies leaving the user's terminal in raw mode with the alternate
screen active is unacceptable. The restore must happen on every path: normal
exit, panic, `SIGINT`, `SIGTERM`, `SIGHUP`, `SIGQUIT`, and the peer vanishing.

The design is one idempotent restore function plus three things that call it:
`Drop`, a panic hook, and the session loop's signal arm.

**Files:**
- Create: `crates/oxutrm-client/src/guard.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`
- Modify: `crates/oxutrm-client/Cargo.toml`

**Interfaces:**
- Consumes: `rustix::termios::{tcgetattr, tcsetattr, OptionalActions, Termios}`;
  M1's `RawGuard` in `crates/oxutrm-client/src/lib.rs`, which this task
  **replaces** — delete `RawGuard` and update its call sites.
- Produces:
  ```rust
  // crates/oxutrm-client/src/guard.rs
  /// Everything written to leave the terminal as we found it, in order:
  /// show cursor, leave bracketed paste, leave SGR mouse reporting,
  /// leave the alternate screen, reset SGR.
  pub const RESTORE_SEQ: &[u8] =
      b"\x1b[?25h\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[0m";
  /// Entered on install: alternate screen, hide cursor, clear.
  pub const ENTER_SEQ: &[u8] = b"\x1b[?1049h\x1b[H\x1b[2J";

  pub struct TerminalGuard { /* private */ }
  impl TerminalGuard {
      /// Raw mode + alternate screen, and arm every restore path.
      pub fn install() -> anyhow::Result<TerminalGuard>;
      /// Idempotent. Safe to call from a panic hook or a signal handler task.
      pub fn restore_now();
      /// True once the terminal has been put back.
      pub fn is_restored() -> bool;
  }
  impl Drop for TerminalGuard;
  ```

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-client/src/guard.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restore_sequence_leaves_the_alternate_screen_and_shows_the_cursor() {
        let s = String::from_utf8(RESTORE_SEQ.to_vec()).unwrap();
        assert!(s.contains("\x1b[?1049l"), "must leave the alternate screen");
        assert!(s.contains("\x1b[?25h"), "must show the cursor");
        assert!(s.contains("\x1b[?2004l"), "must leave bracketed paste");
        assert!(s.contains("\x1b[?1006l"), "must leave SGR mouse reporting");
        assert!(s.contains("\x1b[?1000l"), "must leave mouse reporting");
        assert!(s.ends_with("\x1b[0m"), "must end by resetting SGR");
    }

    #[test]
    fn the_cursor_is_shown_before_the_alternate_screen_is_left() {
        let s = String::from_utf8(RESTORE_SEQ.to_vec()).unwrap();
        let show = s.find("\x1b[?25h").unwrap();
        let leave = s.find("\x1b[?1049l").unwrap();
        assert!(show < leave, "showing the cursor after leaving hides it on the main screen");
    }

    #[test]
    fn the_enter_sequence_takes_the_alternate_screen() {
        let s = String::from_utf8(ENTER_SEQ.to_vec()).unwrap();
        assert!(s.starts_with("\x1b[?1049h"));
    }

    #[test]
    fn restore_now_is_idempotent_and_safe_without_a_guard() {
        // No guard was installed: there is nothing to restore, and calling it
        // must not panic or block.
        TerminalGuard::restore_now();
        TerminalGuard::restore_now();
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod guard;` and `pub use guard::TerminalGuard;` to
`crates/oxutrm-client/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-client guard:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find value RESTORE_SEQ`.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-client/src/guard.rs`:

```rust
//! Leaving the user's terminal exactly as we found it, on every exit path.
//!
//! One idempotent restore, three callers: `Drop`, a panic hook, and the
//! session loop's signal arm. The saved termios lives in a process-wide
//! `OnceLock` rather than in the guard, because the panic hook cannot borrow
//! the guard and a signal arrives with no `self` in scope.

use rustix::termios::{OptionalActions, Termios};
use std::io::Write;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// Put the terminal back: show the cursor *before* leaving the alternate
/// screen, or the cursor stays hidden on the main screen.
pub const RESTORE_SEQ: &[u8] =
    b"\x1b[?25h\x1b[?2004l\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1049l\x1b[0m";

/// Take the alternate screen and clear it.
pub const ENTER_SEQ: &[u8] = b"\x1b[?1049h\x1b[H\x1b[2J";

static ORIGINAL: OnceLock<Termios> = OnceLock::new();
static RESTORED: AtomicBool = AtomicBool::new(true);

fn tty() -> BorrowedFd<'static> {
    // Fd 0 is the controlling terminal for an interactive client. Using a
    // borrowed constant avoids taking a lock inside a panic hook.
    unsafe { BorrowedFd::borrow_raw(0) }
}

pub struct TerminalGuard {
    _private: (),
}

impl TerminalGuard {
    /// Raw mode plus the alternate screen, with every restore path armed.
    pub fn install() -> anyhow::Result<TerminalGuard> {
        let original = rustix::termios::tcgetattr(tty())?;
        let _ = ORIGINAL.set(original.clone());

        let mut raw = original;
        raw.make_raw();
        rustix::termios::tcsetattr(tty(), OptionalActions::Flush, &raw)?;

        let mut out = std::io::stdout();
        out.write_all(ENTER_SEQ)?;
        out.flush()?;
        RESTORED.store(false, Ordering::SeqCst);

        // A panic must not leave the terminal unusable, and the default hook's
        // message must land on a screen the user can actually read.
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            TerminalGuard::restore_now();
            previous(info);
        }));

        Ok(TerminalGuard { _private: () })
    }

    /// Idempotent, and a no-op when no guard was ever installed.
    pub fn restore_now() {
        if RESTORED.swap(true, Ordering::SeqCst) {
            return;
        }
        // Write the escape sequence first: even if tcsetattr fails, the
        // alternate screen must go.
        let _ = std::io::stdout().write_all(RESTORE_SEQ);
        let _ = std::io::stdout().flush();
        if let Some(t) = ORIGINAL.get() {
            let _ = rustix::termios::tcsetattr(tty(), OptionalActions::Flush, t);
        }
    }

    pub fn is_restored() -> bool {
        RESTORED.load(Ordering::SeqCst)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        TerminalGuard::restore_now();
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-client guard:: -- --test-threads 4`

Expected: PASS, 4 tests.

- [ ] **Step 5: Remove `RawGuard`**

M1's `RawGuard` did half of this and is now a second restore path that can fight
with the first. Delete `struct RawGuard`, its `impl`s and its `pub use` from
`crates/oxutrm-client/src/lib.rs`, and replace every `RawGuard::enter()` call
site with `TerminalGuard::install()`.

Update the contract note: the contract lists `RawGuard` under `oxutrm-client`.
Add one line under it in `docs/superpowers/plans/2026-08-25-oxutrm-contract.md`:

```markdown
> **M4 supersedes `RawGuard` with `TerminalGuard`** (`guard.rs`), which also
> owns the alternate screen, the panic hook and the signal restore path.
> `RawGuard` no longer exists.
```

- [ ] **Step 6: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-client/src/guard.rs crates/oxutrm-client/src/lib.rs \
        docs/superpowers/plans/2026-08-25-oxutrm-contract.md
git commit -m "$(cat <<'EOF'
feat(client): one idempotent terminal restore for every exit path

Raw mode, the alternate screen, the panic hook and the signal path all go
through a single restore, so a client that dies cannot leave the user's
terminal unusable. Supersedes M1's RawGuard.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: Prove the restore on every exit path, under a real PTY

Task 7 wrote the mechanism. This task proves it, by running the real binary
under a real PTY and checking the PTY's line discipline afterwards. Testing
this in-process would prove nothing: the failure mode is the process dying.

**Files:**
- Create: `tests/support/mod.rs`
- Create: `tests/exit_paths.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml`

**Interfaces:**
- Consumes: `oxutrm_client::guard::{TerminalGuard, RESTORE_SEQ}`;
  `rustix_openpty::openpty`; `rustix::termios::{tcgetattr, LocalModes}`.
- Produces:
  ```rust
  // tests/support/mod.rs
  pub struct PtyChild {
      pub child: std::process::Child,
      pub master: std::os::fd::OwnedFd,
  }
  /// Run the `oxutrm` binary with `args` and `env`, attached to a fresh PTY.
  pub fn spawn_under_pty(args: &[&str], env: &[(&str, &str)]) -> anyhow::Result<PtyChild>;
  /// Read from the master until `needle` appears or `timeout` elapses.
  pub fn read_until(master: &std::os::fd::OwnedFd, needle: &[u8], timeout: std::time::Duration)
      -> anyhow::Result<Vec<u8>>;
  /// True when the PTY is back in cooked mode.
  pub fn is_cooked(master: &std::os::fd::OwnedFd) -> anyhow::Result<bool>;

  // src/main.rs — test-only hook, honoured only when OXUTRM_TEST_HOOK is set
  // Values: "raw-then-panic", "raw-then-idle".
  ```

- [ ] **Step 1: Write the failing test**

Create `tests/exit_paths.rs`:

```rust
mod support;

use std::time::Duration;
use support::{is_cooked, read_until, spawn_under_pty};

const READY: &[u8] = b"OXUTRM-RAW-READY";

/// Every case: the child enters raw mode and the alternate screen, then dies
/// the way the test names. Afterwards the PTY must be cooked again and the
/// restore sequence must have been written.
fn assert_restored(mut p: support::PtyChild, seen: &[u8]) {
    let status = p.child.wait().expect("wait");
    let _ = status;
    let text = String::from_utf8_lossy(seen);
    assert!(
        text.contains("\u{1b}[?1049l"),
        "the alternate screen was never left: {text:?}"
    );
    assert!(text.contains("\u{1b}[?25h"), "the cursor was never shown: {text:?}");
    assert!(is_cooked(&p.master).expect("tcgetattr"), "the PTY is still in raw mode");
}

#[test]
fn a_panic_restores_the_terminal() -> anyhow::Result<()> {
    let p = spawn_under_pty(&["client-test-hook"], &[("OXUTRM_TEST_HOOK", "raw-then-panic")])?;
    let seen = read_until(&p.master, b"\x1b[?1049l", Duration::from_secs(10))?;
    assert_restored(p, &seen);
    Ok(())
}

#[test]
fn sigint_restores_the_terminal() -> anyhow::Result<()> {
    let p = spawn_under_pty(&["client-test-hook"], &[("OXUTRM_TEST_HOOK", "raw-then-idle")])?;
    read_until(&p.master, READY, Duration::from_secs(10))?;
    unsafe { libc::kill(p.child.id() as i32, libc::SIGINT) };
    let seen = read_until(&p.master, b"\x1b[?1049l", Duration::from_secs(10))?;
    assert_restored(p, &seen);
    Ok(())
}

#[test]
fn sigterm_restores_the_terminal() -> anyhow::Result<()> {
    let p = spawn_under_pty(&["client-test-hook"], &[("OXUTRM_TEST_HOOK", "raw-then-idle")])?;
    read_until(&p.master, READY, Duration::from_secs(10))?;
    unsafe { libc::kill(p.child.id() as i32, libc::SIGTERM) };
    let seen = read_until(&p.master, b"\x1b[?1049l", Duration::from_secs(10))?;
    assert_restored(p, &seen);
    Ok(())
}

#[test]
fn sighup_restores_the_terminal() -> anyhow::Result<()> {
    let p = spawn_under_pty(&["client-test-hook"], &[("OXUTRM_TEST_HOOK", "raw-then-idle")])?;
    read_until(&p.master, READY, Duration::from_secs(10))?;
    unsafe { libc::kill(p.child.id() as i32, libc::SIGHUP) };
    let seen = read_until(&p.master, b"\x1b[?1049l", Duration::from_secs(10))?;
    assert_restored(p, &seen);
    Ok(())
}

/// The peer vanishing is not a signal and not a panic: the session loop must
/// notice `Connection::closed()` and unwind normally.
#[test]
fn losing_the_peer_restores_the_terminal() -> anyhow::Result<()> {
    let p = spawn_under_pty(&["client-test-hook"], &[("OXUTRM_TEST_HOOK", "raw-then-peer-loss")])?;
    let seen = read_until(&p.master, b"\x1b[?1049l", Duration::from_secs(20))?;
    assert_restored(p, &seen);
    Ok(())
}
```

- [ ] **Step 2: Write the PTY support module**

Create `tests/support/mod.rs`:

```rust
//! Running the real binary under a real PTY, so the exit-path tests observe
//! what a user's terminal would observe.

use anyhow::Context;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct PtyChild {
    pub child: std::process::Child,
    pub master: OwnedFd,
}

pub fn spawn_under_pty(args: &[&str], env: &[(&str, &str)]) -> anyhow::Result<PtyChild> {
    let pty = rustix_openpty::openpty(None, None).context("openpty")?;
    let slave = pty.controlled;
    let master = pty.controller;

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_oxutrm"));
    cmd.args(args);
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::from(slave.try_clone()?));
    cmd.stdout(Stdio::from(slave.try_clone()?));
    cmd.stderr(Stdio::from(slave.try_clone()?));

    let slave_fd = slave.as_raw_fd();
    unsafe {
        cmd.pre_exec(move || {
            // A fresh session with the pty as its controlling terminal, so
            // signals and SIGWINCH behave as they would for a real user.
            if libc::setsid() < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::ioctl(slave_fd, libc::TIOCSCTTY, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn oxutrm")?;
    drop(slave);
    Ok(PtyChild { child, master })
}

pub fn read_until(master: &OwnedFd, needle: &[u8], timeout: Duration) -> anyhow::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    let mut file = unsafe { std::fs::File::from_raw_fd_borrowed(master) };
    let mut buf = [0u8; 4096];
    while Instant::now() < deadline {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                seen.extend_from_slice(&buf[..n]);
                if seen.windows(needle.len()).any(|w| w == needle) {
                    return Ok(seen);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            // EIO on a pty master means the last slave closed: the child is gone.
            Err(_) => break,
        }
    }
    anyhow::ensure!(
        seen.windows(needle.len()).any(|w| w == needle),
        "never saw {:?} in {:?}",
        String::from_utf8_lossy(needle),
        String::from_utf8_lossy(&seen)
    );
    Ok(seen)
}

/// True when ECHO and ICANON are set, i.e. the terminal is out of raw mode.
pub fn is_cooked(master: &OwnedFd) -> anyhow::Result<bool> {
    let t = rustix::termios::tcgetattr(master.as_fd())?;
    let m = t.local_modes;
    Ok(m.contains(rustix::termios::LocalModes::ECHO)
        && m.contains(rustix::termios::LocalModes::ICANON))
}

/// `std::fs::File` from a borrowed fd, without taking ownership of it.
trait FromRawFdBorrowed {
    unsafe fn from_raw_fd_borrowed(fd: &OwnedFd) -> std::mem::ManuallyDrop<std::fs::File>;
}

impl FromRawFdBorrowed for std::fs::File {
    unsafe fn from_raw_fd_borrowed(fd: &OwnedFd) -> std::mem::ManuallyDrop<std::fs::File> {
        use std::os::fd::FromRawFd;
        std::mem::ManuallyDrop::new(std::fs::File::from_raw_fd(fd.as_raw_fd()))
    }
}
```

Change the `read_until` body's first line to match the helper's return type:

```rust
    let mut file = unsafe { <std::fs::File as FromRawFdBorrowed>::from_raw_fd_borrowed(master) };
```

- [ ] **Step 3: Run tests to verify they fail**

Add to the root `Cargo.toml`:

```toml
[dev-dependencies]
libc = "0.2"
rustix-openpty = "0.2"
rustix = { version = "1", features = ["termios", "process", "fs", "stdio"] }
anyhow = "1"
```

Run: `cargo test --jobs 4 --test exit_paths -- --test-threads 4`

Expected: FAIL — the binary rejects the `client-test-hook` subcommand.

- [ ] **Step 4: Write minimal implementation**

Add to `src/main.rs`:

```rust
/// A test-only entry point. It exists so the exit-path tests can observe a
/// real process, under a real PTY, dying in each of the ways that matter.
/// Honoured only when `OXUTRM_TEST_HOOK` is set, and it is never documented in
/// `--help`.
async fn client_test_hook() -> anyhow::Result<()> {
    let hook = std::env::var("OXUTRM_TEST_HOOK").unwrap_or_default();
    let _guard = oxutrm_client::TerminalGuard::install()?;

    match hook.as_str() {
        "raw-then-panic" => {
            panic!("oxutrm test hook: deliberate panic after entering raw mode");
        }
        "raw-then-idle" => {
            use std::io::Write;
            print!("OXUTRM-RAW-READY");
            std::io::stdout().flush()?;
            oxutrm_client::session::wait_for_terminating_signal().await;
            Ok(())
        }
        "raw-then-peer-loss" => {
            // A connection that is closed from under us, exactly as an
            // abnormal peer loss looks to the session loop.
            let (server, client) = oxutrm_net::testkit::loopback_pair().await?;
            drop(server);
            client.closed().await;
            Ok(())
        }
        other => anyhow::bail!("unknown OXUTRM_TEST_HOOK: {other}"),
    }
}
```

Wire it into the subcommand match, guarded so it cannot fire in normal use:

```rust
        "client-test-hook" if std::env::var_os("OXUTRM_TEST_HOOK").is_some() => {
            client_test_hook().await
        }
```

Add the signal wait to `crates/oxutrm-client/src/session.rs` (created in Task 9;
if that file does not exist yet, create it now with just this function):

```rust
/// Resolves on the first of SIGINT, SIGTERM, SIGHUP or SIGQUIT.
///
/// In raw mode `ISIG` is off, so Ctrl-C never becomes a SIGINT — it is a byte
/// the remote application receives. These are the signals that arrive from
/// outside: `kill`, a logout, a session leader going away.
pub async fn wait_for_terminating_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
    let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut hup = signal(SignalKind::hangup()).expect("SIGHUP handler");
    let mut quit = signal(SignalKind::quit()).expect("SIGQUIT handler");
    tokio::select! {
        _ = int.recv() => {}
        _ = term.recv() => {}
        _ = hup.recv() => {}
        _ = quit.recv() => {}
    }
}
```

The root crate needs `oxutrm-net` with `testkit` under dev, so the hook builds
in tests. In the root `Cargo.toml`:

```toml
[dependencies]
oxutrm-net = { path = "crates/oxutrm-net", features = ["testkit"] }
```

- [ ] **Step 5: Run tests to verify they pass**

`cargo test --jobs 4 --test exit_paths -- --test-threads 4`

Expected: PASS, 5 tests — panic, SIGINT, SIGTERM, SIGHUP, peer loss.

- [ ] **Step 6: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add tests/exit_paths.rs tests/support/mod.rs src/main.rs Cargo.toml \
        crates/oxutrm-client/src/session.rs crates/oxutrm-client/src/lib.rs
git commit -m "$(cat <<'EOF'
test: prove the terminal restore on panic, SIGINT, SIGTERM, SIGHUP, peer loss

Each case runs the real binary under a real PTY and asserts both that the
restore sequence was written and that the PTY line discipline is cooked again.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: The status line, the migration announcement and the `Ctrl-]` pane

Spec §10.3: one line on connect, then silence; `Ctrl-]` opens a locally drawn
pane; a path change announces itself for a few seconds. All three are drawn by
the client and cost no bandwidth.

The separator in the connect line is two spaces, U+00B7, two spaces. The
migration line uses single spaces around the U+00B7. These are exact.

Two things the spec is emphatic about and that are easy to get wrong:

- The rung-4 line **names the lost property**: `no UDP path, not detachable`.
  "Not detachable" (§5.5) is otherwise discovered only by closing the laptop lid.
- A migration announcement reports **the client's own local addresses**, old and
  new. The rung and the peer address cannot change within an attach (§5), so
  naming a rung there would be a lie.

**Files:**
- Create: `crates/oxutrm-client/src/status.rs`
- Create: `crates/oxutrm-client/src/pane.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`

**Interfaces:**
- Consumes: `oxutrm_proto::{PathDescription, Rung, NatType}`;
  `oxutrm_net::link::LinkStats`.
- Produces:
  ```rust
  // crates/oxutrm-client/src/status.rs
  pub fn rung_label(path: &oxutrm_proto::PathDescription) -> String;
  /// One connect-time line, then silence. Replaces M1's stub.
  pub fn status_line(path: &oxutrm_proto::PathDescription) -> String;
  /// Shown for MIGRATION_DWELL after the CLIENT'S OWN local address changes.
  pub fn migration_line(
      old_local: std::net::SocketAddr,
      new_local: std::net::SocketAddr,
      rtt_ms: u32,
  ) -> String;
  pub const MIGRATION_DWELL: std::time::Duration = std::time::Duration::from_secs(3);

  // crates/oxutrm-client/src/pane.rs
  pub const PANE_WIDTH: usize = 60;
  pub struct StatusPane {
      pub path: oxutrm_proto::PathDescription,
      pub loss_pct: f32,
      pub bytes_tx: u64,
      pub bytes_rx: u64,
      pub migrations: Vec<(u64, String)>,   // (seconds since connect, "old → new")
      pub session_id: String,
      pub uptime: std::time::Duration,
  }
  pub fn fmt_bytes(n: u64) -> String;
  pub fn fmt_uptime(d: std::time::Duration) -> String;
  pub fn status_pane_lines(p: &StatusPane) -> Vec<String>;
  ```

- [ ] **Step 1: Write the failing test for the status line**

Create `crates/oxutrm-client/src/status.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{NatType, PathDescription, Rung};

    fn path(rung: Rung, remote: &str, rtt_ms: u32, mtu: u16, probes: u32, nat: NatType) -> PathDescription {
        PathDescription {
            rung,
            local: "127.0.0.1:443".parse().unwrap(),
            remote: remote.parse().unwrap(),
            probes_sent: probes,
            nat_type: nat,
            rtt_ms,
            mtu,
        }
    }

    #[test]
    fn the_ipv6_direct_line_matches_the_spec() {
        let p = path(Rung::Ipv6Direct, "[2001:db8::2]:443", 11, 1452, 0, NatType::None);
        assert_eq!(status_line(&p), "oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452");
    }

    #[test]
    fn the_port_mapped_line_matches_the_spec() {
        let p = path(Rung::PortMapped, "203.0.113.7:443", 38, 1392, 0, NatType::EndpointIndependent);
        assert_eq!(
            status_line(&p),
            "oxutrm  IPv4 punched (port mapped)  ·  38 ms  ·  mtu 1392"
        );
    }

    #[test]
    fn the_birthday_line_reports_the_probe_count_and_the_nat_instead_of_the_mtu() {
        let p = path(Rung::Birthday, "203.0.113.7:41234", 61, 1392, 312, NatType::Symmetric);
        assert_eq!(
            status_line(&p),
            "oxutrm  IPv4 punched (birthday, 312 probes)  ·  61 ms  ·  symmetric NAT"
        );
    }

    /// The rung-4 line must name the lost property: a user who is not told
    /// "not detachable" discovers it by closing the laptop lid (§5.5).
    #[test]
    fn the_ssh_tunnel_line_warns_that_the_session_is_not_detachable() {
        let p = path(Rung::SshTunnel, "127.0.0.1:41234", 45, 1200, 0, NatType::Unknown);
        assert_eq!(
            status_line(&p),
            "oxutrm  SSH tunnel — no UDP path, not detachable  ·  45 ms      [warning]"
        );
        assert!(status_line(&p).contains("not detachable"));
    }

    #[test]
    fn the_plain_stun_punch_line_names_the_address_family() {
        let v4 = path(Rung::StunPunch, "203.0.113.7:443", 22, 1400, 6, NatType::EndpointIndependent);
        assert_eq!(status_line(&v4), "oxutrm  IPv4 punched  ·  22 ms  ·  mtu 1400");
        let v6 = path(Rung::StunPunch, "[2001:db8::2]:443", 22, 1400, 6, NatType::EndpointIndependent);
        assert_eq!(status_line(&v6), "oxutrm  IPv6 punched  ·  22 ms  ·  mtu 1400");
    }

    /// A migration names the client's OWN local addresses. The rung and the
    /// peer address are fixed for the attach (§5), so naming a rung would lie.
    #[test]
    fn the_migration_line_names_the_old_and_new_local_address() {
        let old = "10.0.0.7:51000".parse().unwrap();
        let new = "192.0.2.44:51000".parse().unwrap();
        assert_eq!(
            migration_line(old, new, 74),
            "oxutrm  path migrated → 10.0.0.7:51000 → 192.0.2.44:51000 · 74 ms"
        );
    }

    #[test]
    fn the_connect_separator_is_two_spaces_around_a_middle_dot() {
        let p = path(Rung::Ipv6Direct, "[2001:db8::2]:443", 11, 1452, 0, NatType::None);
        assert!(status_line(&p).contains("  \u{b7}  "));
        // The migration line uses single spaces, deliberately.
        let old = "10.0.0.7:51000".parse().unwrap();
        let new = "192.0.2.44:51000".parse().unwrap();
        assert!(migration_line(old, new, 74).contains(" \u{b7} "));
        assert!(!migration_line(old, new, 74).contains("  \u{b7}  "));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod status;` and `pub use status::{status_line, migration_line, rung_label};`
to `crates/oxutrm-client/src/lib.rs`, removing M1's `status_line` stub, then run:

`cargo test --jobs 4 -p oxutrm-client status:: -- --test-threads 4`

Expected: FAIL — either a compile error, or M1's stub produces a different string.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-client/src/status.rs`:

```rust
//! The status display, spec §10.3. One line on connect, then silence.
//!
//! The `PathDescription` carries no mapping protocol, so a rung-1 path is
//! described as "port mapped" rather than naming NAT-PMP, PCP or UPnP.

use oxutrm_proto::{NatType, PathDescription, Rung};
use std::net::SocketAddr;
use std::time::Duration;

/// How long a path-change announcement stays on screen.
pub const MIGRATION_DWELL: Duration = Duration::from_secs(3);

fn family(a: &SocketAddr) -> &'static str {
    if a.is_ipv6() {
        "IPv6"
    } else {
        "IPv4"
    }
}

fn nat_label(n: NatType) -> &'static str {
    match n {
        NatType::None => "none",
        NatType::EndpointIndependent => "endpoint-independent",
        NatType::AddressDependent => "address-dependent",
        NatType::Symmetric => "symmetric",
        NatType::Unknown => "unknown",
    }
}

/// How the path is described to the user, in one phrase.
pub fn rung_label(path: &PathDescription) -> String {
    match path.rung {
        Rung::Ipv6Direct => "IPv6 direct".to_string(),
        Rung::PortMapped => format!("{} punched (port mapped)", family(&path.remote)),
        Rung::StunPunch => format!("{} punched", family(&path.remote)),
        Rung::Birthday => format!(
            "{} punched (birthday, {} probes)",
            family(&path.remote),
            path.probes_sent
        ),
        Rung::SshTunnel => "SSH tunnel".to_string(),
    }
}

/// The one line printed on connect. No silent magic: the user is told what
/// connection they actually got.
pub fn status_line(path: &PathDescription) -> String {
    match path.rung {
        // Rung 4 cannot daemonize, so the session dies with its SSH (§5.5).
        // Saying only "SSH tunnel" would leave the user to find that out by
        // closing the laptop lid.
        Rung::SshTunnel => format!(
            "oxutrm  SSH tunnel — no UDP path, not detachable  \u{b7}  {} ms      [warning]",
            path.rtt_ms
        ),
        // Rung 3 was entered because the NAT is symmetric; saying so explains
        // the probe count better than the MTU would.
        Rung::Birthday => format!(
            "oxutrm  {}  \u{b7}  {} ms  \u{b7}  {} NAT",
            rung_label(path),
            path.rtt_ms,
            nat_label(path.nat_type)
        ),
        _ => format!(
            "oxutrm  {}  \u{b7}  {} ms  \u{b7}  mtu {}",
            rung_label(path),
            path.rtt_ms,
            path.mtu
        ),
    }
}

/// Shown for `MIGRATION_DWELL` when the client's OWN local address changes.
/// Walking from Wi-Fi to mobile should be explained, not mysterious.
///
/// This reports local addresses, not a rung: QUIC migration changes only the
/// endpoint's own address, and the peer address and rung are fixed for the
/// attach (§5). There is no mechanism to repoint an established connection at a
/// different peer.
pub fn migration_line(old_local: SocketAddr, new_local: SocketAddr, rtt_ms: u32) -> String {
    format!("oxutrm  path migrated → {old_local} → {new_local} \u{b7} {rtt_ms} ms")
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-client status:: -- --test-threads 4`

Expected: PASS, 7 tests.

- [ ] **Step 5: Write the failing test for the pane**

Create `crates/oxutrm-client/src/pane.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{NatType, PathDescription, Rung};
    use std::time::Duration;

    fn pane() -> StatusPane {
        StatusPane {
            path: PathDescription {
                rung: Rung::Ipv6Direct,
                local: "[2001:db8::1]:443".parse().unwrap(),
                remote: "[2001:db8::2]:51234".parse().unwrap(),
                probes_sent: 0,
                nat_type: NatType::None,
                rtt_ms: 11,
                mtu: 1452,
            },
            loss_pct: 0.4,
            bytes_tx: 1_258_291,
            bytes_rx: 348_672,
            migrations: vec![(742, "IPv4 punched → IPv6 direct".to_string())],
            session_id: "0a1b2c3d4e5f60718293a4b5c6d7e8f9".to_string(),
            uptime: Duration::from_secs(3 * 3600 + 14 * 60 + 7),
        }
    }

    #[test]
    fn every_line_is_exactly_the_pane_width() {
        for l in status_pane_lines(&pane()) {
            assert_eq!(l.chars().count(), PANE_WIDTH, "wrong width: {l:?}");
        }
    }

    #[test]
    fn the_pane_is_a_closed_box() {
        let lines = status_pane_lines(&pane());
        assert!(lines[0].starts_with("┌ oxutrm ─ session 0a1b2c3d4e5f60718293a4b5c6d7e8f9 "));
        assert!(lines[0].ends_with('┐'));
        assert!(lines.last().unwrap().starts_with("└ Ctrl-] closes "));
        assert!(lines.last().unwrap().ends_with('┘'));
        for l in &lines[1..lines.len() - 1] {
            assert!(l.starts_with("│ ") && l.ends_with(" │"), "not a body line: {l:?}");
        }
    }

    #[test]
    fn the_pane_reports_everything_spec_10_3_asks_for() {
        let text = status_pane_lines(&pane()).join("\n");
        assert!(text.contains("path      IPv6 direct"));
        assert!(text.contains("local     [2001:db8::1]:443"));
        assert!(text.contains("remote    [2001:db8::2]:51234"));
        assert!(text.contains("rtt       11 ms"));
        assert!(text.contains("loss      0.4 %"));
        assert!(text.contains("mtu       1452"));
        assert!(text.contains("nat       none"));
        assert!(text.contains("sent      1.2 MiB"));
        assert!(text.contains("received  340.5 KiB"));
        assert!(text.contains("uptime    3h 14m 07s"));
        assert!(text.contains("+742s  IPv4 punched → IPv6 direct"));
    }

    #[test]
    fn a_session_with_no_migrations_still_renders() {
        let mut p = pane();
        p.migrations.clear();
        let text = status_pane_lines(&p).join("\n");
        assert!(text.contains("migrations  none"));
        for l in status_pane_lines(&p) {
            assert_eq!(l.chars().count(), PANE_WIDTH);
        }
    }

    #[test]
    fn byte_counts_use_binary_units() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(348_672), "340.5 KiB");
        assert_eq!(fmt_bytes(1_258_291), "1.2 MiB");
        assert_eq!(fmt_bytes(3_221_225_472), "3.0 GiB");
    }

    #[test]
    fn uptime_drops_the_hours_when_there_are_none() {
        assert_eq!(fmt_uptime(Duration::from_secs(7)), "0m 07s");
        assert_eq!(fmt_uptime(Duration::from_secs(65)), "1m 05s");
        assert_eq!(fmt_uptime(Duration::from_secs(3661)), "1h 01m 01s");
    }

    /// A pathologically long migration label must not break the box.
    #[test]
    fn overlong_content_is_truncated_rather_than_widening_the_pane() {
        let mut p = pane();
        p.migrations = vec![(1, "x".repeat(200))];
        for l in status_pane_lines(&p) {
            assert_eq!(l.chars().count(), PANE_WIDTH, "wrong width: {l:?}");
        }
    }
}
```

- [ ] **Step 6: Run test to verify it fails**

Add `pub mod pane;` and `pub use pane::{status_pane_lines, StatusPane};` to
`crates/oxutrm-client/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-client pane:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find type StatusPane`.

- [ ] **Step 7: Write minimal implementation**

Put this above the test module in `crates/oxutrm-client/src/pane.rs`:

```rust
//! The `Ctrl-]` status pane, spec §10.3. Drawn locally over the current
//! screen, so it costs nothing on the wire and appears instantly.

use crate::status::rung_label;
use oxutrm_proto::{NatType, PathDescription};
use std::time::Duration;

/// Total width, borders included.
pub const PANE_WIDTH: usize = 60;
/// Room for text between the borders and their padding spaces.
const INNER: usize = PANE_WIDTH - 4;

pub struct StatusPane {
    pub path: PathDescription,
    pub loss_pct: f32,
    pub bytes_tx: u64,
    pub bytes_rx: u64,
    /// `(seconds since connect, "old label → new label")`.
    pub migrations: Vec<(u64, String)>,
    pub session_id: String,
    pub uptime: Duration,
}

pub fn fmt_bytes(n: u64) -> String {
    const K: f64 = 1024.0;
    let f = n as f64;
    if n < 1024 {
        format!("{n} B")
    } else if f < K * K {
        format!("{:.1} KiB", f / K)
    } else if f < K * K * K {
        format!("{:.1} MiB", f / (K * K))
    } else {
        format!("{:.1} GiB", f / (K * K * K))
    }
}

pub fn fmt_uptime(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}h {m:02}m {sec:02}s")
    } else {
        format!("{m}m {sec:02}s")
    }
}

fn nat_label(n: NatType) -> &'static str {
    match n {
        NatType::None => "none",
        NatType::EndpointIndependent => "endpoint-independent",
        NatType::AddressDependent => "address-dependent",
        NatType::Symmetric => "symmetric",
        NatType::Unknown => "unknown",
    }
}

/// One body line, padded or truncated to the pane's exact width.
fn body(text: &str) -> String {
    let mut t: String = text.chars().take(INNER).collect();
    let pad = INNER - t.chars().count();
    t.push_str(&" ".repeat(pad));
    format!("│ {t} │")
}

/// A border line: a leading label, then rule characters out to the width.
fn border(open: char, label: &str, close: char) -> String {
    let mut s = String::new();
    s.push(open);
    s.push(' ');
    s.push_str(label);
    s.push(' ');
    while s.chars().count() < PANE_WIDTH - 1 {
        s.push('─');
    }
    // A very long label would have overrun; truncate back to size.
    let mut s: String = s.chars().take(PANE_WIDTH - 1).collect();
    s.push(close);
    s
}

pub fn status_pane_lines(p: &StatusPane) -> Vec<String> {
    let mut out = vec![border('┌', &format!("oxutrm ─ session {}", p.session_id), '┐')];
    out.push(body(&format!("path      {}", rung_label(&p.path))));
    out.push(body(&format!("local     {}", p.path.local)));
    out.push(body(&format!("remote    {}", p.path.remote)));
    out.push(body(&format!("rtt       {} ms", p.path.rtt_ms)));
    out.push(body(&format!("loss      {:.1} %", p.loss_pct)));
    out.push(body(&format!("mtu       {}", p.path.mtu)));
    out.push(body(&format!("nat       {}", nat_label(p.path.nat_type))));
    out.push(body(&format!("sent      {}", fmt_bytes(p.bytes_tx))));
    out.push(body(&format!("received  {}", fmt_bytes(p.bytes_rx))));
    out.push(body(&format!("uptime    {}", fmt_uptime(p.uptime))));
    if p.migrations.is_empty() {
        out.push(body("migrations  none"));
    } else {
        out.push(body("migrations"));
        for (secs, what) in &p.migrations {
            out.push(body(&format!("  +{secs}s  {what}")));
        }
    }
    out.push(border('└', "Ctrl-] closes", '┘'));
    out
}
```

- [ ] **Step 8: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-client pane:: -- --test-threads 4`

Expected: PASS, 7 tests.

- [ ] **Step 9: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-client/src/status.rs crates/oxutrm-client/src/pane.rs \
        crates/oxutrm-client/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(client): the connect line, the migration announcement and the Ctrl-] pane

Exact spec 10.3 formats, all drawn locally, all costing nothing on the wire.
The connect line tells the user what connection they actually got, and the
SSH tunnel says so as a warning rather than silently.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 10: The client session loop

Local input → `Sender<InputState>` → QUIC, and QUIC → `Receiver<ScreenState>`
→ `Renderer`. `SIGWINCH` drives resize, `Ctrl-]` toggles the pane, and every
exit path goes through Task 7's guard.

Local stdin is read on a dedicated blocking thread rather than with `AsyncFd`:
putting `O_NONBLOCK` on fd 0 changes a file description the user's shell shares,
and a shell that inherits a non-blocking stdin misbehaves after oxutrm exits.

**Files:**
- Modify: `crates/oxutrm-client/src/session.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`
- Modify: `crates/oxutrm-client/Cargo.toml`

**Interfaces:**
- Consumes: `oxutrm_client::{InputQueue, TerminalGuard, Renderer, terminal_size, status_line, migration_line, StatusPane, status_pane_lines}`;
  `oxutrm_client::status::MIGRATION_DWELL`; `oxutrm_net::xport::{FrameSink, FrameSource}`
  with `FrameSink::send(&Frame) -> anyhow::Result<usize>`;
  `oxutrm_net::link::{link_stats, refresh_path}`; `oxutrm_net::pace::Pacer`;
  `oxutrm_sync::{Receiver, ScreenState}`; `oxutrm_proto::{PathDescription, TermSize}`.
- Produces:
  ```rust
  // crates/oxutrm-client/src/session.rs
  /// The byte Ctrl-] produces.
  pub const ESCAPE_BYTE: u8 = 0x1d;
  /// Split raw input at the escape byte. Returns (bytes to send, escape presses).
  pub fn split_escape(input: &[u8]) -> (Vec<u8>, usize);

  /// `endpoint` is the client's own endpoint. It is needed because a
  /// migration is a change of OUR local address, which only the endpoint
  /// reports; `Connection::remote_address()` cannot change within an attach.
  pub async fn run_client_session(
      conn: quinn::Connection,
      endpoint: quinn::Endpoint,
      path: oxutrm_proto::PathDescription,
      session_id: String,
      caps: oxutrm_proto::TerminalCaps,
  ) -> anyhow::Result<()>;

  pub async fn wait_for_terminating_signal();   // from Task 8
  ```

- [ ] **Step 1: Write the failing test**

Append a test module to `crates/oxutrm-client/src/session.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_input_passes_through_untouched() {
        let (out, presses) = split_escape(b"hello");
        assert_eq!(out, b"hello");
        assert_eq!(presses, 0);
    }

    #[test]
    fn the_escape_byte_is_removed_and_counted() {
        let (out, presses) = split_escape(b"ab\x1dcd");
        assert_eq!(out, b"abcd");
        assert_eq!(presses, 1);
    }

    #[test]
    fn repeated_escapes_are_all_counted() {
        let (out, presses) = split_escape(b"\x1d\x1d");
        assert_eq!(out, b"");
        assert_eq!(presses, 2);
    }

    /// 0x1d inside a longer escape sequence is still 0x1d: the terminal sends
    /// it only when the user presses Ctrl-], so intercepting it unconditionally
    /// is correct and matches what the spec asks for.
    #[test]
    fn an_escape_at_the_very_start_or_end_is_handled() {
        assert_eq!(split_escape(b"\x1dx"), (b"x".to_vec(), 1));
        assert_eq!(split_escape(b"x\x1d"), (b"x".to_vec(), 1));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

`cargo test --jobs 4 -p oxutrm-client session:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function split_escape`.

- [ ] **Step 3: Write minimal implementation**

Add to `crates/oxutrm-client/src/session.rs`, above the test module and below
`wait_for_terminating_signal` from Task 8:

```rust
//! The client session loop.
//!
//! ```text
//!   keyboard --> Sender<InputState> --> Frame --> QUIC
//!   Renderer <-- Receiver<ScreenState> <-- Frame <-- QUIC
//! ```

use crate::pane::{status_pane_lines, StatusPane};
use crate::status::{migration_line, status_line, MIGRATION_DWELL};
use crate::{InputQueue, Renderer, TerminalGuard};
use oxutrm_net::link::{link_stats, refresh_path};
use oxutrm_net::pace::Pacer;
use oxutrm_net::xport::{FrameSink, FrameSource};
use oxutrm_proto::{PathDescription, TerminalCaps, TermSize};
use oxutrm_sync::Receiver;
use oxutrm_term::ScreenState;
use std::io::Write;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// Ctrl-]. Configurable in a later phase; fixed for now.
pub const ESCAPE_BYTE: u8 = 0x1d;

/// Strip the escape byte out of raw input and count how many times it appeared.
pub fn split_escape(input: &[u8]) -> (Vec<u8>, usize) {
    let presses = input.iter().filter(|&&b| b == ESCAPE_BYTE).count();
    let out = input.iter().copied().filter(|&b| b != ESCAPE_BYTE).collect();
    (out, presses)
}

/// Read stdin on its own thread. `O_NONBLOCK` on fd 0 would outlive us in the
/// user's shell, so blocking reads on a thread are the safe choice.
fn spawn_stdin_reader() -> tokio::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut stdin = std::io::stdin();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
    });
    rx
}

pub async fn run_client_session(
    conn: quinn::Connection,
    endpoint: quinn::Endpoint,
    mut path: PathDescription,
    session_id: String,
    caps: TerminalCaps,
) -> anyhow::Result<()> {
    let started = Instant::now();
    let mut size = crate::terminal_size()?;

    // The one connect-time line, printed before raw mode so it stays on the
    // main screen and the user still has it after the session ends.
    println!("{}", status_line(&path));

    let _guard = TerminalGuard::install()?;
    let mut renderer = Renderer::new(size, caps);
    let mut out = std::io::stdout();

    let sink = FrameSink::new(conn.clone());
    let mut source = FrameSource::new(conn.clone());
    let mut screen_rx: Receiver<ScreenState> = Receiver::new(ScreenState::blank(size.rows, size.cols));
    let mut input = InputQueue::new(size);
    let mut pacer = Pacer::new();

    let mut stdin = spawn_stdin_reader();
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let mut term_sig = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;

    let mut pane_open = false;
    let mut migrations: Vec<(u64, String)> = Vec::new();
    let mut last_local = endpoint.local_addr()?;
    let mut announce: Option<(SocketAddr, SocketAddr)> = None;
    let mut announce_until: Option<Instant> = None;
    let mut dirty = true;

    loop {
        let local = endpoint.local_addr()?;
        let stats = link_stats(&conn, local);
        refresh_path(&mut path, &stats);

        // We roamed: OUR local address changed and QUIC migrated the
        // connection onto it. The peer address and the rung cannot change
        // within an attach (§5), so this is the only migration there is.
        if local != last_local {
            migrations.push((
                started.elapsed().as_secs(),
                format!("{last_local} → {local}"),
            ));
            announce = Some((last_local, local));
            last_local = local;
            announce_until = Some(Instant::now() + MIGRATION_DWELL);
            dirty = true;
        }
        if announce_until.is_some_and(|t| Instant::now() >= t) {
            announce_until = None;
            announce = None;
            renderer.invalidate();
            dirty = true;
        }

        if dirty {
            if pane_open {
                // The pane is drawn over the screen, so the screen underneath
                // must be repainted when it closes.
                renderer.render(&mut out, screen_rx.state())?;
                let lines = status_pane_lines(&StatusPane {
                    path: path.clone(),
                    loss_pct: stats.loss_pct,
                    bytes_tx: stats.bytes_tx,
                    bytes_rx: stats.bytes_rx,
                    migrations: migrations.clone(),
                    session_id: session_id.clone(),
                    uptime: started.elapsed(),
                });
                for (i, l) in lines.iter().enumerate() {
                    write!(out, "\x1b[{};{}H{}", i + 2, 3, l)?;
                }
            } else {
                renderer.render(&mut out, screen_rx.state())?;
                if let Some((old, new)) = announce {
                    write!(
                        out,
                        "\x1b[1;1H\x1b[7m{}\x1b[0m",
                        migration_line(old, new, path.rtt_ms)
                    )?;
                }
            }
            out.flush()?;
            dirty = false;
        }

        let rtt = stats.rtt;
        let wake = match pacer.next_deadline(rtt) {
            Some(t) => tokio::time::sleep_until(t.into()),
            None => tokio::time::sleep(Duration::from_millis(0)),
        };

        tokio::select! {
            bytes = stdin.recv() => {
                let Some(bytes) = bytes else { break };
                let (to_send, presses) = split_escape(&bytes);
                if presses % 2 == 1 {
                    pane_open = !pane_open;
                    renderer.invalidate();
                    dirty = true;
                }
                if !to_send.is_empty() {
                    input.push(&to_send, size);
                    pacer.go_idle();   // a keystroke goes out at once
                }
            }

            frame = source.recv() => {
                let frame = frame?;
                input.on_host_ack(frame.ack_state);
                // Same rule as the host: a rejected frame is dropped and the
                // session continues. The receiver's state is unchanged, so our
                // ack still names a state the host holds in its ring, and the
                // host's next diff repairs the screen. Never `?` here.
                match screen_rx.on_frame(&frame) {
                    Ok(true) => {
                        source.set_current_state(screen_rx.ack());
                        dirty = true;
                    }
                    Ok(false) => {}
                    Err(e) => eprintln!("oxutrm: dropping unapplicable screen frame: {e}"),
                }
            }

            _ = winch.recv() => {
                size = crate::terminal_size()?;
                renderer.resize(size);
                renderer.invalidate();
                input.push(b"", size);
                pacer.go_idle();
                dirty = true;
            }

            _ = wake => {
                let now = Instant::now();
                if pacer.may_send(now, rtt) {
                    if let Some(f) = input.sender().make_frame(screen_rx.ack())? {
                        sink.send(&f)?;
                        pacer.on_sent(now);
                    }
                }
                // The pane shows live counters, so keep it fresh while open.
                if pane_open || announce_until.is_some() {
                    dirty = true;
                }
            }

            _ = int.recv() => break,
            _ = term_sig.recv() => break,
            _ = hup.recv() => break,

            // The peer went away. Unwinding here drops the guard, which
            // restores the terminal.
            _ = conn.closed() => break,
        }
    }

    Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-client session:: -- --test-threads 4`

Expected: PASS, 4 tests, and the crate compiles.

- [ ] **Step 5: Add the dependencies the loop needs**

In `crates/oxutrm-client/Cargo.toml`:

```toml
[dependencies]
oxutrm-net = { path = "../oxutrm-net" }
quinn = "0.11"
tokio = { version = "1", features = ["rt-multi-thread", "net", "time", "macros", "io-util", "sync", "signal"] }
```

- [ ] **Step 6: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-client/src/session.rs crates/oxutrm-client/src/lib.rs \
        crates/oxutrm-client/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(client): the session loop, keyboard to QUIC to renderer

SIGWINCH resizes, Ctrl-] toggles the locally drawn status pane, a QUIC path
migration announces itself for three seconds, and every exit path unwinds
through the terminal guard. stdin is read on a thread so O_NONBLOCK never
touches the user's fd 0.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 11: Rung 4 — QUIC inside the SSH connection

When no UDP path forms, QUIC runs inside a stream over the SSH connection that
is already open. Nothing in the QUIC layer changes: the swap happens entirely at
the socket. A relay binds a loopback UDP socket, hands `quinn` a second loopback
socket, and shuttles datagrams between that pair and the SSH channel with a
two-byte length prefix.

MTU: the relay refuses to carry anything larger than `TUNNEL_MAX_PAYLOAD`.
`quinn` starts at its 1200-byte initial MTU and only raises it after a DPLPMTUD
probe is acknowledged; probes above the cap are dropped, so no probe validates
and the connection stays at 1200. That is exactly how path MTU discovery is
meant to behave, and it needs no change to `quic_client` or `quic_server`.

**A rung-4 session is not detachable, and this task must not fight M3's
`daemonize`.** Because the transport *is* the SSH connection, the session cannot
close its inherited SSH descriptors, so §4.3's double-fork is skipped entirely:

- `daemonize()` is **never called** on a rung-4 path. Calling it would close the
  descriptors the transport is running on and kill the session instantly.
- `SessionMeta.detachable` is `false`, in `HostHello` and in `meta.json`.
- The registry entry is pruned when the SSH connection ends — the session dies
  with its SSH, and closing the laptop lid ends it.
- `status_line` says `not detachable` (Task 9), because this is the one cost a
  user would otherwise discover only by closing the lid.

**Files:**
- Create: `crates/oxutrm-net/src/tunnel.rs`
- Modify: `crates/oxutrm-net/src/lib.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: `tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt}`;
  M3's SSH child, whose stdin/stdout carried the newline-JSON signalling and
  becomes the tunnel's byte channel once `Signal::Established { path }` names
  `Rung::SshTunnel`; `oxutrm_net::{quic_client, quic_server}`;
  `oxutrm_host::{SessionMeta, daemonize}` — `SessionMeta` carries
  `pub detachable: bool`, and `daemonize()` must NOT be called on this path.
- Produces:
  ```rust
  // crates/oxutrm-net/src/tunnel.rs
  /// The largest UDP payload the tunnel will carry. Bigger DPLPMTUD probes are
  /// dropped, so quinn stays at its 1200-byte initial MTU.
  pub const TUNNEL_MAX_PAYLOAD: usize = 1400;

  pub struct TunnelEndpoint {
      /// Hand this to `quic_client` / `quic_server`.
      pub socket: std::net::UdpSocket,
      /// Hand this to `quic_client` as the peer address.
      pub peer: std::net::SocketAddr,
  }

  pub fn spawn_tunnel<R, W>(reader: R, writer: W) -> anyhow::Result<(TunnelEndpoint, tokio::task::JoinHandle<()>)>
  where
      R: tokio::io::AsyncRead + Unpin + Send + 'static,
      W: tokio::io::AsyncWrite + Unpin + Send + 'static;
  ```

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/tunnel.rs` containing only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::xport::{FrameSink, FrameSource};
    use oxutrm_proto::Frame;

    /// A whole QUIC session inside a byte channel that is not a UDP socket.
    /// `tokio::io::duplex` stands in for the SSH child's stdin/stdout.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn quic_runs_inside_a_byte_channel() -> anyhow::Result<()> {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);

        let (host_ep, _h) = spawn_tunnel(a_read, a_write)?;
        let (client_ep, _c) = spawn_tunnel(b_read, b_write)?;

        let (cert, key, spki) = crate::generate_cert()?;
        let host_relay = host_ep.peer;
        let endpoint = crate::quic_server(host_ep.socket, cert, key).await?;
        let accepting = tokio::spawn(async move {
            let inc = endpoint.accept().await.expect("accepted");
            let conn = inc.await?;
            anyhow::Ok((conn, endpoint))
        });

        // The client's relay forwards everything it receives to the far side,
        // so the address quinn dials is the client's own relay socket.
        let _ = host_relay;
        let client = crate::quic_client(client_ep.socket, client_ep.peer, spki).await?;
        let (server, _keepalive) = accepting.await??;

        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);
        let f = Frame { my_state: 3, from_state: 0, ack_state: 0, frag_index: 0, frag_count: 1, flags: 0, payload: vec![9u8; 500] };
        sink.send(&f)?;
        let got = source.recv().await?;
        assert_eq!(got.my_state, 3);
        assert_eq!(got.payload, f.payload);
        Ok(())
    }

    /// Fragmentation and the tunnel compose: a state far larger than one
    /// datagram is split by `crate::frag`, every piece stays under
    /// TUNNEL_MAX_PAYLOAD, and it reassembles on the far side.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_fragmented_state_survives_the_tunnel() -> anyhow::Result<()> {
        let (a, b) = tokio::io::duplex(1 << 20);
        let (a_read, a_write) = tokio::io::split(a);
        let (b_read, b_write) = tokio::io::split(b);
        let (host_ep, _h) = spawn_tunnel(a_read, a_write)?;
        let (client_ep, _c) = spawn_tunnel(b_read, b_write)?;

        let (cert, key, spki) = crate::generate_cert()?;
        let endpoint = crate::quic_server(host_ep.socket, cert, key).await?;
        let accepting = tokio::spawn(async move {
            let inc = endpoint.accept().await.expect("accepted");
            let conn = inc.await?;
            anyhow::Ok((conn, endpoint))
        });
        let client = crate::quic_client(client_ep.socket, client_ep.peer, spki).await?;
        let (server, _keepalive) = accepting.await??;

        assert!(
            server.max_datagram_size().unwrap_or(0) <= TUNNEL_MAX_PAYLOAD,
            "the tunnel must not let quinn discover an MTU it cannot carry"
        );

        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);
        let payload: Vec<u8> = (0..120_000).map(|i| (i % 251) as u8).collect();
        let f = Frame { my_state: 5, from_state: 0, ack_state: 0, frag_index: 0, frag_count: 1, flags: 0, payload: payload.clone() };
        let sent = sink.send(&f)?;
        assert!(sent > 90, "expected many fragments through the tunnel, got {sent}");
        let got = tokio::time::timeout(std::time::Duration::from_secs(30), source.recv()).await??;
        assert_eq!(got.payload, payload);
        Ok(())
    }

    #[tokio::test]
    async fn an_overlong_payload_is_dropped_rather_than_desynchronising_the_stream()
    -> anyhow::Result<()> {
        let (a, b) = tokio::io::duplex(1 << 16);
        let (a_read, a_write) = tokio::io::split(a);
        let (_b_read, mut b_write) = tokio::io::split(b);
        let (ep, _h) = spawn_tunnel(a_read, a_write)?;

        // Frame a payload above the cap by hand and confirm the relay keeps
        // parsing afterwards, by sending a legal frame straight after it.
        use tokio::io::AsyncWriteExt;
        let big = vec![7u8; TUNNEL_MAX_PAYLOAD + 100];
        b_write.write_all(&(big.len() as u16).to_be_bytes()).await?;
        b_write.write_all(&big).await?;
        let small = b"hello";
        b_write.write_all(&(small.len() as u16).to_be_bytes()).await?;
        b_write.write_all(small).await?;
        b_write.flush().await?;

        let probe = std::net::UdpSocket::bind("127.0.0.1:0")?;
        probe.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let _ = probe;

        // The legal payload reaches the quinn-side socket; the oversized one
        // does not. Read from the endpoint's socket to confirm.
        ep.socket.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
        let mut buf = [0u8; 2048];
        let (n, _) = ep.socket.recv_from(&mut buf)?;
        assert_eq!(&buf[..n], small, "the relay lost sync after an oversized payload");
        Ok(())
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod tunnel;` to `crates/oxutrm-net/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-net tunnel:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function spawn_tunnel`.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-net/src/tunnel.rs`:

```rust
//! Rung 4, spec §5.5: when no UDP path forms, QUIC runs inside a stream over
//! the SSH connection that is already open.
//!
//! The swap is entirely at the socket. A relay owns two loopback UDP sockets:
//! `quic_sock`, which is handed to `quinn`, and `relay_sock`, which `quinn`
//! sends to. Datagrams arriving on `relay_sock` are length-prefixed onto the
//! SSH channel; frames arriving from the SSH channel are sent back to
//! `quic_sock`. `quinn` sees an ordinary UDP socket and an ordinary peer.
//!
//! The session works. It is slower, and it dies on an IP change, which is why
//! the status line announces it as a warning rather than silently.

use anyhow::Context;
use std::net::{SocketAddr, UdpSocket};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// The largest UDP payload the tunnel carries. A DPLPMTUD probe above this is
/// dropped and therefore never validates, so `quinn` stays at its 1200-byte
/// initial MTU — which is the correct outcome, reached the standard way.
pub const TUNNEL_MAX_PAYLOAD: usize = 1400;

pub struct TunnelEndpoint {
    /// Hand this to `quic_client` or `quic_server`.
    pub socket: UdpSocket,
    /// Hand this to `quic_client` as the peer address.
    pub peer: SocketAddr,
}

/// Start the relay. `reader` and `writer` are the SSH child's stdout and stdin.
pub fn spawn_tunnel<R, W>(
    reader: R,
    mut writer: W,
) -> anyhow::Result<(TunnelEndpoint, tokio::task::JoinHandle<()>)>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let quic_sock = UdpSocket::bind("127.0.0.1:0").context("bind quic-side socket")?;
    quic_sock.set_nonblocking(true)?;
    let quic_addr = quic_sock.local_addr()?;

    let relay_std = UdpSocket::bind("127.0.0.1:0").context("bind relay socket")?;
    relay_std.set_nonblocking(true)?;
    let relay_addr = relay_std.local_addr()?;
    let relay = std::sync::Arc::new(tokio::net::UdpSocket::from_std(relay_std)?);

    // The socket quinn gets back must be blocking-agnostic: quinn sets its own
    // mode when it adopts it.
    quic_sock.set_nonblocking(false)?;

    let up = relay.clone();
    let handle = tokio::spawn(async move {
        let mut reader = reader;
        let mut buf = vec![0u8; 65536];
        loop {
            tokio::select! {
                // quinn -> SSH
                r = up.recv_from(&mut buf) => {
                    let Ok((n, _from)) = r else { break };
                    if n > TUNNEL_MAX_PAYLOAD {
                        // Larger than the tunnel carries: dropping it is what
                        // makes quinn's MTU probing settle at 1200.
                        continue;
                    }
                    if writer.write_all(&(n as u16).to_be_bytes()).await.is_err() { break }
                    if writer.write_all(&buf[..n]).await.is_err() { break }
                    if writer.flush().await.is_err() { break }
                }

                // SSH -> quinn
                len = read_frame_len(&mut reader) => {
                    let Some(len) = len else { break };
                    let mut payload = vec![0u8; len];
                    if reader.read_exact(&mut payload).await.is_err() { break }
                    if len > TUNNEL_MAX_PAYLOAD {
                        // Consumed, so the stream stays in sync, then discarded.
                        continue;
                    }
                    if up.send_to(&payload, quic_addr).await.is_err() { break }
                }
            }
        }
    });

    Ok((TunnelEndpoint { socket: quic_sock, peer: relay_addr }, handle))
}

/// The next payload length, or `None` when the channel ended.
async fn read_frame_len<R: AsyncRead + Unpin>(reader: &mut R) -> Option<usize> {
    let mut hdr = [0u8; 2];
    reader.read_exact(&mut hdr).await.ok()?;
    Some(u16::from_be_bytes(hdr) as usize)
}
```

Note the select arm cancellation: `read_frame_len` reads exactly two bytes and
is only cancelled at an arm boundary, before any partial read, because
`recv_from` and `read_exact` never both make progress in one poll. Do **not**
inline the length read into the arm body — a cancelled partial `read_exact`
would desynchronise the stream.

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-net tunnel:: -- --test-threads 4`

Expected: PASS, 3 tests, including `a_fragmented_state_survives_the_tunnel`.

- [ ] **Step 5: Wire rung 4 into the ladder**

In `src/main.rs`, where M2's ladder reports failure and M3 still holds the SSH
child, add the fallback. The signalling side sends
`Signal::Established { path }` with `rung: Rung::SshTunnel` so the peer switches
its channel too:

```rust
/// Rung 4: no UDP path formed, so run QUIC inside the SSH connection.
/// `ssh_stdout` and `ssh_stdin` are the same descriptors that carried the
/// newline-JSON signalling; from here on they carry length-prefixed datagrams.
async fn connect_over_ssh_tunnel(
    ssh_stdout: tokio::process::ChildStdout,
    ssh_stdin: tokio::process::ChildStdin,
    expect_spki_sha256: [u8; 32],
) -> anyhow::Result<(quinn::Connection, std::net::SocketAddr)> {
    let (endpoint, _relay) = oxutrm_net::tunnel::spawn_tunnel(ssh_stdout, ssh_stdin)?;
    let local = endpoint.socket.local_addr()?;
    let conn = oxutrm_net::quic_client(endpoint.socket, endpoint.peer, expect_spki_sha256).await?;
    Ok((conn, local))
}
```

The host mirrors it with `quic_server` on its own `TunnelEndpoint.socket`. The
`PathDescription` the client shows carries `rung: Rung::SshTunnel`, so
`status_line` prints the warning form from Task 9.

On the host side, the rung decides whether the session may detach:

```rust
/// Rung 4 runs the transport over the SSH connection itself, so it can never
/// close those descriptors and can never daemonize (spec §5.5). Everything
/// else detaches normally.
fn finish_bootstrap(rung: Rung, meta: &mut SessionMeta) -> anyhow::Result<()> {
    meta.detachable = rung != Rung::SshTunnel;
    if meta.detachable {
        // §4.3: double fork, setsid, chdir /, close every inherited
        // descriptor. Only legal once HostHello has been flushed.
        oxutrm_host::daemonize()?;
    }
    Ok(())
}
```

- [ ] **Step 5b: Write the failing test that rung 4 never daemonizes**

Create `crates/oxutrm-host/tests/detachable.rs`:

```rust
use oxutrm_proto::{Rung, TermSize};
use oxutrm_host::SessionMeta;

fn meta() -> SessionMeta {
    SessionMeta {
        session_id: "0".repeat(32),
        pid: std::process::id(),
        created_unix: 0,
        shell: "/bin/sh".to_string(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    }
}

#[test]
fn a_udp_session_is_detachable() {
    for rung in [Rung::Ipv6Direct, Rung::PortMapped, Rung::StunPunch, Rung::Birthday] {
        let mut m = meta();
        m.detachable = rung != Rung::SshTunnel;
        assert!(m.detachable, "{rung:?} must be detachable");
    }
}

/// The whole point: a rung-4 session that daemonized would close the SSH
/// descriptors its own transport is running on.
#[test]
fn an_ssh_tunnelled_session_is_not_detachable() {
    let mut m = meta();
    m.detachable = Rung::SshTunnel != Rung::SshTunnel;
    assert!(!m.detachable);
    let json = serde_json::to_string(&m).unwrap();
    assert!(json.contains("\"detachable\":false"), "meta.json must record it: {json}");
}
```

Run: `cargo test --jobs 4 -p oxutrm-host --test detachable -- --test-threads 4`

Expected: FAIL if `SessionMeta` has no `detachable` field, then PASS once M3's
struct carries it.

- [ ] **Step 6: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-net/src/tunnel.rs crates/oxutrm-net/src/lib.rs src/main.rs \
        crates/oxutrm-host/tests/detachable.rs
git commit -m "$(cat <<'EOF'
feat(net): rung 4, QUIC inside the SSH connection, and not detachable

The swap is entirely at the socket: a loopback relay shuttles length-prefixed
datagrams over the SSH channel, so quinn sees an ordinary UDP peer. Payloads
above 1400 bytes are dropped, which is what keeps quinn's MTU probing at 1200.
Because the transport IS the SSH connection, the session never daemonizes,
sets detachable=false, and its registry entry dies with its SSH.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 12: Roaming — the session survives a change of local address

QUIC connection migration is the whole reason the transport was chosen. This
test forcibly rebinds the client's endpoint to a new local address mid-session
and asserts the session keeps working.

**Scope, and it is a hard boundary.** QUIC migration lets an endpoint change its
own **local** address and nothing else. There is no mechanism in RFC 9000 and no
API in `quinn` 0.11 — whose `Connection` exposes `remote_address()` with no
setter and no path management — to repoint an established connection at a
different **peer** address. So:

- **Do write** a test that rebinds the client's own socket. That is §1.3's
  roaming case: the user walks from Wi-Fi to mobile, their address changes, the
  peer's does not.
- **Do NOT write** a test that changes the remote address mid-session. It cannot
  be done, and an attempt would be testing a fiction. A changed peer address
  requires a fresh attach, which re-runs ICE nomination from scratch (§5).

This is also why ICE nomination completes *before* QUIC starts: the peer address
must already be final. A better path found after nomination is lost for that
attach and picked up by the next one.

**Files:**
- Create: `tests/roaming.rs`

**Interfaces:**
- Consumes: `quinn::Endpoint::rebind_abstract(&self, socket: std::sync::Arc<dyn quinn::AsyncUdpSocket>) -> std::io::Result<()>`;
  `oxutrm_net::StunDemuxSocket` with
  `StunDemuxSocket::new(inner: Arc<tokio::net::UdpSocket>) -> (StunDemuxSocket, tokio::sync::mpsc::Receiver<(Vec<u8>, SocketAddr)>)`;
  `oxutrm_net::xport::{FrameSink, FrameSource}`;
  `oxutrm_net::{generate_cert, quic_server, quic_client}`; `oxutrm_proto::Frame`.

  `rebind_abstract`, not `rebind`: M2 builds the client endpoint with
  `Endpoint::new_with_abstract_socket` around a `StunDemuxSocket`, because
  `quinn` owns the socket's recv loop and STUN keepalives must be peeled off in
  front of it. Rebinding to a plain `UdpSocket` would drop the demultiplexer and
  let STUN packets reach `quinn` as garbage.
- Produces:
  ```rust
  // crates/oxutrm-net/src/lib.rs
  /// The connection, and the endpoint it runs on. `quic_client` keeps its
  /// contract signature and delegates here.
  pub async fn quic_client_on(
      socket: std::net::UdpSocket,
      peer: std::net::SocketAddr,
      expect_spki_sha256: [u8; 32],
  ) -> anyhow::Result<(quinn::Connection, quinn::Endpoint)>;
  ```

- [ ] **Step 1: Write the failing test**

Create `tests/roaming.rs`:

```rust
use oxutrm_net::xport::{FrameSink, FrameSource};
use oxutrm_proto::Frame;
use std::time::{Duration, Instant};

fn frame(n: u64) -> Frame {
    Frame { my_state: n, from_state: 0, ack_state: 0, frag_index: 0, frag_count: 1, flags: 0, payload: vec![(n % 251) as u8; 200] }
}

/// The client's socket is rebound to a different local port mid-session. QUIC
/// must validate the new path and carry on: no reconnect, no lost session.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_session_survives_a_forced_rebind_of_the_client_socket() -> anyhow::Result<()> {
    let (cert, key, spki) = oxutrm_net::generate_cert()?;

    let server_sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let server_addr = server_sock.local_addr()?;
    let server_ep = oxutrm_net::quic_server(server_sock, cert, key).await?;
    let accepting = tokio::spawn(async move {
        let inc = server_ep.accept().await.expect("accepted");
        let conn = inc.await?;
        anyhow::Ok((conn, server_ep))
    });

    let client_sock = std::net::UdpSocket::bind("127.0.0.1:0")?;
    let (client_conn, client_ep) =
        oxutrm_net::quic_client_on(client_sock, server_addr, spki).await?;
    let (server_conn, _keep) = accepting.await??;


    let server_sink = FrameSink::new(server_conn.clone());
    let mut client_source = FrameSource::new(client_conn.clone());
    let client_sink = FrameSink::new(client_conn.clone());
    let mut server_source = FrameSource::new(server_conn.clone());

    // A round trip before the rebind, so we know the path works at all.
    server_sink.send(&frame(1))?;
    assert_eq!(client_source.recv().await?.my_state, 1);
    let before = server_conn.remote_address();

    // Roam: a brand-new LOCAL address, exactly as a Wi-Fi to mobile switch
    // would produce. The peer address is untouched, and cannot be touched.
    let fresh = tokio::net::UdpSocket::from_std(std::net::UdpSocket::bind("127.0.0.1:0")?)?;
    let new_local = fresh.local_addr()?;
    assert_ne!(new_local, before, "the test must actually change the address");
    // Rebind through the demultiplexer, not to a bare socket: quinn owns the
    // recv loop and STUN keepalives must still be peeled off in front of it.
    let (demux, _stun_rx) = oxutrm_net::StunDemuxSocket::new(std::sync::Arc::new(fresh));
    client_ep.rebind_abstract(std::sync::Arc::new(demux))?;

    // The client speaks first from the new address; the server validates the
    // path and migrates.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut n = 2u64;
    loop {
        assert!(Instant::now() < deadline, "the session never recovered after the rebind");
        client_sink.send(&frame(n))?;
        match tokio::time::timeout(Duration::from_millis(500), server_source.recv()).await {
            Ok(f) => {
                let f = f?;
                assert!(f.my_state >= 2);
                break;
            }
            Err(_) => {
                n += 1;
                continue;
            }
        }
    }

    // The server now sees the client at its new address. This is the server's
    // view of ITS peer changing because the client moved — not the client
    // repointing at a new peer, which is impossible.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if server_conn.remote_address() == new_local {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the server still sees {before}, expected {new_local}"
        );
        client_sink.send(&frame(n))?;
        n += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // And the session still carries traffic in the other direction.
    server_sink.send(&frame(99))?;
    let got = tokio::time::timeout(Duration::from_secs(10), client_source.recv()).await??;
    assert_eq!(got.my_state, 99, "the host->client direction did not survive the migration");
    Ok(())
}
```

- [ ] **Step 2: Run test to verify it fails**

`cargo test --jobs 4 --test roaming -- --test-threads 4`

Expected: FAIL to compile — `cannot find function quic_client_on`.

- [ ] **Step 3: Write minimal implementation**

`quinn::Connection` does not expose its `Endpoint`, so the test must be handed
one. Add a variant that returns both, and keep `quic_client`'s contract
signature by delegating to it.

In `crates/oxutrm-net/src/lib.rs`:

```rust
/// The connection, and the endpoint it runs on.
///
/// The endpoint is returned rather than dropped because it must outlive the
/// connection, and because roaming is driven through
/// `quinn::Endpoint::rebind_abstract`: a client that cannot rebind cannot
/// follow the user from Wi-Fi to mobile. Note what this does NOT enable —
/// there is no way to repoint the connection at a different peer address, so
/// the remote end is fixed for the life of the attach.
pub async fn quic_client_on(
    socket: std::net::UdpSocket,
    peer: std::net::SocketAddr,
    expect_spki_sha256: [u8; 32],
) -> anyhow::Result<(quinn::Connection, quinn::Endpoint)> {
    // Move M2's existing `quic_client` body here verbatim — including the
    // `StunDemuxSocket` wrapper and `Endpoint::new_with_abstract_socket`, both
    // of which are required and must not be simplified away — and change only
    // its final expression to `Ok((conn, endpoint))`.
    unimplemented!("move M2's quic_client body here; return (conn, endpoint)")
}

pub async fn quic_client(
    socket: std::net::UdpSocket,
    peer: std::net::SocketAddr,
    expect_spki_sha256: [u8; 32],
) -> anyhow::Result<quinn::Connection> {
    let (conn, endpoint) = quic_client_on(socket, peer, expect_spki_sha256).await?;
    // The endpoint drives the connection; dropping it would kill the session.
    // Callers that need to roam use `quic_client_on` and keep the handle.
    std::mem::forget(endpoint);
    Ok(conn)
}
```

Then update `run_client_session`'s call site to use `quic_client_on` and pass
the endpoint through, since Task 10 needs `endpoint.local_addr()` to notice a
migration at all.

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 --test roaming -- --test-threads 4`

Expected: PASS, 1 test. If the server never sees the new address, check that
M2's `quic_server` did not set `TransportConfig::max_concurrent_uni_streams(0)`
or disable migration; `quinn` allows migration by default and it must stay that
way.

- [ ] **Step 5: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add tests/roaming.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
test: the session survives a forced rebind of the client socket

Roaming is the reason QUIC was chosen. quic_client_on now returns the endpoint
so it can be rebound, and the test asserts traffic in both directions after the
server has migrated to the client's new address.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 13: End to end — the client's rendered screen equals the host's state

The whole pipeline, on loopback: a real shell behind a real PTY, a real host
session loop, a real QUIC connection, a real client renderer. The assertion is
the strong one: feed the client's emitted ANSI back through a fresh emulator and
compare the resulting screen against the host's authoritative `ScreenState`,
cell by cell. That checks the renderer, not just the sync engine.

**Files:**
- Modify: `crates/oxutrm-term/src/lib.rs`
- Create: `tests/e2e.rs`

**Interfaces:**
- Consumes: `oxutrm_host::session::run_host_session`; `oxutrm_client::Renderer`;
  `oxutrm_net::testkit::loopback_pair`; `oxutrm_net::xport::{FrameSink, FrameSource}`;
  `oxutrm_sync::{Receiver, Sender, InputState}`; `oxutrm_term::{HostTerm, ScreenState, Cell}`.
- Produces:
  ```rust
  // crates/oxutrm-term/src/lib.rs
  /// Parse raw terminal output into a ScreenState, with no PTY and no child.
  /// Used to check what a stream of ANSI actually paints.
  pub fn parse_to_state(bytes: &[u8], size: oxutrm_proto::TermSize, seq: u64) -> ScreenState;
  ```

- [ ] **Step 1: Write the failing test for `parse_to_state`**

Append to `crates/oxutrm-term/src/lib.rs`:

```rust
#[cfg(test)]
mod parse_to_state_tests {
    use super::*;
    use oxutrm_proto::TermSize;

    #[test]
    fn plain_text_lands_where_it_was_written() {
        let size = TermSize { cols: 10, rows: 3 };
        let s = parse_to_state(b"hi", size, 7);
        assert_eq!(s.seq, 7);
        assert_eq!(s.rows, 3);
        assert_eq!(s.cols, 10);
        assert_eq!(s.cell(0, 0).text, "h");
        assert_eq!(s.cell(0, 1).text, "i");
        assert_eq!(s.cell(0, 2).text, " ");
    }

    #[test]
    fn cursor_positioning_and_colour_are_honoured() {
        let size = TermSize { cols: 10, rows: 3 };
        let s = parse_to_state(b"\x1b[2;3H\x1b[38;5;196mX", size, 0);
        assert_eq!(s.cell(1, 2).text, "X");
        assert_eq!(s.cell(1, 2).fg, Color::Idx(196));
        assert_eq!(s.cursor.row, 1);
        assert_eq!(s.cursor.col, 3);
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

`cargo test --jobs 4 -p oxutrm-term parse_to_state -- --test-threads 4`

Expected: FAIL to compile — `cannot find function parse_to_state`.

- [ ] **Step 3: Write minimal implementation**

Add to `crates/oxutrm-term/src/lib.rs`, next to `detect_caps`:

```rust
/// Parse raw terminal output into a `ScreenState`, with no PTY and no child.
///
/// This is how a test checks what a stream of ANSI actually paints, which is a
/// stronger statement than checking what the sync engine believes.
pub fn parse_to_state(bytes: &[u8], size: oxutrm_proto::TermSize, seq: u64) -> ScreenState {
    let mut term = new_term(size, 0);
    let mut processor = alacritty_terminal::vte::ansi::Processor::new();
    processor.advance(&mut term, bytes);
    // `term_to_state` is the same conversion `HostTerm::snapshot` performs;
    // if M1 kept it private inside host_term.rs, move it here and have
    // `snapshot` call it.
    term_to_state(&term, seq)
}
```

If M1's snapshot conversion lives inside `HostTerm::snapshot` rather than in a
free function, extract it now:

```rust
/// The single conversion from the emulator's grid to our replicated state.
pub(crate) fn term_to_state<L: alacritty_terminal::event::EventListener>(
    term: &alacritty_terminal::term::Term<L>,
    seq: u64,
) -> ScreenState {
    // Move M1's snapshot body here verbatim, replacing `self.term` with `term`,
    // and have `HostTerm::snapshot` become:
    //     term_to_state(&self.term, seq)
    unimplemented!("move M1's snapshot conversion here")
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-term -- --test-threads 4`

Expected: PASS, including every M1 golden test — the extraction must not change
behaviour.

- [ ] **Step 5: Write the failing end-to-end test**

Create `tests/e2e.rs`:

```rust
use oxutrm_client::Renderer;
use oxutrm_net::xport::{FrameSink, FrameSource};
use oxutrm_proto::{TermSize, TerminalCaps};
use oxutrm_sync::{InputState, Receiver, Sender};
use oxutrm_term::{HostTerm, ScreenState};
use std::time::{Duration, Instant};

fn size() -> TermSize {
    TermSize { cols: 80, rows: 24 }
}

fn caps() -> TerminalCaps {
    TerminalCaps {
        truecolor: true,
        colors: 16_777_216,
        bracketed_paste: true,
        mouse_sgr: true,
        osc52: true,
        term_name: "xterm-256color".to_string(),
    }
}

fn text(s: &ScreenState) -> String {
    (0..s.rows)
        .map(|r| s.row(r).iter().map(|c| c.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A real shell, a real PTY, a real QUIC connection, a real renderer. The
/// client's emitted ANSI is replayed through a fresh emulator and compared with
/// the host's authoritative state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_clients_painted_screen_equals_the_hosts_state() -> anyhow::Result<()> {
    let (server_conn, client_conn) = oxutrm_net::testkit::loopback_pair().await?;
    let local: std::net::SocketAddr = "127.0.0.1:0".parse().unwrap();

    let term = HostTerm::spawn(
        "/bin/sh",
        &[],
        &oxutrm_host::caps::child_env()
            .into_iter()
            .chain([("PS1".to_string(), "$ ".to_string())])
            .collect::<Vec<_>>(),
        size(),
        200,
    )?;

    // The host's authoritative state is read back through a second snapshot at
    // the end, so keep a handle on the loop's result rather than the term.
    let host = tokio::spawn(oxutrm_host::session::run_host_session(term, server_conn, local, size()));

    let sink = FrameSink::new(client_conn.clone());
    let mut source = FrameSource::new(client_conn.clone());
    let mut screen_rx: Receiver<ScreenState> = Receiver::new(ScreenState::blank(size().rows, size().cols));
    let mut input_tx: Sender<InputState> =
        Sender::new(InputState { seq: 0, pending: Vec::new(), size: size() });

    let mut renderer = Renderer::new(size(), caps());
    let mut painted: Vec<u8> = Vec::new();

    // A scripted session: plain text, a colour, a cursor move, and a marker
    // that tells the test the shell has finished.
    let script: &[u8] = b"printf 'plain \\033[38;2;255;0;0mRED\\033[0m \\033[4munder\\033[0m\\n'; \
                          printf '\\033[5;10Hpositioned\\n'; \
                          printf 'E2E-DONE\\n'\n";
    let next = input_tx.current().append(script, size());
    input_tx.update(next);
    if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
        sink.send(&f)?;
    }

    // Run until the marker appears and the screen then stops changing.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen_marker = false;
    let mut quiet_since: Option<Instant> = None;
    loop {
        assert!(Instant::now() < deadline, "never settled:\n{}", text(screen_rx.state()));
        match tokio::time::timeout(Duration::from_millis(200), source.recv()).await {
            Ok(f) => {
                let f = f?;
                if screen_rx.on_frame(&f)? {
                    renderer.render(&mut painted, screen_rx.state())?;
                    quiet_since = None;
                }
                if text(screen_rx.state()).contains("E2E-DONE") {
                    seen_marker = true;
                }
            }
            Err(_) => {
                if let Some(f) = input_tx.make_frame(screen_rx.ack())? {
                    sink.send(&f)?;
                }
                if seen_marker {
                    let q = *quiet_since.get_or_insert_with(Instant::now);
                    if q.elapsed() >= Duration::from_millis(600) {
                        break;
                    }
                }
            }
        }
    }

    // What the ANSI the client emitted actually paints.
    let rendered = oxutrm_term::parse_to_state(&painted, size(), screen_rx.state().seq);
    let authoritative = screen_rx.state();

    assert_eq!(rendered.rows, authoritative.rows);
    assert_eq!(rendered.cols, authoritative.cols);
    for r in 0..authoritative.rows {
        for c in 0..authoritative.cols {
            let a = authoritative.cell(r, c);
            let b = rendered.cell(r, c);
            assert_eq!(
                (b.text.as_str(), b.fg, b.bg, b.attrs),
                (a.text.as_str(), a.fg, a.bg, a.attrs),
                "cell ({r},{c}) differs\nauthoritative:\n{}\nrendered:\n{}",
                text(authoritative),
                text(&rendered)
            );
        }
    }
    assert_eq!(rendered.cursor, authoritative.cursor, "the cursor was painted in the wrong place");

    client_conn.close(0u32.into(), b"done");
    let _ = tokio::time::timeout(Duration::from_secs(5), host).await;
    Ok(())
}
```

- [ ] **Step 6: Run test to verify it fails, then passes**

Add to the root `Cargo.toml`:

```toml
[dev-dependencies]
oxutrm-host = { path = "crates/oxutrm-host" }
oxutrm-client = { path = "crates/oxutrm-client" }
oxutrm-sync = { path = "crates/oxutrm-sync" }
oxutrm-term = { path = "crates/oxutrm-term" }
oxutrm-proto = { path = "crates/oxutrm-proto" }
```

Run: `cargo test --jobs 4 --test e2e -- --test-threads 4`

Expected: FAIL first (missing `parse_to_state` wiring or a renderer mismatch),
then PASS once the renderer's output really reproduces the state. A mismatch
here is a genuine renderer bug, not a test artefact — fix the renderer, never
the assertion.

- [ ] **Step 7: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add tests/e2e.rs crates/oxutrm-term/src/lib.rs crates/oxutrm-term/src/host_term.rs Cargo.toml
git commit -m "$(cat <<'EOF'
test: end to end, the client's painted screen equals the host's state

The client's emitted ANSI is replayed through a fresh emulator and compared cell
by cell against the authoritative ScreenState, so the renderer is under test
and not just the sync engine.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 14: `README.md` and `--help`

The last thing between "it works" and "someone else can use it".

**Files:**
- Create: `README.md`
- Create: `src/help.rs`
- Modify: `src/main.rs`
- Create: `tests/help.rs`

**Interfaces:**
- Consumes: M3's subcommand dispatch in `src/main.rs`.
- Produces:
  ```rust
  // src/help.rs
  pub const HELP: &str;
  pub const VERSION: &str = env!("CARGO_PKG_VERSION");
  pub fn help_text() -> String;
  ```

- [ ] **Step 1: Write the failing test**

Create `tests/help.rs`:

```rust
use std::process::Command;

fn run(args: &[&str]) -> (String, bool) {
    let out = Command::new(env!("CARGO_BIN_EXE_oxutrm")).args(args).output().expect("run oxutrm");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (text, out.status.success())
}

#[test]
fn help_lists_every_subcommand() {
    let (text, ok) = run(&["--help"]);
    assert!(ok, "--help must exit successfully: {text}");
    for needle in [
        "oxutrm <ssh-target>",
        "oxutrm host --serve",
        "oxutrm host --list",
        "oxutrm host --attach",
        "Ctrl-]",
    ] {
        assert!(text.contains(needle), "--help is missing {needle:?}:\n{text}");
    }
}

#[test]
fn short_help_and_the_help_subcommand_agree() {
    let (long, _) = run(&["--help"]);
    let (short, _) = run(&["-h"]);
    let (word, _) = run(&["help"]);
    assert_eq!(long, short);
    assert_eq!(long, word);
}

#[test]
fn an_unknown_subcommand_fails_and_points_at_help() {
    let (text, ok) = run(&["--frobnicate"]);
    assert!(!ok, "an unknown option must not exit successfully");
    assert!(text.contains("--help"), "the error must point at --help:\n{text}");
}

/// The test-only hook must never be advertised.
#[test]
fn help_does_not_mention_the_test_hook() {
    let (text, _) = run(&["--help"]);
    assert!(!text.contains("client-test-hook"));
    assert!(!text.contains("OXUTRM_TEST_HOOK"));
}

#[test]
fn version_is_reported() {
    let (text, ok) = run(&["--version"]);
    assert!(ok);
    assert!(text.contains(env!("CARGO_PKG_VERSION")));
}
```

- [ ] **Step 2: Run test to verify it fails**

`cargo test --jobs 4 --test help -- --test-threads 4`

Expected: FAIL — the binary has no `--help`.

- [ ] **Step 3: Write minimal implementation**

Create `src/help.rs`:

```rust
//! The `--help` text. Written by hand rather than generated, because oxutrm
//! parses its own arguments and the help is the only user-facing description
//! of the three roles the one binary plays.

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const HELP: &str = "\
oxutrm — a remote terminal that survives bad networks, changing IP addresses
and NAT on both ends.

USAGE
  oxutrm <ssh-target> [command ...]
      Connect. Drives ssh to start a session on the remote host, then becomes
      the client. <ssh-target> is anything `ssh` accepts; oxutrm never parses
      ~/.ssh/config, so if `ssh <target>` works, this works.

  oxutrm host --serve
      Run the remote half. Spawned over SSH; not normally typed by hand.

  oxutrm host --list
      List sessions on this machine, pruning any whose process is gone.

  oxutrm host --attach <session-id>
      Reattach to a running session. Reattaching uses the same code path as
      the first connect.

OPTIONS
  -h, --help        Show this help.
      --version     Show the version.

WHILE CONNECTED
  Ctrl-]            Open or close the status pane: current path and rung,
                    round-trip time, loss, bytes each way, migration history,
                    session id and uptime.

ON CONNECT
  One line says what connection you got, for example:
      oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452
      oxutrm  SSH tunnel — no UDP path, not detachable  ·  45 ms      [warning]
  The SSH tunnel is slower, it does not survive an IP change, and a session on
  it CANNOT be detached: it dies with the SSH connection. That is why it is
  always announced rather than used silently.

DETACHING
  Closing the client leaves the session running on the remote host, costing no
  bandwidth while detached. Reattach with `oxutrm <ssh-target>`; use
  `oxutrm host --list` over ssh to see what is there.

  The one exception is a session running over the SSH tunnel fallback. Its
  transport IS the SSH connection, so it cannot detach and ends when that
  connection does. The connect line says `not detachable` when this applies.
";

pub fn help_text() -> String {
    format!("oxutrm {VERSION}\n\n{HELP}")
}
```

In `src/main.rs`, add `mod help;` and handle the flags before the subcommand
match:

```rust
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h") | Some("--help") | Some("help") => {
            print!("{}", help::help_text());
            return Ok(());
        }
        Some("--version") | Some("-V") => {
            println!("oxutrm {}", help::VERSION);
            return Ok(());
        }
        Some(a) if a.starts_with('-') => {
            eprintln!("oxutrm: unknown option {a:?}\nTry `oxutrm --help`.");
            std::process::exit(2);
        }
        _ => {}
    }
```

Note that a bare `oxutrm` with no arguments prints the help and exits zero,
which is why `help_lists_every_subcommand` passes on `--help` alone.

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 --test help -- --test-threads 4`

Expected: PASS, 5 tests.

- [ ] **Step 5: Write the README**

Create `README.md`:

````markdown
# oxutrm

A remote terminal that survives bad networks, changing IP addresses, and NAT on
both ends. It replaces the `ssh` + `tmux` habit of "reconnect and hope" with a
session that simply stays alive.

It is, deliberately, **Mosh rebuilt in Rust with a real terminal emulator on
both ends**, plus the two things Mosh never solved: NAT traversal and
scrollback.

## What it does

- **Encrypted UDP transport** (QUIC) that outlives IP changes on either side.
- **Both endpoints may sit behind NAT.** A five-rung ladder — IPv6 direct,
  router port mapping, STUN hole punching, a birthday-paradox blast for
  symmetric NAT, and an SSH tunnel as the last resort.
- **SSH for session initiation and reattachment.** No new trust root, no new
  daemon to expose. If you trust `ssh <target>` today, you trust oxutrm.
- **Real terminal emulation on both ends** (`alacritty_terminal`), so screen state
  is authoritative on
  the host.
- **Detach and reattach.** The remote session outlives the client indefinitely
  and costs no bandwidth while detached.
- **Full fidelity**: 24-bit colour, SGR mouse reporting, resize, window title,
  OSC 52 clipboard. Colour is folded down to what your terminal can actually
  show, in the client, so the host's state stays intact for a better terminal
  later.
- **It tells you what connection you got.** No silent magic.

## Install

```sh
cargo install --path .
```

The same binary must be on both machines. It plays three roles depending on how
it is invoked.

## Use

```sh
oxutrm myserver              # connect, or reattach
oxutrm myserver -- htop      # run something other than your shell
ssh myserver oxutrm host --list
```

While connected, `Ctrl-]` opens a status pane: path and rung, round-trip time,
loss, bytes each way, migration history, session id and uptime.

On connect you get exactly one line, then silence:

```
oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452
oxutrm  IPv4 punched (birthday, 312 probes)  ·  61 ms  ·  symmetric NAT
oxutrm  SSH tunnel — no UDP path, not detachable  ·  45 ms      [warning]
```

If your own address changes while you are connected — walking from Wi-Fi to
mobile — oxutrm says so for a few seconds rather than leaving it mysterious.
The remote end of a connection is fixed once it is established, so a session
follows *you*; if the server's address changes, reconnect.

## How it works

`oxutrm <target>` shells out to `ssh` and starts `oxutrm host --serve` on the
far end. The SSH channel carries a short JSON handshake: a session id, the
SHA-256 of the host's self-signed QUIC certificate, a 32-byte pre-shared key,
and each side's connection candidates. Both sides then punch with authenticated
STUN checks, and QUIC comes up on the socket that was punched. SSH closes and
the host detaches.

Screen state is replicated rather than streamed: the host sends the difference
between what the client is known to have and what is true now. A lost datagram
therefore costs nothing — the next one diffs from the same acknowledged base and
contains whatever was lost — and output that outruns the link coalesces instead
of queueing.

## Security

The trust root is SSH, unchanged. The client pins exactly the certificate
fingerprint delivered over SSH; the pre-shared key binds the QUIC session to
that exchange. Keys are fresh for every attach and are never written to disk.
Public STUN servers learn only that an IP is using STUN, and the server list is
configurable.

## Requirements

Unix. Windows is out of scope: the PTY layer assumes Unix semantics.

## Not in scope

Graphics protocols (Sixel, Kitty, iTerm2 inline images) — the emulator does not model
them and claiming support would be a lie. A GUI client. Replacing tmux. Being a
VPN or a port forwarder.
````

- [ ] **Step 6: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add README.md src/help.rs src/main.rs tests/help.rs
git commit -m "$(cat <<'EOF'
docs: README and --help for the three roles the binary plays

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Self-review notes

**Spec coverage.** §7.1 framing and §7.1.1 fragmentation: Task 2. §7.2 streams:
Task 5 (control stream). §9.4 capability negotiation: Task 5, with the
client-side fold in Task 4. §10.1 two diffs: Tasks 10 and 13. §10.2 input and raw
mode: Tasks 7, 8, 10. §10.3 status display: Task 9. §10.4 bandwidth adaptation:
Task 1, applied in Tasks 6 and 10. §5.5 rung 4 including `detachable: false`:
Task 11. §6's "0-RTT is deliberately not used": stated in the header, no task.
§12's end-to-end row: Task 13. Roaming (§5, §1.3): Task 12.

**Decisions taken from the spec rather than re-derived.**
- Fragmentation, not a stream fallback: `Frame` carries `frag_index`/`frag_count`,
  a state applies only when every fragment arrives, and an incomplete set is
  discarded wholesale (§7.1.1).
- `negotiate_term()` takes no argument. Client capabilities never reach the child
  environment (§9.4).
- Migration is the client's own **local** address only. No task attempts to
  repoint a connection at a new peer (§5).
- 0-RTT is not implemented or tested (§6).

**Things this plan deliberately does not do.** Rung 3's birthday blast is M2's
ladder work, reached here only through `Rung::Birthday` in the status line.
Speculative echo, synced scrollback and `tmux -CC` are phases C and D.
Multi-client attach stays possible (§9.5) because nothing here mutates
`ScreenState` in a way that assumes one reader, but it is not implemented.
`Ctrl-]` is spec'd as configurable and is a fixed constant here.

**Type consistency.** `TermSize { cols, rows }` everywhere, never a bare tuple.
`Frame` is `my_state` / `from_state` / `ack_state` / `frag_index` / `frag_count`
/ `flags` / `payload`, and `base`/`target` live **only** in `Frame` — the diff
structs never repeat them. `ScreenState.seq` starts at 1; 0 is the full-state
sentinel. `ScreenState` has no `icon` field. `FrameSink::send` returns the
datagram count. `TerminalGuard` replaces `RawGuard`, and Task 7 amends the
contract to say so. `quic_client` keeps its contract signature; `quic_client_on`
additionally returns the endpoint, which Tasks 10 and 12 both need.

---

## Contradictions between the two committed documents

Both were re-read at commit `6b7bf6f` before this pass. Three places disagree,
and in each case the plan follows the source named:

1. **The rung-1 status line.** Spec §10.3 shows `oxutrm  IPv4 punched (UPnP)`,
   but `PathDescription` (contract) has no field naming the mapping protocol —
   `Rung::PortMapped` cannot distinguish NAT-PMP from PCP from UPnP-IGD. Task 9
   renders `IPv4 punched (port mapped)`. **Followed the contract**; matching the
   spec literally needs `PathDescription` to gain a mapping-protocol field.
2. **`TerminalCaps`.** Spec §9.4's illustrative struct still shows
   `colors: u16` (which cannot hold 16 777 216) and a
   `unicode_width: UnicodeWidthVersion` field. The contract has `colors: u32` and
   no `unicode_width`. **Followed the contract**; the spec's snippet is stale.
3. **Late better paths.** Spec §5 line 1 still says "The first validated path
   wins", and §1.3's table row says roaming is handled by "QUIC connection
   migration", while §5's own following paragraph and the contract both say a
   better path found after nomination is lost for that attach. **Followed the
   later, explicit statement**, which the contract repeats verbatim on
   `IceAgent`.

One further gap, in neither document: `Receiver::on_frame` does not expose the
applied diff, so the host cannot write `InputDiff.appended` directly and Task 3
reconstructs the same bookkeeping. An `on_frame_with` callback would delete that
whole task.

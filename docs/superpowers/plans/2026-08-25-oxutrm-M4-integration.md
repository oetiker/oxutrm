# oxutrm M4 — Integration Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire M1's terminal core and sync engine, M2's QUIC transport and NAT ladder, and M3's SSH bootstrap and session registry into a remote terminal that is usable daily.

**Architecture:** Two symmetric session loops joined by QUIC. The host drains its PTY into `vt100`, feeds a `Sender<ScreenState>`, and paces `Frame`s onto QUIC datagrams; oversized frames go on a fresh unidirectional stream instead. The client feeds local input into a `Sender<InputState>`, applies incoming `ScreenDiff`s, and re-renders through a `Renderer` that down-converts colour to whatever the user's real terminal can show, so the host's state stays full fidelity.

**Tech Stack:** Rust 2021, `quinn` 0.11 (datagrams, uni streams, connection migration, `Endpoint::rebind`), `tokio` 1 (`select!`, `AsyncFd`, unix signals), `vt100` (fork `Junyi-99/vt100-rust` branch `deck`), `rustix` 1 termios, `postcard` 1.

**Spec:** `docs/superpowers/specs/2026-08-25-oxutrm-design.md`
**Contract:** `docs/superpowers/plans/2026-08-25-oxutrm-contract.md` — **normative, read it first.**

**Depends on:** M1 (`2026-08-25-oxutrm-M1-terminal-core.md`), M2 (`2026-08-25-oxutrm-M2-transport.md`), M3 (`2026-08-25-oxutrm-M3-sessions.md`). Assume all three are merged and green.

---

## Global Constraints

Every task's requirements implicitly include the whole contract file. The load-bearing ones, repeated verbatim:

- **Binary and product name is `oxutrm`.** Not `oxuterm`. The checkout directory is `oxuterm` for historical reasons; nothing inside it uses that spelling.
- **Rust edition 2021**, workspace at the repo root, one binary `src/main.rs`.
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

Add it to `crates/oxutrm-net/Cargo.toml` in Task 2. Nothing else new.

---

## File Structure

**New files:**

| File | Responsibility |
|---|---|
| `crates/oxutrm-net/src/pace.rs` | `Pacer` — the §10.4 send-interval policy, pure, clock injected |
| `crates/oxutrm-net/src/link.rs` | `LinkStats` + `link_stats()` — the *only* place that touches `quinn::ConnectionStats` |
| `crates/oxutrm-net/src/xport.rs` | `FrameSink` / `FrameSource` — datagram vs. stream selection, the MTU answer |
| `crates/oxutrm-net/src/tunnel.rs` | Rung 4: a loopback UDP relay over the SSH channel |
| `crates/oxutrm-net/src/testkit.rs` | `loopback_pair()` — two connected `quinn::Connection`s, behind feature `testkit` |
| `crates/oxutrm-host/src/input_cursor.rs` | `InputCursor` — which client input bytes have reached the PTY, pure |
| `crates/oxutrm-host/src/session.rs` | `run_host_session` — PTY ↔ `vt100` ↔ QUIC |
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

## The MTU decision, stated once

`quinn::Connection::max_datagram_size()` reports roughly 1400 bytes on a normal path. A full 200×60 truecolor repaint is 12 000 `Cell`s and does not fit, even zstd-compressed.

**Decision: oversized frames go on a fresh unidirectional QUIC stream, one at a time; datagrams are never fragmented.** Fragmenting an unreliable datagram would mean that losing any one fragment loses the entire state, so 1 % packet loss becomes roughly 25 % state loss for a 30-fragment repaint, destroying the §8.1 property that a lost datagram costs nothing. QUIC already provides an ordered, reliable, congestion-controlled stream, and the oversized case is by construction a rare full repaint whose required semantics are exactly "arrive completely, eventually".

Two consequences the implementation must honour:

1. **Uni streams are reserved for oversized state frames.** Control, scrollback and clipboard are bidirectional (spec §7.2), so stream direction is the discriminator. No extra header is needed: a uni stream carries exactly one postcard-encoded `Frame` and then FIN.
2. **While an oversized stream is in flight the sink sends nothing else.** States coalesce in the `Sender` instead of queueing, which is the same §8.1 property, and it removes any ordering hazard between the datagram path and the stream path.

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

### Task 2: Frame transport — datagram below the MTU, unidirectional stream above it

This is the highest-risk task in M4. Read "The MTU decision, stated once" above before starting.

**Files:**
- Create: `crates/oxutrm-net/src/xport.rs`
- Create: `crates/oxutrm-net/src/testkit.rs`
- Modify: `crates/oxutrm-net/src/lib.rs`
- Modify: `crates/oxutrm-net/Cargo.toml`

**Interfaces:**
- Consumes: `oxutrm_proto::Frame` with `Frame::encode() -> Result<Vec<u8>, ProtoError>` and `Frame::decode(&[u8]) -> Result<Frame, ProtoError>`; `oxutrm_net::{generate_cert, quic_server, quic_client}` from M2. M2's `quic_server` and `quic_client` **must** have set `TransportConfig::datagram_receive_buffer_size(Some(_))` and `datagram_send_buffer_size(Some(_))`, or datagrams are disabled — Step 3's test asserts this.
- Produces:
  ```rust
  // crates/oxutrm-net/src/xport.rs
  /// Headroom left below `max_datagram_size()` so a frame never sits exactly
  /// on the boundary that DPLPMTUD is still probing.
  pub const DATAGRAM_MARGIN: usize = 64;
  /// Refuse to buffer a stream frame larger than this.
  pub const MAX_STREAM_FRAME: usize = 8 * 1024 * 1024;

  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum Channel { Datagram, Stream }

  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum Sent { Datagram, Stream, Skipped }

  pub fn choose_channel(encoded_len: usize, max_datagram: Option<usize>) -> Channel;

  pub struct FrameSink { /* private */ }
  impl FrameSink {
      pub fn new(conn: quinn::Connection) -> FrameSink;
      pub fn is_busy(&self) -> bool;
      pub fn send(&self, f: &oxutrm_proto::Frame) -> anyhow::Result<Sent>;
  }

  pub struct FrameSource { /* private */ }
  impl FrameSource {
      pub fn new(conn: quinn::Connection) -> FrameSource;
      pub async fn recv(&mut self) -> anyhow::Result<oxutrm_proto::Frame>;
  }

  // crates/oxutrm-net/src/testkit.rs, behind feature "testkit"
  pub async fn loopback_pair() -> anyhow::Result<(quinn::Connection, quinn::Connection)>;
  ```

- [ ] **Step 1: Add the `bytes` dependency and the `testkit` feature**

In `crates/oxutrm-net/Cargo.toml`:

```toml
[dependencies]
bytes = "1"

[features]
testkit = []
```

- [ ] **Step 2: Write the loopback test harness**

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
        // Keep the endpoint alive for the life of the connection.
        anyhow::Ok((conn, endpoint))
    });

    let client = crate::quic_client(client_sock, server_addr, spki).await?;
    let (server, server_endpoint) = accepting.await??;
    // Leak the endpoint deliberately: a test pair lives for the test.
    std::mem::forget(server_endpoint);
    Ok((server, client))
}
```

- [ ] **Step 3: Write the failing tests**

Create `crates/oxutrm-net/src/xport.rs` containing only:

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

    #[test]
    fn small_frames_take_the_datagram_path() {
        assert_eq!(choose_channel(100, Some(1400)), Channel::Datagram);
        assert_eq!(choose_channel(1400 - DATAGRAM_MARGIN, Some(1400)), Channel::Datagram);
    }

    #[test]
    fn frames_within_the_margin_take_the_stream_path() {
        assert_eq!(choose_channel(1400 - DATAGRAM_MARGIN + 1, Some(1400)), Channel::Stream);
        assert_eq!(choose_channel(50_000, Some(1400)), Channel::Stream);
    }

    #[test]
    fn everything_takes_the_stream_path_when_datagrams_are_unavailable() {
        assert_eq!(choose_channel(1, None), Channel::Stream);
    }

    #[tokio::test]
    async fn a_small_frame_arrives_over_a_datagram() -> anyhow::Result<()> {
        let (server, client) = crate::testkit::loopback_pair().await?;
        assert!(
            server.max_datagram_size().is_some(),
            "M2's quic_server/quic_client must set datagram buffer sizes, \
             or QUIC datagrams are disabled entirely"
        );

        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);

        let f = Frame { my_state: 7, from_state: 6, ack_state: 3, flags: 0, payload: noise(200) };
        assert_eq!(sink.send(&f)?, Sent::Datagram);

        let got = source.recv().await?;
        assert_eq!(got.my_state, 7);
        assert_eq!(got.from_state, 6);
        assert_eq!(got.ack_state, 3);
        assert_eq!(got.payload, f.payload);
        Ok(())
    }

    /// The one that matters: a payload far larger than any datagram must
    /// arrive byte-for-byte intact.
    #[tokio::test]
    async fn an_oversized_frame_arrives_intact_over_a_stream() -> anyhow::Result<()> {
        let (server, client) = crate::testkit::loopback_pair().await?;
        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);

        // 400 KiB: comfortably larger than a 200x60 truecolor repaint, and
        // roughly 300 datagrams' worth.
        let payload = noise(400 * 1024);
        let f = Frame { my_state: 42, from_state: 0, ack_state: 0, flags: 0, payload: payload.clone() };
        assert_eq!(sink.send(&f)?, Sent::Stream);

        let got = source.recv().await?;
        assert_eq!(got.my_state, 42);
        assert_eq!(got.from_state, 0);
        assert_eq!(got.payload.len(), payload.len(), "truncated on the stream path");
        assert_eq!(got.payload, payload, "corrupted on the stream path");
        Ok(())
    }

    /// While a stream frame is in flight the sink stays busy, so states
    /// coalesce in the Sender instead of queueing on the wire.
    #[tokio::test]
    async fn the_sink_reports_busy_and_skips_while_a_stream_is_in_flight() -> anyhow::Result<()> {
        let (server, client) = crate::testkit::loopback_pair().await?;
        let sink = FrameSink::new(server);
        let mut source = FrameSource::new(client);

        let big = Frame { my_state: 1, from_state: 0, ack_state: 0, flags: 0, payload: noise(400 * 1024) };
        assert_eq!(sink.send(&big)?, Sent::Stream);
        assert!(sink.is_busy());

        // A small frame offered while the stream is in flight is dropped, not
        // sent: the newer state will be diffed afresh once the sink is free.
        let small = Frame { my_state: 2, from_state: 1, ack_state: 0, flags: 0, payload: noise(10) };
        assert_eq!(sink.send(&small)?, Sent::Skipped);

        let got = source.recv().await?;
        assert_eq!(got.my_state, 1);

        // Once drained, the sink frees up and accepts frames again.
        for _ in 0..200 {
            if !sink.is_busy() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(!sink.is_busy(), "sink never cleared after the stream drained");
        assert_eq!(sink.send(&small)?, Sent::Datagram);
        Ok(())
    }
}
```

- [ ] **Step 4: Run tests to verify they fail**

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
pub mod xport;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
```

Run: `cargo test --jobs 4 -p oxutrm-net xport:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function choose_channel`, `cannot find type FrameSink`.

- [ ] **Step 5: Write minimal implementation**

Put this above the test module in `crates/oxutrm-net/src/xport.rs`:

```rust
//! Getting a `Frame` from one peer to the other.
//!
//! A `Frame` that fits in a QUIC datagram goes in a datagram: unreliable,
//! unretransmitted, exactly what screen state wants (spec §8.1). A `Frame`
//! that does not fit — in practice a full repaint — goes on a fresh
//! unidirectional stream instead. Datagrams are never fragmented: losing one
//! fragment of a thirty-fragment state would lose the whole state, which is
//! precisely the property §8.1 exists to avoid.
//!
//! Unidirectional streams are reserved for this. Control, scrollback and
//! clipboard are bidirectional (spec §7.2), so no discriminating header is
//! needed: a uni stream carries one postcard `Frame` and then FIN.

use oxutrm_proto::Frame;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Headroom below `max_datagram_size()`, so a frame never lands exactly on a
/// boundary DPLPMTUD is still probing.
pub const DATAGRAM_MARGIN: usize = 64;

/// A stream frame larger than this is a bug, not a repaint.
pub const MAX_STREAM_FRAME: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Datagram,
    Stream,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sent {
    Datagram,
    Stream,
    /// An oversized transfer was already in flight; this state was dropped and
    /// will be superseded by a fresher diff.
    Skipped,
}

/// `max_datagram` is `quinn::Connection::max_datagram_size()`; `None` means the
/// peer disabled datagrams, in which case everything goes on a stream.
pub fn choose_channel(encoded_len: usize, max_datagram: Option<usize>) -> Channel {
    match max_datagram {
        Some(max) if encoded_len + DATAGRAM_MARGIN <= max => Channel::Datagram,
        _ => Channel::Stream,
    }
}

/// Sends `Frame`s. Cheap to clone-free share: `send` takes `&self`.
pub struct FrameSink {
    conn: quinn::Connection,
    stream_busy: Arc<AtomicBool>,
}

impl FrameSink {
    pub fn new(conn: quinn::Connection) -> FrameSink {
        FrameSink { conn, stream_busy: Arc::new(AtomicBool::new(false)) }
    }

    /// True while an oversized frame is still being delivered.
    pub fn is_busy(&self) -> bool {
        self.stream_busy.load(Ordering::SeqCst)
    }

    pub fn send(&self, f: &Frame) -> anyhow::Result<Sent> {
        // Nothing else goes out while a repaint is in flight: it would either
        // be superseded on arrival or arrive out of order against a base the
        // peer is about to leave.
        if self.is_busy() {
            return Ok(Sent::Skipped);
        }

        let bytes = f.encode()?;
        match choose_channel(bytes.len(), self.conn.max_datagram_size()) {
            Channel::Datagram => {
                self.conn.send_datagram(bytes::Bytes::from(bytes))?;
                Ok(Sent::Datagram)
            }
            Channel::Stream => {
                anyhow::ensure!(
                    bytes.len() <= MAX_STREAM_FRAME,
                    "frame of {} bytes exceeds MAX_STREAM_FRAME",
                    bytes.len()
                );
                self.stream_busy.store(true, Ordering::SeqCst);
                let conn = self.conn.clone();
                let busy = self.stream_busy.clone();
                tokio::spawn(async move {
                    let result = async {
                        let mut s = conn.open_uni().await?;
                        s.write_all(&bytes).await?;
                        s.finish()?;
                        // Resolves once the peer has taken the stream, so the
                        // sink stays busy for as long as the transfer really
                        // occupies the link.
                        let _ = s.stopped().await;
                        anyhow::Ok(())
                    }
                    .await;
                    if let Err(e) = result {
                        // A dead connection is the session loop's problem, not
                        // this task's; it will observe `conn.closed()`.
                        eprintln!("oxutrm: oversized frame not delivered: {e}");
                    }
                    busy.store(false, Ordering::SeqCst);
                });
                Ok(Sent::Stream)
            }
        }
    }
}

/// Receives `Frame`s from either channel.
pub struct FrameSource {
    conn: quinn::Connection,
}

impl FrameSource {
    pub fn new(conn: quinn::Connection) -> FrameSource {
        FrameSource { conn }
    }

    /// The next `Frame`, from whichever channel produces one first.
    pub async fn recv(&mut self) -> anyhow::Result<Frame> {
        loop {
            let bytes = tokio::select! {
                d = self.conn.read_datagram() => d?.to_vec(),
                s = self.conn.accept_uni() => {
                    let mut recv = s?;
                    recv.read_to_end(MAX_STREAM_FRAME).await?
                }
            };
            match Frame::decode(&bytes) {
                Ok(f) => return Ok(f),
                // A malformed frame is a lost frame: the next one diffs from
                // the same acknowledged base and contains whatever was in it.
                Err(e) => eprintln!("oxutrm: dropping undecodable frame: {e}"),
            }
        }
    }
}
```

- [ ] **Step 6: Run tests to verify they pass**

`cargo test --jobs 4 -p oxutrm-net xport:: -- --test-threads 4`

Expected: PASS, 6 tests, including `an_oversized_frame_arrives_intact_over_a_stream`.

- [ ] **Step 7: Run the gates**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
```

Expected: both clean.

- [ ] **Step 8: Commit**

```bash
git add crates/oxutrm-net/src/xport.rs crates/oxutrm-net/src/testkit.rs \
        crates/oxutrm-net/src/lib.rs crates/oxutrm-net/Cargo.toml
git commit -m "$(cat <<'EOF'
feat(net): frame transport, datagram below the MTU and uni stream above it

A full repaint does not fit in a QUIC datagram. Fragmenting an unreliable
datagram would make one lost fragment lose the whole state, destroying the
property in spec 8.1; a unidirectional stream gives exactly the semantics an
oversized repaint needs. One oversized transfer at a time, so states coalesce
rather than queue.

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
never learns about this; `ScreenState` always carries whatever `vt100` produced.

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
//! The host's `ScreenState` always carries full fidelity — whatever `vt100`
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
            text: "X".to_string(),
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

The client detects what its own terminal can do, sends it in `ClientHello`, and
the host turns it into the child's `TERM` and `COLORTERM`. When the user moves
to a different terminal on reattach, `ControlMsg::CapsUpdate` carries the new
capabilities and the host re-exports them for future children.

**Files:**
- Modify: `crates/oxutrm-term/src/host_term.rs`
- Create: `crates/oxutrm-host/src/caps.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`

**Interfaces:**
- Consumes: `oxutrm_term::detect_caps() -> TerminalCaps` and
  `oxutrm_term::negotiate_term(caps: &TerminalCaps) -> (String, Option<String>)`
  (both M1); `oxutrm_term::HostTerm::spawn(shell, args, env, size, scrollback)`;
  `oxutrm_proto::{TerminalCaps, ControlMsg}`.
- Produces:
  ```rust
  // crates/oxutrm-host/src/caps.rs
  /// The environment a session's child shell is started with, given the
  /// client's capabilities. Never contains key material.
  pub fn child_env(caps: &oxutrm_proto::TerminalCaps) -> Vec<(String, String)>;
  /// Serve one `ControlMsg` on the control stream.
  pub fn handle_control(
      msg: &oxutrm_proto::ControlMsg,
      caps: &mut oxutrm_proto::TerminalCaps,
      info: &oxutrm_proto::ControlMsg,
      path: &oxutrm_proto::PathDescription,
  ) -> Option<oxutrm_proto::ControlMsg>;
  ```

- [ ] **Step 1: Write the failing test**

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
    fn a_truecolor_client_gets_colorterm_in_the_child() {
        let env = child_env(&caps(16_777_216, true));
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "COLORTERM"), Some("truecolor"));
    }

    #[test]
    fn a_256_colour_client_gets_no_colorterm() {
        let env = child_env(&caps(256, false));
        assert_eq!(get(&env, "TERM"), Some("xterm-256color"));
        assert_eq!(get(&env, "COLORTERM"), None);
    }

    #[test]
    fn a_16_colour_client_gets_a_16_colour_term() {
        let env = child_env(&caps(16, false));
        assert_eq!(get(&env, "TERM"), Some("xterm"));
        assert_eq!(get(&env, "COLORTERM"), None);
    }

    /// The client's own $TERM is diagnostic only: it must never reach the
    /// child, because the child renders into vt100, not into the user's
    /// terminal.
    #[test]
    fn the_clients_own_term_name_never_reaches_the_child() {
        let env = child_env(&caps(256, false));
        assert!(
            env.iter().all(|(_, v)| v != "foot"),
            "the client's terminal name leaked into the child environment"
        );
    }

    #[test]
    fn no_environment_variable_carries_key_material() {
        let env = child_env(&caps(16_777_216, true));
        for (k, _) in &env {
            let k = k.to_ascii_uppercase();
            assert!(!k.contains("PSK") && !k.contains("KEY") && !k.contains("SECRET"));
        }
    }

    #[test]
    fn a_caps_update_replaces_the_stored_capabilities() {
        let mut current = caps(16, false);
        let info = ControlMsg::SessionInfo {
            session_id: "0".repeat(32),
            shell: "/bin/sh".to_string(),
            created_unix: 0,
        };
        let path = PathDescription {
            rung: Rung::Ipv6Direct,
            local: "[::1]:443".parse().unwrap(),
            remote: "[::1]:1".parse().unwrap(),
            probes_sent: 0,
            nat_type: NatType::None,
            rtt_ms: 11,
            mtu: 1452,
        };
        let reply = handle_control(&ControlMsg::CapsUpdate(caps(16_777_216, true)), &mut current, &info, &path);
        assert!(reply.is_none());
        assert_eq!(current.colors, 16_777_216);
        assert!(current.truecolor);
    }

    #[test]
    fn a_status_request_is_answered_with_the_current_path() {
        let mut current = caps(256, false);
        let info = ControlMsg::SessionInfo {
            session_id: "a".repeat(32),
            shell: "/bin/sh".to_string(),
            created_unix: 7,
        };
        let path = PathDescription {
            rung: Rung::StunPunch,
            local: "127.0.0.1:443".parse().unwrap(),
            remote: "127.0.0.1:9".parse().unwrap(),
            probes_sent: 4,
            nat_type: NatType::EndpointIndependent,
            rtt_ms: 38,
            mtu: 1392,
        };
        match handle_control(&ControlMsg::StatusRequest, &mut current, &info, &path) {
            Some(ControlMsg::StatusReply(p)) => {
                assert_eq!(p.rtt_ms, 38);
                assert_eq!(p.rung, Rung::StunPunch);
            }
            other => panic!("expected a StatusReply, got {other:?}"),
        }
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Add `pub mod caps;` to `crates/oxutrm-host/src/lib.rs`, then run:

`cargo test --jobs 4 -p oxutrm-host caps:: -- --test-threads 4`

Expected: FAIL to compile — `cannot find function child_env`.

- [ ] **Step 3: Write minimal implementation**

Put this above the test module in `crates/oxutrm-host/src/caps.rs`:

```rust
//! Turning the client's declared capabilities into the child shell's
//! environment (spec §9.4).
//!
//! Mosh hardcodes `xterm-256color` and hopes. We can do better because the
//! client re-renders into the user's real terminal and therefore knows what it
//! can display. `TERM` describes what `vt100` emulates, narrowed by what the
//! client can show; the client's own `$TERM` is diagnostic only and never
//! reaches the child.

use oxutrm_proto::{ControlMsg, PathDescription, TerminalCaps};

/// The environment for a session's child shell.
pub fn child_env(caps: &TerminalCaps) -> Vec<(String, String)> {
    let (term, colorterm) = oxutrm_term::negotiate_term(caps);
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
        ControlMsg::CapsUpdate(new_caps) => {
            // The running child keeps the TERM it was started with — changing
            // it under a live process would be a lie. New children get this.
            *caps = new_caps.clone();
            None
        }
        ControlMsg::StatusRequest => Some(ControlMsg::StatusReply(path.clone())),
        ControlMsg::SessionInfo { .. } => Some(info.clone()),
        ControlMsg::StatusReply(_) => None,
    }
}
```

M1's `negotiate_term` must satisfy the table the tests assert. If it does not,
fix it in `crates/oxutrm-term/src/lib.rs` to exactly this:

```rust
/// The honest intersection of what vt100 emulates and what the client can show.
pub fn negotiate_term(caps: &TerminalCaps) -> (String, Option<String>) {
    let term = if caps.colors >= 256 { "xterm-256color" } else { "xterm" };
    let colorterm = if caps.truecolor && caps.colors >= 16_777_216 {
        Some("truecolor".to_string())
    } else {
        None
    };
    (term.to_string(), colorterm)
}
```

- [ ] **Step 4: Run test to verify it passes**

`cargo test --jobs 4 -p oxutrm-host caps:: -- --test-threads 4`

Expected: PASS, 7 tests.

- [ ] **Step 5: Write the failing test that the child really sees the environment**

Create `crates/oxutrm-host/tests/child_env.rs`:

```rust
use oxutrm_proto::{TermSize, TerminalCaps};
use oxutrm_term::HostTerm;

fn caps(colors: u32, truecolor: bool) -> TerminalCaps {
    TerminalCaps {
        truecolor,
        colors,
        bracketed_paste: true,
        mouse_sgr: true,
        osc52: true,
        term_name: "kitty".to_string(),
    }
}

/// Start a shell with the negotiated environment and have it print $TERM and
/// $COLORTERM back through the PTY.
fn shell_reports(caps: &TerminalCaps) -> String {
    let size = TermSize { cols: 80, rows: 10 };
    let env = oxutrm_host::caps::child_env(caps);
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
        let s = term.snapshot(0);
        let text: String = s.cells.iter().map(|c| c.text.as_str()).collect();
        if text.contains('>') {
            return text;
        }
        assert!(std::time::Instant::now() < deadline, "shell never reported: {text:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn a_truecolor_client_gives_the_child_term_and_colorterm() {
    let out = shell_reports(&caps(16_777_216, true));
    assert!(out.contains("<xterm-256color|truecolor>"), "got {out:?}");
}

#[test]
fn a_256_colour_client_gives_the_child_an_empty_colorterm() {
    let out = shell_reports(&caps(256, false));
    assert!(out.contains("<xterm-256color|>"), "got {out:?}");
}
```

- [ ] **Step 6: Run test to verify it fails, then passes**

`cargo test --jobs 4 -p oxutrm-host --test child_env -- --test-threads 4`

If `oxutrm-host`'s `Cargo.toml` lacks `oxutrm-term` as a dependency, add it:

```toml
[dependencies]
oxutrm-term = { path = "../oxutrm-term" }
```

Expected after that: PASS, 2 tests.

- [ ] **Step 7: Run the gates and commit**

```bash
cargo clippy --all-targets --all-features -- -D warnings
cargo test --jobs 4 -- --test-threads 4
git add crates/oxutrm-host/src/caps.rs crates/oxutrm-host/src/lib.rs \
        crates/oxutrm-host/tests/child_env.rs crates/oxutrm-host/Cargo.toml \
        crates/oxutrm-term/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(host): negotiate TERM and COLORTERM from the client's real capabilities

The client's own terminal name stays diagnostic; the child gets the honest
intersection of what vt100 emulates and what the client can display, and
CapsUpdate carries a new terminal on reattach.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: The host session loop

PTY → `vt100` → `Sender<ScreenState>` → QUIC, and QUIC → `Receiver<InputState>`
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
  `oxutrm_net::xport::{FrameSink, FrameSource, Sent}`; `oxutrm_net::pace::Pacer`;
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
//!   PTY --> vt100 --> Sender<ScreenState> --> Frame --> QUIC
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
use oxutrm_net::xport::{FrameSink, FrameSource, Sent};
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

    let mut screen_tx: Sender<ScreenState> = Sender::new(term.snapshot(0));
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
                if input_rx.on_frame(&frame)? {
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
                if dirty && pacer.may_send(now, rtt) && !sink.is_busy() {
                    let next = term.snapshot(screen_tx.current().seq);
                    if next != *screen_tx.current() {
                        screen_tx.update(next);
                    }
                    if let Some(frame) = screen_tx.make_frame(input_rx.ack())? {
                        match sink.send(&frame)? {
                            Sent::Skipped => {}
                            _ => {
                                cursor.on_ack_sent(frame.my_state);
                                pacer.on_sent(now);
                                dirty = false;
                            }
                        }
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

Expected: PASS, 2 tests. If `typed_input_reaches_the_shell_and_the_output_comes_back`
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
feat(host): the session loop, PTY to vt100 to QUIC and back

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

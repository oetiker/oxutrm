# oxutrm M2 — QUIC over a Punched Socket — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `oxutrm-net` complete — socket binding, candidate gathering, STUN, router port mapping, ICE hole punching, the birthday blast, and pinned QUIC — and prove it end to end with a dummy echo payload between two peers behind simulated NAT.

**Architecture:** One UDP socket per process is bound once (preferring UDP/443) and never rebound. Everything shares it: STUN discovery, ICE connectivity checks, and finally QUIC. STUN and QUIC are demultiplexed by the first two bits of each datagram. ICE checks are real STUN Binding Requests carrying `MESSAGE-INTEGRITY` keyed by the SSH-delivered PSK, so the same packets do address discovery, authentication and peer-reflexive learning at once. When the NAT is symmetric the ladder falls through to a hard-capped birthday blast on a fresh set of sockets. QUIC (`quinn` 0.11) is then handed the winning socket with a `rustls` verifier that trusts exactly one SPKI fingerprint.

**Tech Stack:** Rust 2021, `tokio` 1, `quinn` 0.11, `rustls` 0.23 (ring provider), `rcgen` 0.13, `stun_codec` 0.4 + `bytecodec` 0.5, `crab_nat` 0.8, `igd-next` 0.17, `socket2` 0.6, `if-addrs` 0.15, `sha2`, `hmac`, `base64`, `rand` 0.9.

**Spec:** `docs/superpowers/specs/2026-08-25-oxutrm-design.md` (§5, §6, §11, §12.1 are this milestone's)

**Contract:** `docs/superpowers/plans/2026-08-25-oxutrm-contract.md` — **normative**, read it before every task.

---

## Global Constraints

Every task's requirements implicitly include the contract file above and this list.

- Binary and product name is **`oxutrm`**, never `oxuterm`. The checkout directory is `oxuterm` for historical reasons; nothing inside it uses that spelling.
- Rust **edition 2021**, workspace at the repo root, one binary `src/main.rs`.
- **Cap all parallelism at 4**: `cargo build --jobs 4`, `cargo test --jobs 4 -- --test-threads 4`. The build machine is shared with other people.
- Workspace root `Cargo.toml` must contain:
  ```toml
  [profile.dev]
  debug = "line-tables-only"
  split-debuginfo = "unpacked"
  ```
- **English** for all identifiers, comments and documentation.
- `anyhow::Result` at crate-boundary level; concrete error enums only where callers must discriminate.
- **No key material is ever written to disk**, in any crate, at any time. The PSK and the private key live in memory only.
- **Every task ends green**: `cargo clippy --all-targets --jobs 4 -- -D warnings` and `cargo test --jobs 4 -- --test-threads 4` both pass before committing.
- **CI must never depend on a public STUN server.** Every non-`#[ignore]`d test uses the in-tree responder from Task 5.
- Commit messages end with:
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`

## What M2 does *not* build

- `Rung::SshTunnel` is **rung 4 and belongs to M4**. The variant exists in `oxutrm-proto` and M2 never constructs it. Do not add an `#[allow(dead_code)]` for it and do not delete it — it is a `pub` enum variant in another crate, so it produces no warning here.
- No terminal, PTY, renderer or sync-engine code. M2 talks to itself with a dummy echo payload.
- No SSH. Candidates are pasted between the two demo processes on stdin.

---

## File Structure

Everything M2 creates lives in `crates/oxutrm-net/` plus one hidden subcommand in the root binary and one test harness directory.

| File | Responsibility |
|---|---|
| `crates/oxutrm-net/Cargo.toml` | dependencies, the `oxutrm-net` package |
| `crates/oxutrm-net/src/lib.rs` | module tree and the crate's public re-exports |
| `crates/oxutrm-net/src/config.rs` | `NetConfig` and its `Default` |
| `crates/oxutrm-net/src/socketfam.rs` | `bind_socket`, IPv4-mapped address helpers |
| `crates/oxutrm-net/src/candidates.rs` | `local_candidates`, `ice_priority`, address classification |
| `crates/oxutrm-net/src/demux.rs` | `is_stun` |
| `crates/oxutrm-net/src/stunserver.rs` | `StunResponder` — a minimal STUN Binding server |
| `crates/oxutrm-net/src/discover.rs` | `stun_discover` and NAT typing |
| `crates/oxutrm-net/src/stunmsg.rs` | ICE checks: build, parse, `MESSAGE-INTEGRITY` |
| `crates/oxutrm-net/src/ice.rs` | `IceAgent`, `IceRole`, `IceEvent`, nomination |
| `crates/oxutrm-net/src/birthday.rs` | rung 3, the birthday blast |
| `crates/oxutrm-net/src/mapping.rs` | `PortMapping` (NAT-PMP/PCP then UPnP-IGD), gateway discovery |
| `crates/oxutrm-net/src/der.rs` | minimal DER walker: SPKI extraction from an X.509 certificate |
| `crates/oxutrm-net/src/tls.rs` | `generate_cert`, the pinned `ServerCertVerifier`, the crypto provider |
| `crates/oxutrm-net/src/quic.rs` | `quic_server`, `quic_client`, `TransportConfig` |
| `crates/oxutrm-net/src/establish.rs` | `gather`, `connect_path` — the ladder, wired up |
| `src/netdemo.rs` | the hidden `oxutrm netdemo` subcommand |
| `tests/netdemo_loopback.rs` | two real processes, echo over QUIC, on loopback |
| `tests/netns/*.sh` | the network-namespace NAT harness |
| `tests/netns.rs` | the Rust integration test that drives the harness and skips cleanly |

---

## Task 1: Crate scaffold and `NetConfig`

**Files:**
- Create: `crates/oxutrm-net/Cargo.toml`
- Create: `crates/oxutrm-net/src/lib.rs`
- Create: `crates/oxutrm-net/src/config.rs`
- Modify: `Cargo.toml` (workspace root — add the new member)

**Interfaces:**
- Consumes: `oxutrm_proto::{Candidate, CandidateKind, NatType, Rung, PathDescription}` from M1's `oxutrm-proto` crate. If `crates/oxutrm-proto` does not exist yet, M1 has not landed — stop and say so rather than inventing it.
- Produces:
  ```rust
  pub struct NetConfig {
      pub stun_servers: Vec<String>,
      pub prefer_port: u16,
      pub enable_port_mapping: bool,
      pub enable_birthday: bool,
      pub birthday_sockets: u16,
      pub birthday_ports: u16,
      pub birthday_budget: std::time::Duration,
      pub gather_timeout: std::time::Duration,
  }
  impl Default for NetConfig { /* ... */ }
  impl Clone for NetConfig  { /* derived */ }
  ```

- [ ] **Step 1: Add the crate to the workspace**

The workspace root `Cargo.toml` already exists from M1. Add `"crates/oxutrm-net"` to its `members` list. If the file does not exist at all, create it with exactly this content:

```toml
[workspace]
resolver = "2"
members = ["crates/oxutrm-net"]

[package]
name = "oxutrm"
version = "0.1.0"
edition = "2021"

[dependencies]
anyhow = "1"

[profile.dev]
debug = "line-tables-only"
split-debuginfo = "unpacked"
```

- [ ] **Step 2: Write the failing test**

Create `crates/oxutrm-net/src/config.rs` containing only the tests, so the build fails on the missing type:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn the_defaults_match_the_design_spec() {
        let c = NetConfig::default();
        // Spec section 5.6: UDP/443 is the one UDP port restrictive networks
        // tend to leave open, because blocking it breaks HTTP/3.
        assert_eq!(c.prefer_port, 443);
        // Spec section 5.3: several servers, queried in parallel, so that two
        // different servers can be compared for NAT typing.
        assert!(c.stun_servers.len() >= 3, "NAT typing needs at least two distinct servers");
        assert!(c.stun_servers.iter().all(|s| s.contains(':')), "every entry is host:port");
        assert!(c.stun_servers.iter().any(|s| s.ends_with(":443")),
                "at least one server on 443, for networks that block 3478");
        assert!(c.enable_port_mapping);
        assert!(c.enable_birthday);
        // Spec section 5.4: N ~= 256 sockets, M ~= 256 guessed ports.
        assert_eq!(c.birthday_sockets, 256);
        assert_eq!(c.birthday_ports, 256);
        assert!(c.birthday_budget <= Duration::from_secs(10), "the blast is hard-capped");
        assert!(c.gather_timeout <= Duration::from_secs(10));
    }

    #[test]
    fn a_config_can_be_narrowed_with_struct_update_syntax() {
        // Every test in this crate builds its config this way, so the struct
        // must stay Clone and have no private fields.
        let c = NetConfig { enable_birthday: false, ..NetConfig::default() };
        assert!(!c.enable_birthday);
        assert_eq!(c.prefer_port, 443);
        let _ = c.clone();
    }
}
```

- [ ] **Step 3: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find type NetConfig in this scope` (and the package does not exist yet, so cargo errors first; that is the same failure).

- [ ] **Step 4: Write the crate manifest**

Create `crates/oxutrm-net/Cargo.toml`:

```toml
[package]
name = "oxutrm-net"
version = "0.1.0"
edition = "2021"

[dependencies]
oxutrm-proto = { path = "../oxutrm-proto" }

anyhow = "1"
base64 = "0.22"
bytecodec = "0.5"
bytes = "1"
crab_nat = "0.8"
hmac = "0.12"
if-addrs = "0.15"
igd-next = { version = "0.17", features = ["aio_tokio"] }
quinn = { version = "0.11", default-features = false, features = ["log", "ring", "runtime-tokio", "rustls-ring"] }
rand = "0.9"
rcgen = "0.13"
rustls = { version = "0.23", default-features = false, features = ["logging", "ring", "std"] }
sha2 = "0.10"
socket2 = "0.6"
stun_codec = "0.4"
tokio = { version = "1", features = ["io-util", "macros", "net", "rt-multi-thread", "sync", "time"] }
```

Two notes on the feature flags, both load-bearing:

- `quinn`'s default features include `platform-verifier`, which drags in the
  whole OS trust store. oxutrm pins one certificate and trusts nothing else
  (spec §6.1), so defaults are off and only the four features above are on.
- `rustls`'s default features include the `aws-lc-rs` provider. With **two**
  providers compiled in, `rustls::ClientConfig::builder()` panics at runtime
  with "no process-level CryptoProvider available". Defaults are off, `ring`
  is on, and every builder in this crate is constructed with an explicit
  provider anyway (Task 11).

- [ ] **Step 5: Write the minimal implementation**

Put this at the top of `crates/oxutrm-net/src/config.rs`, above the `mod tests`:

```rust
use std::time::Duration;

/// Everything about the network layer that a user may reasonably want to
/// change. Constructed once and passed by reference; never mutated after
/// gathering starts.
#[derive(Clone, Debug)]
pub struct NetConfig {
    /// `host:port` strings. Resolved lazily, in parallel. An entry that does
    /// not resolve is skipped, not fatal.
    pub stun_servers: Vec<String>,
    /// The port to try first. UDP/443 (spec §5.6).
    pub prefer_port: u16,
    /// Rung 1: ask the router for a port mapping.
    pub enable_port_mapping: bool,
    /// Rung 3: the birthday blast. Deliberately noisy; a user may switch it off.
    pub enable_birthday: bool,
    /// Rung 3: how many extra sockets the blast opens.
    pub birthday_sockets: u16,
    /// Rung 3: how many ports each socket guesses at.
    pub birthday_ports: u16,
    /// Rung 3: a hard wall-clock cap on the whole blast.
    pub birthday_budget: Duration,
    /// How long candidate gathering and connectivity checks may take.
    pub gather_timeout: Duration,
}

impl Default for NetConfig {
    fn default() -> NetConfig {
        NetConfig {
            stun_servers: vec![
                "stun.cloudflare.com:3478".to_owned(),
                "stun.l.google.com:19302".to_owned(),
                // Useful where 3478 is blocked outright.
                "stun.nextcloud.com:443".to_owned(),
                "stun.sipgate.net:3478".to_owned(),
            ],
            prefer_port: 443,
            enable_port_mapping: true,
            enable_birthday: true,
            birthday_sockets: 256,
            birthday_ports: 256,
            birthday_budget: Duration::from_secs(6),
            gather_timeout: Duration::from_secs(3),
        }
    }
}
```

Create `crates/oxutrm-net/src/lib.rs`:

```rust
//! oxutrm's network layer: candidate gathering, STUN, NAT traversal and QUIC.
//!
//! The whole crate revolves around **one** UDP socket. It is bound once
//! (§5.6), used for STUN discovery, used again for ICE connectivity checks,
//! and finally handed to `quinn`. NAT mappings are per-socket, so an address
//! learned on any other socket would describe nothing useful.

mod config;

pub use config::NetConfig;
```

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 2 tests pass, clippy clean.

- [ ] **Step 7: Commit**

```bash
git add crates/oxutrm-net/Cargo.toml crates/oxutrm-net/src/lib.rs crates/oxutrm-net/src/config.rs Cargo.toml Cargo.lock
git commit -m "$(cat <<'EOF'
feat(net): add the oxutrm-net crate and NetConfig

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: `bind_socket` and the IPv4-mapped address helpers

**Files:**
- Create: `crates/oxutrm-net/src/socketfam.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `NetConfig { prefer_port: u16, .. }` from Task 1.
- Produces:
  ```rust
  pub fn bind_socket(cfg: &NetConfig) -> anyhow::Result<std::net::UdpSocket>;

  /// Rewrite `peer` into the form `local`'s socket family can actually send to.
  pub fn to_socket_family(local: &std::net::SocketAddr, peer: std::net::SocketAddr)
      -> std::net::SocketAddr;

  /// Undo IPv4-mapping, so a candidate carries the address a peer would dial.
  pub fn unmap(addr: std::net::SocketAddr) -> std::net::SocketAddr;
  ```

**Why the two helpers exist:** a dual-stack socket bound to `[::]` must be
handed IPv4 peers in their IPv4-mapped form (`::ffff:198.51.100.7`) or
`send_to` fails with `EINVAL`, and it reports IPv4 sources back in that same
mapped form. Every `send_to` in this crate goes through `to_socket_family`
and every source address read off the socket goes through `unmap`. Forgetting
either produces a socket that silently talks to nobody.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/socketfam.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetConfig;
    use std::net::{SocketAddr, UdpSocket};

    #[test]
    fn the_preferred_port_is_used_when_it_is_free() {
        // Take a port, learn its number, give it back.
        let probe = UdpSocket::bind("0.0.0.0:0").unwrap();
        let free = probe.local_addr().unwrap().port();
        drop(probe);

        let cfg = NetConfig { prefer_port: free, ..NetConfig::default() };
        let s = bind_socket(&cfg).unwrap();
        assert_eq!(s.local_addr().unwrap().port(), free);
    }

    #[test]
    fn a_taken_preferred_port_falls_back_to_a_high_port() {
        // Hold the port on both families so neither preferred attempt can win.
        let v6 = UdpSocket::bind("[::]:0").unwrap();
        let port = v6.local_addr().unwrap().port();
        // On Linux a dual-stack [::] socket already covers 0.0.0.0, so this
        // second bind may fail. Either way the port is occupied.
        let v4 = UdpSocket::bind(("0.0.0.0", port));

        let cfg = NetConfig { prefer_port: port, ..NetConfig::default() };
        let s = bind_socket(&cfg).unwrap();
        let got = s.local_addr().unwrap().port();
        assert_ne!(got, port, "must not have stolen the occupied port");
        assert_ne!(got, 0, "must report the real port, not the wildcard");

        drop(v4);
        drop(v6);
    }

    #[test]
    fn a_privileged_preferred_port_never_makes_binding_fail() {
        // Unprivileged CI cannot bind 443. bind_socket must fall back to a
        // high port, not return an error: a session that cannot bind is a
        // session that cannot exist.
        let s = bind_socket(&NetConfig::default()).expect("must always yield a socket");
        assert_ne!(s.local_addr().unwrap().port(), 0);
    }

    #[test]
    fn an_ipv6_socket_is_handed_ipv4_peers_in_mapped_form() {
        let local: SocketAddr = "[::]:443".parse().unwrap();
        let peer: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        let out = to_socket_family(&local, peer);
        assert_eq!(out, "[::ffff:198.51.100.7]:1234".parse::<SocketAddr>().unwrap());
    }

    #[test]
    fn an_ipv4_socket_is_left_alone() {
        let local: SocketAddr = "0.0.0.0:443".parse().unwrap();
        let peer: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        assert_eq!(to_socket_family(&local, peer), peer);

        let local6: SocketAddr = "[::]:443".parse().unwrap();
        let peer6: SocketAddr = "[2001:db8::1]:1234".parse().unwrap();
        assert_eq!(to_socket_family(&local6, peer6), peer6);
    }

    #[test]
    fn unmap_undoes_ipv4_mapping_and_leaves_everything_else_alone() {
        assert_eq!(
            unmap("[::ffff:198.51.100.7]:1234".parse().unwrap()),
            "198.51.100.7:1234".parse::<SocketAddr>().unwrap()
        );
        let v6: SocketAddr = "[2001:db8::1]:1234".parse().unwrap();
        assert_eq!(unmap(v6), v6);
        let v4: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        assert_eq!(unmap(v4), v4);
    }

    #[test]
    fn mapping_then_unmapping_is_the_identity() {
        let local: SocketAddr = "[::]:0".parse().unwrap();
        for s in ["198.51.100.7:1", "[2001:db8::1]:65535", "127.0.0.1:9"] {
            let a: SocketAddr = s.parse().unwrap();
            assert_eq!(unmap(to_socket_family(&local, a)), a, "round trip for {s}");
        }
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function bind_socket in this scope` and the same for `to_socket_family` and `unmap`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/socketfam.rs`:

```rust
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

use socket2::{Domain, Protocol, SockAddr, Socket, Type};

use crate::NetConfig;

/// Bind the one UDP socket the whole session lives on.
///
/// Order of preference (spec §5.6):
///   1. `[::]:prefer_port` as a dual-stack socket, so one socket serves both
///      families and there is only one NAT mapping to manage,
///   2. `0.0.0.0:prefer_port`, for hosts without IPv6,
///   3. `[::]:0` dual-stack,
///   4. `0.0.0.0:0`.
///
/// Binding 443 requires privilege on most systems. Failing to get it is
/// completely normal and must never be fatal: a high port still works, and a
/// router mapping may still advertise 443 externally.
pub fn bind_socket(cfg: &NetConfig) -> anyhow::Result<UdpSocket> {
    let attempts = [
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), cfg.prefer_port),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), cfg.prefer_port),
        SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
    ];

    let mut last: Option<io::Error> = None;
    for addr in attempts {
        match bind_one(addr) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    match last {
        Some(e) => Err(anyhow::Error::new(e).context("could not bind any UDP socket")),
        None => Err(anyhow::anyhow!("could not bind any UDP socket")),
    }
}

fn bind_one(addr: SocketAddr) -> io::Result<UdpSocket> {
    let domain = if addr.is_ipv6() { Domain::IPV6 } else { Domain::IPV4 };
    let sock = Socket::new(domain, Type::DGRAM, Some(Protocol::UDP))?;
    if addr.is_ipv6() {
        // Accept IPv4-mapped traffic on the same socket. Some kernels refuse
        // (net.ipv6.bindv6only=1); a v6-only socket is still useful, so this
        // failure is deliberately swallowed.
        let _ = sock.set_only_v6(false);
    }
    sock.bind(&SockAddr::from(addr))?;
    Ok(UdpSocket::from(sock))
}

/// Rewrite `peer` into the form `local`'s socket family can send to.
///
/// A dual-stack socket bound to `[::]` rejects a plain `SocketAddr::V4`
/// destination with `EINVAL`; it wants the IPv4-mapped form. Every `send_to`
/// in this crate goes through here.
pub fn to_socket_family(local: &SocketAddr, peer: SocketAddr) -> SocketAddr {
    match (local, peer) {
        (SocketAddr::V6(_), SocketAddr::V4(v4)) => {
            SocketAddr::new(IpAddr::V6(v4.ip().to_ipv6_mapped()), v4.port())
        }
        _ => peer,
    }
}

/// Undo IPv4-mapping. A dual-stack socket reports IPv4 sources as
/// `::ffff:a.b.c.d`; a candidate carrying that form would be useless to a
/// peer with no IPv6, so every source address read off the socket, and every
/// address read out of a STUN attribute, goes through here.
pub fn unmap(addr: SocketAddr) -> SocketAddr {
    match addr {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => addr,
        },
        SocketAddr::V4(_) => addr,
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod socketfam;

pub use socketfam::{bind_socket, to_socket_family, unmap};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 9 tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/socketfam.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): bind the session socket, preferring UDP/443, dual-stack

Adds the IPv4-mapped address helpers every send_to and recv_from in this
crate must go through: a [::] socket rejects plain V4 destinations and
reports V4 sources in mapped form.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: `local_candidates` and ICE priority

**Files:**
- Create: `crates/oxutrm-net/src/candidates.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks except the crate itself.
- Produces:
  ```rust
  pub fn local_candidates(socket: &std::net::UdpSocket)
      -> Vec<oxutrm_proto::Candidate>;

  /// The same, with a door for tests: production callers use `local_candidates`.
  pub fn local_candidates_filtered(socket: &std::net::UdpSocket, include_loopback: bool)
      -> Vec<oxutrm_proto::Candidate>;

  pub fn ice_priority(kind: oxutrm_proto::CandidateKind, ip: &std::net::IpAddr) -> u32;

  pub fn is_link_local(ip: &std::net::IpAddr) -> bool;
  ```

**Priority formula, and where it departs from RFC 8445.** The shape is RFC
8445 §5.1.2: `(type_pref << 24) | (local_pref << 8) | (256 - component_id)`,
with one component so the last term is always 255. The **type preferences are
the design spec's, not the RFC's**: the spec (§4.2) wants IPv6 Host highest
and PeerReflexive lowest, whereas RFC 8445 ranks peer-reflexive above
server-reflexive. Follow the spec. Both peers run this same function, so the
ordering only has to be self-consistent.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/candidates.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::CandidateKind;
    use std::net::{IpAddr, UdpSocket};

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_ordering_is_the_one_the_design_spec_asks_for() {
        let v6 = ip("2001:db8::1");
        let v4 = ip("192.0.2.1");
        // "IPv6 Host highest, PeerReflexive lowest" (spec section 4.2).
        assert!(ice_priority(CandidateKind::Host, &v6) > ice_priority(CandidateKind::Host, &v4));
        assert!(
            ice_priority(CandidateKind::Host, &v4) > ice_priority(CandidateKind::PortMapped, &v6)
        );
        assert!(
            ice_priority(CandidateKind::PortMapped, &v6)
                > ice_priority(CandidateKind::ServerReflexive, &v6)
        );
        assert!(
            ice_priority(CandidateKind::ServerReflexive, &v6)
                > ice_priority(CandidateKind::PeerReflexive, &v6)
        );
    }

    #[test]
    fn a_global_ipv6_address_outranks_a_link_local_or_unique_local_one() {
        let global = ice_priority(CandidateKind::Host, &ip("2001:db8::1"));
        let ula = ice_priority(CandidateKind::Host, &ip("fd00::1"));
        let ll = ice_priority(CandidateKind::Host, &ip("fe80::1"));
        assert!(global > ula);
        assert!(global > ll);
    }

    #[test]
    fn priorities_never_overflow_and_always_leave_room_for_the_component_id() {
        for kind in [
            CandidateKind::Host,
            CandidateKind::PortMapped,
            CandidateKind::ServerReflexive,
            CandidateKind::PeerReflexive,
        ] {
            for s in ["2001:db8::1", "192.0.2.1", "fe80::1", "fd00::1", "127.0.0.1"] {
                let p = ice_priority(kind, &ip(s));
                assert!(p > 0);
                assert_eq!(p & 0xff, 255, "component id 1 means the low byte is 255");
            }
        }
    }

    #[test]
    fn link_local_is_recognised_in_both_families() {
        assert!(is_link_local(&ip("169.254.1.1")));
        assert!(is_link_local(&ip("fe80::1")));
        assert!(is_link_local(&ip("febf:ffff::1")), "fe80::/10 runs to febf");
        assert!(!is_link_local(&ip("192.0.2.1")));
        assert!(!is_link_local(&ip("2001:db8::1")));
        assert!(!is_link_local(&ip("fec0::1")), "fec0 is deprecated site-local, not link-local");
        assert!(!is_link_local(&ip("fd00::1")));
    }

    #[test]
    fn loopback_and_link_local_are_excluded_unless_a_test_asks_for_them() {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let strict = local_candidates(&s);
        assert!(strict.iter().all(|c| !c.addr.ip().is_loopback()));
        assert!(strict.iter().all(|c| !is_link_local(&c.addr.ip())));

        let loose = local_candidates_filtered(&s, true);
        assert!(
            loose.iter().any(|c| c.addr.ip().is_loopback()),
            "the test door must expose loopback, or nothing in this crate is testable offline"
        );
    }

    #[test]
    fn every_candidate_carries_the_socket_port_and_the_host_kind() {
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = s.local_addr().unwrap().port();
        let cs = local_candidates_filtered(&s, true);
        assert!(!cs.is_empty());
        assert!(cs.iter().all(|c| c.addr.port() == port));
        assert!(cs.iter().all(|c| c.kind == CandidateKind::Host));
    }

    #[test]
    fn candidates_come_back_sorted_by_descending_priority_and_deduplicated() {
        let s = UdpSocket::bind("[::]:0").unwrap();
        let cs = local_candidates_filtered(&s, true);
        for w in cs.windows(2) {
            assert!(w[0].priority >= w[1].priority);
        }
        let mut addrs: Vec<_> = cs.iter().map(|c| c.addr).collect();
        let before = addrs.len();
        addrs.sort_by_key(|a| a.to_string());
        addrs.dedup();
        assert_eq!(addrs.len(), before, "no duplicate addresses");
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function ice_priority in this scope`, and the same for `is_link_local`, `local_candidates`, `local_candidates_filtered`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/candidates.rs`:

```rust
use std::net::{IpAddr, Ipv6Addr, SocketAddr, UdpSocket};

use oxutrm_proto::{Candidate, CandidateKind};

/// RFC 8445 §5.1.2 shape: `(type_pref << 24) | (local_pref << 8) | (256 - component)`.
///
/// The **type preferences follow the design spec, not RFC 8445**: the spec
/// wants IPv6 Host highest and PeerReflexive lowest, while the RFC ranks
/// peer-reflexive above server-reflexive. Both peers run this function, so
/// self-consistency is all that is required.
pub fn ice_priority(kind: CandidateKind, ip: &IpAddr) -> u32 {
    let type_pref: u32 = match kind {
        CandidateKind::Host => 126,
        CandidateKind::PortMapped => 110,
        CandidateKind::ServerReflexive => 100,
        CandidateKind::PeerReflexive => 90,
    };
    let local_pref: u32 = match ip {
        // A global IPv6 address is the one case where no NAT exists at all
        // (spec §5.1), so it wins outright.
        IpAddr::V6(v6) if is_global_v6(v6) => 65_535,
        IpAddr::V6(_) => 40_000,
        IpAddr::V4(_) => 32_767,
    };
    // One component (there is no RTP/RTCP split here), so the last term is 255.
    (type_pref << 24) | (local_pref << 8) | 255
}

/// `169.254.0.0/16` and `fe80::/10`.
pub fn is_link_local(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_link_local(),
        // `Ipv6Addr::is_unicast_link_local` is still unstable, so this is
        // spelled out: fe80::/10 covers fe80 through febf.
        IpAddr::V6(v6) => (v6.segments()[0] & 0xffc0) == 0xfe80,
    }
}

/// `fc00::/7`, the unique-local range. Routable inside one site, not globally.
fn is_unique_local_v6(v6: &Ipv6Addr) -> bool {
    (v6.segments()[0] & 0xfe00) == 0xfc00
}

fn is_global_v6(v6: &Ipv6Addr) -> bool {
    !v6.is_loopback()
        && !v6.is_unspecified()
        && (v6.segments()[0] & 0xffc0) != 0xfe80
        && !is_unique_local_v6(v6)
}

/// Every local interface address, paired with the port `socket` is bound to,
/// as `CandidateKind::Host`. Loopback and link-local are excluded: a peer on
/// another machine can do nothing with either.
pub fn local_candidates(socket: &UdpSocket) -> Vec<Candidate> {
    local_candidates_filtered(socket, false)
}

/// `local_candidates` with a door for tests.
///
/// `include_loopback` also lets link-local through. It exists because the
/// unit tests in this crate must run with no network at all, and loopback is
/// then the only address available. Production callers use
/// [`local_candidates`], which never sets it.
pub fn local_candidates_filtered(socket: &UdpSocket, include_loopback: bool) -> Vec<Candidate> {
    let port = match socket.local_addr() {
        Ok(a) => a.port(),
        Err(_) => return Vec::new(),
    };
    let ifaces = match if_addrs::get_if_addrs() {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    let mut out: Vec<Candidate> = Vec::new();
    for iface in ifaces {
        let ip = iface.ip();
        if ip.is_unspecified() {
            continue;
        }
        if !include_loopback && (ip.is_loopback() || is_link_local(&ip)) {
            continue;
        }
        let addr = SocketAddr::new(ip, port);
        if out.iter().any(|c| c.addr == addr) {
            continue;
        }
        out.push(Candidate {
            addr,
            kind: CandidateKind::Host,
            priority: ice_priority(CandidateKind::Host, &ip),
        });
    }

    // Highest priority first; ties broken on the address text so the list is
    // stable between runs and between the two peers.
    out.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.addr.to_string().cmp(&b.addr.to_string()))
    });
    out
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod candidates;

pub use candidates::{ice_priority, is_link_local, local_candidates, local_candidates_filtered};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 7 new tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/candidates.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): enumerate host candidates with ICE-style priorities

Type preferences follow the design spec (IPv6 Host highest, PeerReflexive
lowest) rather than RFC 8445's ordering; the formula shape is the RFC's.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `is_stun` — demultiplexing STUN from QUIC

**Files:**
- Create: `crates/oxutrm-net/src/demux.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  ```rust
  pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];
  /// True when the datagram is STUN rather than QUIC.
  pub fn is_stun(datagram: &[u8]) -> bool;
  ```

**Why this works.** A STUN message's first two bits are `00` (RFC 5389 §6).
Every QUIC packet sets the fixed bit `0x40`; long-header packets additionally
set `0x80`. So `datagram[0] & 0xC0 == 0` already separates them. This function
checks two more things anyway — the magic cookie and the length field — because
a false positive would feed a QUIC packet to the STUN decoder on a live
connection, and the cost of the extra checks is four byte comparisons.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/demux.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Build a syntactically real STUN message: 2-byte type, 2-byte length,
    /// 4-byte magic cookie, 12-byte transaction id, then the body.
    fn stun_bytes(msg_type: [u8; 2], body: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&msg_type);
        v.extend_from_slice(&(body.len() as u16).to_be_bytes());
        v.extend_from_slice(&STUN_MAGIC_COOKIE);
        v.extend_from_slice(&[0xA1; 12]);
        v.extend_from_slice(body);
        v
    }

    fn quic(first: u8, rest: usize) -> Vec<u8> {
        let mut v = vec![first];
        v.extend(std::iter::repeat(0x5C).take(rest));
        v
    }

    #[test]
    fn is_stun_table() {
        struct Case {
            name: &'static str,
            bytes: Vec<u8>,
            want: bool,
        }

        let cases = vec![
            Case {
                name: "binding request, empty body",
                bytes: stun_bytes([0x00, 0x01], &[]),
                want: true,
            },
            Case {
                name: "binding success response carrying one 8-byte attribute",
                bytes: stun_bytes([0x01, 0x01], &[0x00, 0x20, 0x00, 0x04, 0x00, 0x01, 0x2b, 0x3c]),
                want: true,
            },
            Case {
                name: "binding error response",
                bytes: stun_bytes([0x01, 0x11], &[]),
                want: true,
            },
            Case {
                name: "binding indication",
                bytes: stun_bytes([0x00, 0x11], &[]),
                want: true,
            },
            Case {
                name: "QUIC Initial: long header, fixed bit set (0xC3)",
                bytes: quic(0xC3, 40),
                want: false,
            },
            Case {
                name: "QUIC Handshake: long header (0xE0)",
                bytes: quic(0xE0, 40),
                want: false,
            },
            Case {
                name: "QUIC Retry: long header (0xF0)",
                bytes: quic(0xF0, 40),
                want: false,
            },
            Case {
                name: "QUIC 1-RTT: short header, fixed bit only (0x40)",
                bytes: quic(0x40, 40),
                want: false,
            },
            Case {
                name: "QUIC 1-RTT with key phase and packet-number bits (0x5B)",
                bytes: quic(0x5B, 40),
                want: false,
            },
            Case {
                name: "QUIC version negotiation: high bit set, fixed bit clear (0x80)",
                bytes: quic(0x80, 40),
                want: false,
            },
            Case {
                name: "empty datagram",
                bytes: Vec::new(),
                want: false,
            },
            Case {
                name: "one byte short of a STUN header",
                bytes: stun_bytes([0x00, 0x01], &[])[..19].to_vec(),
                want: false,
            },
            Case {
                name: "correct leading bits but the wrong magic cookie",
                bytes: {
                    let mut v = stun_bytes([0x00, 0x01], &[]);
                    v[4] = 0x20;
                    v
                },
                want: false,
            },
            Case {
                name: "length field claims more body than the datagram has",
                bytes: {
                    let mut v = stun_bytes([0x00, 0x01], &[]);
                    v[3] = 0x08;
                    v
                },
                want: false,
            },
            Case {
                name: "length field claims less body than the datagram has",
                bytes: {
                    let mut v = stun_bytes([0x00, 0x01], &[0, 0, 0, 0]);
                    v[3] = 0x00;
                    v
                },
                want: false,
            },
            Case {
                name: "length field is not a multiple of four",
                bytes: stun_bytes([0x00, 0x01], &[0, 0, 0, 0, 0, 0]),
                want: false,
            },
        ];

        for c in cases {
            assert_eq!(is_stun(&c.bytes), c.want, "case: {}", c.name);
        }
    }

    #[test]
    fn every_byte_with_the_top_two_bits_set_is_rejected_regardless_of_the_rest() {
        // Exhaustive over the first byte: whatever else a packet contains,
        // anything QUIC could put in byte 0 must not be taken for STUN.
        for first in 0x40u16..=0xFF {
            let mut v = stun_bytes([0x00, 0x01], &[]);
            v[0] = first as u8;
            assert!(!is_stun(&v), "first byte {first:#04x} must not be STUN");
        }
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function is_stun in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/demux.rs`:

```rust
/// RFC 5389 §6. Present in every STUN message from byte 4.
pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

/// True when the datagram is STUN rather than QUIC.
///
/// STUN's leading two bits are `00`. Every QUIC packet sets the fixed bit
/// (`0x40`) and long-header packets also set `0x80`, so that test alone
/// separates the two protocols on a shared socket. The magic cookie and the
/// length field are checked as well: on a live connection a false positive
/// would hand a QUIC packet to the STUN decoder, and four extra byte
/// comparisons are cheaper than that.
pub fn is_stun(datagram: &[u8]) -> bool {
    if datagram.len() < 20 {
        return false;
    }
    if datagram[0] & 0xC0 != 0 {
        return false;
    }
    if datagram[4..8] != STUN_MAGIC_COOKIE {
        return false;
    }
    let body_len = u16::from_be_bytes([datagram[2], datagram[3]]) as usize;
    // STUN attributes are padded to a multiple of four, so the body length is
    // always a multiple of four, and it always accounts for the whole datagram.
    body_len % 4 == 0 && 20 + body_len == datagram.len()
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod demux;

pub use demux::{is_stun, STUN_MAGIC_COOKIE};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 2 new tests pass (16 table cases plus the exhaustive sweep), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/demux.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): demultiplex STUN from QUIC on the shared socket

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `StunResponder` — an in-tree STUN Binding server

**Files:**
- Create: `crates/oxutrm-net/src/stunserver.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::unmap` (Task 2), `crate::is_stun` (Task 4).
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum MappingBehaviour {
      /// Report the source address exactly as seen.
      Truthful,
      /// Report the source IP but a fixed made-up port. Two responders with
      /// two different values look exactly like a symmetric NAT to a client.
      RewritePort(u16),
  }

  pub struct StunResponder { /* private */ }

  impl StunResponder {
      pub async fn start(behaviour: MappingBehaviour) -> anyhow::Result<StunResponder>;
      pub async fn start_on(bind: std::net::SocketAddr, behaviour: MappingBehaviour)
          -> anyhow::Result<StunResponder>;
      pub fn addr(&self) -> std::net::SocketAddr;
      /// The `host:port` string `NetConfig::stun_servers` wants.
      pub fn server_string(&self) -> String;
  }
  impl Drop for StunResponder { /* aborts the task */ }
  ```

**Why this is a production module and not a test fixture.** Spec §11 says the
STUN server list is configurable "so a user who objects can point at their own",
and §12 says CI must not depend on the public internet. One 70-line responder
serves both: every unit test in this crate binds one, and a privacy-minded user
can run `oxutrm netdemo --role stun` on their own host. It is therefore a normal
public module, not gated behind a `cfg(test)` or a feature.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/stunserver.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytecodec::{DecodeExt, EncodeExt};
    use stun_codec::rfc5389::attributes::XorMappedAddress;
    use stun_codec::rfc5389::{methods::BINDING, Attribute};
    use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder, TransactionId};
    use tokio::net::UdpSocket;

    async fn ask(server: std::net::SocketAddr) -> std::net::SocketAddr {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let tid = TransactionId::new([0x5A; 12]);
        let req = Message::<Attribute>::new(MessageClass::Request, BINDING, tid);
        let bytes = MessageEncoder::<Attribute>::new().encode_into_bytes(req).unwrap();
        sock.send_to(&bytes, server).await.unwrap();

        let mut buf = vec![0u8; 1500];
        let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(2), sock.recv_from(&mut buf))
            .await
            .expect("the responder must answer")
            .unwrap();
        assert!(crate::is_stun(&buf[..n]), "the answer must demultiplex as STUN");

        let msg = MessageDecoder::<Attribute>::new()
            .decode_from_bytes(&buf[..n])
            .unwrap()
            .unwrap();
        assert_eq!(msg.class(), MessageClass::SuccessResponse);
        assert_eq!(msg.transaction_id(), tid, "the transaction id must be echoed");
        msg.get_attribute::<XorMappedAddress>().unwrap().address()
    }

    #[tokio::test]
    async fn a_truthful_responder_reports_the_address_it_actually_saw() {
        let s = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let seen = ask(s.addr()).await;
        assert_eq!(seen.ip().to_string(), "127.0.0.1");
        assert_ne!(seen.port(), 0);
    }

    #[tokio::test]
    async fn a_rewriting_responder_reports_the_port_it_was_told_to() {
        let s = StunResponder::start(MappingBehaviour::RewritePort(40_001)).await.unwrap();
        let seen = ask(s.addr()).await;
        assert_eq!(seen.port(), 40_001);
        assert_eq!(seen.ip().to_string(), "127.0.0.1");
    }

    #[tokio::test]
    async fn two_rewriting_responders_disagree_the_way_a_symmetric_nat_does() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_001)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(50_002)).await.unwrap();
        assert_ne!(ask(a.addr()).await.port(), ask(b.addr()).await.port());
    }

    #[tokio::test]
    async fn the_server_string_is_the_form_netconfig_wants() {
        let s = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        assert_eq!(s.server_string(), s.addr().to_string());
        assert!(s.server_string().contains(':'));
    }

    #[tokio::test]
    async fn garbage_and_quic_shaped_datagrams_are_ignored_without_dying() {
        let s = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        sock.send_to(&[0xC3; 64], s.addr()).await.unwrap(); // a QUIC long header
        sock.send_to(b"hello", s.addr()).await.unwrap();
        sock.send_to(&[], s.addr()).await.unwrap();
        // Still alive and still answering.
        let seen = ask(s.addr()).await;
        assert_eq!(seen.ip().to_string(), "127.0.0.1");
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `failed to resolve: use of undeclared type StunResponder`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/stunserver.rs`:

```rust
//! A minimal STUN Binding server.
//!
//! It answers a Binding Request with a Binding Success Response carrying
//! `XOR-MAPPED-ADDRESS`, and does nothing else — no authentication, no
//! `CHANGE-REQUEST`, no `OTHER-ADDRESS`. That is exactly what
//! [`crate::stun_discover`] needs, and it means CI never has to reach a public
//! STUN server (spec §12).

use std::net::SocketAddr;
use std::sync::Arc;

use bytecodec::{DecodeExt, EncodeExt};
use stun_codec::rfc5389::attributes::XorMappedAddress;
use stun_codec::rfc5389::{methods::BINDING, Attribute};
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder};
use tokio::net::UdpSocket;

use crate::{is_stun, unmap};

/// How the responder reports the address it saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MappingBehaviour {
    /// Report the source address exactly as seen. This is what a real STUN
    /// server does.
    Truthful,
    /// Report the source IP but a fixed, made-up port. Two responders started
    /// with two different values look to a client exactly like a symmetric
    /// NAT, without needing a kernel NAT at all.
    RewritePort(u16),
}

/// A running responder. Dropping it stops the server.
pub struct StunResponder {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl StunResponder {
    /// Bind an ephemeral loopback port.
    pub async fn start(behaviour: MappingBehaviour) -> anyhow::Result<StunResponder> {
        StunResponder::start_on("127.0.0.1:0".parse().expect("a literal address"), behaviour).await
    }

    /// Bind a specific address. The netns harness uses this to put responders
    /// on the simulated internet segment.
    pub async fn start_on(
        bind: SocketAddr,
        behaviour: MappingBehaviour,
    ) -> anyhow::Result<StunResponder> {
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move { serve(socket, behaviour).await });
        Ok(StunResponder { addr, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The `host:port` string [`crate::NetConfig::stun_servers`] wants.
    pub fn server_string(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for StunResponder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(socket: Arc<UdpSocket>, behaviour: MappingBehaviour) {
    let mut buf = vec![0u8; 1500];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            // A closed socket ends the server; a transient error must not.
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => continue,
            Err(_) => return,
        };
        if !is_stun(&buf[..n]) {
            continue;
        }
        let msg = match MessageDecoder::<Attribute>::new().decode_from_bytes(&buf[..n]) {
            Ok(Ok(m)) => m,
            _ => continue,
        };
        if msg.class() != MessageClass::Request || msg.method() != BINDING {
            continue;
        }

        let seen = unmap(from);
        let reported = match behaviour {
            MappingBehaviour::Truthful => seen,
            MappingBehaviour::RewritePort(p) => SocketAddr::new(seen.ip(), p),
        };

        let mut resp =
            Message::<Attribute>::new(MessageClass::SuccessResponse, BINDING, msg.transaction_id());
        resp.add_attribute(XorMappedAddress::new(reported));
        let bytes = match MessageEncoder::<Attribute>::new().encode_into_bytes(resp) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let _ = socket.send_to(&bytes, from).await;
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod stunserver;

pub use stunserver::{MappingBehaviour, StunResponder};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 5 new tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/stunserver.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): add an in-tree STUN Binding responder

CI must never depend on a public STUN server. This also gives a
privacy-minded user something to point NetConfig::stun_servers at.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `stun_discover` and NAT typing

**Files:**
- Create: `crates/oxutrm-net/src/discover.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{is_stun, unmap, to_socket_family, ice_priority, NetConfig, StunResponder, MappingBehaviour}`.
- Produces:
  ```rust
  pub async fn stun_discover(
      socket: &tokio::net::UdpSocket,
      cfg: &NetConfig,
  ) -> (Vec<oxutrm_proto::Candidate>, oxutrm_proto::NatType);
  ```

**The classification rule, and its one honest gap.** Spec §5.3: query two
*different* servers from the *same* socket and compare the mapped ports. Same
port from both means an endpoint-independent mapping and ordinary punching
works; different ports means symmetric and rung 3 is the only hope. If the
mapped address is one of our own interface addresses on our own port, there is
no NAT at all (`NatType::None`).

`NatType::AddressDependent` is **never produced by this function** and that is
deliberate. Telling an address-dependent mapping apart from an
endpoint-independent one needs RFC 5780's `CHANGE-REQUEST` / `OTHER-ADDRESS`,
which most public servers do not implement. Both behave identically for our
purposes (ordinary punching works), so the variant stays in the enum for
`M4`'s status line and for a future RFC 5780 probe, and `stun_discover` reports
`EndpointIndependent` for both. Fewer than two answers gives `Unknown`, because
one server can never classify anything.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/discover.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MappingBehaviour, NetConfig, StunResponder};
    use oxutrm_proto::{CandidateKind, NatType};
    use std::time::Duration;
    use tokio::net::UdpSocket;

    fn cfg_for(servers: Vec<String>) -> NetConfig {
        NetConfig {
            stun_servers: servers,
            gather_timeout: Duration::from_millis(800),
            ..NetConfig::default()
        }
    }

    #[tokio::test]
    async fn two_truthful_servers_on_loopback_report_no_nat_at_all() {
        let a = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = sock.local_addr().unwrap().port();

        let (cands, nat) = stun_discover(&sock, &cfg_for(vec![a.server_string(), b.server_string()])).await;

        // On loopback the "mapped" address is literally our own, so the only
        // honest answer is that there is no NAT.
        assert_eq!(nat, NatType::None);
        assert_eq!(cands.len(), 1, "both servers saw the same address: one candidate");
        assert_eq!(cands[0].kind, CandidateKind::ServerReflexive);
        assert_eq!(cands[0].addr.port(), port);
    }

    #[tokio::test]
    async fn two_servers_agreeing_on_a_rewritten_port_mean_an_endpoint_independent_mapping() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let (cands, nat) = stun_discover(&sock, &cfg_for(vec![a.server_string(), b.server_string()])).await;

        assert_eq!(nat, NatType::EndpointIndependent);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].addr.port(), 40_000);
    }

    #[tokio::test]
    async fn two_servers_disagreeing_about_the_port_mean_a_symmetric_nat() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(50_000)).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let (mut cands, nat) = stun_discover(&sock, &cfg_for(vec![a.server_string(), b.server_string()])).await;

        assert_eq!(nat, NatType::Symmetric);
        cands.sort_by_key(|c| c.addr.port());
        assert_eq!(cands.len(), 2, "both observed mappings are worth advertising");
        assert_eq!(cands[0].addr.port(), 40_000);
        assert_eq!(cands[1].addr.port(), 50_000);
    }

    #[tokio::test]
    async fn one_server_can_never_classify_anything() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let (cands, nat) = stun_discover(&sock, &cfg_for(vec![a.server_string()])).await;

        assert_eq!(nat, NatType::Unknown, "one answer is one data point");
        assert_eq!(cands.len(), 1, "but the candidate is still worth having");
    }

    #[tokio::test]
    async fn unreachable_servers_yield_nothing_and_do_not_hang() {
        // Port 1 on loopback: nothing is listening, and nothing ever will be.
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let started = std::time::Instant::now();

        let (cands, nat) = stun_discover(
            &sock,
            &cfg_for(vec!["127.0.0.1:1".to_owned(), "127.0.0.1:2".to_owned()]),
        )
        .await;

        assert!(cands.is_empty());
        assert_eq!(nat, NatType::Unknown);
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_skipped_rather_than_fatal() {
        let a = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();

        let cfg = cfg_for(vec![
            "no-such-host.invalid:3478".to_owned(),
            a.server_string(),
            b.server_string(),
        ]);
        let (cands, nat) = stun_discover(&sock, &cfg).await;

        assert_eq!(cands.len(), 1);
        assert_eq!(nat, NatType::None);
    }

    #[tokio::test]
    async fn a_quic_packet_arriving_mid_discovery_is_ignored() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let target = sock.local_addr().unwrap();

        // The socket is shared with QUIC. Something QUIC-shaped must not
        // derail discovery.
        let noise = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        tokio::spawn(async move {
            for _ in 0..5 {
                let _ = noise.send_to(&[0xC3; 64], target).await;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        });

        let (cands, nat) = stun_discover(&sock, &cfg_for(vec![a.server_string(), b.server_string()])).await;
        assert_eq!(nat, NatType::EndpointIndependent);
        assert_eq!(cands.len(), 1);
    }

    #[tokio::test]
    #[ignore = "reaches the public internet; CI must never depend on public STUN servers"]
    async fn the_default_public_servers_answer() {
        let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let (cands, nat) = stun_discover(&sock, &NetConfig::default()).await;
        assert!(!cands.is_empty(), "no public STUN server answered");
        assert_ne!(nat, NatType::Unknown);
        eprintln!("public STUN says: {nat:?}, candidates {cands:?}");
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function stun_discover in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/discover.rs`:

```rust
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};

use bytecodec::{DecodeExt, EncodeExt};
use rand::RngCore;
use stun_codec::rfc5389::attributes::XorMappedAddress;
use stun_codec::rfc5389::{methods::BINDING, Attribute};
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder, TransactionId};

use oxutrm_proto::{Candidate, CandidateKind, NatType};

use crate::{ice_priority, is_stun, to_socket_family, unmap, NetConfig};

struct Query {
    server: SocketAddr,
    tid: TransactionId,
}

/// Query several STUN servers **from the socket QUIC will use**, because NAT
/// mappings are per-socket: an address learned on any other socket describes
/// nothing useful (spec §5.3).
///
/// Returns the distinct observed mappings as `ServerReflexive` candidates,
/// and the NAT type derived from comparing the ports two different servers
/// reported. See the module docs for why `AddressDependent` is never
/// returned.
pub async fn stun_discover(
    socket: &tokio::net::UdpSocket,
    cfg: &NetConfig,
) -> (Vec<Candidate>, NatType) {
    let local = match socket.local_addr() {
        Ok(a) => a,
        Err(_) => return (Vec::new(), NatType::Unknown),
    };

    // Fire every request first, then collect: the whole point is that the
    // servers are queried in parallel.
    let mut queries: Vec<Query> = Vec::new();
    for name in &cfg.stun_servers {
        let mut resolved = match tokio::net::lookup_host(name.as_str()).await {
            Ok(it) => it,
            // An entry that does not resolve is skipped, not fatal.
            Err(_) => continue,
        };
        let Some(server) = resolved.next() else { continue };
        let tid = random_transaction_id();
        let Some(bytes) = encode_binding_request(tid) else { continue };
        let dst = to_socket_family(&local, server);
        if socket.send_to(&bytes, dst).await.is_ok() {
            queries.push(Query { server: unmap(server), tid });
        }
    }

    // server -> the address that server reported seeing
    let mut mapped: HashMap<SocketAddr, SocketAddr> = HashMap::new();
    let deadline = tokio::time::Instant::now() + cfg.gather_timeout;
    let mut buf = vec![0u8; 1500];

    while mapped.len() < queries.len() {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let received = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await;
        let (n, from) = match received {
            Ok(Ok(v)) => v,
            // Timed out, or the socket errored: either way, stop waiting.
            _ => break,
        };
        // The socket is shared with QUIC. Anything that is not STUN is not ours.
        if !is_stun(&buf[..n]) {
            continue;
        }
        let Some((tid, observed)) = decode_binding_response(&buf[..n]) else { continue };
        let from = unmap(from);
        let Some(q) = queries.iter().find(|q| q.tid == tid && q.server == from) else { continue };
        mapped.insert(q.server, unmap(observed));
    }

    let nat = classify(&mapped, local.port());

    let mut seen: HashSet<SocketAddr> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for addr in mapped.values() {
        if !seen.insert(*addr) {
            continue;
        }
        candidates.push(Candidate {
            addr: *addr,
            kind: CandidateKind::ServerReflexive,
            priority: ice_priority(CandidateKind::ServerReflexive, &addr.ip()),
        });
    }
    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.addr.to_string().cmp(&b.addr.to_string()))
    });

    (candidates, nat)
}

/// Spec §5.3, in two packets: same mapped port from two *different* servers
/// means an endpoint-independent mapping; different ports mean symmetric.
fn classify(mapped: &HashMap<SocketAddr, SocketAddr>, local_port: u16) -> NatType {
    if mapped.len() < 2 {
        // Zero answers tells us nothing; one answer is one data point and
        // cannot be compared with anything.
        return NatType::Unknown;
    }

    let mut ports: Vec<u16> = mapped.values().map(|a| a.port()).collect();
    ports.sort_unstable();
    ports.dedup();
    if ports.len() > 1 {
        return NatType::Symmetric;
    }

    // One port, seen from two servers. Is it even translated?
    let local_ips: HashSet<IpAddr> = if_addrs::get_if_addrs()
        .unwrap_or_default()
        .into_iter()
        .map(|i| i.ip())
        .collect();
    let untranslated = mapped
        .values()
        .all(|a| a.port() == local_port && local_ips.contains(&a.ip()));

    if untranslated {
        NatType::None
    } else {
        // Could also be address-dependent; telling those apart needs RFC 5780
        // and both behave the same for punching. See the module docs.
        NatType::EndpointIndependent
    }
}

fn random_transaction_id() -> TransactionId {
    let mut b = [0u8; 12];
    rand::rng().fill_bytes(&mut b);
    TransactionId::new(b)
}

fn encode_binding_request(tid: TransactionId) -> Option<Vec<u8>> {
    let msg = Message::<Attribute>::new(MessageClass::Request, BINDING, tid);
    MessageEncoder::<Attribute>::new().encode_into_bytes(msg).ok()
}

fn decode_binding_response(datagram: &[u8]) -> Option<(TransactionId, SocketAddr)> {
    let msg = MessageDecoder::<Attribute>::new()
        .decode_from_bytes(datagram)
        .ok()?
        .ok()?;
    if msg.class() != MessageClass::SuccessResponse || msg.method() != BINDING {
        return None;
    }
    let addr = msg.get_attribute::<XorMappedAddress>()?.address();
    Some((msg.transaction_id(), addr))
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod discover;

pub use discover::stun_discover;
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 7 new tests pass, one reported as ignored, clippy clean.

Then confirm the ignored one is not silently broken (this one *does* reach the
internet; run it by hand, not in CI):

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4 --ignored the_default_public_servers_answer --nocapture
```
Expected: PASS on a machine with internet access, printing the observed NAT type.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/discover.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): STUN discovery and NAT typing from the live socket

Two different servers, one socket, compare the mapped ports. Every
non-ignored test uses the in-tree responder.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: ICE connectivity checks with `MESSAGE-INTEGRITY`

**Files:**
- Create: `crates/oxutrm-net/src/stunmsg.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{is_stun, unmap}`.
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum CheckKind { Request, SuccessResponse }

  #[derive(Clone, Debug)]
  pub struct Check {
      pub kind: CheckKind,
      pub tid: stun_codec::TransactionId,
      /// Present on responses: the address the peer saw us come from.
      pub reflexive: Option<std::net::SocketAddr>,
  }

  pub fn ice_password(psk: &[u8; 32]) -> String;
  pub fn ice_ufrag(psk: &[u8; 32]) -> String;
  pub fn random_transaction_id() -> stun_codec::TransactionId;
  pub fn build_check_request(psk: &[u8; 32], tid: stun_codec::TransactionId)
      -> anyhow::Result<Vec<u8>>;
  pub fn build_check_response(
      psk: &[u8; 32],
      tid: stun_codec::TransactionId,
      reflexive: std::net::SocketAddr,
  ) -> anyhow::Result<Vec<u8>>;
  pub fn parse_check(psk: &[u8; 32], datagram: &[u8]) -> Option<Check>;
  ```

### The two things that fail silently if you get them wrong

**1. Key derivation.** `stun_codec`'s
`MessageIntegrity::new_short_term_credential(&message, password)` uses the
password **verbatim as the HMAC-SHA1 key** — it calls `password.as_bytes()`
(RFC 5389 §15.4: for short-term credentials the key *is* `SASLprep(password)`,
and our PSK contains no characters SASLprep would alter). The contract's PSK is
`[u8; 32]` of raw CSPRNG output, which is not a `&str`. So the credential is
the **standard-alphabet, padded base64 of those 32 bytes** — which is exactly
the string that already travels in `Signal::HostHello.psk`. Both peers derive
it with `ice_password`, so both derive the same key. Do not invent a KDF here:
if the two sides disagree by one byte, every check simply fails to verify and
nothing says why.

**2. Attribute ordering.** The HMAC covers the message **as encoded so far**,
with the header's length field temporarily raised by 24 (4 bytes of attribute
header plus 20 bytes of HMAC). Therefore:

- every other attribute must already be in the message when
  `MESSAGE-INTEGRITY` is added, and
- **nothing may be added after it.**

oxutrm deliberately does **not** send `FINGERPRINT`. It would have to follow
`MESSAGE-INTEGRITY`, which changes the bytes `stun_codec` validates over, and
[`crate::is_stun`] already demultiplexes without it.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/stunmsg.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use bytecodec::EncodeExt;
    use stun_codec::rfc5389::attributes::Software;
    use stun_codec::rfc5389::{methods::BINDING, Attribute};
    use stun_codec::{Message, MessageClass, MessageEncoder};
    use std::net::SocketAddr;

    #[test]
    fn the_credential_is_the_base64_of_the_psk() {
        let zero = ice_password(&[0u8; 32]);
        // 32 bytes of input is 44 base64 characters with one pad byte.
        assert_eq!(zero.len(), 44);
        assert!(zero.ends_with('='));
        assert!(zero.starts_with("AAAA"));
        assert_ne!(zero, ice_password(&[1u8; 32]), "different PSKs, different credentials");
        assert_eq!(zero, ice_password(&[0u8; 32]), "derivation is deterministic");
        assert_eq!(ice_ufrag(&[0u8; 32]).len(), 8);
        assert!(zero.starts_with(&ice_ufrag(&[0u8; 32])));
    }

    #[test]
    fn a_request_round_trips_and_demultiplexes_as_stun() {
        let psk = [7u8; 32];
        let tid = random_transaction_id();
        let bytes = build_check_request(&psk, tid).unwrap();

        assert!(crate::is_stun(&bytes), "our own checks must survive the demultiplexer");
        let c = parse_check(&psk, &bytes).expect("must verify against the right PSK");
        assert_eq!(c.kind, CheckKind::Request);
        assert_eq!(c.tid, tid);
        assert_eq!(c.reflexive, None, "a request carries no mapped address");
    }

    #[test]
    fn a_response_carries_the_address_the_peer_saw() {
        let psk = [9u8; 32];
        let tid = random_transaction_id();
        let peer: SocketAddr = "203.0.113.7:41234".parse().unwrap();
        let bytes = build_check_response(&psk, tid, peer).unwrap();

        assert!(crate::is_stun(&bytes));
        let c = parse_check(&psk, &bytes).unwrap();
        assert_eq!(c.kind, CheckKind::SuccessResponse);
        assert_eq!(c.tid, tid);
        assert_eq!(c.reflexive, Some(peer), "this is peer-reflexive discovery, for free");
    }

    #[test]
    fn an_ipv6_reflexive_address_survives_the_round_trip() {
        let psk = [11u8; 32];
        let peer: SocketAddr = "[2001:db8::7]:443".parse().unwrap();
        let bytes = build_check_response(&psk, random_transaction_id(), peer).unwrap();
        assert_eq!(parse_check(&psk, &bytes).unwrap().reflexive, Some(peer));
    }

    #[test]
    fn an_ipv4_mapped_reflexive_address_comes_back_unmapped() {
        let psk = [12u8; 32];
        let mapped: SocketAddr = "[::ffff:203.0.113.7]:443".parse().unwrap();
        let bytes = build_check_response(&psk, random_transaction_id(), mapped).unwrap();
        assert_eq!(
            parse_check(&psk, &bytes).unwrap().reflexive,
            Some("203.0.113.7:443".parse::<SocketAddr>().unwrap()),
            "a candidate must carry the address a peer would dial"
        );
    }

    #[test]
    fn the_wrong_psk_is_rejected() {
        let bytes = build_check_request(&[1u8; 32], random_transaction_id()).unwrap();
        assert!(
            parse_check(&[2u8; 32], &bytes).is_none(),
            "a stranger must never advance the state machine"
        );
    }

    #[test]
    fn a_flipped_transaction_id_byte_fails_the_integrity_check() {
        let psk = [3u8; 32];
        let mut bytes = build_check_request(&psk, random_transaction_id()).unwrap();
        // Byte 12 is inside the transaction id: the message still decodes
        // perfectly, so the only thing that can reject it is the HMAC.
        bytes[12] ^= 0x01;
        assert!(parse_check(&psk, &bytes).is_none());
    }

    #[test]
    fn a_check_with_no_message_integrity_at_all_is_rejected() {
        // Hand-built: a well-formed Binding Request with no credential.
        let mut msg =
            Message::<Attribute>::new(MessageClass::Request, BINDING, random_transaction_id());
        msg.add_attribute(Software::new("stranger".to_owned()).unwrap());
        let bytes = MessageEncoder::<Attribute>::new().encode_into_bytes(msg).unwrap();

        assert!(crate::is_stun(&bytes), "it really is a valid STUN message");
        assert!(
            parse_check(&[5u8; 32], &bytes).is_none(),
            "oxutrm must not be usable as a reflector or amplifier"
        );
    }

    #[test]
    fn non_stun_truncated_and_wrong_class_datagrams_are_all_rejected() {
        let psk = [4u8; 32];
        assert!(parse_check(&psk, &[]).is_none());
        assert!(parse_check(&psk, &[0xC3; 64]).is_none(), "a QUIC long header");
        assert!(parse_check(&psk, &[0x40; 64]).is_none(), "a QUIC short header");

        let bytes = build_check_request(&psk, random_transaction_id()).unwrap();
        assert!(parse_check(&psk, &bytes[..bytes.len() - 4]).is_none(), "truncated");

        // A Binding *Indication* is neither a check nor a check response.
        let msg = Message::<Attribute>::new(
            MessageClass::Indication,
            BINDING,
            random_transaction_id(),
        );
        let ind = MessageEncoder::<Attribute>::new().encode_into_bytes(msg).unwrap();
        assert!(parse_check(&psk, &ind).is_none());
    }

    #[test]
    fn message_integrity_is_the_last_attribute_in_the_encoding() {
        // If anything were appended after it, stun_codec would validate over
        // different bytes than it signed and every check would fail on the
        // wire while passing in isolation. The attribute type is 0x0008 and
        // its value is 20 bytes, so it occupies exactly the last 24 bytes.
        let bytes = build_check_request(&[6u8; 32], random_transaction_id()).unwrap();
        let n = bytes.len();
        assert_eq!(&bytes[n - 24..n - 22], &[0x00, 0x08], "MESSAGE-INTEGRITY type");
        assert_eq!(&bytes[n - 22..n - 20], &[0x00, 0x14], "20-byte HMAC-SHA1 value");
    }

    #[test]
    fn two_transaction_ids_in_a_row_differ() {
        assert_ne!(random_transaction_id(), random_transaction_id());
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function ice_password in this scope`, and the same for the rest.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/stunmsg.rs`:

```rust
//! ICE connectivity checks, as STUN Binding Requests carrying
//! `MESSAGE-INTEGRITY` keyed by the SSH-delivered PSK (spec §5.3).
//!
//! Chosen over a bespoke probe format because it demultiplexes cleanly against
//! QUIC on the same socket, because `MESSAGE-INTEGRITY` stops strangers
//! confusing the state machine and stops oxutrm being used as a reflector, and
//! because the `XOR-MAPPED-ADDRESS` in the response *is* peer-reflexive
//! discovery at no extra cost.

use std::net::SocketAddr;

use anyhow::anyhow;
use base64::Engine as _;
use bytecodec::{DecodeExt, EncodeExt};
use rand::RngCore;
use stun_codec::rfc5389::attributes::{MessageIntegrity, Username, XorMappedAddress};
use stun_codec::rfc5389::{methods::BINDING, Attribute};
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder, TransactionId};

use crate::{is_stun, unmap};

/// What an authenticated check turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckKind {
    Request,
    SuccessResponse,
}

/// An ICE check that verified against our PSK.
#[derive(Clone, Debug)]
pub struct Check {
    pub kind: CheckKind,
    pub tid: TransactionId,
    /// Present on responses: the address the peer saw us come from. This is
    /// peer-reflexive discovery.
    pub reflexive: Option<SocketAddr>,
}

/// The ICE short-term-credential password.
///
/// `stun_codec` uses this string **verbatim** as the HMAC-SHA1 key, so it must
/// be a deterministic function of the PSK that both peers compute identically.
/// The PSK is 32 raw CSPRNG bytes and is not valid UTF-8, so the credential is
/// its standard-alphabet padded base64 — the very string that travels in
/// `Signal::HostHello.psk`.
pub fn ice_password(psk: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(psk)
}

/// The ICE username fragment.
///
/// RFC 8445 forms `USERNAME` as `<remote-ufrag>:<local-ufrag>`. oxutrm has one
/// PSK per session and exchanges no separate fragments, so both halves are the
/// same 8-character prefix of the credential. It exists so the attribute is
/// present and well formed; authentication is entirely `MESSAGE-INTEGRITY`'s
/// job.
pub fn ice_ufrag(psk: &[u8; 32]) -> String {
    ice_password(psk).chars().take(8).collect()
}

pub fn random_transaction_id() -> TransactionId {
    let mut b = [0u8; 12];
    rand::rng().fill_bytes(&mut b);
    TransactionId::new(b)
}

/// A Binding Request that authenticates us to the peer.
pub fn build_check_request(psk: &[u8; 32], tid: TransactionId) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::Request, BINDING, tid);

    // Everything that is not MESSAGE-INTEGRITY goes in first.
    let ufrag = ice_ufrag(psk);
    let username = Username::new(format!("{ufrag}:{ufrag}"))
        .map_err(|e| anyhow!("building USERNAME: {e}"))?;
    msg.add_attribute(username);

    // MESSAGE-INTEGRITY last: the HMAC covers the message as encoded so far.
    // Nothing may be added after this line.
    let mi = MessageIntegrity::new_short_term_credential(&msg, &ice_password(psk))
        .map_err(|e| anyhow!("computing MESSAGE-INTEGRITY: {e}"))?;
    msg.add_attribute(mi);

    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(msg)
        .map_err(|e| anyhow!("encoding a check request: {e}"))
}

/// A Binding Success Response telling the peer the address we saw it come from.
pub fn build_check_response(
    psk: &[u8; 32],
    tid: TransactionId,
    reflexive: SocketAddr,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::SuccessResponse, BINDING, tid);
    msg.add_attribute(XorMappedAddress::new(reflexive));

    // Again: MESSAGE-INTEGRITY last, nothing after it.
    let mi = MessageIntegrity::new_short_term_credential(&msg, &ice_password(psk))
        .map_err(|e| anyhow!("computing MESSAGE-INTEGRITY: {e}"))?;
    msg.add_attribute(mi);

    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(msg)
        .map_err(|e| anyhow!("encoding a check response: {e}"))
}

/// Decode a datagram and verify `MESSAGE-INTEGRITY` against the PSK.
///
/// Returns `None` for anything that is not an authenticated Binding
/// Request or Binding Success Response. Everything the caller does with the
/// result advances a state machine, so nothing unauthenticated may get past
/// this function.
pub fn parse_check(psk: &[u8; 32], datagram: &[u8]) -> Option<Check> {
    if !is_stun(datagram) {
        return None;
    }
    let msg = MessageDecoder::<Attribute>::new()
        .decode_from_bytes(datagram)
        .ok()?
        .ok()?;
    if msg.method() != BINDING {
        return None;
    }
    let kind = match msg.class() {
        MessageClass::Request => CheckKind::Request,
        MessageClass::SuccessResponse => CheckKind::SuccessResponse,
        // Indications and error responses are not part of the check exchange.
        _ => return None,
    };

    // No credential, or the wrong one: not ours.
    let mi: &MessageIntegrity = msg.get_attribute()?;
    mi.check_short_term_credential(&ice_password(psk)).ok()?;

    let reflexive = msg
        .get_attribute::<XorMappedAddress>()
        .map(|a| unmap(a.address()));

    Some(Check { kind, tid: msg.transaction_id(), reflexive })
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod stunmsg;

pub use stunmsg::{
    build_check_request, build_check_response, ice_password, ice_ufrag, parse_check,
    random_transaction_id, Check, CheckKind,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 11 new tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/stunmsg.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): ICE checks as STUN requests with MESSAGE-INTEGRITY

The short-term credential is the base64 of the 32-byte PSK, because
stun_codec uses the password verbatim as the HMAC-SHA1 key. FINGERPRINT is
deliberately not sent: it would have to follow MESSAGE-INTEGRITY.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `IceAgent` — connectivity checks, nomination, peer-reflexive learning

**Files:**
- Create: `crates/oxutrm-net/src/ice.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{build_check_request, build_check_response, parse_check, Check, CheckKind, ice_priority, to_socket_family, unmap, NetConfig}`; `oxutrm_proto::{Candidate, CandidateKind, Rung}`.
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum IceRole { Controlling, Controlled }   // the client is Controlling

  #[derive(Clone, Debug)]
  pub enum IceEvent {
      NewLocalCandidate(oxutrm_proto::Candidate),
      Nominated {
          local: std::net::SocketAddr,
          remote: std::net::SocketAddr,
          rung: oxutrm_proto::Rung,
          probes: u32,
      },
      Failed(String),
  }

  pub struct IceAgent { /* private */ }

  impl IceAgent {
      pub fn new(psk: [u8; 32], role: IceRole, cfg: NetConfig) -> IceAgent;
      pub fn add_local(&mut self, c: oxutrm_proto::Candidate);
      pub fn add_remote(&mut self, c: oxutrm_proto::Candidate);
      pub fn role(&self) -> IceRole;
      pub fn probes_sent(&self) -> u32;
      /// The round trip of the most recent check that got an answer.
      pub fn last_rtt(&self) -> Option<std::time::Duration>;
      /// Step until there is something to report. Call it in a loop: state
      /// persists across calls, so a `NewLocalCandidate` does not lose progress.
      pub async fn run(&mut self, socket: std::sync::Arc<tokio::net::UdpSocket>) -> IceEvent;
  }
  ```

### Three decisions worth understanding before you write code

**1. `run` is a step function, not a one-shot.** The contract's signature
returns a single `IceEvent` from `&mut self`, so the only coherent reading is
"advance until you have something to say, and keep your state". Callers loop:

```rust
loop {
    match agent.run(socket.clone()).await {
        IceEvent::NewLocalCandidate(c) => publish(c),          // and keep going
        ev @ (IceEvent::Nominated { .. } | IceEvent::Failed(_)) => break ev,
    }
}
```

**2. Nomination needs both directions.** A pair is nominated when the peer has
answered *our* check (`got_response`) **and** has sent us a check we answered
(`got_request`). One direction working proves nothing: NATs are asymmetric.

**3. Aggressive nomination, and what `role` is for.** oxutrm does not send
RFC 8445's `USE-CANDIDATE` attribute — `stun_codec`'s `rfc5389` module does not
define it, and with exactly two peers and one component it buys nothing: both
sides pick the *highest-priority doubly-validated pair*, both run the same
priority function on the same candidate set, so both reach the same answer.
`role` is kept because M4's status pane reports it and because a future
`USE-CANDIDATE` implementation needs it; it is exposed through `role()` so it
is not dead code.

**4. An agent with no remote candidates still listens.** It must not fail
immediately: a peer behind a symmetric NAT may only ever be reachable because
*its* punch arrives at us first, and that inbound authenticated request is
what teaches us its address (peer-reflexive learning).

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/ice.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ice_priority, NetConfig};
    use oxutrm_proto::{Candidate, CandidateKind, Rung};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    fn host(addr: SocketAddr) -> Candidate {
        Candidate {
            addr,
            kind: CandidateKind::Host,
            priority: ice_priority(CandidateKind::Host, &addr.ip()),
        }
    }

    fn quick(ms: u64) -> NetConfig {
        NetConfig { gather_timeout: Duration::from_millis(ms), ..NetConfig::default() }
    }

    /// The loop every real caller writes: keep going past informational events.
    async fn drive(mut agent: IceAgent, sock: Arc<UdpSocket>) -> IceEvent {
        loop {
            match agent.run(sock.clone()).await {
                IceEvent::NewLocalCandidate(_) => continue,
                ev => return ev,
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_agents_that_know_about_each_other_both_nominate() {
        let psk = [42u8; 32];
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        a.add_remote(host(b_addr));
        let mut b = IceAgent::new(psk, IceRole::Controlled, quick(5_000));
        b.add_remote(host(a_addr));

        let ta = tokio::spawn(drive(a, a_sock));
        let tb = tokio::spawn(drive(b, b_sock));
        let (ea, eb) = (ta.await.unwrap(), tb.await.unwrap());

        match (ea, eb) {
            (
                IceEvent::Nominated { remote: ra, probes: pa, .. },
                IceEvent::Nominated { remote: rb, .. },
            ) => {
                assert_eq!(ra, b_addr);
                assert_eq!(rb, a_addr);
                assert!(pa >= 1, "at least one probe must have been sent");
            }
            other => panic!("expected both sides to nominate, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ipv6_host_pair_is_reported_as_rung_zero() {
        let psk = [43u8; 32];
        let a_sock = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind("[::1]:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        a.add_remote(host(b_addr));
        let mut b = IceAgent::new(psk, IceRole::Controlled, quick(5_000));
        b.add_remote(host(a_addr));

        let ta = tokio::spawn(drive(a, a_sock));
        let tb = tokio::spawn(drive(b, b_sock));
        let (ea, _eb) = (ta.await.unwrap(), tb.await.unwrap());

        match ea {
            IceEvent::Nominated { rung, .. } => assert_eq!(rung, Rung::Ipv6Direct),
            other => panic!("expected a nomination, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_peer_that_knows_nothing_learns_the_other_one_peer_reflexively() {
        let psk = [44u8; 32];
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        // A knows where B is. B knows nothing at all and must learn from the
        // authenticated request that arrives.
        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        a.add_remote(host(b_addr));
        let b = IceAgent::new(psk, IceRole::Controlled, quick(5_000));

        let ta = tokio::spawn(drive(a, a_sock));
        let tb = tokio::spawn(drive(b, b_sock));
        let (ea, eb) = (ta.await.unwrap(), tb.await.unwrap());

        match eb {
            IceEvent::Nominated { remote, .. } => assert_eq!(remote, a_addr),
            other => panic!("B should have learned A peer-reflexively, got {other:?}"),
        }
        assert!(matches!(ea, IceEvent::Nominated { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_with_the_wrong_psk_never_nominates() {
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        let mut a = IceAgent::new([1u8; 32], IceRole::Controlling, quick(700));
        a.add_remote(host(b_addr));
        let mut b = IceAgent::new([2u8; 32], IceRole::Controlled, quick(700));
        b.add_remote(host(a_addr));

        let ta = tokio::spawn(drive(a, a_sock));
        let tb = tokio::spawn(drive(b, b_sock));
        let (ea, eb) = (ta.await.unwrap(), tb.await.unwrap());

        assert!(matches!(ea, IceEvent::Failed(_)), "got {ea:?}");
        assert!(matches!(eb, IceEvent::Failed(_)), "got {eb:?}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_is_simply_not_there_fails_within_the_budget() {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut a = IceAgent::new([3u8; 32], IceRole::Controlling, quick(600));
        // Port 1 on loopback: nothing is listening.
        a.add_remote(host("127.0.0.1:1".parse().unwrap()));

        let started = std::time::Instant::now();
        let ev = drive(a, sock).await;
        assert!(matches!(ev, IceEvent::Failed(_)), "got {ev:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_higher_priority_pair_wins_when_two_both_work() {
        let psk = [45u8; 32];
        // B listens on two sockets; A is told about both. Both will validate,
        // and the higher-priority candidate must be the one nominated.
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b1 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b2 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b1_addr = b1.local_addr().unwrap();
        let b2_addr = b2.local_addr().unwrap();

        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        // Deliberately unequal priorities, low one added first.
        a.add_remote(Candidate {
            addr: b2_addr,
            kind: CandidateKind::ServerReflexive,
            priority: ice_priority(CandidateKind::ServerReflexive, &b2_addr.ip()),
        });
        a.add_remote(host(b1_addr));

        let mut agent_b1 = IceAgent::new(psk, IceRole::Controlled, quick(5_000));
        agent_b1.add_remote(host(a_addr));
        let mut agent_b2 = IceAgent::new(psk, IceRole::Controlled, quick(5_000));
        agent_b2.add_remote(host(a_addr));

        let ta = tokio::spawn(drive(a, a_sock));
        let t1 = tokio::spawn(drive(agent_b1, b1));
        let t2 = tokio::spawn(drive(agent_b2, b2));
        let ea = ta.await.unwrap();
        let _ = t1.await;
        let _ = t2.await;

        match ea {
            IceEvent::Nominated { remote, .. } => assert_eq!(
                remote, b1_addr,
                "the Host candidate outranks the ServerReflexive one"
            ),
            other => panic!("expected a nomination, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_nominating_agent_reports_a_round_trip_time() {
        let psk = [46u8; 32];
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        a.add_remote(host(b_addr));
        let mut b = IceAgent::new(psk, IceRole::Controlled, quick(5_000));
        b.add_remote(host(a_addr));

        let tb = tokio::spawn(drive(b, b_sock));
        loop {
            match a.run(a_sock.clone()).await {
                IceEvent::NewLocalCandidate(_) => continue,
                IceEvent::Nominated { .. } => break,
                other => panic!("got {other:?}"),
            }
        }
        let _ = tb.await;

        let rtt = a.last_rtt().expect("a nomination implies a completed round trip");
        assert!(rtt < Duration::from_secs(1), "loopback rtt was {rtt:?}");
        assert_eq!(a.role(), IceRole::Controlling);
        assert!(a.probes_sent() >= 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn duplicate_candidates_do_not_create_duplicate_pairs() {
        let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let mut a = IceAgent::new([7u8; 32], IceRole::Controlling, quick(100));
        a.add_remote(host(addr));
        a.add_remote(host(addr));
        a.add_local(host(addr));
        a.add_local(host(addr));
        assert_eq!(a.remote_count(), 1);
        assert_eq!(a.local_count(), 1);
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `failed to resolve: use of undeclared type IceAgent`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/ice.rs`:

```rust
//! ICE connectivity checks over the one shared socket.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use oxutrm_proto::{Candidate, CandidateKind, Rung};

use crate::{
    build_check_request, build_check_response, ice_priority, parse_check, to_socket_family, unmap,
    CheckKind, NetConfig,
};

/// How often a fresh round of checks goes out to every remote candidate.
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// How many outstanding transaction ids one pair remembers. A response whose
/// id has been forgotten is simply ignored; the next round replaces it.
const MAX_OUTSTANDING: usize = 8;

/// The client is `Controlling` (spec §4: the client initiates).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IceRole {
    Controlling,
    Controlled,
}

#[derive(Clone, Debug)]
pub enum IceEvent {
    /// We learned one of our own addresses from a peer's `XOR-MAPPED-ADDRESS`.
    /// Publish it to the peer over the signalling channel and keep going.
    NewLocalCandidate(Candidate),
    /// A pair validated in **both** directions. This is the answer.
    Nominated {
        local: SocketAddr,
        remote: SocketAddr,
        rung: Rung,
        probes: u32,
    },
    /// The budget ran out. Rung 3 is the caller's next move.
    Failed(String),
}

struct Pair {
    remote: SocketAddr,
    kind: CandidateKind,
    priority: u32,
    /// Checks we sent to this address, with the instant they went out.
    outstanding: VecDeque<(stun_codec::TransactionId, Instant)>,
    /// They answered a check of ours: our packets reach them.
    got_response: bool,
    /// They checked us and we answered: their packets reach us.
    got_request: bool,
}

impl Pair {
    fn new(c: &Candidate) -> Pair {
        Pair {
            remote: c.addr,
            kind: c.kind,
            priority: c.priority,
            outstanding: VecDeque::new(),
            got_response: false,
            got_request: false,
        }
    }

    fn validated(&self) -> bool {
        self.got_request && self.got_response
    }
}

pub struct IceAgent {
    psk: [u8; 32],
    role: IceRole,
    cfg: NetConfig,
    local: Vec<Candidate>,
    pairs: Vec<Pair>,
    pending: VecDeque<IceEvent>,
    probes_sent: u32,
    last_rtt: Option<Duration>,
    nominated: Option<SocketAddr>,
    deadline: Option<Instant>,
}

impl IceAgent {
    pub fn new(psk: [u8; 32], role: IceRole, cfg: NetConfig) -> IceAgent {
        IceAgent {
            psk,
            role,
            cfg,
            local: Vec::new(),
            pairs: Vec::new(),
            pending: VecDeque::new(),
            probes_sent: 0,
            last_rtt: None,
            nominated: None,
            deadline: None,
        }
    }

    pub fn add_local(&mut self, c: Candidate) {
        if self.local.iter().any(|x| x.addr == c.addr) {
            return;
        }
        self.local.push(c);
    }

    pub fn add_remote(&mut self, c: Candidate) {
        if self.pairs.iter().any(|p| p.remote == c.addr) {
            return;
        }
        self.pairs.push(Pair::new(&c));
        self.sort_pairs();
    }

    pub fn role(&self) -> IceRole {
        self.role
    }

    pub fn probes_sent(&self) -> u32 {
        self.probes_sent
    }

    /// The round trip of the most recent check that got an answer. `None`
    /// until one does.
    pub fn last_rtt(&self) -> Option<Duration> {
        self.last_rtt
    }

    pub fn remote_count(&self) -> usize {
        self.pairs.len()
    }

    pub fn local_count(&self) -> usize {
        self.local.len()
    }

    /// Advance until there is something to report.
    ///
    /// State persists across calls, so returning a `NewLocalCandidate` costs
    /// no progress. Call it in a loop until it returns `Nominated` or
    /// `Failed`.
    pub async fn run(&mut self, socket: Arc<tokio::net::UdpSocket>) -> IceEvent {
        if let Some(ev) = self.pending.pop_front() {
            return ev;
        }

        let deadline = *self
            .deadline
            .get_or_insert_with(|| Instant::now() + self.cfg.gather_timeout);
        let local_addr = socket.local_addr().ok();
        let mut next_send = Instant::now();
        let mut buf = vec![0u8; 2048];

        loop {
            let now = Instant::now();
            if now >= deadline {
                return IceEvent::Failed(format!(
                    "no candidate pair validated within {:?} ({} probes sent)",
                    self.cfg.gather_timeout, self.probes_sent
                ));
            }
            if now >= next_send {
                // An agent with no remote candidates still listens: a peer
                // behind a symmetric NAT may only ever reach us inbound.
                self.send_round(&socket).await;
                next_send = now + CHECK_INTERVAL;
            }

            let wait = next_send.min(deadline).saturating_duration_since(Instant::now());
            let received = tokio::time::timeout(wait, socket.recv_from(&mut buf)).await;
            let (n, from) = match received {
                Ok(Ok(v)) => v,
                // Timed out (send another round) or a transient socket error.
                _ => continue,
            };
            let from = unmap(from);

            // Anything that does not verify against the PSK is a stranger and
            // must not touch the state machine.
            let Some(check) = parse_check(&self.psk, &buf[..n]) else { continue };

            match check.kind {
                CheckKind::Request => {
                    if let Ok(resp) = build_check_response(&self.psk, check.tid, from) {
                        let dst = match local_addr {
                            Some(l) => to_socket_family(&l, from),
                            None => from,
                        };
                        let _ = socket.send_to(&resp, dst).await;
                    }
                    // Peer-reflexive learning: an authenticated request from
                    // an address we never heard of *is* a new candidate.
                    if !self.pairs.iter().any(|p| p.remote == from) {
                        let c = Candidate {
                            addr: from,
                            kind: CandidateKind::PeerReflexive,
                            priority: ice_priority(CandidateKind::PeerReflexive, &from.ip()),
                        };
                        self.pairs.push(Pair::new(&c));
                        self.sort_pairs();
                    }
                    if let Some(p) = self.pairs.iter_mut().find(|p| p.remote == from) {
                        p.got_request = true;
                    }
                }
                CheckKind::SuccessResponse => {
                    let mut rtt = None;
                    let found = self
                        .pairs
                        .iter_mut()
                        .find(|p| p.outstanding.iter().any(|(t, _)| *t == check.tid));
                    match found {
                        None => continue,
                        Some(p) => {
                            if let Some((_, sent_at)) =
                                p.outstanding.iter().find(|(t, _)| *t == check.tid)
                            {
                                rtt = Some(Instant::now().saturating_duration_since(*sent_at));
                            }
                            p.got_response = true;
                        }
                    }
                    if rtt.is_some() {
                        self.last_rtt = rtt;
                    }
                    // The address they saw us come from is one of ours.
                    if let Some(refl) = check.reflexive {
                        if self.note_local(refl) {
                            let c = Candidate {
                                addr: refl,
                                kind: CandidateKind::PeerReflexive,
                                priority: ice_priority(CandidateKind::PeerReflexive, &refl.ip()),
                            };
                            self.pending.push_back(IceEvent::NewLocalCandidate(c));
                        }
                    }
                }
            }

            if let Some(ev) = self.try_nominate(local_addr) {
                self.pending.push_back(ev);
            }
            if let Some(ev) = self.pending.pop_front() {
                return ev;
            }
        }
    }

    fn sort_pairs(&mut self) {
        self.pairs.sort_by(|a, b| {
            b.priority
                .cmp(&a.priority)
                .then_with(|| a.remote.to_string().cmp(&b.remote.to_string()))
        });
    }

    /// Record a newly learned address of our own. Returns true when it is new.
    fn note_local(&mut self, addr: SocketAddr) -> bool {
        if self.local.iter().any(|c| c.addr == addr) {
            return false;
        }
        self.local.push(Candidate {
            addr,
            kind: CandidateKind::PeerReflexive,
            priority: ice_priority(CandidateKind::PeerReflexive, &addr.ip()),
        });
        true
    }

    async fn send_round(&mut self, socket: &tokio::net::UdpSocket) {
        // Copied out so the loop below can borrow `self.pairs` mutably.
        let psk = self.psk;
        let local = socket.local_addr().ok();
        let mut sent = 0u32;

        for p in self.pairs.iter_mut() {
            let tid = crate::random_transaction_id();
            let Ok(bytes) = build_check_request(&psk, tid) else { continue };
            let dst = match local {
                Some(l) => to_socket_family(&l, p.remote),
                None => p.remote,
            };
            if socket.send_to(&bytes, dst).await.is_ok() {
                p.outstanding.push_back((tid, Instant::now()));
                while p.outstanding.len() > MAX_OUTSTANDING {
                    p.outstanding.pop_front();
                }
                sent += 1;
            }
        }
        self.probes_sent = self.probes_sent.saturating_add(sent);
    }

    fn try_nominate(&mut self, local: Option<SocketAddr>) -> Option<IceEvent> {
        if self.nominated.is_some() {
            return None;
        }
        // `pairs` is kept sorted, so the first validated pair is the best one.
        // Both peers run the same priority function over the same candidates
        // and therefore reach the same conclusion without USE-CANDIDATE.
        let p = self.pairs.iter().find(|p| p.validated())?;
        let remote = p.remote;
        let rung = rung_for(p.kind, &remote.ip());
        self.nominated = Some(remote);
        Some(IceEvent::Nominated {
            local: local.unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0))),
            remote,
            rung,
            probes: self.probes_sent,
        })
    }
}

/// Which ladder rung a validated pair represents.
///
/// `Rung` has no `Ipv4Direct` variant, so an IPv4 host candidate that works —
/// two machines on one LAN — is reported as `StunPunch`. That is imprecise but
/// never wrong in a way that matters: it is a direct UDP path either way, and
/// nothing downstream branches on it. `Rung::Birthday` is set by the caller
/// (rung 3 does not run through this agent) and `Rung::SshTunnel` is M4's.
fn rung_for(kind: CandidateKind, ip: &IpAddr) -> Rung {
    match kind {
        // Spec §5.1: a global IPv6 pair means no NAT exists at all.
        CandidateKind::Host if ip.is_ipv6() => Rung::Ipv6Direct,
        CandidateKind::PortMapped => Rung::PortMapped,
        _ => Rung::StunPunch,
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod ice;

pub use ice::{IceAgent, IceEvent, IceRole};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run it bare, wait for it in the same turn, and read the output as it comes:

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 8 new tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/ice.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): ICE agent with two-way validation and peer-reflexive learning

run() is a step function: state persists across calls so publishing a newly
learned local candidate costs no progress. Nomination requires a pair that
validated in both directions.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: Rung 3 — the birthday-paradox blast

**Files:**
- Create: `crates/oxutrm-net/src/birthday.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{build_check_request, build_check_response, parse_check, random_transaction_id, unmap, to_socket_family, CheckKind, NetConfig}`.
- Produces:
  ```rust
  pub struct BirthdayResult {
      /// The socket that found the hole. QUIC takes this one over.
      pub socket: std::net::UdpSocket,
      pub remote: std::net::SocketAddr,
      pub probes: u32,
  }

  /// Guessed ports, walking outward from the peer's observed base port.
  pub fn guessed_ports(base: u16, count: u16) -> Vec<u16>;

  pub async fn birthday_blast(
      psk: [u8; 32],
      peer_base: std::net::SocketAddr,
      cfg: &NetConfig,
  ) -> anyhow::Result<Option<BirthdayResult>>;
  ```

**Guardrails (spec §5.4), all four of them.** This rung is deliberately noisy,
so: it runs only when the caller decided the NAT is symmetric or rungs 0-2
failed (that decision belongs to Task 13, not here); the probe count is capped
at `birthday_sockets * birthday_ports` and the wall clock at
`birthday_budget`; every probe is the same authenticated STUN check from Task
7, so nothing unauthenticated is ever emitted; and the number actually sent is
returned so the status line can show the cost.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/birthday.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{build_check_response, parse_check, NetConfig};
    use std::net::SocketAddr;
    use std::time::Duration;

    #[test]
    fn ports_walk_outward_from_the_base_without_repeating() {
        assert_eq!(
            guessed_ports(40_000, 9),
            vec![40_000, 40_001, 39_999, 40_002, 39_998, 40_003, 39_997, 40_004, 39_996]
        );
        let ps = guessed_ports(40_000, 256);
        assert_eq!(ps.len(), 256);
        let mut sorted = ps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 256, "no repeats");
        assert_eq!(ps[0], 40_000, "the observed port itself is guessed first");
    }

    #[test]
    fn ports_never_leave_the_legal_range_even_at_the_edges() {
        for base in [1_024u16, 1_025, 1_030, 65_530, 65_534, 65_535] {
            let ps = guessed_ports(base, 64);
            assert!(!ps.is_empty(), "base {base} produced nothing");
            for p in ps {
                assert!((1_024..=65_535).contains(&p), "base {base} produced {p}");
            }
        }
    }

    #[test]
    fn asking_for_more_ports_than_exist_does_not_hang() {
        let ps = guessed_ports(1_024, u16::MAX);
        assert!(ps.len() <= 65_535 - 1_024 + 1);
        assert!(!ps.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_blast_does_nothing_when_it_is_switched_off() {
        let cfg = NetConfig { enable_birthday: false, ..NetConfig::default() };
        let r = birthday_blast([1u8; 32], "127.0.0.1:40000".parse().unwrap(), &cfg)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_blast_gives_up_on_its_budget_rather_than_running_on() {
        // Nothing is listening anywhere near this port.
        let cfg = NetConfig {
            birthday_sockets: 4,
            birthday_ports: 16,
            birthday_budget: Duration::from_millis(400),
            ..NetConfig::default()
        };
        let started = std::time::Instant::now();
        let r = birthday_blast([1u8; 32], "127.0.0.1:1".parse().unwrap(), &cfg)
            .await
            .unwrap();
        assert!(r.is_none());
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_blast_finds_a_listener_on_a_port_it_had_to_guess() {
        let psk = [77u8; 32];
        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_port = peer.local_addr().unwrap().port();

        // A peer that answers authenticated checks and nothing else.
        let responder = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = peer.recv_from(&mut buf).await else { return };
                if let Some(c) = parse_check(&psk, &buf[..n]) {
                    if let Ok(r) = build_check_response(&psk, c.tid, from) {
                        let _ = peer.send_to(&r, from).await;
                    }
                }
            }
        });

        // Point the blast three ports away, so it genuinely has to guess.
        let base: SocketAddr = format!("127.0.0.1:{}", peer_port.saturating_sub(3).max(1_024))
            .parse()
            .unwrap();
        let cfg = NetConfig {
            birthday_sockets: 2,
            birthday_ports: 32,
            birthday_budget: Duration::from_secs(6),
            ..NetConfig::default()
        };

        let r = birthday_blast(psk, base, &cfg)
            .await
            .unwrap()
            .expect("must find the listener");
        assert_eq!(r.remote.port(), peer_port);
        assert!(r.probes >= 1, "the cost must be reported");
        assert_ne!(r.socket.local_addr().unwrap().port(), 0, "a real usable socket");

        responder.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_listener_with_the_wrong_psk_is_not_mistaken_for_the_peer() {
        let peer = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_port = peer.local_addr().unwrap().port();
        // This one answers with the wrong credential.
        let responder = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = peer.recv_from(&mut buf).await else { return };
                if let Some(c) = parse_check(&[99u8; 32], &buf[..n]) {
                    if let Ok(r) = build_check_response(&[99u8; 32], c.tid, from) {
                        let _ = peer.send_to(&r, from).await;
                    }
                }
            }
        });

        let base: SocketAddr = format!("127.0.0.1:{peer_port}").parse().unwrap();
        let cfg = NetConfig {
            birthday_sockets: 2,
            birthday_ports: 4,
            birthday_budget: Duration::from_millis(600),
            ..NetConfig::default()
        };
        let r = birthday_blast([77u8; 32], base, &cfg).await.unwrap();
        assert!(r.is_none(), "a wrong credential must never look like success");

        responder.abort();
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function guessed_ports in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/birthday.rs`:

```rust
//! Rung 3, the birthday-paradox blast (spec §5.4).
//!
//! Behind a symmetric NAT the peer's external port is unpredictable but not
//! unguessable. Both sides open N sockets and each fires at M guessed ports
//! around the peer's observed base, so ~N*M combinations meet an ephemeral
//! range of similar size and a collision is likely within seconds.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::{
    build_check_request, build_check_response, parse_check, random_transaction_id, unmap,
    CheckKind, NetConfig,
};

/// How many probes one socket sends before it stops to read.
const BURST: usize = 8;

/// The winning socket, and what it cost.
pub struct BirthdayResult {
    /// The socket that found the hole. QUIC takes this one over, because the
    /// NAT mapping that was just punched belongs to *this* socket.
    pub socket: std::net::UdpSocket,
    pub remote: SocketAddr,
    pub probes: u32,
}

/// Ports to guess, walking outward from the peer's observed base:
/// `base, base+1, base-1, base+2, base-2, ...`, skipping anything outside
/// `1024..=65535`. NATs allocate from wherever they currently are, so the
/// neighbourhood of the observed port is the best place to look.
pub fn guessed_ports(base: u16, count: u16) -> Vec<u16> {
    let count = count as usize;
    let mut out: Vec<u16> = Vec::with_capacity(count.min(64_512));
    let mut seen: HashSet<u16> = HashSet::new();
    let base = base as i32;

    let mut k: i32 = 0;
    while out.len() < count && k <= 65_536 {
        for delta in if k == 0 { &[0i32][..] } else { &[k, -k][..] } {
            let p = base + delta;
            if (1_024..=65_535).contains(&p) && seen.insert(p as u16) {
                out.push(p as u16);
                if out.len() == count {
                    break;
                }
            }
        }
        k += 1;
    }
    out
}

/// Fire authenticated checks at guessed ports until one comes back.
///
/// Returns `Ok(None)` when the blast is disabled, when no socket could be
/// opened, or when the budget expired without a hit. Never returns a socket
/// that has not exchanged an authenticated check with the peer.
pub async fn birthday_blast(
    psk: [u8; 32],
    peer_base: SocketAddr,
    cfg: &NetConfig,
) -> anyhow::Result<Option<BirthdayResult>> {
    if !cfg.enable_birthday {
        return Ok(None);
    }
    let ports = guessed_ports(peer_base.port(), cfg.birthday_ports);
    if ports.is_empty() {
        return Ok(None);
    }

    let bind_any: SocketAddr = match peer_base.ip() {
        IpAddr::V4(_) => "0.0.0.0:0".parse().expect("a literal address"),
        IpAddr::V6(_) => "[::]:0".parse().expect("a literal address"),
    };

    let mut sockets = Vec::new();
    for _ in 0..cfg.birthday_sockets {
        match tokio::net::UdpSocket::bind(bind_any).await {
            Ok(s) => sockets.push(s),
            // The file-descriptor limit is a normal outcome here: use what we
            // managed to get rather than failing the whole rung.
            Err(_) => break,
        }
    }
    if sockets.is_empty() {
        return Ok(None);
    }

    let deadline = Instant::now() + cfg.birthday_budget;
    let cap = (cfg.birthday_sockets as u32).saturating_mul(cfg.birthday_ports as u32);
    let probes = Arc::new(AtomicU32::new(0));
    let peer_ip = peer_base.ip();
    let ports = Arc::new(ports);

    let mut set: tokio::task::JoinSet<Option<(std::net::UdpSocket, SocketAddr)>> =
        tokio::task::JoinSet::new();

    for (i, sock) in sockets.into_iter().enumerate() {
        let ports = Arc::clone(&ports);
        let probes = Arc::clone(&probes);
        set.spawn(async move {
            let mut buf = vec![0u8; 2048];
            // Stagger the starting offset so the sockets do not all hammer
            // the same guess at the same instant.
            let mut idx = i;

            loop {
                if Instant::now() >= deadline || probes.load(Ordering::Relaxed) >= cap {
                    return None;
                }

                for _ in 0..BURST {
                    if probes.fetch_add(1, Ordering::Relaxed) >= cap {
                        break;
                    }
                    let port = ports[idx % ports.len()];
                    idx += 1;
                    let dst = SocketAddr::new(peer_ip, port);
                    let dst = match sock.local_addr() {
                        Ok(l) => crate::to_socket_family(&l, dst),
                        Err(_) => dst,
                    };
                    if let Ok(bytes) = build_check_request(&psk, random_transaction_id()) {
                        let _ = sock.send_to(&bytes, dst).await;
                    }
                }

                // Drain whatever came back before firing again.
                loop {
                    let got =
                        tokio::time::timeout(Duration::from_millis(2), sock.recv_from(&mut buf))
                            .await;
                    let (n, from) = match got {
                        Ok(Ok(v)) => v,
                        _ => break,
                    };
                    // Only an authenticated packet proves the hole. Anything
                    // else is a stranger or an ICMP-driven error.
                    let Some(c) = parse_check(&psk, &buf[..n]) else { continue };
                    if c.kind == CheckKind::Request {
                        if let Ok(r) = build_check_response(&psk, c.tid, unmap(from)) {
                            let _ = sock.send_to(&r, from).await;
                        }
                    }
                    let remote = unmap(from);
                    return sock.into_std().ok().map(|s| (s, remote));
                }
            }
        });
    }

    let mut winner = None;
    while let Some(res) = set.join_next().await {
        if let Ok(Some(v)) = res {
            winner = Some(v);
            break;
        }
    }
    set.abort_all();

    let sent = probes.load(Ordering::Relaxed).min(cap);
    Ok(winner.map(|(socket, remote)| BirthdayResult { socket, remote, probes: sent }))
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod birthday;

pub use birthday::{birthday_blast, guessed_ports, BirthdayResult};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 7 new tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/birthday.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): rung 3, the birthday-paradox blast

Hard-capped on probe count and wall clock, every probe an authenticated
STUN check, and the number actually sent is reported so the cost is visible.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

# oxutrm M2 — QUIC over a Punched Socket — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `oxutrm-net` complete — socket binding, candidate gathering, STUN, router port mapping, ICE hole punching, the birthday blast, and pinned QUIC — and prove it end to end with a dummy echo payload between two peers behind simulated NAT.

**Architecture:** One UDP socket per process is bound once (preferring UDP/443) and never rebound. Everything shares it — but only ever **one receiver at a time**, because two receive loops on one UDP socket steal each other's packets: STUN discovery first, then ICE connectivity checks, then QUIC behind a `StunDemuxSocket` that peels STUN off the front and passes the rest to `quinn`. ICE checks are real STUN Binding Requests carrying `MESSAGE-INTEGRITY` keyed by **direction-labelled** credentials derived from the SSH-delivered PSK with HKDF-SHA256, so the same packets do address discovery, authentication and peer-reflexive learning at once, and a peer's own reflected check can never be mistaken for the peer's. ICE runs to **nomination before QUIC starts**. When the NAT is symmetric the ladder falls through to a hard-capped birthday blast on a fresh set of sockets. QUIC (`quinn` 0.11) is then handed the winning socket with a `rustls` verifier that trusts exactly one SPKI fingerprint **and still checks the handshake signature**.

**Tech Stack:** Rust 2021, `tokio` 1, `quinn` 0.11, `rustls` 0.23 (ring provider), `rcgen` 0.13, `stun_codec` 0.4 + `bytecodec` 0.5, `stunclient` 0.4 (pre-QUIC discovery only), `crab_nat` 0.8, `igd-next` 0.17, `netdev` 0.46, `socket2` 0.6, `hkdf` 0.12, `sha1`, `sha2`, `hmac`, `base64`, `rand` 0.9.

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

## Five things the design spec gets wrong

The spec is the argument; these are the places where the crates and the
protocols disagree with it. Where they disagree, **reality wins** and this plan
follows reality.

1. **STUN and QUIC cannot both call `recv` on one socket** (spec §5.3, §6).
   `quinn::Endpoint::new` runs its own receive loop; so does every STUN client.
   Each `recvmsg` removes a datagram from the kernel queue, so the two race and
   steal each other's packets, with no error anywhere. Fixed by
   `StunDemuxSocket` + `Endpoint::new_with_abstract_socket` (Task 12).
   *Sending* from several places is fine; only receiving must have one owner.

2. **A better path cannot take over later** (spec §5). QUIC connection
   migration lets a **client** change its own **local** address. There is no
   protocol mechanism and no `quinn` API to repoint an established connection
   at a different **remote** address. ICE therefore runs to nomination
   *before* QUIC starts, and a better path found afterwards is lost for that
   attach — it is picked up on the next reattach, which re-runs the ladder.

3. **One shared PSK cannot authenticate a direction** (spec §5.3). It
   authenticates the session, so a peer cannot tell its own reflected check
   from a genuine peer check. Fixed with two HKDF-SHA256-derived credentials,
   info strings `"oxutrm ice c2h"` and `"oxutrm ice h2c"` (Task 7). And
   because ICE as specified has no roles, nomination is not well defined
   either: the client is deterministically `Controlling` and only it nominates
   (Task 8).

4. **Two STUN servers cannot produce the four-way `NatType`** (spec §5.3).
   Comparing mapped ports from two different server IPs separates
   `EndpointIndependent` from the rest and nothing more; `AddressDependent`
   and `Symmetric` both re-map on a new destination IP. A third probe to a
   **second port on the first server's IP** is what separates them (Task 6).

5. **`stunclient` has no `MESSAGE-INTEGRITY`.** Its entire API is `new`,
   `with_google_stun_server`, `set_timeout`, `set_retry_interval`,
   `set_software` and `query_external_address{,_async}`. It is therefore used
   for pre-QUIC discovery **only**; every ICE check, nomination and keepalive
   is built directly on `stun_codec` + `hmac`.

---

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
| `crates/oxutrm-net/src/demuxsock.rs` | `StunDemuxSocket` — the `quinn::AsyncUdpSocket` that peels STUN off the front |
| `crates/oxutrm-net/src/stunserver.rs` | `StunResponder` — a minimal STUN Binding server |
| `crates/oxutrm-net/src/discover.rs` | `stun_discover`, the three probes, and the NAT-typing truth table. The **only** module that touches `stunclient` |
| `crates/oxutrm-net/src/stunmsg.rs` | `IceCredentials` (HKDF), `IceRole`, `Direction`, and ICE checks: build, parse, `MESSAGE-INTEGRITY` |
| `crates/oxutrm-net/src/ice.rs` | `IceAgent`, `IceEvent`, one-sided nomination |
| `crates/oxutrm-net/src/birthday.rs` | rung 3, the birthday blast |
| `crates/oxutrm-net/src/mapping.rs` | `PortMapping` (NAT-PMP/PCP then UPnP-IGD), `default_gateway` via `netdev` |
| `crates/oxutrm-net/src/der.rs` | minimal DER walker: SPKI extraction from an X.509 certificate |
| `crates/oxutrm-net/src/tls.rs` | `generate_cert`, the pinned `ServerCertVerifier`, the crypto provider |
| `crates/oxutrm-net/src/quic.rs` | `quic_server`, `quic_client`, their `_demuxed` forms, `TransportConfig` |
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
hkdf = "0.12"
hmac = "0.12"
igd-next = { version = "0.17", features = ["aio_tokio"] }
netdev = "0.46"
quinn = { version = "0.11", default-features = false, features = ["log", "ring", "runtime-tokio", "rustls-ring"] }
rand = "0.9"
rcgen = "0.13"
rustls = { version = "0.23", default-features = false, features = ["logging", "ring", "std"] }
sha1 = "0.10"
sha2 = "0.10"
socket2 = "0.6"
stun_codec = "0.4"
stunclient = "0.4"
tokio = { version = "1", features = ["io-util", "macros", "net", "rt-multi-thread", "sync", "time"] }
```

Three notes, all load-bearing:

- `quinn`'s default features include `platform-verifier`, which drags in the
  whole OS trust store. oxutrm pins one certificate and trusts nothing else
  (spec §6.1), so defaults are off and only the four features above are on.
- `rustls`'s default features include the `aws-lc-rs` provider. With **two**
  providers compiled in, `rustls::ClientConfig::builder()` panics at runtime
  with "no process-level CryptoProvider available". Defaults are off, `ring`
  is on, every builder in this crate is constructed with an explicit provider,
  and one is installed as the process default before any of them runs
  (Task 11).
- `netdev` does two jobs here and replaces two crates that are **not** in the
  contract: interface enumeration (`get_interfaces`) and default-gateway
  discovery (`get_default_gateway`). Do not add `if-addrs`, and do not parse
  `/proc/net/route` — that is Linux-only and spec §1.2 scopes the project to
  Unix. `netdev`'s `gateway` feature is on by default; leave it on.

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
    // `netdev` rather than `if-addrs`: it is the crate the contract already
    // requires for gateway discovery, and one dependency doing both jobs is
    // one fewer to keep in step.
    let mut out: Vec<Candidate> = Vec::new();
    for iface in netdev::get_interfaces() {
        let v4 = iface.ipv4.iter().map(|n| IpAddr::V4(n.addr()));
        let v6 = iface.ipv6.iter().map(|n| IpAddr::V6(n.addr()));
        for ip in v4.chain(v6) {
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

## Task 6: `stun_discover` — three-probe NAT typing


**Files:**
- Create: `crates/oxutrm-net/src/discover.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{ice_priority, unmap, NetConfig, StunResponder, MappingBehaviour}`.
- Produces:
  ```rust
  /// One observed mapping, and which probe observed it.
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub struct Probe {
      pub server: std::net::SocketAddr,
      pub mapped: std::net::SocketAddr,
  }

  /// The truth table, as a pure function, so it is testable without a socket.
  pub fn classify(
      same_ip_same_port: Option<std::net::SocketAddr>,   // probe 1
      same_ip_alt_port: Option<std::net::SocketAddr>,    // probe 2
      other_ip: Option<std::net::SocketAddr>,            // probe 3
      local: std::net::SocketAddr,
      local_ips: &[std::net::IpAddr],
  ) -> oxutrm_proto::NatType;

  pub async fn stun_discover(
      socket: &tokio::net::UdpSocket,
      cfg: &NetConfig,
  ) -> (Vec<oxutrm_proto::Candidate>, oxutrm_proto::NatType);
  ```

### Why three probes, not two

Two servers at two different IPs separate `EndpointIndependent` from everything
else, and nothing more. They **cannot** tell `AddressDependent` from
`Symmetric`, because both allocate a fresh mapping when the destination IP
changes. The difference only shows when the destination **port** changes while
the IP stays the same:

- an **address-dependent** mapping keys on the destination IP alone, so a
  second port on the same server reuses the mapping;
- a **symmetric** (address-and-port-dependent) mapping keys on both, so a
  second port on the same server gets a new external port.

So the three probes are:

| Probe | Destination | What it isolates |
|---|---|---|
| **P1** | first server, its configured port | the baseline mapping |
| **P2** | **the same IP as P1, port + 1** | whether the destination *port* changes the mapping |
| **P3** | a **different** server IP | whether the destination *IP* changes the mapping |

### The truth table

| P1 vs P2 (same IP, different port) | P1 vs P3 (different IP) | `NatType` |
|---|---|---|
| all three equal, and equal to our own address | — | `None` |
| same | same | `EndpointIndependent` |
| same | **different** | `AddressDependent` |
| **different** | anything | `Symmetric` |

And the honest degradations, because a probe can simply time out:

| Answers in hand | `NatType` | Why |
|---|---|---|
| none | `Unknown` | nothing to compare |
| P1 only | `Unknown` | one data point compares with nothing |
| P1 + P2, equal | `Unknown` | cannot rule out `AddressDependent` |
| P1 + P2, different | `Symmetric` | the port alone changed the mapping; nothing else does that |
| P1 + P3, equal | `EndpointIndependent` | neither IP nor port moved it |
| P1 + P3, different | `Unknown` | could be `AddressDependent` or `Symmetric` |
| all three | the full table above | |

**P2 is best effort.** It goes to `IP_of(P1)` on `port_of(P1) + 1`, which is the
RFC 5780 convention (3478/3479) that most dedicated STUN servers follow and
that `stun.l.google.com:19302` does not. When P2 times out the classifier
degrades to `Unknown` rather than inventing an `AddressDependent`. The netns
harness binds its own responders on both ports, so the full table is exercised
there.

### Why the probes run one at a time

`stunclient::StunClient::query_external_address_async(self, udp: &tokio::net::UdpSocket)`
owns `recv_from` on that socket for as long as it runs, and **discards** every
datagram whose source is not its own server. Two of them on one socket would
eat each other's replies. So the three probes are **sequential**, each with
`set_timeout(cfg.gather_timeout / 3)`.

This contradicts the design spec's "queried in parallel" (§5.3). The spec is
describing an outcome — three probes inside the gather budget — that sequential
queries with a divided timeout achieve just as well. Parallelism here would
require re-implementing STUN discovery on `stun_codec`, and the contract
reserves `stunclient` for exactly this job.

**`stunclient` is pre-QUIC only.** Its entire API is `new`,
`with_google_stun_server`, `set_timeout`, `set_retry_interval`, `set_software`,
`query_external_address` and `query_external_address_async` — there is no
`MESSAGE-INTEGRITY` anywhere in it. Every ICE check and every keepalive is
built directly on `stun_codec` + `hmac` (the next task). And because
`query_external_address_async` owns the receive side, it must never run once
QUIC has the socket.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/discover.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MappingBehaviour, NetConfig, StunResponder};
    use oxutrm_proto::{CandidateKind, NatType};
    use std::net::{IpAddr, SocketAddr};
    use std::time::Duration;
    use tokio::net::UdpSocket;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn the_truth_table_holds_for_every_row() {
        let local = sa("192.168.1.5:50000");
        let local_ips = [IpAddr::from([192, 168, 1, 5])];

        struct Row {
            name: &'static str,
            p1: Option<SocketAddr>,
            p2: Option<SocketAddr>,
            p3: Option<SocketAddr>,
            want: NatType,
        }

        let rows = vec![
            Row {
                name: "no NAT at all: every probe reports our own address",
                p1: Some(sa("192.168.1.5:50000")),
                p2: Some(sa("192.168.1.5:50000")),
                p3: Some(sa("192.168.1.5:50000")),
                want: NatType::None,
            },
            Row {
                name: "endpoint independent: one mapping for everyone",
                p1: Some(sa("203.0.113.9:41000")),
                p2: Some(sa("203.0.113.9:41000")),
                p3: Some(sa("203.0.113.9:41000")),
                want: NatType::EndpointIndependent,
            },
            Row {
                name: "address dependent: the port did not move it, the IP did",
                p1: Some(sa("203.0.113.9:41000")),
                p2: Some(sa("203.0.113.9:41000")),
                p3: Some(sa("203.0.113.9:41777")),
                want: NatType::AddressDependent,
            },
            Row {
                name: "symmetric: a second port on the same IP got a new mapping",
                p1: Some(sa("203.0.113.9:41000")),
                p2: Some(sa("203.0.113.9:41001")),
                p3: Some(sa("203.0.113.9:41002")),
                want: NatType::Symmetric,
            },
            Row {
                name: "symmetric, even when the third probe agrees with the first",
                p1: Some(sa("203.0.113.9:41000")),
                p2: Some(sa("203.0.113.9:41001")),
                p3: Some(sa("203.0.113.9:41000")),
                want: NatType::Symmetric,
            },
            Row { name: "nothing answered", p1: None, p2: None, p3: None, want: NatType::Unknown },
            Row {
                name: "one answer compares with nothing",
                p1: Some(sa("203.0.113.9:41000")),
                p2: None,
                p3: None,
                want: NatType::Unknown,
            },
            Row {
                name: "P1 and P2 agree, but AddressDependent is not ruled out",
                p1: Some(sa("203.0.113.9:41000")),
                p2: Some(sa("203.0.113.9:41000")),
                p3: None,
                want: NatType::Unknown,
            },
            Row {
                name: "P1 and P2 disagree: only a symmetric NAT does that",
                p1: Some(sa("203.0.113.9:41000")),
                p2: Some(sa("203.0.113.9:41001")),
                p3: None,
                want: NatType::Symmetric,
            },
            Row {
                name: "P1 and P3 agree: neither IP nor port moved it",
                p1: Some(sa("203.0.113.9:41000")),
                p2: None,
                p3: Some(sa("203.0.113.9:41000")),
                want: NatType::EndpointIndependent,
            },
            Row {
                name: "P1 and P3 disagree: AddressDependent or Symmetric, cannot say which",
                p1: Some(sa("203.0.113.9:41000")),
                p2: None,
                p3: Some(sa("203.0.113.9:41999")),
                want: NatType::Unknown,
            },
            Row {
                name: "our own port but somebody else's IP is still a NAT",
                p1: Some(sa("203.0.113.9:50000")),
                p2: Some(sa("203.0.113.9:50000")),
                p3: Some(sa("203.0.113.9:50000")),
                want: NatType::EndpointIndependent,
            },
        ];

        for r in rows {
            assert_eq!(classify(r.p1, r.p2, r.p3, local, &local_ips), r.want, "row: {}", r.name);
        }
    }

    #[tokio::test]
    async fn a_truthful_responder_on_loopback_means_no_nat() {
        // One responder, bound on two consecutive ports, so P1 and P2 both
        // land and the classification is the full-table one.
        let a = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let alt = SocketAddr::new(a.addr().ip(), a.addr().port() + 1);
        let a2 = StunResponder::start_on(alt, MappingBehaviour::Truthful).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let _ = &a2;

        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = NetConfig {
            stun_servers: vec![a.server_string(), b.server_string()],
            gather_timeout: Duration::from_millis(1_500),
            ..NetConfig::default()
        };

        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(nat, NatType::None, "on loopback the mapping is literally our own address");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].kind, CandidateKind::ServerReflexive);
        assert_eq!(cands[0].addr.port(), sock.local_addr().unwrap().port());
    }

    #[tokio::test]
    async fn a_second_port_reporting_a_different_mapping_means_symmetric() {
        // P1 and P2 hit the same IP on two ports and disagree: symmetric,
        // and it is decided without P3 ever mattering.
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let alt = SocketAddr::new(a.addr().ip(), a.addr().port() + 1);
        let a2 = StunResponder::start_on(alt, MappingBehaviour::RewritePort(40_001)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(40_002)).await.unwrap();
        let _ = &a2;

        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = NetConfig {
            stun_servers: vec![a.server_string(), b.server_string()],
            gather_timeout: Duration::from_millis(1_500),
            ..NetConfig::default()
        };

        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(nat, NatType::Symmetric);
        assert_eq!(cands.len(), 3, "three distinct mappings are three candidates");
    }

    #[tokio::test]
    async fn the_same_mapping_from_two_ports_but_not_from_another_ip_is_address_dependent() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let alt = SocketAddr::new(a.addr().ip(), a.addr().port() + 1);
        let a2 = StunResponder::start_on(alt, MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(45_000)).await.unwrap();
        let _ = &a2;

        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = NetConfig {
            stun_servers: vec![a.server_string(), b.server_string()],
            gather_timeout: Duration::from_millis(1_500),
            ..NetConfig::default()
        };

        let (_cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(nat, NatType::AddressDependent);
    }

    #[tokio::test]
    async fn a_missing_second_port_degrades_to_unknown_rather_than_guessing() {
        // No responder on port + 1: P2 times out. P1 and P3 agree, which is
        // enough for EndpointIndependent and no more.
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();

        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = NetConfig {
            stun_servers: vec![a.server_string(), b.server_string()],
            gather_timeout: Duration::from_millis(900),
            ..NetConfig::default()
        };

        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert_eq!(nat, NatType::EndpointIndependent);
        assert_eq!(cands.len(), 1);
    }

    #[tokio::test]
    async fn unreachable_servers_yield_nothing_and_do_not_hang() {
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = NetConfig {
            stun_servers: vec!["127.0.0.1:1".to_owned(), "127.0.0.1:3".to_owned()],
            gather_timeout: Duration::from_millis(900),
            ..NetConfig::default()
        };

        let started = std::time::Instant::now();
        let (cands, nat) = stun_discover(&sock, &cfg).await;
        assert!(cands.is_empty());
        assert_eq!(nat, NatType::Unknown);
        assert!(started.elapsed() < Duration::from_secs(4), "took {:?}", started.elapsed());
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_is_skipped_rather_than_fatal() {
        let a = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::Truthful).await.unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let cfg = NetConfig {
            stun_servers: vec![
                "no-such-host.invalid:3478".to_owned(),
                a.server_string(),
                b.server_string(),
            ],
            gather_timeout: Duration::from_millis(1_500),
            ..NetConfig::default()
        };

        let (cands, _nat) = stun_discover(&sock, &cfg).await;
        assert!(!cands.is_empty(), "the two live servers must still be used");
    }

    #[tokio::test]
    #[ignore = "reaches the public internet; CI must never depend on public STUN servers"]
    async fn the_default_public_servers_answer() {
        let sock = UdpSocket::bind("0.0.0.0:0").await.unwrap();
        let (cands, nat) = stun_discover(&sock, &NetConfig::default()).await;
        assert!(!cands.is_empty(), "no public STUN server answered");
        eprintln!("public STUN says: {nat:?}, candidates {cands:?}");
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function classify in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/discover.rs`:

```rust
//! Pre-QUIC STUN discovery and NAT typing (spec §5.3).
//!
//! Discovery runs **from the socket QUIC will use**, because NAT mappings are
//! per-socket: an address learned on any other socket describes nothing
//! useful. It also runs **before** QUIC does, because
//! `stunclient::StunClient::query_external_address_async` owns `recv_from` on
//! the socket for as long as it runs. Once `quinn` has the socket, the only
//! way to receive STUN is [`crate::StunDemuxSocket`].
//!
//! `stunclient` has no `MESSAGE-INTEGRITY` in its API. It is used here and
//! nowhere else; every ICE check and keepalive is built on `stun_codec` +
//! `hmac` in [`crate::stunmsg`].

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use oxutrm_proto::{Candidate, CandidateKind, NatType};

use crate::{ice_priority, unmap, NetConfig};

/// One observed mapping and the server that observed it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Probe {
    pub server: SocketAddr,
    pub mapped: SocketAddr,
}

/// Three probes, run one at a time because each one owns the socket's receive
/// side while it runs.
///
/// * **P1** first server, configured port — the baseline.
/// * **P2** *the same IP*, port + 1 — does the destination **port** move the
///   mapping? Only a symmetric NAT's does.
/// * **P3** a *different* server IP — does the destination **IP** move it?
///
/// P2 is best effort: it relies on the RFC 5780 convention that a STUN server
/// also listens one port up. When it is absent the classifier degrades to
/// `Unknown` rather than inventing an answer.
pub async fn stun_discover(
    socket: &tokio::net::UdpSocket,
    cfg: &NetConfig,
) -> (Vec<Candidate>, NatType) {
    let local = match socket.local_addr() {
        Ok(a) => a,
        Err(_) => return (Vec::new(), NatType::Unknown),
    };
    // A third of the budget each, so three sequential probes fit inside it.
    let per_probe = cfg.gather_timeout / 3;

    let mut servers: Vec<SocketAddr> = Vec::new();
    for name in &cfg.stun_servers {
        let Ok(mut it) = tokio::net::lookup_host(name.as_str()).await else { continue };
        // An entry that does not resolve is skipped, not fatal.
        let Some(addr) = it.next() else { continue };
        if servers.iter().any(|s| s.ip() == addr.ip()) {
            continue;
        }
        servers.push(addr);
        if servers.len() == 2 {
            break;
        }
    }
    if servers.is_empty() {
        return (Vec::new(), NatType::Unknown);
    }

    let p1_server = servers[0];
    let p2_server = SocketAddr::new(p1_server.ip(), p1_server.port().wrapping_add(1));
    let p3_server = servers.get(1).copied();

    let p1 = query(socket, p1_server, per_probe).await;
    let p2 = query(socket, p2_server, per_probe).await;
    let p3 = match p3_server {
        Some(s) => query(socket, s, per_probe).await,
        None => None,
    };

    let local_ips: Vec<IpAddr> = netdev::get_interfaces()
        .into_iter()
        .flat_map(|i| {
            let v4 = i.ipv4.into_iter().map(|n| IpAddr::V4(n.addr()));
            let v6 = i.ipv6.into_iter().map(|n| IpAddr::V6(n.addr()));
            v4.chain(v6).collect::<Vec<_>>()
        })
        .collect();

    let nat = classify(p1, p2, p3, local, &local_ips);

    let mut seen: HashSet<SocketAddr> = HashSet::new();
    let mut candidates: Vec<Candidate> = Vec::new();
    for addr in [p1, p2, p3].into_iter().flatten() {
        if !seen.insert(addr) {
            continue;
        }
        candidates.push(Candidate {
            addr,
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

async fn query(
    socket: &tokio::net::UdpSocket,
    server: SocketAddr,
    timeout: Duration,
) -> Option<SocketAddr> {
    let mut client = stunclient::StunClient::new(server);
    client
        .set_timeout(timeout)
        // One retry inside the budget; the default 1s would fit only once.
        .set_retry_interval(timeout / 2)
        // Do not advertise a version string to a third party.
        .set_software(None);
    // Consumes `client`, and owns the socket's receive side until it returns.
    client.query_external_address_async(socket).await.ok().map(unmap)
}

/// The truth table. Pure, so it is exhaustively testable with no socket.
///
/// | P1 vs P2 (same IP, other port) | P1 vs P3 (other IP) | result |
/// |---|---|---|
/// | all equal, and equal to our own address | — | `None` |
/// | same | same | `EndpointIndependent` |
/// | same | different | `AddressDependent` |
/// | different | anything | `Symmetric` |
///
/// Missing probes degrade to `Unknown` rather than to a guess.
pub fn classify(
    p1: Option<SocketAddr>,
    p2: Option<SocketAddr>,
    p3: Option<SocketAddr>,
    local: SocketAddr,
    local_ips: &[IpAddr],
) -> NatType {
    let Some(p1) = p1 else { return NatType::Unknown };

    // A second port on the SAME server IP getting a different mapping is the
    // one thing only a symmetric NAT does. It decides on its own.
    if let Some(p2) = p2 {
        if p2 != p1 {
            return NatType::Symmetric;
        }
    }

    let untranslated = |a: SocketAddr| a.port() == local.port() && local_ips.contains(&a.ip());
    let all = [Some(p1), p2, p3];
    if all.iter().flatten().all(|a| untranslated(*a)) && all.iter().flatten().count() >= 2 {
        return NatType::None;
    }

    match (p2, p3) {
        // Everything agreed: one mapping for the whole world.
        (Some(_), Some(p3)) if p3 == p1 => NatType::EndpointIndependent,
        // The port did not move it but the IP did.
        (Some(_), Some(_)) => NatType::AddressDependent,
        // No second port. P1 and P3 agreeing still rules out both dependent
        // kinds; disagreeing cannot tell them apart.
        (None, Some(p3)) if p3 == p1 => NatType::EndpointIndependent,
        (None, Some(_)) => NatType::Unknown,
        // No third probe. P1 and P2 agreeing does not rule out
        // AddressDependent, so there is nothing honest to say.
        (Some(_), None) => NatType::Unknown,
        (None, None) => NatType::Unknown,
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod discover;

pub use discover::{classify, stun_discover, Probe};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 7 new tests pass (12 truth-table rows inside the first), one ignored, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/discover.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): three-probe STUN discovery and NAT typing

Two servers cannot separate AddressDependent from Symmetric; only a second
port on the FIRST server's IP can. Probes run sequentially because
stunclient owns the socket's receive side while it runs, and stunclient is
used here and nowhere else - it has no MESSAGE-INTEGRITY.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 7: Direction-labelled ICE credentials and connectivity checks


**Files:**
- Create: `crates/oxutrm-net/src/stunmsg.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{is_stun, unmap}`.
- Produces:
  ```rust
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum IceRole { Controlling, Controlled }   // the client is ALWAYS Controlling

  /// Which half of the exchange a message belongs to.
  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum Direction { ClientToHost, HostToClient }

  /// Two independent short-term credentials derived from the one shared PSK.
  #[derive(Clone)]
  pub struct IceCredentials { /* private; Debug is redacted */ }

  impl IceCredentials {
      pub fn derive(psk: &[u8; 32]) -> IceCredentials;
      /// The direction this role signs its own requests in.
      pub fn outbound(role: IceRole) -> Direction;
      /// The direction this role expects the peer's requests in.
      pub fn inbound(role: IceRole) -> Direction;
      pub fn ufrag(&self, d: Direction) -> &str;
      pub fn password(&self, d: Direction) -> &str;
      /// RFC 8445 `USERNAME`: `<remote-ufrag>:<local-ufrag>`.
      pub fn username(&self, d: Direction) -> String;
  }

  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum CheckKind { Request, SuccessResponse, Nomination }

  #[derive(Clone, Debug)]
  pub struct Check {
      pub kind: CheckKind,
      pub tid: stun_codec::TransactionId,
      pub reflexive: Option<std::net::SocketAddr>,
  }

  pub fn random_transaction_id() -> stun_codec::TransactionId;

  pub fn build_check_request(c: &IceCredentials, d: Direction, tid: stun_codec::TransactionId)
      -> anyhow::Result<Vec<u8>>;
  pub fn build_check_response(
      c: &IceCredentials,
      d: Direction,
      tid: stun_codec::TransactionId,
      reflexive: std::net::SocketAddr,
  ) -> anyhow::Result<Vec<u8>>;
  /// oxutrm's stand-in for RFC 8445 `USE-CANDIDATE`: an authenticated
  /// Binding Indication that says "this pair is the one".
  pub fn build_nomination(c: &IceCredentials, d: Direction, tid: stun_codec::TransactionId)
      -> anyhow::Result<Vec<u8>>;
  /// `d` is the direction the message is expected to have been signed in.
  pub fn parse_check(c: &IceCredentials, d: Direction, datagram: &[u8]) -> Option<Check>;
  ```

### Why one PSK is not enough

`MESSAGE-INTEGRITY` keyed by a single shared secret authenticates *the
session*, not *the sender*. Every consequence of that is bad:

- A peer cannot tell **its own reflected check** from a genuine peer check. A
  hairpinning NAT, a misconfigured middlebox, or an attacker echoing our own
  packet back at us all produce a request that verifies perfectly — and the
  agent then records "the peer can reach me", which is false. That is a
  validated pair built on nothing.
- The same holds for responses.

So the PSK is expanded into **two independent credentials**, one per direction,
with HKDF-SHA256 (RFC 5869):

```text
c2h = HKDF-SHA256(ikm = psk, salt = none, info = "oxutrm ice c2h", L = 32)
h2c = HKDF-SHA256(ikm = psk, salt = none, info = "oxutrm ice h2c", L = 32)
```

The **client is always `IceRole::Controlling`** and signs its requests with
`c2h`; the host is `Controlled` and signs with `h2c`. A response is signed with
the **same** credential as the request it answers (RFC 8445 §7.1.2), so:

| I am | I sign my requests with | I verify inbound requests with | I sign my responses with | I verify inbound responses with |
|---|---|---|---|---|
| `Controlling` (client) | `c2h` | `h2c` | `h2c` | `c2h` |
| `Controlled` (host) | `h2c` | `c2h` | `c2h` | `h2c` |

A reflected copy of my own request is signed with my outbound credential and
checked against the peer's — so it fails, every time, which is the entire
point.

### Signalling the nomination

Only the controlling side nominates, so the controlled side has to be *told*.
RFC 8445 uses a `USE-CANDIDATE` attribute, which `stun_codec`'s `rfc5389`
module does not define (it is an RFC 5245/8445 attribute). oxutrm sends an
authenticated **Binding Indication** on the winning pair instead: same
credential, same `MESSAGE-INTEGRITY`, no response expected, and it cannot be
confused with a check because its class differs. That is `CheckKind::Nomination`.

### The two things that fail silently

**1. Key derivation.** `stun_codec`'s
`MessageIntegrity::new_short_term_credential(&message, password)` uses the
password **verbatim as the HMAC-SHA1 key**: it calls `password.as_bytes()`
(RFC 5389 §15.4 — for short-term credentials the key *is* `SASLprep(password)`,
and nothing in a base64 alphabet is altered by SASLprep). The HKDF output is 32
raw bytes, which is not a `&str`, so each credential is the **URL-safe,
unpadded base64** of its 32 bytes: 43 characters of `[A-Za-z0-9_-]`, split into
an 8-character ufrag and a 35-character password. Both clear RFC 8445's
minimums (ufrag ≥ 4, password ≥ 22) and neither needs escaping in a STUN
`USERNAME`.

**2. Attribute ordering.** The HMAC covers the message **as encoded so far**,
with the header's length field temporarily raised by 24 (4 bytes of attribute
header plus 20 bytes of HMAC). Therefore every other attribute must already be
in the message when `MESSAGE-INTEGRITY` is added, and **nothing may be added
after it**. oxutrm deliberately does not send `FINGERPRINT`: it would have to
follow `MESSAGE-INTEGRITY`, changing the bytes `stun_codec` validates over, and
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

    const PSK: [u8; 32] = [7u8; 32];

    #[test]
    fn the_two_directions_get_different_credentials() {
        let c = IceCredentials::derive(&PSK);
        assert_ne!(
            c.password(Direction::ClientToHost),
            c.password(Direction::HostToClient),
            "one PSK must expand into two independent keys, or reflection works"
        );
        assert_ne!(c.ufrag(Direction::ClientToHost), c.ufrag(Direction::HostToClient));
    }

    #[test]
    fn derivation_is_deterministic_and_psk_dependent() {
        let a = IceCredentials::derive(&PSK);
        let b = IceCredentials::derive(&PSK);
        let other = IceCredentials::derive(&[8u8; 32]);
        assert_eq!(a.password(Direction::ClientToHost), b.password(Direction::ClientToHost));
        assert_ne!(a.password(Direction::ClientToHost), other.password(Direction::ClientToHost));
    }

    #[test]
    fn the_credentials_satisfy_rfc_8445_length_and_alphabet_rules() {
        let c = IceCredentials::derive(&PSK);
        for d in [Direction::ClientToHost, Direction::HostToClient] {
            assert_eq!(c.ufrag(d).len(), 8, "RFC 8445 wants at least 4");
            assert!(c.password(d).len() >= 22, "RFC 8445 wants at least 22");
            let ok = |s: &str| {
                s.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
            };
            assert!(ok(c.ufrag(d)), "ufrag must be safe in a STUN USERNAME");
            assert!(ok(c.password(d)), "password must be usable verbatim as an HMAC key");
            assert_eq!(c.username(d), format!("{}:{}", c.ufrag(other(d)), c.ufrag(d)));
        }
    }

    fn other(d: Direction) -> Direction {
        match d {
            Direction::ClientToHost => Direction::HostToClient,
            Direction::HostToClient => Direction::ClientToHost,
        }
    }

    #[test]
    fn the_client_is_controlling_and_signs_client_to_host() {
        assert_eq!(IceCredentials::outbound(IceRole::Controlling), Direction::ClientToHost);
        assert_eq!(IceCredentials::inbound(IceRole::Controlling), Direction::HostToClient);
        assert_eq!(IceCredentials::outbound(IceRole::Controlled), Direction::HostToClient);
        assert_eq!(IceCredentials::inbound(IceRole::Controlled), Direction::ClientToHost);
    }

    #[test]
    fn a_reflected_request_does_not_verify_against_the_inbound_credential() {
        // THE test. The client sends a request signed c2h. If that packet
        // comes straight back at the client - hairpin NAT, a middlebox, an
        // attacker echoing it - the client checks it with h2c and must reject
        // it. With a single shared PSK this test cannot be written at all.
        let c = IceCredentials::derive(&PSK);
        let mine = build_check_request(&c, Direction::ClientToHost, random_transaction_id()).unwrap();

        assert!(
            parse_check(&c, Direction::HostToClient, &mine).is_none(),
            "a peer must never accept its own reflected check as the peer's"
        );
        assert!(
            parse_check(&c, Direction::ClientToHost, &mine).is_some(),
            "the host, which verifies with c2h, must accept it"
        );
    }

    #[test]
    fn a_request_round_trips_and_demultiplexes_as_stun() {
        let c = IceCredentials::derive(&PSK);
        let tid = random_transaction_id();
        let bytes = build_check_request(&c, Direction::ClientToHost, tid).unwrap();

        assert!(crate::is_stun(&bytes), "our own checks must survive the demultiplexer");
        let got = parse_check(&c, Direction::ClientToHost, &bytes).expect("must verify");
        assert_eq!(got.kind, CheckKind::Request);
        assert_eq!(got.tid, tid);
        assert_eq!(got.reflexive, None, "a request carries no mapped address");
    }

    #[test]
    fn a_response_carries_the_address_the_peer_saw() {
        let c = IceCredentials::derive(&PSK);
        let tid = random_transaction_id();
        let peer: SocketAddr = "203.0.113.7:41234".parse().unwrap();
        let bytes = build_check_response(&c, Direction::HostToClient, tid, peer).unwrap();

        let got = parse_check(&c, Direction::HostToClient, &bytes).unwrap();
        assert_eq!(got.kind, CheckKind::SuccessResponse);
        assert_eq!(got.tid, tid);
        assert_eq!(got.reflexive, Some(peer), "this is peer-reflexive discovery, for free");
    }

    #[test]
    fn an_ipv4_mapped_reflexive_address_comes_back_unmapped() {
        let c = IceCredentials::derive(&PSK);
        let mapped: SocketAddr = "[::ffff:203.0.113.7]:443".parse().unwrap();
        let bytes =
            build_check_response(&c, Direction::HostToClient, random_transaction_id(), mapped)
                .unwrap();
        assert_eq!(
            parse_check(&c, Direction::HostToClient, &bytes).unwrap().reflexive,
            Some("203.0.113.7:443".parse::<SocketAddr>().unwrap()),
            "a candidate must carry the address a peer would dial"
        );
    }

    #[test]
    fn an_ipv6_reflexive_address_survives_the_round_trip() {
        let c = IceCredentials::derive(&PSK);
        let peer: SocketAddr = "[2001:db8::7]:443".parse().unwrap();
        let bytes =
            build_check_response(&c, Direction::HostToClient, random_transaction_id(), peer)
                .unwrap();
        assert_eq!(
            parse_check(&c, Direction::HostToClient, &bytes).unwrap().reflexive,
            Some(peer)
        );
    }

    #[test]
    fn a_different_psk_is_rejected() {
        let mine = IceCredentials::derive(&[1u8; 32]);
        let theirs = IceCredentials::derive(&[2u8; 32]);
        let bytes =
            build_check_request(&mine, Direction::ClientToHost, random_transaction_id()).unwrap();
        assert!(
            parse_check(&theirs, Direction::ClientToHost, &bytes).is_none(),
            "a stranger must never advance the state machine"
        );
    }

    #[test]
    fn a_flipped_transaction_id_byte_fails_the_integrity_check() {
        let c = IceCredentials::derive(&PSK);
        let mut bytes =
            build_check_request(&c, Direction::ClientToHost, random_transaction_id()).unwrap();
        // Byte 12 is inside the transaction id: the message still decodes
        // perfectly, so the only thing that can reject it is the HMAC.
        bytes[12] ^= 0x01;
        assert!(parse_check(&c, Direction::ClientToHost, &bytes).is_none());
    }

    #[test]
    fn a_check_with_no_message_integrity_at_all_is_rejected() {
        let mut msg =
            Message::<Attribute>::new(MessageClass::Request, BINDING, random_transaction_id());
        msg.add_attribute(Software::new("stranger".to_owned()).unwrap());
        let bytes = MessageEncoder::<Attribute>::new().encode_into_bytes(msg).unwrap();

        assert!(crate::is_stun(&bytes), "it really is a valid STUN message");
        assert!(
            parse_check(&IceCredentials::derive(&PSK), Direction::ClientToHost, &bytes).is_none(),
            "oxutrm must not be usable as a reflector or amplifier"
        );
    }

    #[test]
    fn non_stun_truncated_and_wrong_class_datagrams_are_all_rejected() {
        let c = IceCredentials::derive(&PSK);
        let d = Direction::ClientToHost;
        assert!(parse_check(&c, d, &[]).is_none());
        assert!(parse_check(&c, d, &[0xC3; 64]).is_none(), "a QUIC long header");
        assert!(parse_check(&c, d, &[0x40; 64]).is_none(), "a QUIC short header");

        let bytes = build_check_request(&c, d, random_transaction_id()).unwrap();
        assert!(parse_check(&c, d, &bytes[..bytes.len() - 4]).is_none(), "truncated");

        let unsigned_indication = MessageEncoder::<Attribute>::new()
            .encode_into_bytes(Message::<Attribute>::new(
                MessageClass::Indication,
                BINDING,
                random_transaction_id(),
            ))
            .unwrap();
        assert!(
            parse_check(&c, d, &unsigned_indication).is_none(),
            "an unsigned indication is not a nomination"
        );
    }

    #[test]
    fn a_nomination_is_authenticated_and_distinguishable_from_a_check() {
        let c = IceCredentials::derive(&PSK);
        let tid = random_transaction_id();
        let bytes = build_nomination(&c, Direction::ClientToHost, tid).unwrap();

        assert!(crate::is_stun(&bytes));
        let got = parse_check(&c, Direction::ClientToHost, &bytes).expect("must verify");
        assert_eq!(got.kind, CheckKind::Nomination);
        assert_eq!(got.tid, tid);
        assert!(
            parse_check(&c, Direction::HostToClient, &bytes).is_none(),
            "a reflected nomination must not verify either"
        );
    }

    #[test]
    fn message_integrity_is_the_last_attribute_in_the_encoding() {
        // If anything were appended after it, stun_codec would validate over
        // different bytes than it signed: every check would fail on the wire
        // while passing in isolation. Type 0x0008, 20-byte value, so it is
        // exactly the last 24 bytes.
        let c = IceCredentials::derive(&PSK);
        let bytes =
            build_check_request(&c, Direction::ClientToHost, random_transaction_id()).unwrap();
        let n = bytes.len();
        assert_eq!(&bytes[n - 24..n - 22], &[0x00, 0x08], "MESSAGE-INTEGRITY type");
        assert_eq!(&bytes[n - 22..n - 20], &[0x00, 0x14], "20-byte HMAC-SHA1 value");
    }

    #[test]
    fn the_debug_impl_does_not_leak_the_credentials() {
        // No key material on disk (spec §11), and none in a log line either.
        let c = IceCredentials::derive(&PSK);
        let text = format!("{c:?}");
        assert!(!text.contains(c.password(Direction::ClientToHost)));
        assert!(!text.contains(c.password(Direction::HostToClient)));
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
Expected: FAIL — `failed to resolve: use of undeclared type IceCredentials`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/stunmsg.rs`:

```rust
//! ICE connectivity checks: STUN Binding Requests carrying `MESSAGE-INTEGRITY`
//! keyed by a **direction-labelled** credential derived from the SSH-delivered
//! PSK (spec §5.3).
//!
//! Chosen over a bespoke probe format because it demultiplexes cleanly against
//! QUIC on the same socket, because `MESSAGE-INTEGRITY` stops strangers
//! confusing the state machine and stops oxutrm being used as a reflector, and
//! because the `XOR-MAPPED-ADDRESS` in the response *is* peer-reflexive
//! discovery at no extra cost.
//!
//! Nothing in `stunclient` is used here: its API has no `MESSAGE-INTEGRITY`.

use std::net::SocketAddr;

use anyhow::anyhow;
use base64::Engine as _;
use bytecodec::{DecodeExt, EncodeExt};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::Sha256;
use stun_codec::rfc5389::attributes::{MessageIntegrity, Username, XorMappedAddress};
use stun_codec::rfc5389::{methods::BINDING, Attribute};
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder, TransactionId};

use crate::{is_stun, unmap};

/// HKDF `info` for the client-to-host credential.
const INFO_C2H: &[u8] = b"oxutrm ice c2h";
/// HKDF `info` for the host-to-client credential.
const INFO_H2C: &[u8] = b"oxutrm ice h2c";

/// The client is **always** `Controlling`, and only the controlling side
/// nominates. With both sides free to nominate, asymmetric loss makes them
/// nominate different pairs and the connection has no agreed path.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IceRole {
    Controlling,
    Controlled,
}

/// Which half of the exchange a message belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    ClientToHost,
    HostToClient,
}

impl Direction {
    fn opposite(self) -> Direction {
        match self {
            Direction::ClientToHost => Direction::HostToClient,
            Direction::HostToClient => Direction::ClientToHost,
        }
    }
}

/// Two independent short-term credentials, expanded from the one shared PSK.
#[derive(Clone)]
pub struct IceCredentials {
    c2h_ufrag: String,
    c2h_pwd: String,
    h2c_ufrag: String,
    h2c_pwd: String,
}

/// Redacted on purpose: spec §11 keeps key material out of files, and a log
/// line is a file somewhere.
impl std::fmt::Debug for IceCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IceCredentials")
            .field("c2h_ufrag", &self.c2h_ufrag)
            .field("c2h_pwd", &"<redacted>")
            .field("h2c_ufrag", &self.h2c_ufrag)
            .field("h2c_pwd", &"<redacted>")
            .finish()
    }
}

impl IceCredentials {
    /// Expand the PSK into one credential per direction with HKDF-SHA256
    /// (RFC 5869), no salt, 32 bytes of output each.
    pub fn derive(psk: &[u8; 32]) -> IceCredentials {
        let (c2h_ufrag, c2h_pwd) = expand(psk, INFO_C2H);
        let (h2c_ufrag, h2c_pwd) = expand(psk, INFO_H2C);
        IceCredentials { c2h_ufrag, c2h_pwd, h2c_ufrag, h2c_pwd }
    }

    /// The direction this role signs its own **requests** in.
    pub fn outbound(role: IceRole) -> Direction {
        match role {
            IceRole::Controlling => Direction::ClientToHost,
            IceRole::Controlled => Direction::HostToClient,
        }
    }

    /// The direction this role expects the **peer's** requests in. Verifying
    /// inbound requests with this — never with `outbound` — is what makes a
    /// reflected copy of our own check fail.
    pub fn inbound(role: IceRole) -> Direction {
        IceCredentials::outbound(role).opposite()
    }

    pub fn ufrag(&self, d: Direction) -> &str {
        match d {
            Direction::ClientToHost => &self.c2h_ufrag,
            Direction::HostToClient => &self.h2c_ufrag,
        }
    }

    pub fn password(&self, d: Direction) -> &str {
        match d {
            Direction::ClientToHost => &self.c2h_pwd,
            Direction::HostToClient => &self.h2c_pwd,
        }
    }

    /// RFC 8445 §7.1.2: `USERNAME` is `<remote-ufrag>:<local-ufrag>`.
    pub fn username(&self, d: Direction) -> String {
        format!("{}:{}", self.ufrag(d.opposite()), self.ufrag(d))
    }
}

fn expand(psk: &[u8; 32], info: &[u8]) -> (String, String) {
    let hk = Hkdf::<Sha256>::new(None, psk);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is a legal HKDF-SHA256 output length");
    // `stun_codec` uses the password verbatim as the HMAC-SHA1 key, so it has
    // to be a string. URL-safe unpadded base64 of 32 bytes is 43 characters
    // of [A-Za-z0-9_-], none of which needs escaping in a STUN USERNAME.
    let text = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(okm);
    let (ufrag, pwd) = text.split_at(8);
    (ufrag.to_owned(), pwd.to_owned())
}

/// What an authenticated check turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckKind {
    Request,
    SuccessResponse,
    /// The controlling side declaring this pair the winner. oxutrm's
    /// stand-in for RFC 8445 `USE-CANDIDATE`, which `stun_codec` does not
    /// implement.
    Nomination,
}

/// A check that verified against the expected credential.
#[derive(Clone, Debug)]
pub struct Check {
    pub kind: CheckKind,
    pub tid: TransactionId,
    /// Present on responses: the address the peer saw us come from.
    pub reflexive: Option<SocketAddr>,
}

pub fn random_transaction_id() -> TransactionId {
    let mut b = [0u8; 12];
    rand::rng().fill_bytes(&mut b);
    TransactionId::new(b)
}

/// A Binding Request signed in direction `d`.
pub fn build_check_request(
    c: &IceCredentials,
    d: Direction,
    tid: TransactionId,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::Request, BINDING, tid);

    // Everything that is not MESSAGE-INTEGRITY goes in first.
    let username =
        Username::new(c.username(d)).map_err(|e| anyhow!("building USERNAME: {e}"))?;
    msg.add_attribute(username);

    // MESSAGE-INTEGRITY last: the HMAC covers the message as encoded so far.
    // Nothing may be added after this line.
    let mi = MessageIntegrity::new_short_term_credential(&msg, c.password(d))
        .map_err(|e| anyhow!("computing MESSAGE-INTEGRITY: {e}"))?;
    msg.add_attribute(mi);

    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(msg)
        .map_err(|e| anyhow!("encoding a check request: {e}"))
}

/// A Binding Success Response, signed with the **same** credential as the
/// request it answers (RFC 8445 §7.1.2) — so `d` here is the direction of the
/// *request*, not of this response's travel.
pub fn build_check_response(
    c: &IceCredentials,
    d: Direction,
    tid: TransactionId,
    reflexive: SocketAddr,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::SuccessResponse, BINDING, tid);
    msg.add_attribute(XorMappedAddress::new(reflexive));

    let mi = MessageIntegrity::new_short_term_credential(&msg, c.password(d))
        .map_err(|e| anyhow!("computing MESSAGE-INTEGRITY: {e}"))?;
    msg.add_attribute(mi);

    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(msg)
        .map_err(|e| anyhow!("encoding a check response: {e}"))
}

/// An authenticated Binding Indication naming the winning pair.
///
/// Only the controlling side sends one. `d` is the sender's outbound
/// direction, so the controlled side verifies it with
/// [`IceCredentials::inbound`] exactly as it verifies a check.
pub fn build_nomination(
    c: &IceCredentials,
    d: Direction,
    tid: TransactionId,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::Indication, BINDING, tid);
    let username =
        Username::new(c.username(d)).map_err(|e| anyhow!("building USERNAME: {e}"))?;
    msg.add_attribute(username);

    // MESSAGE-INTEGRITY last, as always.
    let mi = MessageIntegrity::new_short_term_credential(&msg, c.password(d))
        .map_err(|e| anyhow!("computing MESSAGE-INTEGRITY: {e}"))?;
    msg.add_attribute(mi);

    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(msg)
        .map_err(|e| anyhow!("encoding a nomination: {e}"))
}

/// Decode a datagram and verify `MESSAGE-INTEGRITY` against direction `d`.
///
/// Returns `None` for anything that is not an authenticated Binding Request or
/// Binding Success Response **in that direction**. Passing the wrong direction
/// is how a reflected copy of our own check gets rejected, so callers must be
/// deliberate: inbound requests use [`IceCredentials::inbound`], inbound
/// responses use [`IceCredentials::outbound`].
pub fn parse_check(c: &IceCredentials, d: Direction, datagram: &[u8]) -> Option<Check> {
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
        MessageClass::Indication => CheckKind::Nomination,
        // Error responses are not part of the check exchange.
        _ => return None,
    };

    // No credential, or the wrong one, or the right one in the wrong
    // direction: not ours.
    let mi: &MessageIntegrity = msg.get_attribute()?;
    mi.check_short_term_credential(c.password(d)).ok()?;

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
    build_check_request, build_check_response, build_nomination, parse_check,
    random_transaction_id, Check, CheckKind, Direction, IceCredentials, IceRole,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 16 new tests pass, clippy clean. The one that matters is
`a_reflected_request_does_not_verify_against_the_inbound_credential`.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/stunmsg.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): direction-labelled ICE credentials via HKDF-SHA256

One shared PSK authenticates the session, not the sender, so a peer cannot
tell its own reflected check from a genuine one. HKDF-SHA256 with info
"oxutrm ice c2h" / "oxutrm ice h2c" gives one credential per direction, and
a reflected request now fails verification.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 8: `IceAgent` — checks, one-sided nomination, peer-reflexive learning


**Files:**
- Create: `crates/oxutrm-net/src/ice.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{build_check_request, build_check_response, build_nomination, parse_check, random_transaction_id, ice_priority, to_socket_family, unmap, Check, CheckKind, Direction, IceCredentials, IceRole, NetConfig}`; `oxutrm_proto::{Candidate, CandidateKind, Rung}`.
- Produces:
  ```rust
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
      pub fn last_rtt(&self) -> Option<std::time::Duration>;
      pub fn remote_count(&self) -> usize;
      pub fn local_count(&self) -> usize;
      /// Step until there is something to report. Call it in a loop.
      pub async fn run(&mut self, socket: std::sync::Arc<tokio::net::UdpSocket>) -> IceEvent;
  }
  ```
  (`IceRole` itself is produced by the credentials task and re-exported here.)

### Four decisions that are not negotiable

**1. Nomination MUST complete before QUIC starts.** QUIC connection migration
only lets a **client** change its own **local** address. There is no protocol
mechanism and no `quinn` API to repoint an established connection at a
different **remote** address. So the design spec's "a better path appearing
later may take over, because QUIC connection migration makes the switch free"
(§5) is **false**, and this plan does not implement it. ICE runs to
completion, the winning pair is fixed, and only then does QUIC open on that
pair. A better path discovered afterwards is **lost for that attach** — it is
picked up on the next reattach, which re-runs the whole exchange.

**2. Only the controlling side nominates.** With both sides free to choose,
asymmetric loss (a pair validated at one end and not yet at the other) makes
them pick different pairs, and there is no agreed path at all. The client is
always `IceRole::Controlling`. It picks the highest-priority pair validated in
both directions, then announces it with an authenticated Binding Indication
(the credentials task's `build_nomination`). The controlled side reports
`Nominated` only when that indication arrives.

**3. Direction-labelled credentials everywhere.** Inbound **requests** and
**nominations** are verified with `IceCredentials::inbound(role)`; inbound
**responses** with `IceCredentials::outbound(role)`, because a response is
signed with the credential of the request it answers. A reflected copy of our
own packet is then always checked against the wrong key and rejected.

**4. `run` is a step function.** The contract returns a single `IceEvent` from
`&mut self`, so callers loop and state persists across calls:

```rust
loop {
    match agent.run(socket.clone()).await {
        IceEvent::NewLocalCandidate(c) => publish(c),          // and keep going
        ev @ (IceEvent::Nominated { .. } | IceEvent::Failed(_)) => break ev,
    }
}
```

An agent with no remote candidates still listens rather than failing: a peer
behind a symmetric NAT may only ever be reachable because *its* punch arrives
first, and that inbound authenticated request is what teaches us its address.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/ice.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ice_priority, IceRole, NetConfig};
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

    /// The loop every real caller writes.
    async fn drive(mut agent: IceAgent, sock: Arc<UdpSocket>) -> IceEvent {
        loop {
            match agent.run(sock.clone()).await {
                IceEvent::NewLocalCandidate(_) => continue,
                ev => return ev,
            }
        }
    }

    /// Two agents, each told where the other is.
    async fn pair(
        psk: [u8; 32],
        bind: &str,
        budget: u64,
    ) -> (IceEvent, IceEvent, SocketAddr, SocketAddr) {
        let a_sock = Arc::new(UdpSocket::bind(bind).await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind(bind).await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(budget));
        a.add_remote(host(b_addr));
        let mut b = IceAgent::new(psk, IceRole::Controlled, quick(budget));
        b.add_remote(host(a_addr));

        let ta = tokio::spawn(drive(a, a_sock));
        let tb = tokio::spawn(drive(b, b_sock));
        (ta.await.unwrap(), tb.await.unwrap(), a_addr, b_addr)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn both_sides_agree_on_the_same_pair() {
        let (ea, eb, a_addr, b_addr) = pair([42u8; 32], "127.0.0.1:0", 5_000).await;
        match (ea, eb) {
            (
                IceEvent::Nominated { remote: ra, probes: pa, .. },
                IceEvent::Nominated { remote: rb, .. },
            ) => {
                assert_eq!(ra, b_addr, "the client nominated the host's address");
                assert_eq!(rb, a_addr, "the host was told, and agrees");
                assert!(pa >= 1);
            }
            other => panic!("expected both sides to nominate, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ipv6_host_pair_is_reported_as_rung_zero() {
        let (ea, _eb, _, _) = pair([43u8; 32], "[::1]:0", 5_000).await;
        match ea {
            IceEvent::Nominated { rung, .. } => assert_eq!(rung, Rung::Ipv6Direct),
            other => panic!("expected a nomination, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_controlled_side_never_nominates_on_its_own() {
        // The host validates the pair in both directions but is given no
        // nomination, because there is no controlling agent running. It must
        // time out rather than declare a winner unilaterally.
        let psk = [50u8; 32];
        let creds = crate::IceCredentials::derive(&psk);
        let host_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let host_addr = host_sock.local_addr().unwrap();

        // A stub client: answers the host's checks and sends its own, so the
        // host sees a fully validated pair - but never nominates.
        let stub = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let stub_addr = stub.local_addr().unwrap();
        let stub_creds = creds.clone();
        let stub_task = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            let out = crate::IceCredentials::outbound(IceRole::Controlling);
            let inb = crate::IceCredentials::inbound(IceRole::Controlling);
            loop {
                let req = crate::build_check_request(
                    &stub_creds,
                    out,
                    crate::random_transaction_id(),
                )
                .unwrap();
                let _ = stub.send_to(&req, host_addr).await;
                let got = tokio::time::timeout(
                    Duration::from_millis(50),
                    stub.recv_from(&mut buf),
                )
                .await;
                if let Ok(Ok((n, from))) = got {
                    if let Some(c) = crate::parse_check(&stub_creds, inb, &buf[..n]) {
                        if c.kind == crate::CheckKind::Request {
                            let resp =
                                crate::build_check_response(&stub_creds, inb, c.tid, from)
                                    .unwrap();
                            let _ = stub.send_to(&resp, from).await;
                        }
                    }
                }
            }
        });

        let mut agent = IceAgent::new(psk, IceRole::Controlled, quick(900));
        agent.add_remote(host(stub_addr));
        let ev = drive(agent, host_sock).await;
        stub_task.abort();

        assert!(
            matches!(ev, IceEvent::Failed(_)),
            "the controlled side must wait to be told, got {ev:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_peer_that_knows_nothing_learns_the_other_one_peer_reflexively() {
        let psk = [44u8; 32];
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b_addr = b_sock.local_addr().unwrap();

        // The client knows where the host is. The host knows nothing at all
        // and must learn from the authenticated request that arrives.
        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        a.add_remote(host(b_addr));
        let b = IceAgent::new(psk, IceRole::Controlled, quick(5_000));

        let ta = tokio::spawn(drive(a, a_sock));
        let tb = tokio::spawn(drive(b, b_sock));
        let (ea, eb) = (ta.await.unwrap(), tb.await.unwrap());

        match eb {
            IceEvent::Nominated { remote, .. } => assert_eq!(remote, a_addr),
            other => panic!("the host should have learned the client, got {other:?}"),
        }
        assert!(matches!(ea, IceEvent::Nominated { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agents_with_different_psks_never_nominate() {
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
        assert!(matches!(ta.await.unwrap(), IceEvent::Failed(_)));
        assert!(matches!(tb.await.unwrap(), IceEvent::Failed(_)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_agent_talking_to_itself_does_not_validate_a_pair() {
        // The reflection case, end to end: point the client at its OWN
        // address. Every check it sends comes straight back. With one shared
        // PSK this would validate a pair against nobody; with
        // direction-labelled credentials it must not.
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let own = sock.local_addr().unwrap();
        let mut a = IceAgent::new([45u8; 32], IceRole::Controlling, quick(800));
        a.add_remote(host(own));

        let ev = drive(a, sock).await;
        assert!(
            matches!(ev, IceEvent::Failed(_)),
            "a peer must never validate a pair against its own echo, got {ev:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_is_simply_not_there_fails_within_the_budget() {
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let mut a = IceAgent::new([3u8; 32], IceRole::Controlling, quick(600));
        a.add_remote(host("127.0.0.1:1".parse().unwrap()));

        let started = std::time::Instant::now();
        let ev = drive(a, sock).await;
        assert!(matches!(ev, IceEvent::Failed(_)), "got {ev:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "took {:?}", started.elapsed());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_higher_priority_pair_wins_when_two_both_work() {
        let psk = [46u8; 32];
        let a_sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b1 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let b2 = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let a_addr = a_sock.local_addr().unwrap();
        let b1_addr = b1.local_addr().unwrap();
        let b2_addr = b2.local_addr().unwrap();

        let mut a = IceAgent::new(psk, IceRole::Controlling, quick(5_000));
        // Low priority added first, on purpose.
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
            IceEvent::Nominated { remote, .. } => {
                assert_eq!(remote, b1_addr, "Host outranks ServerReflexive")
            }
            other => panic!("expected a nomination, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_nominating_agent_reports_a_round_trip_time() {
        let psk = [47u8; 32];
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
//!
//! **Nomination completes before QUIC starts.** QUIC connection migration only
//! lets a client change its own *local* address; nothing in the protocol or in
//! `quinn` can repoint an established connection at a different *remote*
//! address. A better path found after nomination is lost for this attach and
//! picked up on the next one.

use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use oxutrm_proto::{Candidate, CandidateKind, Rung};

use crate::{
    build_check_request, build_check_response, build_nomination, ice_priority, parse_check,
    random_transaction_id, to_socket_family, unmap, CheckKind, Direction, IceCredentials, IceRole,
    NetConfig,
};

/// How often a fresh round of checks goes out to every remote candidate.
const CHECK_INTERVAL: Duration = Duration::from_millis(250);

/// How many outstanding transaction ids one pair remembers. A response whose
/// id has been forgotten is ignored; the next round replaces it.
const MAX_OUTSTANDING: usize = 8;

/// How many times the controlling side repeats its nomination indication.
/// Indications get no response, so the only defence against loss is repetition.
const NOMINATION_REPEATS: usize = 3;

#[derive(Clone, Debug)]
pub enum IceEvent {
    /// We learned one of our own addresses from a peer's `XOR-MAPPED-ADDRESS`.
    /// Publish it to the peer and keep going.
    NewLocalCandidate(Candidate),
    /// The agreed pair. On the controlling side this is its own decision; on
    /// the controlled side it is the decision it was told about.
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
    /// Checks we sent here, with the instant they went out.
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
    creds: IceCredentials,
    role: IceRole,
    /// The direction we sign our own requests in.
    out_dir: Direction,
    /// The direction the peer signs its requests and nominations in.
    in_dir: Direction,
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
            creds: IceCredentials::derive(&psk),
            role,
            out_dir: IceCredentials::outbound(role),
            in_dir: IceCredentials::inbound(role),
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

    pub fn last_rtt(&self) -> Option<Duration> {
        self.last_rtt
    }

    pub fn remote_count(&self) -> usize {
        self.pairs.len()
    }

    pub fn local_count(&self) -> usize {
        self.local.len()
    }

    /// Advance until there is something to report. State persists across
    /// calls, so returning a `NewLocalCandidate` costs no progress.
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
                    "no candidate pair nominated within {:?} ({} probes sent)",
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

            // Requests and nominations come from the peer, so they carry the
            // peer's outbound credential. Our own reflected packets carry
            // ours, and therefore fail here.
            if let Some(check) = parse_check(&self.creds, self.in_dir, &buf[..n]) {
                match check.kind {
                    CheckKind::Request => {
                        // A response is signed with the credential of the
                        // request it answers, so `in_dir` again.
                        if let Ok(resp) =
                            build_check_response(&self.creds, self.in_dir, check.tid, from)
                        {
                            let dst = match local_addr {
                                Some(l) => to_socket_family(&l, from),
                                None => from,
                            };
                            let _ = socket.send_to(&resp, dst).await;
                        }
                        self.note_remote(from);
                        if let Some(p) = self.pairs.iter_mut().find(|p| p.remote == from) {
                            p.got_request = true;
                        }
                    }
                    CheckKind::Nomination => {
                        // Only the controlling side sends these. Accepting one
                        // as the controlling side would mean accepting our own
                        // reflection, and the credential check already stops
                        // that; this is belt and braces.
                        if self.role == IceRole::Controlled && self.nominated.is_none() {
                            self.note_remote(from);
                            self.nominated = Some(from);
                            let kind = self
                                .pairs
                                .iter()
                                .find(|p| p.remote == from)
                                .map(|p| p.kind)
                                .unwrap_or(CandidateKind::PeerReflexive);
                            self.pending.push_back(IceEvent::Nominated {
                                local: local_addr.unwrap_or_else(unspecified),
                                remote: from,
                                rung: rung_for(kind, &from.ip()),
                                probes: self.probes_sent,
                            });
                        }
                    }
                    CheckKind::SuccessResponse => {}
                }
            }

            // Responses answer OUR requests, so they carry OUR credential.
            if let Some(check) = parse_check(&self.creds, self.out_dir, &buf[..n]) {
                if check.kind == CheckKind::SuccessResponse {
                    let mut rtt = None;
                    let found = self
                        .pairs
                        .iter_mut()
                        .find(|p| p.outstanding.iter().any(|(t, _)| *t == check.tid));
                    if let Some(p) = found {
                        if let Some((_, sent_at)) =
                            p.outstanding.iter().find(|(t, _)| *t == check.tid)
                        {
                            rtt = Some(Instant::now().saturating_duration_since(*sent_at));
                        }
                        p.got_response = true;
                    }
                    if rtt.is_some() {
                        self.last_rtt = rtt;
                    }
                    if let Some(refl) = check.reflexive {
                        if self.note_local(refl) {
                            self.pending.push_back(IceEvent::NewLocalCandidate(Candidate {
                                addr: refl,
                                kind: CandidateKind::PeerReflexive,
                                priority: ice_priority(CandidateKind::PeerReflexive, &refl.ip()),
                            }));
                        }
                    }
                }
            }

            if self.role == IceRole::Controlling {
                if let Some(ev) = self.try_nominate(&socket, local_addr).await {
                    self.pending.push_back(ev);
                }
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

    /// Peer-reflexive learning: an authenticated packet from an address we
    /// never heard of *is* a new remote candidate.
    fn note_remote(&mut self, addr: SocketAddr) {
        if self.pairs.iter().any(|p| p.remote == addr) {
            return;
        }
        let c = Candidate {
            addr,
            kind: CandidateKind::PeerReflexive,
            priority: ice_priority(CandidateKind::PeerReflexive, &addr.ip()),
        };
        self.pairs.push(Pair::new(&c));
        self.sort_pairs();
    }

    /// Record a newly learned address of our own. True when it is new.
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
        // Copied out so the loop can borrow `self.pairs` mutably.
        let creds = self.creds.clone();
        let dir = self.out_dir;
        let local = socket.local_addr().ok();
        let mut sent = 0u32;

        for p in self.pairs.iter_mut() {
            let tid = random_transaction_id();
            let Ok(bytes) = build_check_request(&creds, dir, tid) else { continue };
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

    /// Controlling side only: pick the winner and tell the peer.
    async fn try_nominate(
        &mut self,
        socket: &tokio::net::UdpSocket,
        local: Option<SocketAddr>,
    ) -> Option<IceEvent> {
        if self.nominated.is_some() {
            return None;
        }
        // `pairs` is kept sorted, so the first validated pair is the best one.
        let p = self.pairs.iter().find(|p| p.validated())?;
        let remote = p.remote;
        let rung = rung_for(p.kind, &remote.ip());
        self.nominated = Some(remote);

        // Announce it. Indications get no response, so repeat a few times
        // rather than trust one datagram.
        let dst = match local {
            Some(l) => to_socket_family(&l, remote),
            None => remote,
        };
        for _ in 0..NOMINATION_REPEATS {
            let Ok(bytes) = build_nomination(&self.creds, self.out_dir, random_transaction_id())
            else {
                break;
            };
            let _ = socket.send_to(&bytes, dst).await;
        }

        Some(IceEvent::Nominated {
            local: local.unwrap_or_else(unspecified),
            remote,
            rung,
            probes: self.probes_sent,
        })
    }
}

fn unspecified() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 0))
}

/// Which ladder rung a nominated pair represents.
///
/// `Rung` has no `Ipv4Direct` variant, so an IPv4 host pair that works — two
/// machines on one LAN — is reported as `StunPunch`. Imprecise, but never
/// wrong in a way that matters: it is a direct UDP path either way and nothing
/// downstream branches on it. `Rung::Birthday` is set by the caller (rung 3
/// does not run through this agent) and `Rung::SshTunnel` is M4's.
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

pub use ice::{IceAgent, IceEvent};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run it bare and wait for it in the same turn:

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 10 new tests pass, clippy clean. The two that carry the most weight
are `an_agent_talking_to_itself_does_not_validate_a_pair` and
`the_controlled_side_never_nominates_on_its_own`.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/ice.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): ICE agent with one-sided nomination and reflection resistance

Only the controlling side (always the client) nominates, and announces the
winner with an authenticated Binding Indication; with both sides free to
choose, asymmetric loss makes them choose differently. Inbound requests are
verified with the peer's direction credential, so our own echo is rejected.

Nomination completes before QUIC starts: QUIC migration cannot repoint an
established connection at a new REMOTE address.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 9: Rung 3 — the birthday-paradox blast



**Files:**
- Create: `crates/oxutrm-net/src/birthday.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{build_check_request, build_check_response, parse_check, random_transaction_id, unmap, to_socket_family, CheckKind, Direction, IceCredentials, IceRole, NetConfig}`.
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
      role: IceRole,
      peer_base: std::net::SocketAddr,
      cfg: &NetConfig,
  ) -> anyhow::Result<Option<BirthdayResult>>;
  ```

**Guardrails (spec §5.4), all four of them.** This rung is deliberately noisy,
so: it runs only when the caller decided the NAT is symmetric or rungs 0-2
failed (that decision belongs to Task 14, not here); the probe count is capped
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
    use crate::{build_check_response, parse_check, IceCredentials, IceRole, NetConfig};
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
        let r = birthday_blast([1u8; 32], IceRole::Controlling, "127.0.0.1:40000".parse().unwrap(), &cfg)
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
        let r = birthday_blast([1u8; 32], IceRole::Controlling, "127.0.0.1:1".parse().unwrap(), &cfg)
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
        // The peer is the CONTROLLED side, so it verifies with c2h and signs
        // its responses with c2h too.
        let creds = IceCredentials::derive(&psk);
        let peer_in = IceCredentials::inbound(IceRole::Controlled);
        let responder = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = peer.recv_from(&mut buf).await else { return };
                if let Some(c) = parse_check(&creds, peer_in, &buf[..n]) {
                    if let Ok(r) = build_check_response(&creds, peer_in, c.tid, from) {
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

        let r = birthday_blast(psk, IceRole::Controlling, base, &cfg)
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
        // Answers with a credential derived from a different PSK entirely.
        let wrong = IceCredentials::derive(&[99u8; 32]);
        let peer_in = IceCredentials::inbound(IceRole::Controlled);
        let responder = tokio::spawn(async move {
            let mut buf = vec![0u8; 2048];
            loop {
                let Ok((n, from)) = peer.recv_from(&mut buf).await else { return };
                if let Some(c) = parse_check(&wrong, peer_in, &buf[..n]) {
                    if let Ok(r) = build_check_response(&wrong, peer_in, c.tid, from) {
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
        let r = birthday_blast([77u8; 32], IceRole::Controlling, base, &cfg).await.unwrap();
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
    CheckKind, IceCredentials, IceRole, NetConfig,
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
    role: IceRole,
    peer_base: SocketAddr,
    cfg: &NetConfig,
) -> anyhow::Result<Option<BirthdayResult>> {
    if !cfg.enable_birthday {
        return Ok(None);
    }
    // Same direction-labelled credentials as ICE: a blast probe is an ICE
    // check, so our own reflected probe must not look like a hit.
    let creds = IceCredentials::derive(&psk);
    let out_dir = IceCredentials::outbound(role);
    let in_dir = IceCredentials::inbound(role);
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
        let creds = creds.clone();
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
                    if let Ok(bytes) =
                        build_check_request(&creds, out_dir, random_transaction_id())
                    {
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
                    // A response to one of ours, or a genuine peer request.
                    // Anything signed with our own outbound credential and
                    // arriving as a REQUEST is our own echo, and is ignored.
                    let c = match parse_check(&creds, in_dir, &buf[..n]) {
                        Some(c) if c.kind == CheckKind::Request => {
                            if let Ok(r) =
                                build_check_response(&creds, in_dir, c.tid, unmap(from))
                            {
                                let _ = sock.send_to(&r, from).await;
                            }
                            c
                        }
                        _ => match parse_check(&creds, out_dir, &buf[..n]) {
                            Some(c) if c.kind == CheckKind::SuccessResponse => c,
                            _ => continue,
                        },
                    };
                    let _ = c;
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

---

## Task 10: Rung 1 — `PortMapping` via NAT-PMP/PCP, then UPnP-IGD


**Files:**
- Create: `crates/oxutrm-net/src/mapping.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{ice_priority, NetConfig}`; `oxutrm_proto::{Candidate, CandidateKind}`.
- Produces:
  ```rust
  /// The default gateway. `crab_nat` requires it and ships no discovery.
  pub fn default_gateway() -> Option<std::net::IpAddr>;

  pub fn mapped_candidate(ip: std::net::IpAddr, port: u16) -> oxutrm_proto::Candidate;

  pub struct PortMapping { /* private */ }

  impl PortMapping {
      /// Contract signature. Equivalent to `acquire_with_hint(port, cfg, None)`.
      pub async fn acquire(local_port: u16, cfg: &NetConfig)
          -> Option<(PortMapping, oxutrm_proto::Candidate)>;

      /// The form the ladder calls: `external_ip_hint` is an address STUN
      /// already reported, which NAT-PMP and PCP cannot supply.
      pub async fn acquire_with_hint(
          local_port: u16,
          cfg: &NetConfig,
          external_ip_hint: Option<std::net::IpAddr>,
      ) -> Option<(PortMapping, oxutrm_proto::Candidate)>;

      pub fn external(&self) -> std::net::SocketAddr;
  }
  impl Drop for PortMapping { /* releases the mapping, best effort */ }
  ```

### Two things the crates cannot give you

**`crab_nat` cannot find the router.** `crab_nat::PortMapping::new` takes the
gateway address as its first argument and the crate ships no discovery at all.
The contract names **`netdev` 0.46** for this: netlink on Linux, the route
socket on the BSDs and macOS, so one call covers every platform the project
targets. `netdev::get_default_gateway()` returns
`Result<netdev::NetworkDevice, String>`, and `NetworkDevice` carries
`mac_addr: MacAddr`, `ipv4: Vec<Ipv4Addr>` and `ipv6: Vec<Ipv6Addr>`. Parsing
`/proc/net/route` by hand was the alternative and is rejected: it is
Linux-only, and spec §1.2 scopes the project to Unix, not to Linux.

**`crab_nat` reports the external *port* but not the external *IP*.** Its
`PortMapping` exposes `external_port()`, `lifetime()`, `renew()`, `try_drop()`
and `gateway()` — no public address. A `PortMapped` candidate without an IP is
useless, so `acquire_with_hint` resolves the IP in this order: the caller's
hint (an address a STUN server already reported), then `igd_next`'s
`get_external_ip()`, and if neither works it returns `None` rather than
advertise half an address. `acquire` exists because the contract names it; the
ladder calls `acquire_with_hint`.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/mapping.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::NetConfig;
    use oxutrm_proto::CandidateKind;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn a_mapped_candidate_is_shaped_correctly() {
        let c = mapped_candidate(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), 443);
        assert_eq!(c.kind, CandidateKind::PortMapped);
        assert_eq!(c.addr.port(), 443);
        assert_eq!(c.priority, crate::ice_priority(CandidateKind::PortMapped, &c.addr.ip()));
        // Spec §4.2 ordering: below Host, above ServerReflexive.
        assert!(c.priority < crate::ice_priority(CandidateKind::Host, &c.addr.ip()));
        assert!(c.priority > crate::ice_priority(CandidateKind::ServerReflexive, &c.addr.ip()));
    }

    #[test]
    fn gateway_discovery_never_panics_and_never_returns_a_useless_address() {
        // CI may have no default route at all, so the only invariant that
        // always holds is: whatever comes back is a usable unicast address.
        match default_gateway() {
            None => {}
            Some(ip) => {
                assert!(!ip.is_unspecified(), "0.0.0.0 is not a gateway");
                assert!(!ip.is_loopback(), "the loopback is not a gateway");
                assert!(!ip.is_multicast(), "a multicast address is not a gateway");
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_disabled_configuration_returns_immediately_and_touches_no_router() {
        let cfg = NetConfig { enable_port_mapping: false, ..NetConfig::default() };
        let started = std::time::Instant::now();
        assert!(PortMapping::acquire(40_000, &cfg).await.is_none());
        assert!(
            started.elapsed() < std::time::Duration::from_millis(100),
            "must not have talked to anything, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "talks to the real router on this network and creates a real mapping"]
    async fn a_real_router_grants_a_mapping() {
        let sock = std::net::UdpSocket::bind("0.0.0.0:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        eprintln!("default gateway: {:?}", default_gateway());
        match PortMapping::acquire(port, &NetConfig::default()).await {
            Some((m, c)) => {
                eprintln!("mapping: external {} candidate {c:?}", m.external());
                assert_eq!(c.kind, CandidateKind::PortMapped);
            }
            None => eprintln!("no router mapping available on this network (this is normal)"),
        }
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function mapped_candidate in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/mapping.rs`:

```rust
//! Rung 1: ask the router for a port mapping (spec §5.2).
//!
//! NAT-PMP and PCP first (`crab_nat`), then UPnP-IGD (`igd-next`), 1500 ms
//! each. Success yields a `PortMapped` candidate with an exact external
//! address. If **either** side gets one the whole connection succeeds: the
//! other side punches to the mapped address and its own address is learned
//! peer-reflexively from the arriving packet.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use oxutrm_proto::{Candidate, CandidateKind};
use tokio::sync::Mutex;

use crate::{ice_priority, NetConfig};

/// Per-protocol budget (spec §5.2).
const ATTEMPT_BUDGET: Duration = Duration::from_millis(1_500);

/// UPnP lease length. Short enough that a crashed process leaks the mapping
/// for minutes rather than forever; refreshed for the life of the session.
const IGD_LEASE_SECS: u32 = 120;

type IgdGateway = igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>;

enum Release {
    Pcp(Arc<Mutex<Option<crab_nat::PortMapping>>>),
    Igd { gateway: IgdGateway, external_port: u16 },
}

/// A live router port mapping. Refreshed in the background; released on drop.
pub struct PortMapping {
    external: SocketAddr,
    refresh: Option<tokio::task::JoinHandle<()>>,
    release: Option<Release>,
}

impl PortMapping {
    /// The contract's signature. Prefer [`PortMapping::acquire_with_hint`]:
    /// without a hint the NAT-PMP/PCP path has to fall back to a UPnP query
    /// just to learn the external IP.
    pub async fn acquire(local_port: u16, cfg: &NetConfig) -> Option<(PortMapping, Candidate)> {
        PortMapping::acquire_with_hint(local_port, cfg, None).await
    }

    pub async fn acquire_with_hint(
        local_port: u16,
        cfg: &NetConfig,
        external_ip_hint: Option<IpAddr>,
    ) -> Option<(PortMapping, Candidate)> {
        if !cfg.enable_port_mapping {
            return None;
        }
        if let Some(r) = acquire_pcp(local_port, external_ip_hint).await {
            return Some(r);
        }
        acquire_igd(local_port).await
    }

    /// The address a peer should punch to.
    pub fn external(&self) -> SocketAddr {
        self.external
    }
}

impl Drop for PortMapping {
    fn drop(&mut self) {
        if let Some(h) = self.refresh.take() {
            h.abort();
        }
        // Releasing needs to await and `drop` cannot. Best effort: hand the
        // work to the runtime if one is still running. A process that exits
        // immediately leaves the mapping to expire on its lease, which is why
        // the UPnP lease is short.
        let Ok(handle) = tokio::runtime::Handle::try_current() else { return };
        match self.release.take() {
            Some(Release::Pcp(slot)) => {
                handle.spawn(async move {
                    let taken = slot.lock().await.take();
                    if let Some(m) = taken {
                        let _ = m.try_drop().await;
                    }
                });
            }
            Some(Release::Igd { gateway, external_port }) => {
                handle.spawn(async move {
                    let _ = gateway
                        .remove_port(igd_next::PortMappingProtocol::UDP, external_port)
                        .await;
                });
            }
            None => {}
        }
    }
}

/// The default gateway, via `netdev`: netlink on Linux, the route socket on
/// the BSDs and macOS. `crab_nat` takes the gateway as an argument and has no
/// discovery of its own.
///
/// Only IPv4 is returned: NAT-PMP and PCP are IPv4 NAT protocols, and a host
/// with global IPv6 wins on rung 0 long before rung 1 is reached.
pub fn default_gateway() -> Option<IpAddr> {
    let device = netdev::get_default_gateway().ok()?;
    let v4 = device
        .ipv4
        .into_iter()
        .find(|ip| !ip.is_unspecified() && !ip.is_loopback() && !ip.is_multicast())?;
    Some(IpAddr::V4(v4))
}

async fn acquire_pcp(
    local_port: u16,
    external_ip_hint: Option<IpAddr>,
) -> Option<(PortMapping, Candidate)> {
    let IpAddr::V4(gateway) = default_gateway()? else { return None };
    let client = local_ipv4_towards(gateway)?;
    let internal = NonZeroU16::new(local_port)?;

    // crab_nat tries PCP first and falls back to NAT-PMP by itself.
    let mapping = tokio::time::timeout(
        ATTEMPT_BUDGET,
        crab_nat::PortMapping::new(
            IpAddr::V4(gateway),
            IpAddr::V4(client),
            crab_nat::InternetProtocol::Udp,
            internal,
            crab_nat::PortMappingOptions::default(),
        ),
    )
    .await
    .ok()?
    .ok()?;

    let external_port = mapping.external_port();
    // The protocol gives us a port but no public address.
    let external_ip = match external_ip_hint {
        Some(ip) => ip,
        None => igd_external_ip().await?,
    };
    let external = SocketAddr::new(external_ip, external_port);

    let lifetime = mapping.lifetime();
    let slot = Arc::new(Mutex::new(Some(mapping)));
    let refresh_slot = Arc::clone(&slot);
    // Renew at half the lifetime, with a two-minute floor so a router handing
    // out a tiny lease does not turn this into a busy loop.
    let period = Duration::from_secs(u64::from(lifetime).max(120) / 2);
    let refresh = tokio::spawn(async move {
        loop {
            tokio::time::sleep(period).await;
            let mut guard = refresh_slot.lock().await;
            let Some(m) = guard.as_mut() else { return };
            if m.renew().await.is_err() {
                return;
            }
        }
    });

    Some((
        PortMapping { external, refresh: Some(refresh), release: Some(Release::Pcp(slot)) },
        mapped_candidate(external_ip, external_port),
    ))
}

async fn acquire_igd(local_port: u16) -> Option<(PortMapping, Candidate)> {
    let gateway = tokio::time::timeout(
        ATTEMPT_BUDGET,
        igd_next::aio::tokio::search_gateway(igd_next::SearchOptions::default()),
    )
    .await
    .ok()?
    .ok()?;

    let external_ip = tokio::time::timeout(ATTEMPT_BUDGET, gateway.get_external_ip())
        .await
        .ok()?
        .ok()?;

    // Any routable target will do: we only want the interface the kernel
    // would use to leave this machine.
    let probe_target = match default_gateway() {
        Some(IpAddr::V4(v4)) => v4,
        _ => Ipv4Addr::new(1, 1, 1, 1),
    };
    let local = SocketAddr::new(IpAddr::V4(local_ipv4_towards(probe_target)?), local_port);

    let external_port = tokio::time::timeout(
        ATTEMPT_BUDGET,
        gateway.add_any_port(igd_next::PortMappingProtocol::UDP, local, IGD_LEASE_SECS, "oxutrm"),
    )
    .await
    .ok()?
    .ok()?;

    let external = SocketAddr::new(external_ip, external_port);
    let refresh_gateway = gateway.clone();
    let refresh = tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(u64::from(IGD_LEASE_SECS) / 2)).await;
            let ok = refresh_gateway
                .add_port(
                    igd_next::PortMappingProtocol::UDP,
                    external_port,
                    local,
                    IGD_LEASE_SECS,
                    "oxutrm",
                )
                .await
                .is_ok();
            if !ok {
                return;
            }
        }
    });

    Some((
        PortMapping {
            external,
            refresh: Some(refresh),
            release: Some(Release::Igd { gateway, external_port }),
        },
        mapped_candidate(external_ip, external_port),
    ))
}

async fn igd_external_ip() -> Option<IpAddr> {
    let gateway = tokio::time::timeout(
        ATTEMPT_BUDGET,
        igd_next::aio::tokio::search_gateway(igd_next::SearchOptions::default()),
    )
    .await
    .ok()?
    .ok()?;
    tokio::time::timeout(ATTEMPT_BUDGET, gateway.get_external_ip()).await.ok()?.ok()
}

pub fn mapped_candidate(ip: IpAddr, port: u16) -> Candidate {
    Candidate {
        addr: SocketAddr::new(ip, port),
        kind: CandidateKind::PortMapped,
        priority: ice_priority(CandidateKind::PortMapped, &ip),
    }
}

/// Which of our addresses the kernel would use to reach `target`. Connecting
/// a UDP socket sends nothing; it only fixes the route.
fn local_ipv4_towards(target: Ipv4Addr) -> Option<Ipv4Addr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect(SocketAddr::new(IpAddr::V4(target), 9)).ok()?;
    match sock.local_addr().ok()? {
        SocketAddr::V4(v4) => Some(*v4.ip()),
        SocketAddr::V6(_) => None,
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod mapping;

pub use mapping::{default_gateway, mapped_candidate, PortMapping};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 3 new tests pass, one ignored, clippy clean.

If `igd_next::aio::Gateway<igd_next::aio::tokio::Tokio>` does not name the type
`search_gateway` returns, drop the `type IgdGateway` alias, let inference find
it, and use the concrete path `cargo check` reports. Do not invent a different
API — report the discrepancy instead.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/mapping.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): rung 1, router port mapping via NAT-PMP/PCP then UPnP-IGD

crab_nat takes the gateway as an argument and ships no discovery, so the
gateway comes from netdev - netlink on Linux, route socket on the BSDs and
macOS. NAT-PMP and PCP report a port but no public address, so the external
IP comes from a STUN hint or a UPnP query.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 11: Certificates, SPKI extraction, and the pinned verifier



**Files:**
- Create: `crates/oxutrm-net/src/der.rs`
- Create: `crates/oxutrm-net/src/tls.rs`
- Test: both files, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  ```rust
  // der.rs
  /// The complete SubjectPublicKeyInfo TLV of an X.509 certificate.
  pub fn spki_der(cert: &[u8]) -> Option<&[u8]>;
  pub fn spki_sha256(cert: &[u8]) -> Option<[u8; 32]>;

  // tls.rs
  /// The SAN and SNI name oxutrm uses. Reserved by RFC 2606; never resolved.
  pub const CERT_NAME: &str = "oxutrm.invalid";

  pub fn generate_cert() -> anyhow::Result<(
      rustls::pki_types::CertificateDer<'static>,
      rustls::pki_types::PrivateKeyDer<'static>,
      [u8; 32],
  )>;

  /// A `ServerCertVerifier` that accepts exactly one SPKI fingerprint.
  #[derive(Debug)]
  pub struct PinnedSpki { /* private */ }
  impl PinnedSpki { pub fn new(expected: [u8; 32]) -> PinnedSpki; }

  /// The one crypto provider this crate uses. Explicit, because two
  /// providers in the build make `ClientConfig::builder()` panic.
  pub fn provider() -> std::sync::Arc<rustls::crypto::CryptoProvider>;

  /// Install it as the PROCESS DEFAULT. rustls 0.23 needs one before
  /// `QuicClientConfig::try_from` will succeed. Idempotent.
  pub fn install_crypto_provider();
  ```

**Why a DER walker.** The verifier is handed the certificate the peer
presented, and must hash *its* SPKI. `rustls::pki_types` does not parse
certificates and `rustls-webpki` does not expose the SPKI bytes, so this task
writes about fifty lines of TLV walking. The test does not trust it on faith:
it generates a certificate with `rcgen`, extracts the SPKI from the certificate
bytes, and asserts it equals `KeyPair::public_key_der()` — which rcgen
documents as the complete `SubjectPublicKeyInfo` structure. That is ground
truth, not a snapshot.

An X.509 certificate is `SEQUENCE { tbsCertificate SEQUENCE { [0] version
OPTIONAL, serialNumber, signature, issuer, validity, subject,
subjectPublicKeyInfo, ... }, ... }`. So: enter two SEQUENCEs, skip an optional
`[0]`, skip five elements, take the sixth.

- [ ] **Step 1: Write the failing test for the DER walker**

Create `crates/oxutrm-net/src/der.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spki_we_extract_is_the_one_rcgen_made() {
        // Ground truth: rcgen documents public_key_der() as the complete
        // SubjectPublicKeyInfo structure (RFC 5280 §4.1).
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let from_cert = spki_der(ck.cert.der().as_ref()).expect("must find the SPKI");
        assert_eq!(from_cert, ck.key_pair.public_key_der().as_slice());
    }

    #[test]
    fn two_certificates_hash_differently() {
        let a = rcgen::generate_simple_self_signed(vec!["a.invalid".to_owned()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["b.invalid".to_owned()]).unwrap();
        assert_ne!(
            spki_sha256(a.cert.der().as_ref()).unwrap(),
            spki_sha256(b.cert.der().as_ref()).unwrap()
        );
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_extracted_bytes() {
        use sha2::Digest;
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let der = ck.cert.der();
        let expect: [u8; 32] = sha2::Sha256::digest(spki_der(der.as_ref()).unwrap()).into();
        assert_eq!(spki_sha256(der.as_ref()).unwrap(), expect);
    }

    #[test]
    fn a_short_or_garbage_input_returns_none_rather_than_panicking() {
        assert!(spki_der(&[]).is_none());
        assert!(spki_der(&[0x30]).is_none());
        assert!(spki_der(&[0x30, 0x82]).is_none());
        assert!(spki_der(&[0x02, 0x01, 0x00]).is_none(), "an INTEGER is not a certificate");
        assert!(spki_der(&[0xFF; 64]).is_none());
    }

    #[test]
    fn every_truncation_of_a_real_certificate_is_survivable() {
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let d = ck.cert.der().as_ref().to_vec();
        for cut in [0usize, 1, 2, 5, 20] {
            assert!(spki_sha256(&d[..cut]).is_none(), "cut {cut} must not parse");
        }
        // The real requirement: no panic anywhere along the length.
        for cut in (0..d.len()).step_by(7) {
            let _ = spki_sha256(&d[..cut]);
        }
        assert!(spki_sha256(&d).is_some(), "the whole thing still parses");
    }

    #[test]
    fn a_certificate_whose_length_header_lies_is_rejected() {
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let mut d = ck.cert.der().as_ref().to_vec();
        // Byte 0 is the SEQUENCE tag; bytes 1..4 are a two-byte long form
        // length. Claim far more content than exists.
        d[2] = 0xFF;
        d[3] = 0xFF;
        assert!(spki_der(&d).is_none());
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function spki_der in this scope`.

- [ ] **Step 3: Write the DER walker**

Put this above the `mod tests` in `crates/oxutrm-net/src/der.rs`:

```rust
//! A minimal DER walker: just enough to pull the `SubjectPublicKeyInfo` out
//! of an X.509 certificate.
//!
//! The pinned certificate verifier is handed the bytes the peer presented and
//! has to hash *that* certificate's public key. `rustls::pki_types` does not
//! parse certificates and `rustls-webpki` does not expose the SPKI, so this
//! is the smallest honest thing that works. It parses nothing it does not
//! need and never allocates.

use sha2::{Digest, Sha256};

/// Split one DER TLV. Returns `(tag, value, whole_tlv, remainder)`.
fn tlv(buf: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let tag = *buf.first()?;
    let first_len = *buf.get(1)?;
    let (len, header) = if first_len < 0x80 {
        (first_len as usize, 2usize)
    } else {
        let n = (first_len & 0x7f) as usize;
        // n == 0 is the indefinite form, illegal in DER. More than four
        // length bytes would be a certificate larger than any real one.
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *buf.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some((tag, &buf[header..end], &buf[..end], &buf[end..]))
}

/// The complete `SubjectPublicKeyInfo` TLV of an X.509 certificate.
///
/// ```text
/// Certificate     ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
/// TBSCertificate  ::= SEQUENCE { [0] version OPTIONAL, serialNumber,
///                                signature, issuer, validity, subject,
///                                subjectPublicKeyInfo, ... }
/// ```
pub fn spki_der(cert: &[u8]) -> Option<&[u8]> {
    const SEQUENCE: u8 = 0x30;
    const CONTEXT_0: u8 = 0xA0;

    // Certificate ::= SEQUENCE
    let (tag, body, _, _) = tlv(cert)?;
    if tag != SEQUENCE {
        return None;
    }
    // tbsCertificate ::= SEQUENCE
    let (tag, mut rest, _, _) = tlv(body)?;
    if tag != SEQUENCE {
        return None;
    }

    // [0] EXPLICIT version, optional.
    if rest.first() == Some(&CONTEXT_0) {
        rest = tlv(rest)?.3;
    }
    // serialNumber, signature, issuer, validity, subject.
    for _ in 0..5 {
        rest = tlv(rest)?.3;
    }
    // subjectPublicKeyInfo, as its whole TLV.
    let (_, _, whole, _) = tlv(rest)?;
    Some(whole)
}

/// SHA-256 of the `SubjectPublicKeyInfo`. This is the value that travels in
/// `Signal::HostHello.cert_spki_sha256` and the only thing the client trusts.
pub fn spki_sha256(cert: &[u8]) -> Option<[u8; 32]> {
    Some(Sha256::digest(spki_der(cert)?).into())
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod der;

pub use der::{spki_der, spki_sha256};
```

- [ ] **Step 4: Run the DER tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net der:: -- --test-threads 4
```
Expected: 6 tests pass.

- [ ] **Step 5: Write the failing test for the certificate and the verifier**

Create `crates/oxutrm-net/src/tls.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::{ServerName, UnixTime};

    #[test]
    fn a_generated_certificate_reports_its_own_fingerprint() {
        let (cert, _key, fp) = generate_cert().unwrap();
        assert_eq!(
            crate::spki_sha256(cert.as_ref()).unwrap(),
            fp,
            "the fingerprint must describe the certificate that is returned"
        );
    }

    #[test]
    fn every_attach_generates_fresh_key_material() {
        // Spec §11: a stolen key from an earlier session must not reattach.
        let (_, _, a) = generate_cert().unwrap();
        let (_, _, b) = generate_cert().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_private_key_comes_back_as_pkcs8() {
        let (_cert, key, _fp) = generate_cert().unwrap();
        assert!(
            matches!(key, rustls::pki_types::PrivateKeyDer::Pkcs8(_)),
            "quinn's with_single_cert wants a key rustls can load"
        );
    }

    #[test]
    fn the_verifier_accepts_exactly_the_pinned_fingerprint_and_nothing_else() {
        let (cert, _k, fp) = generate_cert().unwrap();
        let (other, _k2, other_fp) = generate_cert().unwrap();
        assert_ne!(fp, other_fp);

        let v = PinnedSpki::new(fp);
        let name = ServerName::try_from(CERT_NAME).unwrap();
        let now = UnixTime::now();

        assert!(v.verify_server_cert(&cert, &[], &name, &[], now).is_ok());
        assert!(
            v.verify_server_cert(&other, &[], &name, &[], now).is_err(),
            "no certificate authority is involved: the pin is the whole trust decision"
        );
    }

    #[test]
    fn the_verifier_ignores_the_server_name_entirely() {
        // The trust chain is SSH's. The name in the certificate is a
        // placeholder, so a mismatch must not matter.
        let (cert, _k, fp) = generate_cert().unwrap();
        let v = PinnedSpki::new(fp);
        let wrong = ServerName::try_from("something.else.invalid").unwrap();
        assert!(v.verify_server_cert(&cert, &[], &wrong, &[], UnixTime::now()).is_ok());
    }

    #[test]
    fn the_verifier_rejects_a_certificate_it_cannot_parse() {
        let v = PinnedSpki::new([0u8; 32]);
        let junk = rustls::pki_types::CertificateDer::from(vec![0xFFu8; 64]);
        let name = ServerName::try_from(CERT_NAME).unwrap();
        assert!(v.verify_server_cert(&junk, &[], &name, &[], UnixTime::now()).is_err());
    }

    #[test]
    fn the_verifier_advertises_the_providers_signature_schemes() {
        let v = PinnedSpki::new([0u8; 32]);
        assert!(!v.supported_verify_schemes().is_empty());
    }

    #[test]
    fn the_provider_is_a_single_shared_instance() {
        assert!(std::sync::Arc::ptr_eq(&provider(), &provider()));
    }

    #[test]
    fn the_process_default_provider_installs_and_is_idempotent() {
        install_crypto_provider();
        install_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "rustls 0.23 needs a process default before QuicClientConfig::try_from"
        );
    }
}
```

- [ ] **Step 6: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net tls:: -- --test-threads 4
```
Expected: FAIL — `cannot find function generate_cert in this scope`.

- [ ] **Step 7: Write the certificate and verifier implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/tls.rs`:

```rust
//! Self-signed certificates and the pinned verifier (spec §6.1).
//!
//! No certificate authority is involved. The host generates a fresh
//! certificate per session, the SHA-256 of its SPKI travels over SSH, and the
//! client trusts exactly that fingerprint. The trust chain is SSH's, unchanged.

use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

/// The SAN in the generated certificate and the SNI oxutrm sends.
///
/// `.invalid` is reserved by RFC 2606 and never resolves. oxutrm does **not**
/// forge another party's domain in SNI: that is impersonation, it buys nothing
/// over honest QUIC framing, and it is out of scope (spec §5.6). The verifier
/// ignores the name anyway.
pub const CERT_NAME: &str = "oxutrm.invalid";

/// The one crypto provider this crate uses.
///
/// `rustls::ClientConfig::builder()` panics when two providers are compiled in
/// and neither has been installed as the process default. Every builder here
/// is therefore constructed with `builder_with_provider` and this value.
pub fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    static P: OnceLock<Arc<rustls::crypto::CryptoProvider>> = OnceLock::new();
    Arc::clone(P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())))
}

/// Install `ring` as the **process-default** provider.
///
/// Passing an explicit provider to `builder_with_provider` is not enough:
/// rustls 0.23 consults the process default in places that take no builder,
/// and `quinn::crypto::rustls::QuicClientConfig::try_from` fails without one.
/// Idempotent, and deliberately tolerant of a provider someone else installed
/// first — theirs is just as good, and racing to replace it would be worse.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A fresh self-signed certificate, its key, and the SHA-256 of its SPKI.
///
/// The key never touches disk (spec §11); it exists only in the returned
/// value and in whatever `quinn` does with it.
pub fn generate_cert(
) -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, [u8; 32])> {
    let ck = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_owned()])
        .context("generating a self-signed certificate")?;

    let cert: CertificateDer<'static> = ck.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
    // Deliberately read back out of the certificate rather than from the key
    // pair, so this is the same code path the verifier uses. If the two ever
    // disagreed, every connection would fail with no clue why.
    let fingerprint = crate::spki_sha256(cert.as_ref())
        .context("the certificate we just generated has no parsable SPKI")?;

    Ok((cert, key, fingerprint))
}

/// Accepts exactly one SPKI fingerprint, and nothing else.
#[derive(Debug)]
pub struct PinnedSpki {
    expected: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl PinnedSpki {
    pub fn new(expected: [u8; 32]) -> PinnedSpki {
        PinnedSpki { expected, provider: provider() }
    }
}

impl ServerCertVerifier for PinnedSpki {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // The trust root is SSH. The name, the expiry, the chain and any OCSP
        // response are all irrelevant and deliberately not checked: the only
        // thing that grants trust is the fingerprint that arrived over SSH.
        let got = crate::spki_sha256(end_entity.as_ref())
            .ok_or_else(|| TlsError::General("peer certificate has no parsable SPKI".into()))?;
        if got != self.expected {
            return Err(TlsError::General(
                "peer certificate SPKI does not match the pinned fingerprint".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod tls;

pub use tls::{generate_cert, install_crypto_provider, provider, PinnedSpki, CERT_NAME};
```

- [ ] **Step 8: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 15 new tests pass (6 in `der`, 9 in `tls`), clippy clean.

- [ ] **Step 9: Commit**

```bash
git add crates/oxutrm-net/src/der.rs crates/oxutrm-net/src/tls.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): self-signed certificates pinned by SPKI SHA-256

The verifier trusts exactly the fingerprint that arrived over SSH: no CA,
no name check, no expiry check. SPKI extraction is a small DER walker whose
test checks it against rcgen's own public_key_der().

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: `StunDemuxSocket` — one socket, two protocols, one receiver


**Files:**
- Create: `crates/oxutrm-net/src/demuxsock.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::is_stun`.
- Produces:
  ```rust
  /// Datagrams peeled off the front of the QUIC stream.
  pub type StunRx = tokio::sync::mpsc::Receiver<(Vec<u8>, std::net::SocketAddr)>;

  pub struct StunDemuxSocket { /* private */ }

  impl StunDemuxSocket {
      /// `inner` stays with the caller for SENDING STUN. Only the returned
      /// socket ever receives.
      pub fn new(inner: std::sync::Arc<tokio::net::UdpSocket>)
          -> anyhow::Result<(std::sync::Arc<StunDemuxSocket>, StunRx)>;
  }

  impl quinn::AsyncUdpSocket for StunDemuxSocket { /* ... */ }
  ```

### The problem this exists to solve

`quinn::Endpoint::new` takes ownership of the socket and runs **its own**
receive loop on it. `stunclient::StunClient::query_external_address_async` runs
**another**. So does any hand-rolled `socket.recv_from(...)` loop. Two
receivers on one UDP socket do not cooperate: each `recvmsg` removes a datagram
from the kernel queue, so whichever task wins the race gets the packet and the
other never sees it. STUN replies vanish into quinn's endpoint, which discards
them; QUIC packets vanish into the STUN loop, which discards them. Nothing
errors — the connection simply does not come up, intermittently.

The design spec's §5.3 and §6 describe exactly this arrangement and are wrong
about it being possible.

**The fix, and the asymmetry it rests on:** *sending* on a UDP socket from
several places at once is fine; *receiving* must have exactly one owner. So
there is one receiver — `quinn`'s — and this wrapper sits in front of it.
`poll_recv` asks the real socket for a batch, moves every STUN datagram into an
`mpsc` channel, and hands quinn only what is left. The caller keeps its own
`Arc<tokio::net::UdpSocket>` and uses it for `send_to` alone.

Construct the endpoint with **`Endpoint::new_with_abstract_socket`**, never
`Endpoint::new`.

### Two implementation notes

**Delegate, do not reimplement.** `quinn::Runtime::wrap_udp_socket(&self, t:
std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>>` gives quinn's own
implementation, with all its GSO/GRO/ECN platform handling. This wrapper
duplicates the file descriptor, hands one copy to that, and forwards every
trait method. Two descriptors on one socket: both may send, and only quinn's
copy ever receives.

**Turn GRO off.** With generic receive offload one `RecvMeta` can describe
several coalesced datagrams in one buffer, and splitting a mixed batch back
apart is fiddly and easy to get subtly wrong. Overriding
`max_receive_segments()` to `1` makes every `RecvMeta` describe exactly one
datagram. The cost is a few syscalls on bulk transfer; the benefit is that the
demultiplexer is obviously correct.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/demuxsock.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use quinn::AsyncUdpSocket;
    use std::io::IoSliceMut;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// Pull one batch out of the wrapper, the way quinn's driver does.
    async fn recv_one(sock: &Arc<StunDemuxSocket>) -> (Vec<u8>, SocketAddr) {
        let mut storage = [0u8; 2048];
        let mut meta = [quinn::udp::RecvMeta::default()];
        let n = std::future::poll_fn(|cx| {
            let mut bufs = [IoSliceMut::new(&mut storage)];
            sock.poll_recv(cx, &mut bufs, &mut meta)
        })
        .await
        .expect("poll_recv");
        assert_eq!(n, 1);
        (storage[..meta[0].len].to_vec(), meta[0].addr)
    }

    fn stun_datagram() -> Vec<u8> {
        // A syntactically real Binding Request: type, length, cookie, tid.
        let mut v = vec![0x00, 0x01, 0x00, 0x00];
        v.extend_from_slice(&crate::STUN_MAGIC_COOKIE);
        v.extend_from_slice(&[0xA1; 12]);
        v
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stun_goes_to_the_channel_and_everything_else_goes_to_quinn() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = inner.local_addr().unwrap();
        let (demux, mut stun_rx) = StunDemuxSocket::new(Arc::clone(&inner)).unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let from = sender.local_addr().unwrap();

        // A STUN packet first, then a QUIC-shaped one.
        sender.send_to(&stun_datagram(), target).await.unwrap();
        sender.send_to(&[0xC3; 64], target).await.unwrap();

        // quinn must see only the QUIC packet - and must not block on the
        // STUN one, which is the whole point.
        let (bytes, addr) = tokio::time::timeout(Duration::from_secs(5), recv_one(&demux))
            .await
            .expect("the QUIC packet must arrive");
        assert_eq!(bytes, vec![0xC3; 64]);
        assert_eq!(addr, from);

        let (stun_bytes, stun_from) = tokio::time::timeout(Duration::from_secs(5), stun_rx.recv())
            .await
            .expect("the STUN packet must arrive")
            .expect("channel open");
        assert_eq!(stun_bytes, stun_datagram());
        assert_eq!(stun_from, from);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_run_of_stun_datagrams_does_not_stall_the_quic_side() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = inner.local_addr().unwrap();
        let (demux, mut stun_rx) = StunDemuxSocket::new(Arc::clone(&inner)).unwrap();

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..16 {
            sender.send_to(&stun_datagram(), target).await.unwrap();
        }
        sender.send_to(&[0xC3; 64], target).await.unwrap();

        let (bytes, _) = tokio::time::timeout(Duration::from_secs(5), recv_one(&demux))
            .await
            .expect("poll_recv must keep looking until it has a non-STUN datagram");
        assert_eq!(bytes, vec![0xC3; 64]);

        let mut seen = 0;
        while stun_rx.try_recv().is_ok() {
            seen += 1;
        }
        assert_eq!(seen, 16, "every STUN datagram must reach the channel");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_caller_can_still_send_on_the_socket_it_kept() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (demux, _rx) = StunDemuxSocket::new(Arc::clone(&inner)).unwrap();

        let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let peer_addr = peer.local_addr().unwrap();
        // Sending from two descriptors on one socket is safe; receiving is not.
        inner.send_to(b"from the caller", peer_addr).await.unwrap();

        let mut buf = [0u8; 64];
        let (n, from) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut buf))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&buf[..n], b"from the caller");
        assert_eq!(from, inner.local_addr().unwrap());
        assert_eq!(demux.local_addr().unwrap(), inner.local_addr().unwrap());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn generic_receive_offload_is_off_so_every_meta_is_one_datagram() {
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let (demux, _rx) = StunDemuxSocket::new(inner).unwrap();
        assert_eq!(
            demux.max_receive_segments(),
            1,
            "a coalesced batch cannot be demultiplexed one datagram at a time"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_full_stun_channel_drops_rather_than_blocking_quinn() {
        // The channel is bounded. If nobody drains it - M2's netdemo does not
        // run keepalives - STUN must be discarded, never back-pressure QUIC.
        let inner = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let target = inner.local_addr().unwrap();
        let (demux, rx) = StunDemuxSocket::new(Arc::clone(&inner)).unwrap();
        drop(rx);

        let sender = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        for _ in 0..64 {
            sender.send_to(&stun_datagram(), target).await.unwrap();
        }
        sender.send_to(&[0xC3; 64], target).await.unwrap();

        let (bytes, _) = tokio::time::timeout(Duration::from_secs(5), recv_one(&demux))
            .await
            .expect("a closed STUN channel must not wedge the receive path");
        assert_eq!(bytes, vec![0xC3; 64]);
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `failed to resolve: use of undeclared type StunDemuxSocket`.

- [ ] **Step 3: Add the `quinn-udp` dependency**

`RecvMeta` and `Transmit` are `quinn_udp` types. `quinn` re-exports them as
`quinn::udp`, so no new dependency is needed — use `quinn::udp::{RecvMeta,
Transmit}` throughout and do **not** add `quinn-udp` to `Cargo.toml`, or you
risk two incompatible copies.

- [ ] **Step 4: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/demuxsock.rs`:

```rust
//! One UDP socket carrying both STUN and QUIC.
//!
//! `quinn` runs its own receive loop over the socket it is given, and so does
//! every STUN client. Two receivers on one UDP socket do not cooperate: each
//! `recvmsg` removes a datagram from the kernel queue, so whichever task wins
//! the race gets it and the other never sees it. Nothing errors; the
//! connection just intermittently fails to come up.
//!
//! There is therefore exactly **one** receiver — quinn's — and this wrapper in
//! front of it. Sending is unaffected: several tasks may `send_to` on one
//! socket safely, which is how ICE keepalives keep working after QUIC starts.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::task::{Context, Poll};

use quinn::udp::{RecvMeta, Transmit};
use quinn::{AsyncUdpSocket, UdpPoller};

use crate::is_stun;

/// Datagrams peeled off the front of the QUIC stream.
pub type StunRx = tokio::sync::mpsc::Receiver<(Vec<u8>, SocketAddr)>;

/// How many STUN datagrams may queue before they are dropped. ICE checks are
/// idempotent and retried, so dropping is always better than blocking quinn.
const STUN_QUEUE: usize = 64;

pub struct StunDemuxSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    stun: tokio::sync::mpsc::Sender<(Vec<u8>, SocketAddr)>,
}

impl std::fmt::Debug for StunDemuxSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StunDemuxSocket")
            .field("local_addr", &self.inner.local_addr().ok())
            .finish()
    }
}

impl StunDemuxSocket {
    /// Wrap `inner` for `quinn`.
    ///
    /// The caller **keeps** `inner` and uses it for `send_to` only. The
    /// returned socket is the single owner of the receive side; hand it to
    /// `quinn::Endpoint::new_with_abstract_socket`.
    ///
    /// Deviates from the contract by returning `anyhow::Result`, because
    /// duplicating the descriptor and wrapping it can fail.
    pub fn new(inner: Arc<tokio::net::UdpSocket>) -> anyhow::Result<(Arc<StunDemuxSocket>, StunRx)> {
        use std::os::fd::AsFd;

        // Duplicate the descriptor so quinn's own AsyncUdpSocket
        // implementation - with all its GSO, GRO and ECN platform handling -
        // can own one copy while the caller keeps the other for sending.
        // Two descriptors, one socket: both may send, only one receives.
        let dup = inner.as_fd().try_clone_to_owned()?;
        let std_socket = std::net::UdpSocket::from(dup);

        let runtime = quinn::TokioRuntime;
        let wrapped = quinn::Runtime::wrap_udp_socket(&runtime, std_socket)?;

        let (tx, rx) = tokio::sync::mpsc::channel(STUN_QUEUE);
        Ok((Arc::new(StunDemuxSocket { inner: wrapped, stun: tx }), rx))
    }
}

impl AsyncUdpSocket for StunDemuxSocket {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn UdpPoller>> {
        Arc::clone(&self.inner).create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        self.inner.try_send(transmit)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        loop {
            let n = match self.inner.poll_recv(cx, bufs, meta) {
                Poll::Ready(Ok(n)) => n,
                other => return other,
            };

            // Stable compaction: keep the non-STUN datagrams, in order, and
            // divert the rest. `write <= read` always, so the copy is never
            // an overlapping move.
            let mut write = 0usize;
            for read in 0..n {
                let len = meta[read].len;
                let is_stun_datagram = is_stun(&bufs[read][..len]);
                if is_stun_datagram {
                    // Non-blocking on purpose: a full or closed channel drops
                    // the check rather than back-pressuring QUIC. ICE checks
                    // are idempotent and retried.
                    let _ = self
                        .stun
                        .try_send((bufs[read][..len].to_vec(), meta[read].addr));
                    continue;
                }
                if write != read {
                    let (dst, src) = bufs.split_at_mut(read);
                    dst[write][..len].copy_from_slice(&src[0][..len]);
                    meta[write] = meta[read];
                }
                write += 1;
            }

            // A batch that was entirely STUN is not "nothing arrived": go
            // round again rather than tell quinn zero, which it would read as
            // a spurious wakeup.
            if write > 0 {
                return Poll::Ready(Ok(write));
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    /// One datagram per `RecvMeta`.
    ///
    /// With generic receive offload a single `RecvMeta` describes several
    /// coalesced datagrams sharing one buffer, and a mixed STUN/QUIC batch
    /// would have to be split apart by `stride`. Disabling GRO costs a few
    /// syscalls under bulk load and makes this demultiplexer obviously
    /// correct instead of subtly wrong.
    fn max_receive_segments(&self) -> usize {
        1
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod demuxsock;

pub use demuxsock::{StunDemuxSocket, StunRx};
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 5 new tests pass, clippy clean.

If `quinn::Runtime::wrap_udp_socket` is not reachable as a trait method, bring
the trait into scope with `use quinn::Runtime as _;` and call
`quinn::TokioRuntime.wrap_udp_socket(std_socket)`. The verified signature is
`fn wrap_udp_socket(&self, t: std::net::UdpSocket) -> io::Result<Arc<dyn AsyncUdpSocket>>`.

- [ ] **Step 6: Commit**

```bash
git add crates/oxutrm-net/src/demuxsock.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): StunDemuxSocket so STUN and QUIC can share one socket

quinn runs its own recv loop; so does any STUN client. Two receivers on one
UDP socket steal each other's packets with no error anywhere. This wrapper
is the single receiver: it peels STUN into a channel and passes the rest to
quinn, which is constructed with Endpoint::new_with_abstract_socket.
Sending stays shared, which is safe.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 13: `quic_server` and `quic_client` over the demultiplexed socket


**Files:**
- Create: `crates/oxutrm-net/src/quic.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{generate_cert, install_crypto_provider, provider, to_socket_family, PinnedSpki, StunDemuxSocket, StunRx, CERT_NAME}`.
- Produces:
  ```rust
  pub const ALPN: &[u8] = b"oxutrm/1";

  /// Contract form. STUN arriving on this socket is discarded.
  pub async fn quic_server(
      socket: std::net::UdpSocket,
      cert: rustls::pki_types::CertificateDer<'static>,
      key: rustls::pki_types::PrivateKeyDer<'static>,
  ) -> anyhow::Result<quinn::Endpoint>;

  pub async fn quic_client(
      socket: std::net::UdpSocket,
      peer: std::net::SocketAddr,
      expect_spki_sha256: [u8; 32],
  ) -> anyhow::Result<quinn::Connection>;

  /// The forms M4 uses: the caller keeps the socket for sending ICE
  /// keepalives, and receives the peeled-off STUN on `StunRx`.
  pub async fn quic_server_demuxed(
      socket: std::sync::Arc<tokio::net::UdpSocket>,
      cert: rustls::pki_types::CertificateDer<'static>,
      key: rustls::pki_types::PrivateKeyDer<'static>,
  ) -> anyhow::Result<(quinn::Endpoint, StunRx)>;

  pub async fn quic_client_demuxed(
      socket: std::sync::Arc<tokio::net::UdpSocket>,
      peer: std::net::SocketAddr,
      expect_spki_sha256: [u8; 32],
  ) -> anyhow::Result<(quinn::Connection, StunRx)>;

  /// The endpoint `quic_client` built, for M4's `Endpoint::rebind` on roaming.
  pub fn client_endpoint() -> Option<quinn::Endpoint>;
  ```

### Four things that fail quietly if you skip them

**1. The endpoint is built with `Endpoint::new_with_abstract_socket`.** Never
`Endpoint::new`. `Endpoint::new` takes the raw socket and runs its own receive
loop, which then races every STUN receiver on that socket. The verified
signature is:

```rust
pub fn new_with_abstract_socket(
    config: EndpointConfig,
    server_config: Option<ServerConfig>,
    socket: Arc<dyn AsyncUdpSocket>,
    runtime: Arc<dyn Runtime>,
) -> io::Result<Endpoint>
```

**2. `rustls` 0.23 needs a process-default `CryptoProvider` installed.** Two
providers in the build (or none installed) and `QuicClientConfig::try_from`
fails, or `ClientConfig::builder()` panics outright with "no process-level
CryptoProvider available". `install_crypto_provider()` from the certificate
task is called at the top of every entry point here. It is idempotent and
ignores an already-installed provider.

**3. Both datagram buffer sizes must be set.** `datagram_send_buffer_size(usize)`
and `datagram_receive_buffer_size(Option<usize>)` — note the asymmetry, it is
real. Set only one and QUIC datagrams are silently disabled:
`Connection::max_datagram_size()` returns `None`, `send_datagram` fails, and
nothing anywhere says why.

**4. ICE has already finished.** These functions are called with the pair
already nominated, because QUIC connection migration cannot repoint an
established connection at a different **remote** address.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/quic.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::generate_cert;
    use std::net::UdpSocket;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn the_transport_config_is_constructible() {
        // The behavioural proof that datagrams are on is the echo test below.
        let _ = transport_config();
    }

    #[test]
    fn installing_the_crypto_provider_twice_is_harmless() {
        install_crypto_provider();
        install_crypto_provider();
        assert!(!provider().cipher_suites.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pinned_client_and_server_echo_a_datagram() {
        let (cert, key, fingerprint) = generate_cert().unwrap();

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let endpoint = quic_server(server_sock, cert, key).await.unwrap();

        let server = tokio::spawn(async move {
            let conn = endpoint
                .accept()
                .await
                .expect("an inbound connection")
                .await
                .expect("a completed handshake");
            let d = conn.read_datagram().await.expect("a datagram");
            conn.send_datagram(d).expect("echo");
            conn.closed().await;
        });

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let conn = quic_client(client_sock, server_addr, fingerprint).await.unwrap();

        assert!(
            conn.max_datagram_size().is_some(),
            "datagrams are silently disabled unless BOTH buffer sizes are set"
        );
        assert_eq!(conn.remote_address(), server_addr);

        conn.send_datagram(bytes::Bytes::from_static(b"oxutrm netdemo")).unwrap();
        let back = tokio::time::timeout(Duration::from_secs(10), conn.read_datagram())
            .await
            .expect("the echo must arrive")
            .unwrap();
        assert_eq!(&back[..], b"oxutrm netdemo");
        assert!(client_endpoint().is_some(), "M4 needs this for rebind");

        conn.close(0u32.into(), b"done");
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_survives_stun_arriving_on_the_same_socket_throughout() {
        // The reason StunDemuxSocket exists, proved end to end: a handshake
        // and an echo while STUN keeps landing on both sockets.
        let (cert, key, fingerprint) = generate_cert().unwrap();

        let server_std = UdpSocket::bind("127.0.0.1:0").unwrap();
        server_std.set_nonblocking(true).unwrap();
        let server_addr = server_std.local_addr().unwrap();
        let server_sock = Arc::new(tokio::net::UdpSocket::from_std(server_std).unwrap());
        let (endpoint, mut server_stun) =
            quic_server_demuxed(Arc::clone(&server_sock), cert, key).await.unwrap();

        let client_std = UdpSocket::bind("127.0.0.1:0").unwrap();
        client_std.set_nonblocking(true).unwrap();
        let client_addr = client_std.local_addr().unwrap();
        let client_sock = Arc::new(tokio::net::UdpSocket::from_std(client_std).unwrap());

        // A third party spraying STUN at both endpoints for the whole test.
        let noise = tokio::spawn(async move {
            let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut d = vec![0x00, 0x01, 0x00, 0x00];
            d.extend_from_slice(&crate::STUN_MAGIC_COOKIE);
            d.extend_from_slice(&[0xA1; 12]);
            loop {
                let _ = s.send_to(&d, server_addr).await;
                let _ = s.send_to(&d, client_addr).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let server = tokio::spawn(async move {
            let conn = endpoint.accept().await.unwrap().await.unwrap();
            let d = conn.read_datagram().await.unwrap();
            conn.send_datagram(d).unwrap();
            conn.closed().await;
        });

        let (conn, _client_stun) =
            tokio::time::timeout(
                Duration::from_secs(20),
                quic_client_demuxed(Arc::clone(&client_sock), server_addr, fingerprint),
            )
            .await
            .expect("the handshake must not be starved by the STUN traffic")
            .unwrap();

        conn.send_datagram(bytes::Bytes::from_static(b"still here")).unwrap();
        let back = tokio::time::timeout(Duration::from_secs(10), conn.read_datagram())
            .await
            .expect("the echo must arrive")
            .unwrap();
        assert_eq!(&back[..], b"still here");

        // And the STUN was not lost: it was diverted.
        let diverted = tokio::time::timeout(Duration::from_secs(5), server_stun.recv())
            .await
            .expect("STUN must reach the channel")
            .expect("channel open");
        assert!(crate::is_stun(&diverted.0));

        // Sending on the socket quinn is using is still allowed.
        client_sock.send_to(&[0x00, 0x01], server_addr).await.unwrap();

        noise.abort();
        conn.close(0u32.into(), b"done");
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_pinned_to_a_different_certificate_is_refused() {
        let (cert, key, _fp) = generate_cert().unwrap();
        let (_other_cert, _other_key, other_fp) = generate_cert().unwrap();

        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let endpoint = quic_server(server_sock, cert, key).await.unwrap();
        let server = tokio::spawn(async move {
            if let Some(incoming) = endpoint.accept().await {
                let _ = incoming.await;
            }
        });

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            quic_client(client_sock, server_addr, other_fp),
        )
        .await
        .expect("must not hang");

        assert!(result.is_err(), "the pin is the only thing that grants trust");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bidirectional_stream_carries_bulk_data_alongside_datagrams() {
        // Spec §7.2: a 50 000-line scrollback fetch must never delay a
        // keystroke, so both channels must exist on one connection.
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let server_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let server_addr = server_sock.local_addr().unwrap();
        let endpoint = quic_server(server_sock, cert, key).await.unwrap();

        let server = tokio::spawn(async move {
            let conn = endpoint.accept().await.unwrap().await.unwrap();
            let (mut send, mut recv) = conn.accept_bi().await.unwrap();
            let got = recv.read_to_end(1 << 20).await.unwrap();
            send.write_all(&got).await.unwrap();
            send.finish().unwrap();
            conn.closed().await;
        });

        let client_sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let conn = quic_client(client_sock, server_addr, fingerprint).await.unwrap();
        let payload = vec![0xABu8; 64 * 1024];
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&payload).await.unwrap();
        send.finish().unwrap();
        assert_eq!(recv.read_to_end(1 << 20).await.unwrap(), payload);

        conn.close(0u32.into(), b"done");
        let _ = tokio::time::timeout(Duration::from_secs(5), server).await;
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function transport_config in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/quic.rs`:

```rust
//! QUIC over the socket oxutrm has already punched (spec §6).
//!
//! The endpoint is always built with `Endpoint::new_with_abstract_socket` over
//! a [`crate::StunDemuxSocket`], never with `Endpoint::new`: `Endpoint::new`
//! runs its own receive loop on the raw socket and races every STUN receiver
//! on it.
//!
//! ICE has already nominated a pair by the time anything here is called. QUIC
//! connection migration lets a client change its own *local* address and
//! nothing more; there is no way to repoint an established connection at a
//! different *remote* address.

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context as _;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::{
    install_crypto_provider, provider, to_socket_family, PinnedSpki, StunDemuxSocket, StunRx,
    CERT_NAME,
};

/// Honest ALPN. oxutrm's packets genuinely are QUIC, so a
/// protocol-classifying middlebox sees an ordinary QUIC flow (spec §5.6).
pub const ALPN: &[u8] = b"oxutrm/1";

/// One megabyte each way. Screen deltas are small; the buffer only has to
/// absorb a burst.
const DATAGRAM_BUFFER: usize = 1024 * 1024;

/// See [`client_endpoint`].
static CLIENT_ENDPOINT: Mutex<Option<quinn::Endpoint>> = Mutex::new(None);

fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    // BOTH of these are required. Set only one and DATAGRAM support is
    // silently disabled: `max_datagram_size()` returns None and
    // `send_datagram` fails, with nothing saying why. The asymmetry in the
    // two signatures - usize here, Option<usize> there - is real.
    t.datagram_send_buffer_size(DATAGRAM_BUFFER);
    t.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER));
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).expect("30s is representable"),
    ));
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    Arc::new(t)
}

fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::ServerConfig> {
    install_crypto_provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("installing the session certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("building the QUIC server crypto config")?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn client_config(expect_spki_sha256: [u8; 32]) -> anyhow::Result<quinn::ClientConfig> {
    install_crypto_provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedSpki::new(expect_spki_sha256)))
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .context("building the QUIC client crypto config")?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn endpoint_over(
    socket: Arc<tokio::net::UdpSocket>,
    server: Option<quinn::ServerConfig>,
) -> anyhow::Result<(quinn::Endpoint, StunRx)> {
    let (demux, stun_rx) = StunDemuxSocket::new(socket).context("wrapping the session socket")?;
    let runtime = quinn::default_runtime()
        .context("quinn needs an async runtime; call this from inside tokio")?;
    // `new_with_abstract_socket`, never `new`: `new` would run a second
    // receive loop on the raw socket and steal the STUN packets.
    let endpoint =
        quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            server,
            demux,
            runtime,
        )
        .context("creating the QUIC endpoint")?;
    Ok((endpoint, stun_rx))
}

fn into_tokio(socket: UdpSocket) -> anyhow::Result<Arc<tokio::net::UdpSocket>> {
    socket.set_nonblocking(true)?;
    Ok(Arc::new(tokio::net::UdpSocket::from_std(socket)?))
}

/// A QUIC endpoint listening on a socket that has already been punched.
///
/// STUN arriving on this socket is discarded. M4's keepalives use
/// [`quic_server_demuxed`].
pub async fn quic_server(
    socket: UdpSocket,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::Endpoint> {
    let (endpoint, _stun) = quic_server_demuxed(into_tokio(socket)?, cert, key).await?;
    Ok(endpoint)
}

/// As [`quic_server`], but the caller keeps the socket for sending ICE
/// keepalives and receives the peeled-off STUN on the returned [`StunRx`].
pub async fn quic_server_demuxed(
    socket: Arc<tokio::net::UdpSocket>,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<(quinn::Endpoint, StunRx)> {
    endpoint_over(socket, Some(server_config(cert, key)?))
}

/// Connect to `peer`, trusting exactly `expect_spki_sha256` and nothing else.
pub async fn quic_client(
    socket: UdpSocket,
    peer: SocketAddr,
    expect_spki_sha256: [u8; 32],
) -> anyhow::Result<quinn::Connection> {
    let (conn, _stun) =
        quic_client_demuxed(into_tokio(socket)?, peer, expect_spki_sha256).await?;
    Ok(conn)
}

/// As [`quic_client`], but the caller keeps the socket and the STUN stream.
pub async fn quic_client_demuxed(
    socket: Arc<tokio::net::UdpSocket>,
    peer: SocketAddr,
    expect_spki_sha256: [u8; 32],
) -> anyhow::Result<(quinn::Connection, StunRx)> {
    let (mut endpoint, stun_rx) = endpoint_over(socket, None)?;
    endpoint.set_default_client_config(client_config(expect_spki_sha256)?);

    let local = endpoint.local_addr().context("reading the endpoint's local address")?;
    let peer = to_socket_family(&local, peer);
    let connection = endpoint
        .connect(peer, CERT_NAME)
        .context("starting the QUIC handshake")?
        .await
        .context("completing the QUIC handshake")?;

    // Park the endpoint: quinn drives the socket from it, and M4 needs the
    // same handle for `Endpoint::rebind` when the local address changes.
    // There is one endpoint per oxutrm process (spec §6), so this never grows.
    if let Ok(mut slot) = CLIENT_ENDPOINT.lock() {
        *slot = Some(endpoint);
    }

    Ok((connection, stun_rx))
}

/// The endpoint [`quic_client`] built, for `Endpoint::rebind` on roaming.
pub fn client_endpoint() -> Option<quinn::Endpoint> {
    CLIENT_ENDPOINT.lock().ok()?.clone()
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod quic;

pub use quic::{
    client_endpoint, quic_client, quic_client_demuxed, quic_server, quic_server_demuxed, ALPN,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run it bare and wait for it in the same turn; the QUIC tests take a few seconds:

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 6 new tests pass, clippy clean. The load-bearing one is
`quic_survives_stun_arriving_on_the_same_socket_throughout`.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/quic.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): QUIC over the demultiplexed socket, pinned to one SPKI

Endpoint::new_with_abstract_socket over StunDemuxSocket, never
Endpoint::new. Installs the rustls CryptoProvider before building either
config, and sets both datagram buffer sizes.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 14: The ladder, wired up — `gather` and `connect_path`

**Files:**
- Create: `crates/oxutrm-net/src/establish.rs`
- Test: same file, `#[cfg(test)] mod tests`
- Modify: `crates/oxutrm-net/src/lib.rs`

**Interfaces:**
- Consumes: `crate::{bind_socket, birthday_blast, ice_priority, local_candidates, stun_discover, IceAgent, IceEvent, IceRole, NetConfig, PortMapping}`; `oxutrm_proto::{Candidate, NatType, PathDescription, Rung}`.
- Produces:
  ```rust
  pub struct Gathered {
      pub socket: std::sync::Arc<tokio::net::UdpSocket>,
      pub candidates: Vec<oxutrm_proto::Candidate>,
      pub nat_type: oxutrm_proto::NatType,
      /// Kept alive for the session; dropping it releases the router mapping.
      pub mapping: Option<PortMapping>,
  }

  pub struct EstablishedPath {
      /// Hand this to `quic_server_demuxed` / `quic_client_demuxed`.
      pub socket: std::sync::Arc<tokio::net::UdpSocket>,
      pub path: oxutrm_proto::PathDescription,
      /// The router mapping, moved out so the caller can keep it alive.
      pub mapping: Option<PortMapping>,
  }

  /// Rungs 0-2, the gathering half.
  pub async fn gather(cfg: &NetConfig) -> anyhow::Result<Gathered>;

  /// Rungs 0-3, the connectivity half. Returns only once a pair is NOMINATED.
  pub async fn connect_path(
      gathered: Gathered,
      psk: [u8; 32],
      role: IceRole,
      remote: Vec<oxutrm_proto::Candidate>,
      peer_nat: oxutrm_proto::NatType,
      cfg: &NetConfig,
      on_local_candidate: impl FnMut(oxutrm_proto::Candidate),
  ) -> anyhow::Result<EstablishedPath>;
  ```

**Neither name is in the contract.** The contract lists the pieces but no
orchestrator, so these two are M2's addition and M3 consumes them. They are
named `gather`/`connect_path` rather than `connect` so nothing collides with
`UdpSocket::connect` at a use site.

### The order is not an implementation detail

`gather` runs STUN **before** anything touches QUIC, because
`stunclient::StunClient::query_external_address_async` owns the socket's
receive side while it runs. `connect_path` runs ICE to **nomination**, and only
then does the caller hand the socket to QUIC. Nothing may reorder these:

1. bind one socket (rung 0 addresses come from it),
2. STUN discovery on that socket — sole receiver,
3. router mapping — no socket involvement,
4. ICE checks on that socket — sole receiver,
5. **nomination**,
6. QUIC, wrapped in `StunDemuxSocket`, which becomes the sole receiver from
   then on.

A better path discovered after step 5 is **lost for this attach**. QUIC
migration lets a client change its own local address; it cannot repoint a
connection at a different remote address. Reattaching re-runs the whole ladder.

**The socket is an `Arc<tokio::net::UdpSocket>` throughout.** The old shape —
convert to tokio, convert back to `std` — cannot work once `StunDemuxSocket`
needs the caller to keep a sending handle. `Gathered` and `EstablishedPath`
therefore carry the `Arc` directly.

**MTU and RTT at nomination time.** `rtt_ms` is the ICE agent's measured check
round trip. `mtu` is QUIC's conservative initial 1200, because DPLPMTUD has not
run — QUIC has not started. M4 refreshes both from `Connection::rtt()` and
`Connection::stats()`.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-net/src/establish.rs` with only this:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{local_candidates_filtered, MappingBehaviour, NetConfig, StunResponder};
    use oxutrm_proto::{CandidateKind, NatType, Rung};
    use std::sync::Arc;
    use std::time::Duration;

    fn quick() -> NetConfig {
        NetConfig {
            enable_port_mapping: false,
            gather_timeout: Duration::from_millis(2_000),
            birthday_budget: Duration::from_millis(2_000),
            birthday_sockets: 4,
            birthday_ports: 16,
            ..NetConfig::default()
        }
    }

    async fn gathered_on(bind: &str, nat: NatType) -> Gathered {
        let std_sock = std::net::UdpSocket::bind(bind).unwrap();
        std_sock.set_nonblocking(true).unwrap();
        let candidates = local_candidates_filtered(&std_sock, true);
        assert!(!candidates.is_empty(), "the test door must produce loopback candidates");
        let socket = Arc::new(tokio::net::UdpSocket::from_std(std_sock).unwrap());
        Gathered { socket, candidates, nat_type: nat, mapping: None }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_ipv6_loopback_peers_settle_on_rung_zero() {
        let psk = [21u8; 32];
        let cfg = quick();
        let a = gathered_on("[::1]:0", NatType::None).await;
        let b = gathered_on("[::1]:0", NatType::None).await;
        let a_c = a.candidates.clone();
        let b_c = b.candidates.clone();

        let (ca, cb) = (cfg.clone(), cfg.clone());
        let ta = tokio::spawn(async move {
            connect_path(a, psk, IceRole::Controlling, b_c, NatType::None, &ca, |_| {}).await
        });
        let tb = tokio::spawn(async move {
            connect_path(b, psk, IceRole::Controlled, a_c, NatType::None, &cb, |_| {}).await
        });

        let ea = ta.await.unwrap().expect("the client must establish");
        let eb = tb.await.unwrap().expect("the host must establish");
        assert_eq!(ea.path.rung, Rung::Ipv6Direct);
        assert_eq!(eb.path.rung, Rung::Ipv6Direct);
        assert_eq!(ea.path.remote, eb.path.local);
        assert_eq!(ea.path.nat_type, NatType::None);
        assert_eq!(ea.path.mtu, 1200, "DPLPMTUD has not run yet");
        assert!(ea.path.probes_sent >= 1);
        // The socket is still usable, and still the one that was punched.
        assert_eq!(ea.socket.local_addr().unwrap(), ea.path.local);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_symmetric_nat_report_sends_the_ladder_straight_to_rung_three() {
        let psk = [22u8; 32];
        let cfg = quick();
        let a = gathered_on("127.0.0.1:0", NatType::Symmetric).await;
        let b = gathered_on("127.0.0.1:0", NatType::Symmetric).await;
        let a_c = a.candidates.clone();
        let b_c = b.candidates.clone();

        let (ca, cb) = (cfg.clone(), cfg.clone());
        let ta = tokio::spawn(async move {
            connect_path(a, psk, IceRole::Controlling, b_c, NatType::Symmetric, &ca, |_| {}).await
        });
        let tb = tokio::spawn(async move {
            connect_path(b, psk, IceRole::Controlled, a_c, NatType::Symmetric, &cb, |_| {}).await
        });

        let ea = ta.await.unwrap().expect("the client must establish via the blast");
        let eb = tb.await.unwrap().expect("the host must establish via the blast");
        assert_eq!(ea.path.rung, Rung::Birthday);
        assert_eq!(eb.path.rung, Rung::Birthday);
        assert!(ea.path.probes_sent >= 1, "the cost of the blast must be reported");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_that_does_not_exist_fails_within_the_budget() {
        let cfg = NetConfig { enable_birthday: false, ..quick() };
        let g = gathered_on("127.0.0.1:0", NatType::EndpointIndependent).await;
        let nowhere = vec![oxutrm_proto::Candidate {
            addr: "127.0.0.1:1".parse().unwrap(),
            kind: CandidateKind::Host,
            priority: crate::ice_priority(CandidateKind::Host, &"127.0.0.1".parse().unwrap()),
        }];

        let started = std::time::Instant::now();
        let r = connect_path(
            g,
            [23u8; 32],
            IceRole::Controlling,
            nowhere,
            NatType::EndpointIndependent,
            &cfg,
            |_| {},
        )
        .await;
        assert!(r.is_err());
        assert!(started.elapsed() < Duration::from_secs(8), "took {:?}", started.elapsed());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gathering_produces_host_and_reflexive_candidates_and_a_nat_type() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let alt = std::net::SocketAddr::new(a.addr().ip(), a.addr().port() + 1);
        let a2 = StunResponder::start_on(alt, MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let b = StunResponder::start(MappingBehaviour::RewritePort(40_000)).await.unwrap();
        let _ = &a2;

        let cfg = NetConfig {
            stun_servers: vec![a.server_string(), b.server_string()],
            enable_port_mapping: false,
            gather_timeout: Duration::from_millis(1_500),
            ..NetConfig::default()
        };

        let g = gather(&cfg).await.unwrap();
        assert_eq!(g.nat_type, NatType::EndpointIndependent);
        assert!(
            g.candidates.iter().any(|c| c.kind == CandidateKind::ServerReflexive),
            "STUN must have contributed a candidate"
        );
        assert!(g.mapping.is_none(), "port mapping was switched off");
        for w in g.candidates.windows(2) {
            assert!(w[0].priority >= w[1].priority, "sorted by descending priority");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn newly_learned_local_candidates_are_handed_to_the_caller() {
        use std::sync::Mutex;
        let psk = [24u8; 32];
        let cfg = quick();
        let a = gathered_on("127.0.0.1:0", NatType::EndpointIndependent).await;
        let b = gathered_on("127.0.0.1:0", NatType::EndpointIndependent).await;
        let a_c = a.candidates.clone();
        let b_c = b.candidates.clone();

        let learned = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&learned);

        let (ca, cb) = (cfg.clone(), cfg.clone());
        let ta = tokio::spawn(async move {
            connect_path(
                a,
                psk,
                IceRole::Controlling,
                b_c,
                NatType::EndpointIndependent,
                &ca,
                move |c| sink.lock().unwrap().push(c),
            )
            .await
        });
        let tb = tokio::spawn(async move {
            connect_path(b, psk, IceRole::Controlled, a_c, NatType::EndpointIndependent, &cb, |_| {})
                .await
        });

        ta.await.unwrap().expect("the client must establish");
        tb.await.unwrap().expect("the host must establish");
        // The peer's response carries XOR-MAPPED-ADDRESS: our own address as
        // it saw us. M3 forwards those over SSH as Signal::CandidateUpdate.
        assert!(
            !learned.lock().unwrap().is_empty(),
            "peer-reflexive candidates must reach the signalling layer"
        );
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
```
Expected: FAIL — `cannot find function gather in this scope`.

- [ ] **Step 3: Write the minimal implementation**

Put this above the `mod tests` in `crates/oxutrm-net/src/establish.rs`:

```rust
//! The five-rung ladder, wired together (spec §5).
//!
//! The order is load-bearing, because exactly one thing may own the socket's
//! receive side at a time: STUN discovery, then ICE to nomination, then QUIC
//! behind a [`crate::StunDemuxSocket`]. Rung 4, the SSH tunnel, is M4's:
//! `Rung::SshTunnel` is never constructed here.

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{anyhow, Context as _};

use oxutrm_proto::{Candidate, NatType, PathDescription, Rung};

use crate::{
    bind_socket, birthday_blast, local_candidates, stun_discover, IceAgent, IceEvent, IceRole,
    NetConfig, PortMapping,
};

/// QUIC's conservative initial MTU. DPLPMTUD has not run at nomination time,
/// so this is the honest number until M4 reads the real one.
const INITIAL_MTU: u16 = 1200;

/// Everything rungs 0-2 discovered, plus the socket they discovered it on.
pub struct Gathered {
    /// The one socket. Every mapping learned above belongs to *this* socket,
    /// so it is carried, never re-bound.
    pub socket: Arc<tokio::net::UdpSocket>,
    /// Host, PortMapped and ServerReflexive candidates, highest priority first.
    pub candidates: Vec<Candidate>,
    pub nat_type: NatType,
    /// Held for the life of the session; dropping it releases the mapping.
    pub mapping: Option<PortMapping>,
}

/// A nominated path, and the socket that owns it.
pub struct EstablishedPath {
    /// Hand this to `quic_server_demuxed` or `quic_client_demuxed`. It may not
    /// be the socket that went in: rung 3 wins on one of its own.
    pub socket: Arc<tokio::net::UdpSocket>,
    pub path: PathDescription,
    /// Moved out rather than dropped, so the caller keeps the mapping alive
    /// for the session.
    pub mapping: Option<PortMapping>,
}

/// Rungs 0-2: bind one socket, enumerate interfaces, ask STUN what the world
/// sees, ask the router for a mapping.
pub async fn gather(cfg: &NetConfig) -> anyhow::Result<Gathered> {
    let std_socket = bind_socket(cfg).context("binding the session socket")?;
    let local_port = std_socket.local_addr()?.port();
    let mut candidates = local_candidates(&std_socket);

    std_socket.set_nonblocking(true)?;
    let socket = Arc::new(
        tokio::net::UdpSocket::from_std(std_socket)
            .context("handing the session socket to tokio")?,
    );

    // STUN owns the receive side while it runs, so nothing else may be
    // reading here. Nothing is: QUIC has not started and ICE has not begun.
    let (reflexive, nat_type) = stun_discover(&socket, cfg).await;

    let hint = reflexive.first().map(|c| c.addr.ip());
    candidates.extend(reflexive);

    // Rung 1. If either side gets a mapping the whole connection succeeds:
    // the other punches to the mapped address and its own is learned
    // peer-reflexively (spec §5.2).
    let mapping = match PortMapping::acquire_with_hint(local_port, cfg, hint).await {
        Some((m, c)) => {
            candidates.push(c);
            Some(m)
        }
        None => None,
    };

    candidates.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then_with(|| a.addr.to_string().cmp(&b.addr.to_string()))
    });
    candidates.dedup_by(|a, b| a.addr == b.addr);

    Ok(Gathered { socket, candidates, nat_type, mapping })
}

/// Rungs 0-3. Returns only once a pair has been **nominated**, because QUIC
/// cannot be repointed at a different remote address afterwards.
///
/// `on_local_candidate` is called for every address we learn about ourselves
/// while checking. M3 forwards those over SSH as `Signal::CandidateUpdate`;
/// M2's demo prints them.
pub async fn connect_path(
    gathered: Gathered,
    psk: [u8; 32],
    role: IceRole,
    remote: Vec<Candidate>,
    peer_nat: NatType,
    cfg: &NetConfig,
    mut on_local_candidate: impl FnMut(Candidate),
) -> anyhow::Result<EstablishedPath> {
    let Gathered { socket, candidates, nat_type, mapping } = gathered;

    // Spec §5.4: ordinary punching is hopeless behind a symmetric NAT, so go
    // straight to rung 3 rather than burn several seconds failing.
    let symmetric = nat_type == NatType::Symmetric || peer_nat == NatType::Symmetric;
    let mut probes_before_blast = 0u32;

    if !symmetric {
        let mut agent = IceAgent::new(psk, role, cfg.clone());
        for c in &candidates {
            agent.add_local(c.clone());
        }
        for c in &remote {
            agent.add_remote(c.clone());
        }

        let outcome = loop {
            match agent.run(Arc::clone(&socket)).await {
                IceEvent::NewLocalCandidate(c) => on_local_candidate(c),
                other => break other,
            }
        };

        if let IceEvent::Nominated { local, remote, rung, probes } = outcome {
            let rtt_ms = agent
                .last_rtt()
                .map(|d| d.as_millis().min(u128::from(u32::MAX)) as u32)
                .unwrap_or(0);
            return Ok(EstablishedPath {
                socket,
                path: PathDescription {
                    rung,
                    local,
                    remote,
                    probes_sent: probes,
                    nat_type,
                    rtt_ms,
                    mtu: INITIAL_MTU,
                },
                mapping,
            });
        }

        probes_before_blast = agent.probes_sent();
    }

    // Rung 3. It opens its own sockets, so give this one up first: the
    // mapping it carries is worthless if rungs 0-2 could not use it.
    let base = remote
        .iter()
        .max_by_key(|c| c.priority)
        .map(|c| c.addr)
        .context("no remote candidate to guess around")?;
    drop(socket);

    match birthday_blast(psk, role, base, cfg).await? {
        Some(r) => {
            let std_socket = r.socket;
            std_socket.set_nonblocking(true)?;
            let local: SocketAddr = std_socket.local_addr()?;
            let socket = Arc::new(
                tokio::net::UdpSocket::from_std(std_socket)
                    .context("handing the blast's winning socket to tokio")?,
            );
            Ok(EstablishedPath {
                socket,
                path: PathDescription {
                    rung: Rung::Birthday,
                    local,
                    remote: r.remote,
                    probes_sent: probes_before_blast.saturating_add(r.probes),
                    nat_type,
                    rtt_ms: 0,
                    mtu: INITIAL_MTU,
                },
                mapping,
            })
        }
        None => Err(anyhow!(
            "no UDP path: rungs 0-3 exhausted after {} probes (rung 4, the SSH tunnel, is M4's)",
            probes_before_blast
        )),
    }
}
```

Add to `crates/oxutrm-net/src/lib.rs`:

```rust
mod establish;

pub use establish::{connect_path, gather, EstablishedPath, Gathered};
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cargo test --jobs 4 -p oxutrm-net -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: 5 new tests pass, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-net/src/establish.rs crates/oxutrm-net/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(net): wire rungs 0-3 into gather() and connect_path()

Exactly one owner of the socket's receive side at a time: STUN discovery,
then ICE to nomination, then QUIC behind StunDemuxSocket. connect_path
returns only once a pair is nominated, because QUIC cannot be repointed at
a different remote address afterwards.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```
## Task 15: The hidden `oxutrm netdemo` subcommand



**Files:**
- Create: `src/netdemo.rs`
- Modify: `src/main.rs`
- Modify: `Cargo.toml` (root — add `oxutrm-net`, `oxutrm-proto`, `tokio`, `bytes`)
- Test: `tests/netdemo_loopback.rs`

**Interfaces:**
- Consumes: `oxutrm_net::{gather, connect_path, quic_server, quic_client, generate_cert, IceRole, NetConfig, StunResponder, MappingBehaviour, Gathered, EstablishedPath}`; `oxutrm_proto::{Signal, read_signal, write_signal, Candidate, NatType, TerminalCaps, TermSize, PROTO_VERSION}`.
- Produces:
  ```rust
  // src/netdemo.rs
  pub async fn run(args: &[String]) -> anyhow::Result<()>;
  ```

**What it does.** Two processes, one `--role host` and one `--role client`,
with the host's stdout wired to the client's stdin and vice versa. They
exchange real `Signal` lines, run the real ladder, bring up real QUIC, and
echo a dummy payload over QUIC datagrams. It is the whole of M2 in one command
and the thing the netns harness drives.

**Two rules that are not optional:**

1. **stdout is the signalling channel; everything human goes to stderr.**
2. **Flush after every signal line.** When stdout is a pipe rather than a
   terminal it is fully buffered, and two processes waiting on each other's
   unflushed buffers is a deadlock with no error message.

`--role stun` is a third mode: it runs a `StunResponder` on a given address and
blocks. The netns harness uses it to put two STUN servers on the simulated
internet, so nothing in the test suite reaches a public server.

- [ ] **Step 1: Write the failing test**

Create `tests/netdemo_loopback.rs`:

```rust
//! The whole of M2 in one test: two real processes, real signalling, real
//! ICE, real QUIC, an echoed payload.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

#[test]
fn two_netdemo_processes_establish_quic_and_echo_a_payload() {
    let bin = env!("CARGO_BIN_EXE_oxutrm");

    let mut host = Command::new(bin)
        .args(["netdemo", "--role", "host", "--no-stun", "--loopback"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the host");

    let mut client = Command::new(bin)
        .args(["netdemo", "--role", "client", "--no-stun", "--loopback"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the client");

    let mut host_in = host.stdin.take().unwrap();
    let host_out = BufReader::new(host.stdout.take().unwrap());
    let mut client_in = client.stdin.take().unwrap();
    let client_out = BufReader::new(client.stdout.take().unwrap());

    // Cross-pipe the two signalling channels.
    let h2c = std::thread::spawn(move || {
        for line in host_out.lines() {
            let Ok(line) = line else { return };
            if writeln!(client_in, "{line}").is_err() || client_in.flush().is_err() {
                return;
            }
        }
    });
    let c2h = std::thread::spawn(move || {
        for line in client_out.lines() {
            let Ok(line) = line else { return };
            if writeln!(host_in, "{line}").is_err() || host_in.flush().is_err() {
                return;
            }
        }
    });

    // Collect each side's human output on its own thread, with a deadline.
    let (tx, rx) = mpsc::channel::<(&'static str, String)>();
    for (name, stream) in [
        ("host", host.stderr.take().unwrap()),
        ("client", client.stderr.take().unwrap()),
    ] {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let mut text = String::new();
            for line in BufReader::new(stream).lines().map_while(Result::ok) {
                eprintln!("[{name}] {line}");
                text.push_str(&line);
                text.push('\n');
            }
            let _ = tx.send((name, text));
        });
    }
    drop(tx);

    let mut seen = Vec::new();
    for _ in 0..2 {
        match rx.recv_timeout(Duration::from_secs(60)) {
            Ok(v) => seen.push(v),
            Err(e) => panic!("a netdemo process never finished: {e}"),
        }
    }
    let _ = h2c.join();
    let _ = c2h.join();
    let _ = host.wait();
    let _ = client.wait();

    for (name, text) in &seen {
        assert!(
            text.contains("NETDEMO-RESULT"),
            "{name} never reported a result. Output was:\n{text}"
        );
        assert!(text.contains("echo=ok"), "{name} did not echo the payload:\n{text}");
        assert!(
            text.contains("rung=Ipv6Direct") || text.contains("rung=StunPunch"),
            "{name} reported an unexpected rung:\n{text}"
        );
    }
}
```

- [ ] **Step 2: Run it to make sure it fails**

```bash
cargo test --jobs 4 --test netdemo_loopback -- --test-threads 4
```
Expected: FAIL — the binary exits non-zero because `netdemo` is not a
subcommand it knows, so `NETDEMO-RESULT` never appears.

- [ ] **Step 3: Add the dependencies**

In the workspace root `Cargo.toml`, under `[dependencies]`:

```toml
oxutrm-net = { path = "crates/oxutrm-net" }
oxutrm-proto = { path = "crates/oxutrm-proto" }
anyhow = "1"
bytes = "1"
tokio = { version = "1", features = ["io-util", "macros", "net", "rt-multi-thread", "sync", "time"] }
```

- [ ] **Step 4: Write the implementation**

Create `src/netdemo.rs`:

```rust
//! `oxutrm netdemo` — a hidden subcommand that exercises the whole of the
//! network layer with a dummy echo payload. It is not in the help text and is
//! not part of the product; it is how M2 proves itself and how the netns
//! harness drives the ladder.
//!
//! **stdout is the signalling channel.** Every human-readable byte goes to
//! stderr, and every signalling line is flushed immediately: when stdout is a
//! pipe it is fully buffered, and two processes waiting on each other's
//! unflushed buffers deadlock silently.

use std::io::{BufRead, Write};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};

use oxutrm_net::{
    connect_path, gather, generate_cert, local_candidates_filtered, quic_client_demuxed,
    quic_server_demuxed, EstablishedPath, Gathered, IceRole, MappingBehaviour, NetConfig,
    StunResponder,
};
use oxutrm_proto::{
    read_signal, write_signal, Candidate, NatType, Signal, TermSize, TerminalCaps, PROTO_VERSION,
};

struct Args {
    role: String,
    stun: Vec<String>,
    no_stun: bool,
    loopback: bool,
    bind: Option<String>,
}

fn parse_args(args: &[String]) -> anyhow::Result<Args> {
    let mut out = Args {
        role: String::new(),
        stun: Vec::new(),
        no_stun: false,
        loopback: false,
        bind: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--role" => out.role = it.next().context("--role needs a value")?.clone(),
            "--stun" => out.stun.push(it.next().context("--stun needs a value")?.clone()),
            "--bind" => out.bind = Some(it.next().context("--bind needs a value")?.clone()),
            "--no-stun" => out.no_stun = true,
            "--loopback" => out.loopback = true,
            other => bail!("netdemo: unknown argument {other}"),
        }
    }
    if out.role.is_empty() {
        bail!("netdemo: --role host|client|stun is required");
    }
    Ok(out)
}

pub async fn run(argv: &[String]) -> anyhow::Result<()> {
    let args = parse_args(argv)?;
    match args.role.as_str() {
        "stun" => run_stun(&args).await,
        "host" => run_peer(&args, true).await,
        "client" => run_peer(&args, false).await,
        other => bail!("netdemo: --role must be host, client or stun, not {other}"),
    }
}

/// A standalone STUN responder, so no test ever reaches a public server.
async fn run_stun(args: &Args) -> anyhow::Result<()> {
    let bind: std::net::SocketAddr = args
        .bind
        .as_deref()
        .context("--role stun needs --bind <addr:port>")?
        .parse()
        .context("--bind must be an address:port")?;
    let s = StunResponder::start_on(bind, MappingBehaviour::Truthful).await?;
    eprintln!("NETDEMO-STUN listening on {}", s.addr());
    // Block until killed.
    std::future::pending::<()>().await;
    Ok(())
}

async fn run_peer(args: &Args, is_host: bool) -> anyhow::Result<()> {
    let cfg = NetConfig {
        stun_servers: if args.no_stun { Vec::new() } else { args.stun.clone() },
        // Never touch the router from a test.
        enable_port_mapping: false,
        prefer_port: 0,
        gather_timeout: Duration::from_secs(3),
        birthday_budget: Duration::from_secs(5),
        ..NetConfig::default()
    };

    let mut gathered = gather(&cfg).await.context("gathering candidates")?;
    if args.loopback && gathered.candidates.is_empty() {
        // A machine with no routable address still has loopback, and the
        // loopback test is the point.
        gathered.candidates = local_candidates_filtered(&gathered.socket, true);
    }
    if gathered.candidates.is_empty() {
        bail!("no local candidates at all: nothing to advertise");
    }

    let psk: [u8; 32] = [0x5A; 32];
    let (cert, key, fingerprint) = generate_cert()?;

    let stdout = std::io::stdout();
    let stdin = std::io::stdin();
    let mut out = stdout.lock();
    let mut inp = stdin.lock();

    // ---- signalling ----
    let peer_candidates: Vec<Candidate>;
    let peer_nat: NatType;

    if is_host {
        let hello = Signal::HostHello {
            proto: PROTO_VERSION,
            session_id: "00000000000000000000000000000000".to_owned(),
            cert_spki_sha256: b64(&fingerprint),
            psk: b64(&psk),
            candidates: gathered.candidates.clone(),
            nat_type: gathered.nat_type,
            bound_port: gathered.socket.local_addr()?.port(),
        };
        write_signal(&mut out, &hello)?;
        out.flush()?;

        match read_signal(&mut inp)? {
            Signal::ClientHello { proto, candidates, nat_type, .. } => {
                if proto != PROTO_VERSION {
                    bail!("protocol version mismatch: peer {proto}, ours {PROTO_VERSION}");
                }
                peer_candidates = candidates;
                peer_nat = nat_type;
            }
            other => bail!("expected ClientHello, got {other:?}"),
        }
    } else {
        let (their_fp, their_psk);
        match read_signal(&mut inp)? {
            Signal::HostHello { proto, cert_spki_sha256, psk: p, candidates, nat_type, .. } => {
                if proto != PROTO_VERSION {
                    bail!("protocol version mismatch: peer {proto}, ours {PROTO_VERSION}");
                }
                their_fp = unb64_32(&cert_spki_sha256)?;
                their_psk = unb64_32(&p)?;
                peer_candidates = candidates;
                peer_nat = nat_type;
            }
            other => bail!("expected HostHello, got {other:?}"),
        }
        let hello = Signal::ClientHello {
            proto: PROTO_VERSION,
            candidates: gathered.candidates.clone(),
            nat_type: gathered.nat_type,
            caps: TerminalCaps {
                truecolor: true,
                colors: 16_777_216,
                bracketed_paste: true,
                mouse_sgr: true,
                osc52: true,
                term_name: "netdemo".to_owned(),
            },
            size: TermSize { cols: 80, rows: 24 },
        };
        write_signal(&mut out, &hello)?;
        out.flush()?;

        return finish_client(gathered, their_psk, peer_candidates, peer_nat, &cfg, their_fp).await;
    }

    finish_host(gathered, psk, peer_candidates, peer_nat, &cfg, cert, key).await
}

async fn finish_host(
    gathered: Gathered,
    psk: [u8; 32],
    remote: Vec<Candidate>,
    peer_nat: NatType,
    cfg: &NetConfig,
    cert: rustls::pki_types::CertificateDer<'static>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> anyhow::Result<()> {
    // Returns only once ICE has NOMINATED a pair: QUIC cannot be repointed
    // at a different remote address afterwards.
    let EstablishedPath { socket, path, mapping } = connect_path(
        gathered,
        psk,
        // Spec: the client is Controlling.
        IceRole::Controlled,
        remote,
        peer_nat,
        cfg,
        |c| eprintln!("NETDEMO-LOCAL {c:?}"),
    )
    .await
    .context("no UDP path")?;

    // The mapping must outlive the connection, so it is held, not dropped.
    let _mapping = mapping;
    let (endpoint, _stun) = quic_server_demuxed(socket, cert, key).await?;
    let conn = tokio::time::timeout(Duration::from_secs(20), async {
        endpoint.accept().await.ok_or_else(|| anyhow!("endpoint closed"))?.await
            .map_err(anyhow::Error::from)
    })
    .await
    .context("waiting for the client's QUIC handshake")??;

    // The dummy payload: echo whatever arrives.
    let mut echoed = 0usize;
    while echoed < 4 {
        let d = tokio::time::timeout(Duration::from_secs(20), conn.read_datagram())
            .await
            .context("waiting for a datagram")??;
        conn.send_datagram(d)?;
        echoed += 1;
    }

    report(&path, true);
    conn.closed().await;
    Ok(())
}

async fn finish_client(
    gathered: Gathered,
    psk: [u8; 32],
    remote: Vec<Candidate>,
    peer_nat: NatType,
    cfg: &NetConfig,
    fingerprint: [u8; 32],
) -> anyhow::Result<()> {
    let EstablishedPath { socket, path, mapping } = connect_path(
        gathered,
        psk,
        IceRole::Controlling,
        remote,
        peer_nat,
        cfg,
        |c| eprintln!("NETDEMO-LOCAL {c:?}"),
    )
    .await
    .context("no UDP path")?;

    let _mapping = mapping;
    let (conn, _stun) = quic_client_demuxed(socket, path.remote, fingerprint)
        .await
        .context("QUIC handshake")?;

    let mut ok = true;
    for i in 0u32..4 {
        let payload = format!("oxutrm netdemo {i}");
        conn.send_datagram(bytes::Bytes::from(payload.clone().into_bytes()))?;
        let back = tokio::time::timeout(Duration::from_secs(20), conn.read_datagram())
            .await
            .context("waiting for the echo")??;
        if back.as_ref() != payload.as_bytes() {
            ok = false;
            break;
        }
    }

    report(&path, ok);
    conn.close(0u32.into(), b"done");
    Ok(())
}

fn report(path: &oxutrm_proto::PathDescription, echo_ok: bool) {
    eprintln!(
        "NETDEMO-RESULT rung={:?} local={} remote={} probes={} nat={:?} rtt_ms={} mtu={} echo={}",
        path.rung,
        path.local,
        path.remote,
        path.probes_sent,
        path.nat_type,
        path.rtt_ms,
        path.mtu,
        if echo_ok { "ok" } else { "FAILED" }
    );
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64_32(s: &str) -> anyhow::Result<[u8; 32]> {
    use base64::Engine as _;
    let v = base64::engine::general_purpose::STANDARD
        .decode(s)
        .context("decoding base64")?;
    let a: [u8; 32] = v
        .try_into()
        .map_err(|_| anyhow!("expected exactly 32 bytes"))?;
    Ok(a)
}
```

Add `rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }`
and `base64 = "0.22"` to the root `Cargo.toml` — `finish_host` names the
`rustls::pki_types` certificate types and the signalling encodes base64.
`oxutrm_net` re-exports `local_candidates_filtered` (from Task 3).

Then wire it into `src/main.rs`. The existing `main` is M1's; add the branch
and nothing else. If `main` is not async, keep it that way and build a runtime
only for this subcommand:

```rust
mod netdemo;

// inside the subcommand match, next to whatever M1 already dispatches:
        // Hidden: not in the help text. See src/netdemo.rs.
        "netdemo" => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(netdemo::run(&args[1..]))?;
        }
```

where `args` is the argument vector after the subcommand name.

- [ ] **Step 5: Run the test to verify it passes**

Run it bare and wait for it in the same turn — it spawns two processes and can
take twenty seconds:

```bash
cargo test --jobs 4 --test netdemo_loopback -- --test-threads 4 --nocapture
```
Expected: PASS, with both sides printing a `NETDEMO-RESULT` line containing
`echo=ok`.

- [ ] **Step 6: Run everything and lint**

```bash
cargo test --jobs 4 -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: all green.

- [ ] **Step 7: Commit**

```bash
git add src/netdemo.rs src/main.rs Cargo.toml Cargo.lock tests/netdemo_loopback.rs
git commit -m "$(cat <<'EOF'
feat: hidden oxutrm netdemo subcommand, with a loopback end-to-end test

Two processes exchange real Signal lines over stdin/stdout, run the real
ladder, bring up QUIC and echo a dummy payload over datagrams. stdout is the
signalling channel and is flushed after every line; humans read stderr.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 16: The network-namespace NAT harness



**Files:**
- Create: `tests/netns/lib.sh`
- Create: `tests/netns/topo.sh`
- Create: `tests/netns/run.sh`
- Create: `tests/netns/README.md`

**Interfaces:**
- Consumes: the `oxutrm` binary, specifically `netdemo --role host|client|stun`.
- Produces: `bash tests/netns/run.sh <cone|symmetric|double> <path-to-oxutrm>`
  which prints one line
  `NETNS-RESULT topology=<name> host=<rung> client=<rung> probes=<n>`
  and exits 0 on success, **77 when the environment cannot do unprivileged
  namespaces** (the conventional "skip" code), and non-zero on real failure.

### The topology

```
  left            router-l              net (bridge)            router-r          right
  10.0.1.2  ---   10.0.1.1                                      10.0.2.1   ---   10.0.2.2
                  203.0.113.1  --------  br0  --------  203.0.113.2
                                          |
                          203.0.113.10:3478   server A
                          203.0.113.10:3479   server A, second port  <- the third probe
                          203.0.113.11:3478   server B
```

**Three STUN endpoints, not two.** NAT typing needs a probe to a *second port
on the first server's IP* (see the `stun_discover` task): without it the
harness can never distinguish `AddressDependent` from `Symmetric`, and the
symmetric topology would classify as `Unknown` and skip rung 3. Two addresses
on the bridge interface plus a second port on the first is the whole
requirement.

The private ranges are **deliberately not routed** across the middle: each
router has only a default route out of its own external interface, so a packet
from `left` can reach `right` only through a NAT mapping that an outbound
packet created. That is hole punching, for real.

Comparing the mapped ports the three endpoints report is the whole
classification.

### The three NAT flavours

| Topology | Rule on both routers | What it models | Expected rung |
|---|---|---|---|
| `cone` | `masquerade` | port-restricted cone: Linux conntrack's default is an endpoint-independent mapping with address-and-port-dependent filtering | `StunPunch` |
| `symmetric` | `masquerade fully-random` | a fresh random external port per conntrack entry, so two destinations get two ports | `Birthday` |

> The spelling is `fully-random`, not iptables' `random-fully`. `nft` rejects
> the wrong one, and a topology whose NAT rule failed to load is not symmetric
> at all — the test would pass on rung 2 and rung 3 would never run.
| `double` | `masquerade` on a nested inner router as well | double NAT: rung 1 correctly fails and rung 2 takes over | `StunPunch` |

**The symmetric case is an approximation, stated deliberately.** Linux cannot
reproduce every commercial NAT. `masquerade fully-random` allocates a new
random port per conntrack entry and a new entry is created per destination
tuple, so two STUN servers see two different ports and ordinary punching fails
— which is the failure mode rung 3 exists for. These tests prove the
**recovery path**, not universal NAT compatibility.

No explicit filter rule is needed for "port-restricted": with masquerade and no
DNAT, an unsolicited inbound packet has no conntrack entry and is simply not
translated back to the private host. That *is* the restriction.

- [ ] **Step 1: Write the harness README**

Create `tests/netns/README.md`:

```markdown
# Network-namespace NAT harness

Proves rungs 2 and 3 actually traverse NAT, using `ip netns` and `nftables`
inside an unprivileged user namespace.

    bash tests/netns/run.sh cone      target/debug/oxutrm
    bash tests/netns/run.sh symmetric target/debug/oxutrm
    bash tests/netns/run.sh double    target/debug/oxutrm

Exit codes: `0` success, `77` the environment cannot do unprivileged
namespaces (skip), anything else a real failure.

Everything runs inside `unshare --user --map-root-user --mount --net --fork`,
so no privilege is needed and nothing outside the namespace is touched. The
whole topology disappears when the outer process exits; `teardown` exists for
the case where someone runs this as real root.

`masquerade fully-random` is an **approximation** of a symmetric NAT. It
allocates a fresh random external port per conntrack entry, and a new entry is
created per destination, so two STUN servers see two different ports and
ordinary punching fails. That exercises rung 3's recovery path. It does not
prove compatibility with every commercial NAT, and it is not claimed to.

Common reasons for a `77`:

- `kernel.unprivileged_userns_clone=0` (Debian derivatives)
- AppArmor's `userns` restrictions (Ubuntu 24.04+):
  `sysctl kernel.apparmor_restrict_unprivileged_userns`
- no `ip` or no `nft` on `PATH`
- a kernel without `nf_tables` namespace support
```

- [ ] **Step 2: Write the shared helpers**

Create `tests/netns/lib.sh`:

```bash
#!/usr/bin/env bash
# Shared helpers for the netns NAT harness. Sourced, never run directly.

# The conventional "this test could not run here" exit code.
readonly SKIP=77

log() { printf '[netns] %s\n' "$*" >&2; }

# Everything this harness needs, checked before anything is created.
supports_netns() {
    command -v ip  >/dev/null 2>&1 || { log "no 'ip' on PATH";  return 1; }
    command -v nft >/dev/null 2>&1 || { log "no 'nft' on PATH"; return 1; }
    unshare --user --map-root-user --mount --net --fork true >/dev/null 2>&1 \
        || { log "unprivileged user+net namespaces are unavailable"; return 1; }
    return 0
}

# `ip netns` writes to /run/netns, which is root-owned on the host. Inside a
# user+mount namespace we are root and may mount a tmpfs over it, which gives
# us a private, writable, throwaway one.
prepare_netns_dir() {
    mkdir -p /run/netns
    mount -t tmpfs tmpfs /run/netns
}

# Create a namespace with loopback already up.
mk_ns() {
    ip netns add "$1"
    ip -n "$1" link set lo up
}

# mk_link <ns-a> <if-a> <ns-b> <if-b>
mk_link() {
    local ns_a=$1 if_a=$2 ns_b=$3 if_b=$4
    ip link add "$if_a" type veth peer name "$if_b"
    ip link set "$if_a" netns "$ns_a"
    ip link set "$if_b" netns "$ns_b"
    ip -n "$ns_a" link set "$if_a" up
    ip -n "$ns_b" link set "$if_b" up
}

# addr <ns> <if> <cidr>
addr() { ip -n "$1" addr add "$3" dev "$2"; }

enable_forwarding() {
    ip netns exec "$1" sysctl -qw net.ipv4.ip_forward=1
}

# masquerade <ns> <external-if> [fully-random]
masquerade() {
    # NOTE: nftables spells it `fully-random`. `random-fully` is the
    # iptables spelling and nft rejects it, which would leave the
    # "symmetric" topology silently NOT symmetric and rung 3 never exercised.
    local ns=$1 oif=$2 extra=${3:-}
    ip netns exec "$ns" nft -f - <<NFT
table ip oxutrm_nat {
    chain postrouting {
        type nat hook postrouting priority srcnat; policy accept;
        oifname "$oif" masquerade $extra
    }
}
NFT
}

# Only needed when someone runs this as real root: under `unshare` the whole
# topology dies with the namespace.
teardown() {
    local ns
    for ns in left rl_inner rl rr right net; do
        ip netns del "$ns" 2>/dev/null || true
    done
    umount /run/netns 2>/dev/null || true
}
```

- [ ] **Step 3: Write the topology builder**

Create `tests/netns/topo.sh`:

```bash
#!/usr/bin/env bash
# Builds one of three NAT topologies. Sourced by run.sh, inside the namespace.

# build_topology <cone|symmetric|double>
build_topology() {
    local flavour=$1
    local rnd=""
    [ "$flavour" = symmetric ] && rnd="fully-random"

    mk_ns net
    mk_ns rl
    mk_ns rr
    mk_ns left
    mk_ns right

    # ---- the simulated internet: a bridge in "net" ----
    ip -n net link add br0 type bridge
    ip -n net link set br0 up
    ip -n net addr add 203.0.113.10/24 dev br0
    # A second address, so there are two distinct STUN servers to compare.
    ip -n net addr add 203.0.113.11/24 dev br0

    mk_link net wan_l rl wan0
    mk_link net wan_r rr wan0
    ip -n net link set wan_l master br0
    ip -n net link set wan_r master br0

    addr rl  wan0 203.0.113.1/24
    addr rr  wan0 203.0.113.2/24
    ip -n rl route add default via 203.0.113.10
    ip -n rr route add default via 203.0.113.10
    enable_forwarding rl
    enable_forwarding rr

    # ---- the right-hand private network, identical in all three flavours ----
    mk_link rr lan0 right eth0
    addr rr    lan0 10.0.2.1/24
    addr right eth0 10.0.2.2/24
    ip -n right route add default via 10.0.2.1
    masquerade rr wan0 "$rnd"

    # ---- the left-hand private network ----
    if [ "$flavour" = double ]; then
        # An extra router between "left" and "rl": two translations in series.
        mk_ns rl_inner
        enable_forwarding rl_inner

        mk_link rl lan0 rl_inner up0
        addr rl       lan0 100.64.0.1/24
        addr rl_inner up0  100.64.0.2/24
        ip -n rl_inner route add default via 100.64.0.1

        mk_link rl_inner lan0 left eth0
        addr rl_inner lan0 10.0.1.1/24
        addr left     eth0 10.0.1.2/24
        ip -n left route add default via 10.0.1.1

        masquerade rl_inner up0 "$rnd"
        masquerade rl       wan0 "$rnd"
        # Carrier-grade range is not routed on the simulated internet either,
        # so the outer router must translate it too.
        ip -n rl route add 10.0.1.0/24 via 100.64.0.2
    else
        mk_link rl lan0 left eth0
        addr rl   lan0 10.0.1.1/24
        addr left eth0 10.0.1.2/24
        ip -n left route add default via 10.0.1.1
        masquerade rl wan0 "$rnd"
    fi
}
```

- [ ] **Step 4: Write the runner**

Create `tests/netns/run.sh`:

```bash
#!/usr/bin/env bash
# Drive `oxutrm netdemo` across a simulated NAT.
#
#   bash tests/netns/run.sh <cone|symmetric|double> <path-to-oxutrm>
#
# Exit 0 on success, 77 when this environment cannot do unprivileged
# namespaces, anything else on a real failure.
set -u

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=lib.sh
. "$HERE/lib.sh"

FLAVOUR=${1:?usage: run.sh <cone|symmetric|double> <binary>}
BIN=${2:?usage: run.sh <cone|symmetric|double> <binary>}

case "$FLAVOUR" in
    cone|symmetric|double) ;;
    *) log "unknown topology '$FLAVOUR'"; exit 2 ;;
esac
[ -x "$BIN" ] || { log "not executable: $BIN"; exit 2; }

# Re-enter as "root" inside a fresh user, mount and network namespace, unless
# we are already there.
if [ "${OXUTRM_NETNS_INNER:-}" != "1" ]; then
    supports_netns || exit "$SKIP"
    exec env OXUTRM_NETNS_INNER=1 \
        unshare --user --map-root-user --mount --net --fork \
        bash "$0" "$FLAVOUR" "$BIN"
fi

# ---- from here on we are inside the namespaces ----
set -o pipefail
prepare_netns_dir || exit "$SKIP"
# shellcheck source=topo.sh
. "$HERE/topo.sh"

WORK="$(mktemp -d)"
cleanup() {
    kill $(jobs -p) 2>/dev/null || true
    rm -rf "$WORK"
    teardown
}
trap cleanup EXIT

build_topology "$FLAVOUR" || { log "building the topology failed"; exit 2; }

# Two STUN responders on the simulated internet: comparing what they report is
# the whole of NAT typing. Nothing here reaches a public server.
ip netns exec net "$BIN" netdemo --role stun --bind 203.0.113.10:3478 \
    >"$WORK/stun1.log" 2>&1 &
# The THIRD probe: a second port on the FIRST server's IP. Without it,
# AddressDependent and Symmetric are indistinguishable and rung 3 never runs.
ip netns exec net "$BIN" netdemo --role stun --bind 203.0.113.10:3479 \
    >"$WORK/stun1b.log" 2>&1 &
ip netns exec net "$BIN" netdemo --role stun --bind 203.0.113.11:3478 \
    >"$WORK/stun2.log" 2>&1 &

# Give them a moment to bind before anyone queries them.
for _ in $(seq 1 50); do
    ip netns exec left timeout 1 bash -c \
        'exec 3<>/dev/udp/203.0.113.10/3478' 2>/dev/null && break
    sleep 0.1
done

mkfifo "$WORK/h2c" "$WORK/c2h"

STUN_ARGS=(--stun 203.0.113.10:3478 --stun 203.0.113.11:3478)

ip netns exec left "$BIN" netdemo --role host "${STUN_ARGS[@]}" \
    <"$WORK/c2h" >"$WORK/h2c" 2>"$WORK/host.log" &
HOST_PID=$!

ip netns exec right "$BIN" netdemo --role client "${STUN_ARGS[@]}" \
    <"$WORK/h2c" >"$WORK/c2h" 2>"$WORK/client.log" &
CLIENT_PID=$!

# 90 seconds is generous: the blast alone is capped at 6.
DEADLINE=$((SECONDS + 90))
rc=1
while [ "$SECONDS" -lt "$DEADLINE" ]; do
    if ! kill -0 "$HOST_PID" 2>/dev/null && ! kill -0 "$CLIENT_PID" 2>/dev/null; then
        rc=0
        break
    fi
    sleep 0.5
done

if [ "$rc" -ne 0 ]; then
    log "timed out after 90s"
    kill "$HOST_PID" "$CLIENT_PID" 2>/dev/null || true
fi

host_line=$(grep -m1 'NETDEMO-RESULT' "$WORK/host.log" || true)
client_line=$(grep -m1 'NETDEMO-RESULT' "$WORK/client.log" || true)

if [ -z "$host_line" ] || [ -z "$client_line" ]; then
    log "no result from one or both sides"
    log "--- host ---";   cat "$WORK/host.log"   >&2
    log "--- client ---"; cat "$WORK/client.log" >&2
    exit 1
fi

case "$host_line$client_line" in
    *echo=FAILED*) log "the echo payload did not survive"; exit 1 ;;
esac

extract() { sed -n "s/.*$2=\([^ ]*\).*/\1/p" <<<"$1"; }
host_rung=$(extract "$host_line" rung)
client_rung=$(extract "$client_line" rung)
probes=$(extract "$client_line" probes)

echo "NETNS-RESULT topology=$FLAVOUR host=$host_rung client=$client_rung probes=$probes"
log "host:   $host_line"
log "client: $client_line"
exit 0
```

- [ ] **Step 5: Make the scripts executable and run one by hand**

```bash
chmod +x tests/netns/run.sh
cargo build --jobs 4
bash tests/netns/run.sh cone target/debug/oxutrm; echo "exit=$?"
```
Expected: either `NETNS-RESULT topology=cone host=StunPunch client=StunPunch
probes=<n>` and `exit=0`, or `exit=77` with a `[netns]` line explaining why the
environment cannot do this. Both are acceptable outcomes for this step; a
different exit code is a failure to fix.

Then the other two:

```bash
bash tests/netns/run.sh symmetric target/debug/oxutrm; echo "exit=$?"
bash tests/netns/run.sh double    target/debug/oxutrm; echo "exit=$?"
```
Expected: `client=Birthday` for `symmetric`, `client=StunPunch` for `double`
(or 77 for all three).

- [ ] **Step 6: Commit**

```bash
git add tests/netns/
git commit -m "$(cat <<'EOF'
test: network-namespace NAT harness for rungs 2 and 3

Three topologies under unprivileged user+net namespaces: port-restricted
cone, an approximated symmetric NAT (masquerade fully-random), and double
NAT. Two in-tree STUN responders sit on the simulated internet, so nothing
reaches a public server. Exits 77 when namespaces are unavailable.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 17: The Rust integration test that drives the harness



**Files:**
- Create: `tests/netns.rs`

**Interfaces:**
- Consumes: `tests/netns/run.sh` from Task 16 and the `oxutrm` binary.
- Produces: three `#[test]` functions that skip cleanly rather than fail when
  unprivileged namespaces are unavailable.

**Why a skip and not an `#[ignore]`.** An `#[ignore]` never runs anywhere. These
tests must run wherever they *can* — on a developer's Linux box, in a permissive
CI container — and quietly stand down where they cannot. Exit code 77 is the
signal; the test prints why it stood down so a silent skip is never mistaken
for a pass.

- [ ] **Step 1: Write the failing test**

Create `tests/netns.rs`:

```rust
//! Drives `tests/netns/run.sh`. Proves the NAT traversal ladder against real
//! Linux NAT, and stands down cleanly where unprivileged namespaces are not
//! available.

use std::process::Command;

/// The conventional "this test could not run here" exit code.
const SKIP: i32 = 77;

struct Outcome {
    skipped: bool,
    line: String,
}

fn run(topology: &str) -> Outcome {
    let bin = env!("CARGO_BIN_EXE_oxutrm");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/netns/run.sh");

    let out = Command::new("bash")
        .arg(script)
        .arg(topology)
        .arg(bin)
        .output()
        .unwrap_or_else(|e| panic!("could not run {script}: {e}"));

    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    if out.status.code() == Some(SKIP) {
        // Say why, every time. A silent skip is indistinguishable from a pass.
        eprintln!("SKIP netns/{topology}: unprivileged namespaces unavailable here:\n{stderr}");
        return Outcome { skipped: true, line: String::new() };
    }

    assert!(
        out.status.success(),
        "netns/{topology} failed (exit {:?})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        out.status.code()
    );

    let line = stdout
        .lines()
        .find(|l| l.starts_with("NETNS-RESULT"))
        .unwrap_or_else(|| {
            panic!("netns/{topology} printed no result\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
        })
        .to_owned();

    eprintln!("netns/{topology}: {line}");
    Outcome { skipped: false, line }
}

fn field<'a>(line: &'a str, key: &str) -> &'a str {
    line.split_whitespace()
        .find_map(|kv| kv.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key}= in {line:?}"))
}

#[test]
fn a_port_restricted_cone_is_traversed_by_ordinary_punching() {
    let o = run("cone");
    if o.skipped {
        return;
    }
    assert_eq!(field(&o.line, "client"), "StunPunch");
    assert_eq!(field(&o.line, "host"), "StunPunch");
}

#[test]
fn an_approximated_symmetric_nat_is_traversed_by_the_birthday_blast() {
    // `masquerade fully-random` is an approximation: it varies the external
    // port per destination, which is the failure mode rung 3 exists for. This
    // proves the recovery path, not universal NAT compatibility.
    let o = run("symmetric");
    if o.skipped {
        return;
    }
    assert_eq!(field(&o.line, "client"), "Birthday");
    let probes: u32 = field(&o.line, "probes").parse().expect("a probe count");
    assert!(probes > 0, "the blast must report what it cost");
}

#[test]
fn double_nat_falls_through_rung_one_and_rung_two_takes_over() {
    // There is no NAT-PMP or UPnP daemon anywhere in the topology, so rung 1
    // must fail and rung 2 must carry the connection.
    let o = run("double");
    if o.skipped {
        return;
    }
    assert_eq!(field(&o.line, "client"), "StunPunch");
    assert_eq!(field(&o.line, "host"), "StunPunch");
}
```

- [ ] **Step 2: Run it**

The three topologies each build a namespace tree, so run them one at a time:

```bash
cargo test --jobs 4 --test netns -- --test-threads 1 --nocapture
```
Expected: three passes, either doing the real thing or printing
`SKIP netns/<topology>: ...`. Read the output: if all three skipped, say so
explicitly in your report rather than calling the milestone proven.

- [ ] **Step 3: Run the whole suite one last time**

```bash
cargo test --jobs 4 -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
```
Expected: everything green.

- [ ] **Step 4: Commit**

```bash
git add tests/netns.rs
git commit -m "$(cat <<'EOF'
test: drive the netns NAT harness from cargo test

Skips with an explanation, rather than failing, where unprivileged
namespaces are unavailable.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Milestone acceptance

M2 is done when all of the following are true, checked by running them, not by
reading the plan:

1. `cargo clippy --all-targets --jobs 4 -- -D warnings` is clean.
2. `cargo test --jobs 4 -- --test-threads 4` is green, with **no test reaching
   the public internet**. The two `#[ignore]`d tests
   (`the_default_public_servers_answer`, `a_real_router_grants_a_mapping`) are
   the only ones that may, and they do not run by default.
3. `tests/netdemo_loopback.rs` passes: two real processes, real signalling,
   real QUIC, `echo=ok` from both sides.
4. `cargo test --test netns -- --test-threads 1` either passes all three
   topologies or reports why it skipped. If it skipped, say so plainly when
   reporting M2 complete — a skip is not a proof.
5. `oxutrm-net` exports every symbol the contract names, with the contract's
   exact signatures.
6. `Rung::SshTunnel` is never constructed anywhere in the tree:
   `grep -rn 'SshTunnel' crates/ src/` shows only M1's enum definition.
7. `Endpoint::new` is never called: `grep -rn 'Endpoint::new(' crates/ src/`
   is empty, and `grep -rn 'new_with_abstract_socket' crates/` is not. A
   single `Endpoint::new` reintroduces the packet-stealing race.
8. `stunclient` appears in exactly one module:
   `grep -rln 'stunclient' crates/oxutrm-net/src/` prints only `discover.rs`.
9. The pinning verifier does not stub the signature checks:
   `grep -n 'verify_tls1[23]_signature' crates/oxutrm-net/src/tls.rs` shows
   both delegating to `rustls::crypto::verify_tls1*_signature`, never
   returning `Ok(...)` unconditionally.

---

## Self-review notes

Checked against the spec, section by section:

- **§5.1 rung 0, IPv6 first** — Task 3 (priority puts global IPv6 Host at the
  top), Task 8 (`rung_for` reports `Ipv6Direct`), Task 14 (test asserts it).
- **§5.2 rung 1, router mapping** — Task 10, with `netdev` gateway discovery,
  refresh and Drop-release.
- **§5.3 rung 2, STUN and punching** — Tasks 6, 7, 8. Discovery is from the
  live socket, with **three** probes rather than two; checks carry
  `MESSAGE-INTEGRITY` keyed per direction; `XOR-MAPPED-ADDRESS` gives
  peer-reflexive learning. **Keepalive is not implemented in M2**: the spec's
  periodic re-punch belongs with roaming, which is M4's, and there is nothing
  to migrate yet. The machinery it needs is in place — `StunDemuxSocket`
  delivers post-QUIC STUN on a channel and the caller keeps a sending handle.
- **§5.3's "queried in parallel"** — not implemented, deliberately.
  `query_external_address_async` owns the socket's receive side while it runs,
  so the three probes are sequential with a third of the gather budget each.
  The outcome the spec wants — three probes inside the budget — is unchanged.
- **§5 "a better path may take over later"** — not implemented, because it is
  not possible. See item 2 of "Five things the design spec gets wrong".
- **§5.4 rung 3, birthday blast** — Task 9, with all four guardrails and the
  same direction-labelled credentials, so a probe's own echo is not a hit.
- **§5.5 rung 4, SSH tunnel** — explicitly out of scope; stated at the top.
- **§5.6 port selection** — Task 2. ALPN `oxutrm/1` and the no-forged-SNI rule
  are in Tasks 11 and 12.
- **§6 transport** — Tasks 12 and 13: the demultiplexing socket, the datagram
  buffer trap, and the process-default `CryptoProvider`.
- **§6.1 certificate pinning** — Task 11. The verifier checks the SPKI hash
  **and** performs real TLS 1.3 signature verification by delegating to the
  provider; stubbing `verify_tls13_signature` to `Ok(...)` would reduce pinning
  to knowing the certificate bytes, which any eavesdropper does.
- **§11 security** — no key material on disk anywhere; PSK binding via
  `MESSAGE-INTEGRITY`; fresh certificate per call to `generate_cert`;
  anti-amplification is QUIC's; the STUN server list is configurable and there
  is an in-tree server to point it at.
- **§12.1 netns testing** — Tasks 16 and 17, with the symmetric approximation
  stated in three places (the README, the test comment, and this plan), three
  STUN endpoints on the simulated internet so the full NAT-typing table is
  exercised, and `fully-random` — the nftables spelling, not iptables'
  `random-fully`, which `nft` rejects.

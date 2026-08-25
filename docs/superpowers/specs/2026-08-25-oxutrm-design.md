# oxutrm — Design Specification

**Date:** 2026-08-25
**Status:** Approved design, phases A+B specified in full; C and D sketched.

---

## 1. Purpose

oxutrm is a remote terminal that survives bad networks, changing IP addresses,
and NAT on both ends. It replaces the `ssh` + `tmux` habit of "reconnect and
hope" with a session that simply stays alive.

It is, deliberately, **Mosh rebuilt in Rust with a real terminal emulator on
both ends**, plus the two things Mosh never solved: NAT traversal and
scrollback.

### 1.1 Goals

- **Encrypted UDP transport** that outlives IP changes on either side.
- **Both endpoints may sit behind NAT.**
- **SSH for session initiation and reattachment** — no new trust root, no new
  daemon to expose.
- **Real `vt100` emulation on both ends**, so screen state is authoritative on
  the host and predictions on the client are correct rather than approximated.
- **Detach and reattach**: the remote session outlives the client indefinitely.
- **Working scrollback**, which Mosh cannot provide.
- **Full fidelity**: 24-bit colour, SGR mouse reporting, resize, window title,
  OSC 52 clipboard.
- **Bandwidth adaptation** so a poor link degrades gracefully instead of
  falling behind.
- **`tmux -CC` control mode integration** (phase D), so window switching and the
  status bar become local and instant.
- **Tell the user what connection they got.** No silent magic.

### 1.2 Non-goals

- Graphics protocols (Sixel, Kitty, iTerm2 inline images). `vt100` does not
  model them; claiming support would be a lie.
- A GUI client. oxutrm renders into an existing terminal.
- Replacing tmux. oxutrm integrates with it (phase D) rather than competing.
- Being a general-purpose VPN or port forwarder.
- Windows support in v1. Unix PTY semantics are assumed throughout.

### 1.3 What differs from Mosh, and why it matters

| Area | Mosh | oxutrm |
|---|---|---|
| Transport | hand-rolled AES-OCB over UDP | QUIC (`quinn`), key pinned via SSH |
| Roaming | custom address-update logic | QUIC connection migration (specified behaviour) |
| Congestion control | hand-rolled | QUIC's, already correct |
| NAT | none — server must be reachable | five-rung ladder, both ends may be NATed |
| Firewalls | UDP 60000-61000, widely blocked | UDP/443, the one UDP port usually open |
| Client prediction | heuristic overlay | a second real `vt100`, reconciled |
| Scrollback | broken | synced, and local scrolling is instant |
| Reattach | none | same code path as first connect |
| `TERM` | hardcoded `xterm-256color` | negotiated from the client's real capabilities |
| Bulk transfers | none | separate QUIC streams, cannot block the screen |

---

## 2. Phase plan

The full system is too large for one implementation cycle. It is split into four
phases, each with its own plan and working milestone.

| Phase | Contents | Status |
|---|---|---|
| **A — Link** | UDP socket, QUIC, NAT ladder, roaming, SSH bootstrap and signalling, session registry, detach/reattach | **specified here** |
| **B — Terminal sync** | Host PTY + `vt100`, state-diff engine, client renderer, resize, mouse, colour, OSC | **specified here** |
| **C — Feel** | Speculative local echo, synced scrollback, bandwidth adaptation | sketched, §14 |
| **D — tmux** | `tmux -CC` control mode, one `vt100` per pane, local layout and status bar | sketched, §15 |

**A+B together are the first usable deliverable**: a working remote terminal.

---

## 3. Architecture overview

One binary, three roles. Exactly one thing to install per machine.

| Invocation | Runs where | Job |
|---|---|---|
| `oxutrm <ssh-target>` | local | wrapper: drives SSH, then becomes the client |
| `oxutrm host --serve` | remote | spawned over SSH; owns the PTY and authoritative `vt100` |
| `oxutrm host --list` / `--attach <id>` | remote | session registry queries, spawned over SSH |

oxutrm never parses `~/.ssh/config`. It shells out to `ssh` and assumes the
user has already made `ssh <target>` work, by whatever means — jump host,
reverse tunnel, VPN, direct.

```
  laptop                          bastion / whatever                remote host
  ------                          ------------------                -----------
  oxutrm (client)  --- ssh ------------------------------------>  oxutrm host
      |                                                                 |
      |  1. SSH carries handshake + candidate exchange (signalling)     |
      |                                                                 |
      |<========== 2. STUN punching, then QUIC, direct UDP ============>|
      |                                                                 |
      |            3. SSH closes; host daemonizes                       |
```

---

## 4. Bootstrap and signalling

### 4.1 Why SSH stays open

Mosh closes SSH the moment it has a key. oxutrm keeps the SSH channel open
**for the duration of connection establishment**, because NAT traversal needs a
bidirectional signalling channel: candidates are discovered asynchronously
(router mapping, STUN answers, peer-reflexive learning) and must be exchanged as
they appear. Once a QUIC connection is up, SSH is closed.

Reattachment reopens SSH and repeats the same exchange. **Connect and reattach
are one code path.**

### 4.2 Signalling messages

Newline-delimited JSON on the SSH child's stdin/stdout. JSON, not a binary
format, because this channel is low-volume, human-debuggable, and version
skew here must fail loudly.

```rust
/// host -> client, first line
struct HostHello {
    proto: u32,                  // protocol version, hard failure on mismatch
    session_id: String,          // 128-bit, hex
    cert_spki_sha256: [u8; 32],  // pins the host's self-signed QUIC certificate
    psk: String,                 // base64, 32 random bytes: ICE credential + QUIC PSK binding
    candidates: Vec<Candidate>,
    nat_type: NatType,
    bound_port: u16,
}

/// client -> host, first line
struct ClientHello {
    proto: u32,
    candidates: Vec<Candidate>,
    nat_type: NatType,
    caps: TerminalCaps,
    size: (u16, u16),            // cols, rows
}

/// either direction, repeatable until the link is up
struct CandidateUpdate { candidates: Vec<Candidate> }

/// either direction, terminates signalling
struct Established { path: PathDescription }
struct Failed { reason: String }
```

```rust
struct Candidate {
    addr: SocketAddr,
    kind: CandidateKind,   // Host | PortMapped | ServerReflexive | PeerReflexive
    priority: u32,         // ICE-style; IPv6 Host highest, PeerReflexive lowest
}

enum NatType { None, EndpointIndependent, AddressDependent, Symmetric, Unknown }
```

`psk` is 32 bytes from the OS CSPRNG. It never touches disk on either side.

### 4.3 Daemonizing

After `HostHello` is written and the link is established, the host process must
**fully detach**: `fork`, `setsid`, `fork` again, `chdir("/")`, and close every
file descriptor inherited from SSH, reopening `0/1/2` on `/dev/null`. If any SSH
descriptor is retained, closing the laptop lid kills the session — which is the
exact failure the project exists to prevent.

---

## 5. Connection establishment — the ladder

Rungs run **concurrently where cheap and ordered where costly**. The first
validated path wins. A better path appearing later may take over, because QUIC
connection migration makes the switch free and invisible.

| Rung | Mechanism | Crate | Wins when |
|---|---|---|---|
| **0** | **IPv6 direct** | std | both ends have global IPv6 — no NAT exists at all |
| **1** | **Router port mapping**: NAT-PMP, then PCP, then UPnP-IGD | `crab_nat`, `igd-next` | typical home router; yields an exact public IP **and** port |
| **2** | **STUN + hole punch** | `stunclient`, `stun_codec` | most NATs |
| **3** | **Birthday-paradox blast** | own | symmetric NAT |
| **4** | **SSH tunnel** | own | UDP blocked entirely |

### 5.1 Rung 0 — IPv6 first

Where both ends have a global IPv6 address there is no NAT to traverse; only a
stateful firewall pinhole, which the outbound punch itself creates. Rung 0 is
raced against rung 1 and 2, Happy-Eyeballs style, and preferred when it wins.

This is the single highest-value rung and is therefore implemented first.

### 5.2 Rung 1 — router mapping

Try NAT-PMP and PCP (`crab_nat`), then UPnP-IGD (`igd-next`), with a 1500 ms
budget each. Success yields a `PortMapped` candidate with the exact external
address. Mappings are refreshed for the life of the session and released on
clean shutdown.

**If either side gets a port mapping, the whole connection succeeds** — the
other side punches to the mapped address, and its own address is learned
peer-reflexively from the arriving packet.

### 5.3 Rung 2 — STUN and punching

**Address discovery.** STUN binding requests are sent **from the socket QUIC
will use**, because NAT mappings are per-socket; an address learned on any other
socket describes nothing useful. This is also why an HTTP echo service such as
`ifconfig.co` is *not* used for discovery: it answers over TCP and reports only
the IP, while the port is the hard half.

Two or three servers are queried in parallel from a configurable list with these
defaults:

```
stun.cloudflare.com:3478
stun.l.google.com:19302
stun.nextcloud.com:443        # useful where 3478 is blocked
stun.sipgate.net:3478
```

**NAT typing, free.** Comparing the mapped port reported by two *different*
STUN servers classifies the NAT in two packets:

- same port from both → endpoint-independent mapping (cone); ordinary punching
  will work.
- different ports → **symmetric**; ordinary punching is hopeless, so skip
  directly to rung 3 rather than burning several seconds failing.

**Punching uses real ICE.** Probes are **STUN Binding Requests carrying a
`MESSAGE-INTEGRITY` attribute keyed by the `psk`**, exactly as ICE
connectivity checks do. This is chosen over a bespoke probe format because:

- it demultiplexes cleanly against QUIC on the same socket — a STUN packet's
  first two bits are `00`, while every QUIC packet has the fixed bit set;
- `MESSAGE-INTEGRITY` means strangers cannot confuse the state machine and
  oxutrm cannot be abused as a reflector;
- the `XOR-MAPPED-ADDRESS` in the response *is* peer-reflexive discovery, free;
- `stun_codec` already implements it.

Both sides send checks to every candidate of the peer, simultaneously. The first
pair to produce a valid response in both directions is nominated.

**Keepalive.** Periodic STUN binding requests continue on the live socket. If
the reported mapping changes, oxutrm knows its address changed **before** QUIC
has to discover it the hard way, and can pre-emptively re-punch.

### 5.4 Rung 3 — birthday-paradox blast

Behind a symmetric NAT the external port is unpredictable, but not
*unguessable*. Both sides open **N ≈ 256 sockets** and each fires at
**M ≈ 256 guessed ports** around the peer's observed base port. That is ~65k
combinations against an ephemeral range of similar size, so a collision is
likely within a few seconds.

Guardrails, because this is deliberately noisy:

- entered **only** when STUN typing reported `Symmetric`, or after rungs 0-2
  have failed;
- total probes and total duration are hard-capped and configurable;
- probes are the same authenticated STUN checks, so nothing unauthenticated is
  ever emitted;
- the number of probes actually sent is reported in the status line, so the cost
  is visible.

### 5.5 Rung 4 — SSH tunnel fallback

If no UDP path forms, QUIC runs inside a stream over the SSH connection already
held. The session works; it is slower and it dies on IP change. This is shown as
a **warning** in the status line, never silently.

### 5.6 Port selection

The host binds **UDP/443** when permitted. Restrictive networks tend to leave
UDP/443 open because blocking it breaks HTTP/3 for every browser present; this
is precisely where Mosh's UDP 60000-61000 fails. Where binding 443 is not
permitted, a high port is bound and 443 is advertised as a candidate if a router
mapping can provide it.

oxutrm's packets genuinely **are** QUIC, so protocol-classifying middleboxes
see an ordinary QUIC flow. oxutrm does **not** forge another party's domain in
SNI: that is impersonation, it buys nothing over honest QUIC framing, and it is
out of scope. ALPN is `oxutrm/1`.

---

## 6. Transport — QUIC

`quinn` 0.11 over a `UdpSocket` that oxutrm has already punched. `quinn`
accepts a pre-existing socket, so punching and QUIC never fight over the port.

What QUIC provides, that would otherwise have to be written and debugged:

| Feature | Use in oxutrm |
|---|---|
| TLS 1.3 with a pinned self-signed certificate | encryption and authentication rooted in SSH |
| **Connection migration** | roaming across IP changes, by specification |
| **Congestion control** | the substrate of bandwidth adaptation (§10.4) |
| **Unreliable datagrams** (RFC 9221) | screen deltas that must never be retransmitted |
| **Reliable streams** | scrollback, clipboard, files, tmux control |
| **Anti-amplification limit** | an unauthenticated peer can never make the host send >3x |
| **DPLPMTUD** (RFC 8899) | correct packet size through VPNs and tunnels |
| **ECN** | graceful degradation instead of stuttering |
| **GSO/GRO** | cheap full-screen repaints |
| **0-RTT** | fast reattach |

`TransportConfig` must set `datagram_receive_buffer_size` and
`datagram_send_buffer_size`, or datagrams are disabled.

### 6.1 Certificate pinning

The host generates a self-signed certificate at session creation. The SHA-256 of
its SPKI travels in `HostHello` over SSH. The client installs a `rustls`
`ServerCertVerifier` that accepts **exactly that fingerprint and nothing else**.
No certificate authority is involved; the trust chain is SSH's, unchanged.

The `psk` additionally binds the QUIC session to the SSH exchange, so a peer
that somehow obtained the certificate but not the PSK cannot complete ICE.

---

## 7. Wire protocol

`postcard` for compactness and `serde` derive. All integers varint-encoded.
`proto: u32` is checked at handshake; mismatch is a hard, loud failure.

### 7.1 Datagram framing

Every datagram carries three sequence numbers and a payload:

```rust
struct Frame {
    my_state: u64,       // the state this datagram describes
    from_state: u64,     // the peer-acknowledged state it is a diff against
    ack_state: u64,      // highest peer state I have applied
    flags: u8,           // bit 0: payload is zstd-compressed
    payload: Vec<u8>,    // postcard-encoded ScreenDiff or InputDiff
}
```

Compression is applied only when it actually shrinks the payload, and the
threshold is measured, not assumed.

### 7.2 Streams

| Stream | Type | Contents |
|---|---|---|
| Control | bidi, client-opened, long-lived | `SessionInfo`, `CapsUpdate`, `StatusRequest` |
| Scrollback | bidi, client-opened, per request | `ScrollbackReq { from, to }` → lines → FIN |
| Clipboard | bidi, per transfer | OSC 52 payloads, both directions |
| Tmux | bidi, long-lived | phase D only |

Separate streams matter: a 50 000-line scrollback fetch must never delay a
keystroke.

---

## 8. State synchronisation

The core idea, in one sentence: **do not send what happened; send the difference
between what the peer is known to have and what is true now.** This is Mosh's
SSP, kept because it is genuinely the right design, and simplified because QUIC
supplies the parts Mosh had to build.

### 8.1 Properties

- **A lost datagram costs nothing.** The next one diffs from the same
  acknowledged base and therefore *contains* whatever was lost. There is no
  retransmission of screen state, ever.
- **It cannot fall behind.** If output outruns the link, states are simply
  replaced rather than queued; the next datagram is current by construction. A
  runaway `yes` produces one frame, not a backlog.
- **It is idempotent**, so duplication and reordering need no special handling.

### 8.2 Host → client: `ScreenState`

```rust
struct ScreenState {
    seq: u64,
    rows: u16,
    cols: u16,
    cells: Vec<Cell>,          // rows * cols, row-major
    cursor: Cursor,            // row, col, visible, shape
    modes: Modes,              // alt-screen, bracketed paste, mouse mode, keypad/cursor app
    title: String,
    icon: String,
    bell: u32,                 // monotonic counter; client rings on increase
    scrollback_len: u64,       // total lines ever scrolled off; lines travel on a stream
}

struct Cell { text: CompactString, fg: Color, bg: Color, attrs: Attrs }
```

`text` is a small string rather than a `char` so grapheme clusters and combining
marks survive intact. Wide-character continuation cells are represented
explicitly, not as spaces.

```rust
struct ScreenDiff {
    base: u64,
    target: u64,
    resize: Option<(u16, u16)>,
    rows: Vec<RowPatch>,                 // changed rows only
    cursor: Option<Cursor>,
    modes: Option<Modes>,
    title: Option<String>,
    icon: Option<String>,
    bell: Option<u32>,
    scrollback_len: Option<u64>,
}

struct RowPatch { row: u16, runs: Vec<Run> }
struct Run { start_col: u16, repeat: u16, cells: Vec<Cell> }  // repeat collapses identical runs
```

The host retains a ring of its last **32** states so it can diff from whatever
the client last acknowledged. If the client's acknowledged state has fallen out
of the ring, the host sends a full state (`base == 0`).

### 8.3 Client → host: `InputState`

The same machinery, inverted.

```rust
struct InputState {
    seq: u64,
    pending: Vec<u8>,        // user input not yet acknowledged, in order
    size: (u16, u16),        // latest requested terminal size
}

struct InputDiff {
    base: u64,
    target: u64,
    appended: Vec<u8>,
    size: Option<(u16, u16)>,
}
```

When the host acknowledges state *N*, the client forms state *N+1* with the
consumed prefix removed. Unacknowledged input is therefore retransmitted
automatically, without a retransmission mechanism.

Mouse events and special keys are encoded into `pending` as the byte sequences
the remote application expects, so the host writes them straight to the PTY.

### 8.4 Purity, and the property that matters

`oxutrm-sync` performs **no I/O whatsoever**. Its entire surface is:

```rust
fn diff(&self, from: &State) -> Diff;
fn apply(&mut self, d: &Diff) -> Result<(), ApplyError>;
```

This makes the highest-risk component of the project exhaustively testable
without sockets. See §12.

---

## 9. Host design

### 9.1 Contents of a session

- a PTY with the user's shell (`rustix-openpty`, as in `ansidrama`),
- a `vt100::Parser` with scrollback — **the single source of truth**,
- the current `ScreenState` and a ring of the last 32,
- **zero or one** attached client; detached is a normal state, not an error,
- creation time, last activity, and the child's status.

### 9.2 Registry

`$XDG_RUNTIME_DIR/oxutrm/<session-id>/`, mode `0700`, containing:

- `sock` — a Unix domain socket the session listens on,
- `meta.json` — session id, pid, creation time, shell, terminal size.

**No key material is ever written to the registry, or to disk at all.**

`oxutrm host --list` reads the directory and prunes entries whose pid is gone.
`oxutrm host --attach <id>` connects to `sock` and relays SSH signalling into
the running session process, which then performs a fresh ICE exchange.

### 9.3 Lifecycle

1. `--serve` creates the session, writes `HostHello`, establishes the link,
   then daemonizes (§4.3).
2. Detached: the host keeps draining the PTY into `vt100` and **transmits
   nothing**. Staying detached for a week costs no bandwidth.
3. The session ends when the shell exits, or after an optional idle timeout,
   default **never**.
4. On clean shutdown the registry entry and any router port mappings are
   released.

### 9.4 Terminal capability negotiation

Mosh hardcodes `xterm-256color` and hopes. oxutrm does better, because the
client re-renders into the user's *actual* terminal and therefore knows what can
be displayed.

`ClientHello.caps` carries:

```rust
struct TerminalCaps {
    truecolor: bool,       // COLORTERM=truecolor present
    colors: u16,           // 8 / 16 / 256 / 16777216
    unicode_width: UnicodeWidthVersion,
    bracketed_paste: bool,
    mouse_sgr: bool,
    osc52: bool,
    term_name: String,     // the client's own $TERM, for diagnosis only
}
```

The host sets `TERM` and `COLORTERM` in the child environment to the honest
intersection of what `vt100` emulates and what the client can render. Colours the
client cannot show are down-converted **in the client**, so the host's state
stays full fidelity for a future client that can.

### 9.5 A property worth preserving

Because every client tracks its **own** acknowledged state number, multiple
simultaneous clients require no redesign — session sharing and read-only
observers fall out of the model. Not v1, but the design must not close the door:
`ScreenState` must never be mutated in a way that assumes a single reader.

---

## 10. Client design

### 10.1 Two diffs, not one

1. Apply the host's `ScreenDiff` to the authoritative `ScreenState`.
2. Diff the resulting display against a model of **what is currently painted on
   the physical terminal**, and emit the minimal ANSI to reconcile it.

Step 2 is what keeps oxutrm smooth on a slow local terminal, and it is what
makes the local status pane and local scrolling possible without repainting the
world.

### 10.2 Input

Raw mode via `rustix` termios, restored on every exit path including panic and
signal. `SIGWINCH` drives resize. Bracketed paste and SGR mouse reporting are
enabled on the local terminal according to the modes the host reports, so the
local terminal mirrors the remote application's expectations.

### 10.3 Status display

On connect, exactly one line, then silence:

```
oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452
oxutrm  IPv4 punched (UPnP)  ·  38 ms  ·  mtu 1392
oxutrm  IPv4 punched (birthday, 312 probes)  ·  61 ms  ·  symmetric NAT
oxutrm  SSH tunnel — no UDP path available  ·  45 ms            [warning]
```

Additionally:

- **`Ctrl-]`** (configurable) opens a locally drawn status pane: current path
  and rung, round-trip time, loss, bytes each way, migration history, session id
  and uptime.
- **Path changes announce themselves** for a few seconds:
  `oxutrm  path migrated → IPv6 direct · 74 ms`. Walking from Wi-Fi to mobile
  should be explained, not mysterious.

### 10.4 Bandwidth adaptation

Taken from `quinn`'s `Connection::stats()` rather than measured again:

```
interval = clamp(rtt / 2, 8ms, 100ms)
```

with an immediate send when the link has been idle. Under load, states coalesce
rather than queue (§8.1), so the adaptation is a policy of a dozen lines rather
than a subsystem.

---

## 11. Security model

- **Trust root is SSH, unchanged.** No certificate authority, no new
  credential store, no additional service exposed. If you trust `ssh <target>`
  today, you trust oxutrm.
- **Certificate pinning**: the client accepts exactly the SPKI fingerprint
  delivered over SSH.
- **PSK binding**: ICE checks carry `MESSAGE-INTEGRITY` over the SSH-delivered
  PSK, so unauthenticated peers cannot advance the state machine, and oxutrm
  cannot be used as a reflector or amplifier.
- **Fresh keys per attach.** Each attach generates a new certificate and PSK, so
  a stolen key from an earlier session cannot reattach.
- **No key material on disk**, ever, on either side.
- **Anti-amplification** is QUIC's, by specification: at most 3x the received
  bytes before address validation.
- **Registry permissions**: `0700` directory, `0600` files, under
  `$XDG_RUNTIME_DIR`.
- **Public STUN servers learn only that an IP is using STUN.** They see no
  session content, no peer address, and no identity. The server list is
  configurable so a user who objects can point at their own.
- **The SSH tunnel fallback is announced**, because its security properties
  (and its failure modes on IP change) differ from the direct path.

---

## 12. Testing strategy

Testing effort is matched to where the risk actually is.

| Layer | Method | What it proves |
|---|---|---|
| `oxutrm-sync` | **property tests** (`proptest`) | **the one that matters**: for any sequence of terminal output and any subset of resulting diffs dropped, duplicated or reordered, the client state converges to the host state |
| `oxutrm-term` | golden tests over recorded `.ansi` fixtures (reusing `ansidrama`'s corpus), snapshotting `ScreenState` | emulation fidelity, wide characters, wrapping, attributes |
| `oxutrm-proto` | round-trip and version-skew tests | wire compatibility, loud failure on mismatch |
| `oxutrm-net` | **Linux network namespaces** with `nftables` NAT between them | NAT traversal actually works |
| end-to-end | host and client on loopback, scripted shell, compare final screens | the whole pipeline |

### 12.1 NAT testing with network namespaces

This is the only honest way to test rungs 1-3, and it runs unprivileged in CI
with rootless namespaces.

Three topologies, built with `ip netns` and `nftables`:

- **port-restricted cone** — a plain `masquerade` rule. Linux conntrack's
  default is endpoint-independent mapping with address-and-port-dependent
  filtering, which is exactly this.
- **symmetric (approximated)** — `masquerade random-fully`, which varies the
  external port per destination and therefore exercises the same failure mode
  and the same rung-3 recovery.
- **double NAT** — two nested namespaces, to confirm rung 1 correctly fails and
  rung 2 takes over.

IPv6 (rung 0) is tested with a namespace pair carrying global addresses and a
stateful firewall, confirming the pinhole behaviour.

The approximation in the symmetric case is stated deliberately: Linux cannot
reproduce every commercial NAT. The tests prove the *recovery path*, not
universal compatibility.

### 12.2 Build hygiene

From the outset, in the workspace root:

```toml
[profile.dev]
debug = "line-tables-only"
split-debuginfo = "unpacked"
```

This workspace will grow large test binaries and shares a target directory with
other projects. Builds and tests are capped at **4 parallel jobs**
(`--jobs 4`, `--test-threads 4`) because the build machine is shared.

---

## 13. Crate layout

```
oxutrm/
├── Cargo.toml                  workspace
├── src/main.rs                 single binary, subcommand dispatch
└── crates/
    ├── oxutrm-proto/          wire types, postcard, version negotiation
    ├── oxutrm-sync/           state + diff engine — NO I/O AT ALL
    ├── oxutrm-term/           vt100 wrapper, ScreenState, PTY, capabilities
    ├── oxutrm-net/            candidates, STUN, NAT-PMP/PCP/UPnP, punching, quinn
    ├── oxutrm-host/           session registry, daemonize, PTY supervision
    └── oxutrm-client/         renderer, speculation (phase C), input
```

The load-bearing boundary is that **`oxutrm-sync` has no sockets**. Everything
risky about the protocol is therefore testable in isolation.

### 13.1 Dependencies

| Crate | Version | Role |
|---|---|---|
| `quinn` | 0.11 | QUIC transport |
| `rustls` | matching `quinn` | TLS 1.3, custom pinning verifier |
| `vt100` | 0.16 (fork `Junyi-99/vt100-rust`, branch `deck`) | terminal emulation, both ends |
| `rustix`, `rustix-openpty` | 1 / 0.2 | PTY, termios, process control |
| `stunclient`, `stun_codec` | 0.4 / 0.4 | STUN discovery and ICE checks |
| `crab_nat` | 0.8 | NAT-PMP and PCP |
| `igd-next` | 0.17 | UPnP-IGD |
| `tokio` | 1 | async runtime (`quinn` requires one) |
| `postcard`, `serde` | 1 / 1 | wire encoding |
| `serde_json` | 1 | SSH signalling |
| `zstd` | 0.13 | opportunistic payload compression |
| `anyhow` | 1 | error handling, matching house style |
| `proptest` | 1 | the convergence property |

The `vt100` fork is the same one `ansidrama` uses, keeping one emulator across
both projects.

---

## 14. Phase C sketch — speculation, scrollback, adaptation

Specified in its own document when phase A+B lands. The shape:

**Speculative echo.** The client holds a **second `vt100`**, seeded from the
authoritative screen. Predicted echo bytes are fed into it and drawn
immediately. Because prediction runs through a *real emulator*, wide characters,
right-margin wrapping and attribute inheritance are correct — precisely the
cases Mosh's overlay gets wrong.

Each prediction is tagged with the input sequence number that caused it. When
the host state acknowledging that input arrives: match → retire silently;
mismatch → discard all outstanding predictions and repaint from authority. A
rolling hit-rate governs whether to predict at all, so oxutrm stops guessing
inside `vim` rather than flickering. Unconfirmed cells may be underlined above a
latency threshold; configurable, off by default on fast links.

**Synced scrollback.** The host's `vt100` retains N lines. `scrollback_len` in
`ScreenState` tells the client how much history exists; the client fetches
ranges over a dedicated stream and caches them. Local scrolling is then instant
and **keeps working while the link is down**.

**Adaptation** is §10.4, refined with loss-aware frame dropping.

---

## 15. Phase D sketch — tmux control mode

Specified in its own document. The shape:

The host runs `tmux -CC`, parses control-mode output (`%output`, `%window-add`,
`%layout-change`, `%session-changed`, `%begin`/`%end`), and maintains **one
`vt100` per pane**, each synced as its own `ScreenState`.

The client then draws pane borders, the layout and the status bar **locally**.
The payoff is large: window switching and the status clock become
zero-latency and zero-bandwidth, instead of being a full-screen repaint over
the network every second.

This requires the session layer to hold **several** terminal states rather than
one. Phase A+B must therefore keep `SessionState` a collection from the start,
even while that collection always has exactly one member. This is the single
forward-compatibility constraint phase D places on phase A+B.

---

## 16. Milestones for phase A+B

| # | Deliverable | Proves |
|---|---|---|
| **M1** | Loopback terminal: shell → `vt100` → sync engine → renderer, one process, no network | terminal core and sync engine, with the convergence property green |
| **M2** | QUIC over a punched socket, dummy payload, rungs 0-2, netns tests | NAT traversal actually works |
| **M3** | SSH bootstrap, signalling, daemonize, registry, detach and reattach | the session model |
| **M4** | Joined up: a real remote terminal. Rungs 3-4, status display, capability negotiation | **usable daily** |

M1 is deliberately first: it needs no network, and it de-risks the component
whose correctness is hardest to recover from later.

---

## 17. Open items, deliberately deferred

These are named rather than left implicit:

- **Windows support.** Out of scope; the PTY layer assumes Unix.
- **Graphics protocols.** Out of scope; `vt100` does not model them.
- **Multiple simultaneous clients.** Not implemented, but §9.5 keeps it
  possible.
- **MASQUE / CONNECT-UDP relay (RFC 9298).** The standards-based successor to
  rung 4 if a relay is ever wanted. No public MASQUE proxies exist today, and
  the SSH tunnel is better because it is already trusted.
- **File transfer over a QUIC stream.** The stream machinery makes it cheap;
  it is simply not needed for A+B.

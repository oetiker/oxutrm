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
  OSC 52 clipboard, wide characters, and the eight SGR attributes bold, dim,
  italic, underline, inverse, blink, strikethrough and hidden. The last three
  come from the patch in §13.2; without it they would be silently discarded.
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
- **Reflow on resize.** `Screen::set_size` clears each row's `wrapped` flag, pads
  or cuts rows to the new width, and drops rows off the bottom **without** pushing
  them into scrollback. Narrowing a window therefore loses text that a reflowing
  emulator would have rewrapped. This matches what xterm and tmux do, so it is
  recorded as an accepted limitation rather than papered over; implementing reflow
  is out of scope.
- **Extended underline styles and underline colour** (SGR 4:3 curly/dotted/dashed,
  SGR 58 and 59). `Attrs` cannot represent them and the patch of §13.2 does not
  add them.

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
they appear. Once ICE has nominated a pair and a QUIC connection is up over it,
SSH is closed — **except on rung 4**, where SSH *is* the transport and stays open
for the life of the session (§5.5).

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
    attach_id: u64,              // increments per attach; scopes all sync sequence numbers (§8.5)
    cert_spki_sha256: [u8; 32],  // pins the host's self-signed QUIC certificate
    psk: String,                 // base64, 32 random bytes: root secret for the ICE
                                 // credentials derived in §5.3; never used directly
    candidates: Vec<Candidate>,
    nat_type: NatType,
    bound_port: u16,
    detachable: bool,            // false once the session has fallen back to rung 4 (§5.5)
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

`psk` is 32 bytes from the OS CSPRNG. It never touches disk on either side. It is
a root secret, not a credential: the ICE short-term credentials are derived from
it (§5.3), never sent, and never used verbatim.

### 4.3 Daemonizing

After `HostHello` is written and the link is established **on rungs 0-3**, the
host process must **fully detach**: `fork`, `setsid`, `fork` again, `chdir("/")`,
and close every file descriptor inherited from SSH, reopening `0/1/2` on
`/dev/null`. If any SSH descriptor is retained, closing the laptop lid kills the
session — which is the exact failure the project exists to prevent.

**Rung 4 is the exception and does not daemonize.** Its QUIC connection lives
inside a stream on the SSH connection, so closing the SSH descriptors would
destroy the very transport the session is running on. A rung-4 session therefore
keeps SSH open, stays a child of `sshd`, and is **not detachable**: it advertises
`detachable: false`, its `meta.json` records the same, and its registry entry is
removed when its SSH connection ends. This is stated in the status line (§10.3)
so the reduced guarantee is never a surprise. A rung-4 session that later wants
detachability must be re-established from scratch on a UDP rung.

---

## 5. Connection establishment — the ladder

Rungs run **concurrently where cheap and ordered where costly**. The first
validated path wins.

**ICE nomination completes before QUIC starts.** QUIC is handed a socket whose
peer address is already decided, and that address does not change for the life of
the attach. This is a hard constraint, not a preference: QUIC connection
migration lets an endpoint change its **own local** address, but there is no
mechanism in RFC 9000 — and no API in `quinn` 0.11, whose `Connection` exposes
`remote_address()` with no setter and no path management — to repoint an
established connection at a **different peer address**. A better path discovered
after nomination is therefore **lost for that attach**; it will be found and used
by the next one. What QUIC migration does still buy is §1.3's roaming case, where
the client's own address changes underneath an unchanged peer address.

The nomination deadline is the ladder's total budget; whatever is validated when
it expires is what QUIC gets.

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

**Finding the router is oxutrm's job, not `crab_nat`'s.** `crab_nat` 0.8's entry
point is
`PortMapping::new(gateway: GatewayAddress, client: IpAddr, protocol, internal_port, options)`
— it speaks PCP and NAT-PMP to a gateway it is *told about*, and ships no
discovery of any kind. oxutrm supplies the address by looking up the default
route (`netdev::get_default_gateway`, which wraps netlink on Linux and the route
socket on the BSDs and macOS). `igd-next` is the exception: it finds the IGD
itself over SSDP and needs no help. If no default gateway can be determined, rung
1 is skipped rather than guessed at.

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

**NAT typing takes three probes, not two.** Two servers at different IPs
distinguish endpoint-independent mapping from everything else, but they cannot
tell `AddressDependent` from `Symmetric` — both simply produce "two different
ports". Resolving all four `NatType` variants needs a third binding request to a
**second port on the same server IP** as the first (both Cloudflare and Google
publish an alternate port; the configurable server list carries the pair). Let
`A1` be the mapping seen from server A port 1, `A2` from server A port 2, and
`B1` from server B port 1:

| `A1` vs `A2` | `A1` vs `B1` | Verdict |
|---|---|---|
| same | same | `EndpointIndependent` — ordinary punching works |
| same | different | `AddressDependent` — punching works only to a peer we have already written to |
| different | different | `Symmetric` — ordinary punching is hopeless |

Any probe that times out yields `Unknown`, which is treated as
`EndpointIndependent` for scheduling purposes: rung 2 is attempted and rung 3
follows if it fails. A `Symmetric` verdict skips directly to rung 3 rather than
burning several seconds failing.

**Punching uses real ICE.** Probes are **STUN Binding Requests carrying a
`MESSAGE-INTEGRITY` attribute keyed by the `psk`**, exactly as ICE
connectivity checks do. This is chosen over a bespoke probe format because:

- it demultiplexes cleanly against QUIC on the same socket — a STUN packet's
  first two bits are `00`, while every QUIC packet has the fixed bit set;
- `MESSAGE-INTEGRITY` means strangers cannot confuse the state machine and
  oxutrm cannot be abused as a reflector;
- the `XOR-MAPPED-ADDRESS` in the response *is* peer-reflexive discovery, free;
- `stun_codec` already implements it.

**Credentials are derived, and directional.** A single shared secret keying both
directions would leave a side unable to tell its own reflected check from a
genuine peer check. The two short-term credential pairs are therefore derived
from the `psk` with HKDF-SHA256, using the info strings `"oxutrm ice c2h"` for
checks the client sends to the host and `"oxutrm ice h2c"` for the reverse. Each
pair supplies a `USERNAME` fragment and the `MESSAGE-INTEGRITY` key for its own
direction; a check arriving under the wrong direction's key is discarded.

**Roles are fixed, not negotiated.** The **client is always the controlling
agent** and the host is always controlled — the client initiates the SSH exchange,
so the assignment is unambiguous on both sides without a tie-breaker and the ICE
role-conflict case (the 487 response) cannot arise. Checks carry
`ICE-CONTROLLING` and `ICE-CONTROLLED` accordingly.

Both sides send checks to every candidate of the peer, simultaneously, and each
side records which pairs have produced a valid response. **Only the controlling
side nominates**, choosing the highest-priority pair that has been validated in
both directions, and it signals the choice with the `USE-CANDIDATE` attribute.
The host adopts the nominated pair and sends nothing further about path
selection. This is what makes §5's "nomination completes before QUIC starts"
well-defined: exactly one side decides, so the two ends cannot pick differently.

**Keepalive.** Periodic STUN binding requests continue on the live socket. If
the reported mapping changes, oxutrm knows its address changed **before** QUIC
has to discover it the hard way, and can pre-emptively re-punch.

### 5.3.1 Sharing the socket with QUIC

Sending STUN from the socket QUIC will use is not something the crates do for
free, and getting it wrong is the most likely way to lose a week.

`quinn::Endpoint::new` takes **ownership** of the socket and its driver task runs
a `recv` loop on it. `stunclient`'s only useful entry point is
`query_external_address(&self, udp: &UdpSocket)`, which performs its **own**
`recv`. Run both against one socket and the two receive loops race, each randomly
stealing packets the other needed. `quinn` 0.11 offers no hook for packets it
does not recognise, so the STUN traffic cannot simply be picked out of the
endpoint either.

The resolution:

- oxutrm implements the `quinn::AsyncUdpSocket` trait over the punched socket and
  constructs the endpoint with
  `Endpoint::new_with_abstract_socket(config, server_config, Arc<dyn AsyncUdpSocket>, runtime)`.
  This wrapper is the **single** reader of the socket. On each received packet it
  inspects the first two bits: `00` means STUN, which is routed to the ICE state
  machine; everything else is passed through to `quinn`. The demultiplexing
  argument above is what makes this sound, and it is the wrapper that
  operationalises it.
- `grease_quic_bit` must be left **off**, since greasing the QUIC fixed bit is
  precisely what would break that test.
- `stunclient` is used **only for pre-QUIC address discovery**, on the socket
  before `quinn` is given the abstract wrapper over it. It is not used
  afterwards, and never for connectivity checks: its API has no
  `MESSAGE-INTEGRITY` support at all — its entire surface is `new`,
  `with_google_stun_server`, `set_timeout`, `set_retry_interval`, `set_software`
  and `query_external_address{,_async}`.
- All ICE connectivity checks, the `USE-CANDIDATE` nomination and the keepalives
  are built **directly on `stun_codec`**, which does implement the attributes
  required, and are sent and received through the wrapper.

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

Because the transport *is* the SSH connection, rung 4 gives up the property the
rest of the design is built around: it **cannot daemonize** and is therefore
**not detachable** (§4.3). The session sets `detachable: false` in `HostHello`
and in `meta.json`, and its registry entry is pruned when the SSH connection
ends. Closing the laptop lid ends a rung-4 session. This is the honest cost of
the fallback and it is why the status line calls it out.

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

`quinn` 0.11 over a `UdpSocket` that oxutrm has already punched, wrapped in the
`AsyncUdpSocket` implementation of §5.3.1 and installed with
`Endpoint::new_with_abstract_socket`. Punching and QUIC never fight over the
port, because the wrapper — not `quinn`, and not `stunclient` — is the one thing
reading the socket.

The **host is the QUIC server** and the client is the QUIC client. This follows
from §6.1 (the host owns the certificate the client pins) and is what §7.2's
"client-opened" streams assume.

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

**0-RTT is deliberately not used.** It would require a TLS resumption ticket
issued under the same server configuration, and §11's fresh certificate per
attach guarantees that every attach meets a server which cannot decrypt any
earlier ticket. Reattach pays a full handshake; that is the price of the
"fresh keys per attach" property, and it is worth it.

`TransportConfig` must set `datagram_receive_buffer_size` (whose default is
`None`, which disables incoming datagrams outright) and
`datagram_send_buffer_size`. `grease_quic_bit` must be left disabled, for the
demultiplexing reason in §5.3.1.

### 6.1 Certificate pinning

The host generates a self-signed certificate at session creation. The SHA-256 of
its SPKI travels in `HostHello` over SSH. The client installs a `rustls`
`ServerCertVerifier` that accepts **exactly that fingerprint and nothing else**.
No certificate authority is involved; the trust chain is SSH's, unchanged.

**The verifier must still do real cryptography.** `quinn` 0.11 depends on
`rustls` 0.23 (and re-exports it), whose
`rustls::client::danger::ServerCertVerifier` requires implementations of
`verify_tls12_signature`, `verify_tls13_signature` and `supported_verify_schemes`
alongside `verify_server_cert`. The universally copy-pasted "skip verification"
example stubs all of these to `Ok(())`, and doing so here would be a real
vulnerability: it discards the proof that the peer holds the certificate's
private key, reducing pinning to *knowing the certificate bytes*. oxutrm's
verifier therefore does both things — it checks the SPKI hash against the pinned
value in `verify_server_cert`, **and** delegates the signature methods to the
crypto provider's real TLS 1.3 verification. Anything less is not pinning.

Two mechanical consequences of `rustls` 0.23 that the implementation must
observe: a `CryptoProvider` has to be installed (explicitly, or as the process
default) before `quinn::crypto::rustls::QuicClientConfig::try_from` will succeed,
and the client configuration must be TLS 1.3 only, which QUIC requires anyway.

The `psk` additionally binds the QUIC session to the SSH exchange, so a peer
that somehow obtained the certificate but not the PSK cannot complete ICE.

---

## 7. Wire protocol

`postcard` for compactness and `serde` derive. All integers varint-encoded.
`proto: u32` is checked at handshake; mismatch is a hard, loud failure.

### 7.1 Datagram framing

Every datagram carries three sequence numbers, its position in a fragment set,
and a payload:

```rust
struct Frame {
    my_state: u64,       // the state this datagram describes
    from_state: u64,     // the peer-acknowledged state it is a diff against
    ack_state: u64,      // highest peer state I have applied
    frag_index: u16,     // 0-based position in this state's fragment set
    frag_count: u16,     // total fragments for this state; 1 means unfragmented
    flags: u8,           // bit 0: payload is zstd-compressed
    payload: Vec<u8>,    // a slice of the postcard-encoded ScreenDiff or InputDiff
}
```

**`Frame` is the sole carrier of the sequence numbers.** The diff structures in
§8 do not repeat `base` and `target`; there is exactly one place a receiver looks
for them, so the two can never disagree.

Compression is applied to the whole encoded diff before fragmentation, only when
it actually shrinks the payload, and the threshold is measured, not assumed. The
`flags` byte is identical across every fragment of one state.

### 7.1.1 Fragmentation

A diff is not guaranteed to fit in a datagram. `Connection::max_datagram_size()`
is on the order of 1200 bytes after overhead, while a full `ScreenState` for
80×24 with truecolor cells encodes to well over 10 KB — and the full state is
exactly what §8.2's ring-miss recovery has to send. QUIC datagrams are never
fragmented by the transport and `send_datagram` rejects an oversized payload with
`SendDatagramError::TooLarge`, so oxutrm fragments them itself.

The encoded diff is split into `frag_count` pieces that each fit
`max_datagram_size()`, and every piece is sent as its own datagram carrying the
same `my_state` and `from_state`.

**A state is applied only when all of its fragments have arrived.** An incomplete
set is **discarded wholesale** — never partially applied, never held waiting for
a retransmission, because there are no retransmissions. This is what preserves
§8.1: the receiver's acknowledged state is unchanged by a lost fragment, so the
sender's next diff is computed against that same base and therefore *contains*
everything the dropped set was carrying. Losing one fragment costs exactly one
send interval, and nothing else.

Consequences that follow, and are normative:

- The receiver holds at most one incomplete fragment set per peer. A fragment
  naming a `my_state` newer than the set in progress **replaces** it; the older
  partial set is dropped immediately rather than being kept in hope.
- Fragments of a state older than the receiver's current state are discarded on
  arrival.
- `frag_count` is bounded by configuration. A diff that would exceed the bound is
  a bug in diff generation, not a condition to handle at runtime.

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
  retransmission of screen state, ever. A lost *fragment* costs the same nothing,
  for the same reason, provided the incomplete set is discarded rather than
  partially applied (§7.1.1).
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

`Attrs` carries **bold, dim, italic, underline, inverse, blink, strikethrough and
hidden**. All eight are values the emulator actually holds — the last three only
because of the patch in §13.2, which makes the parser stop discarding the SGR
codes that set them. Extended underline styles and underline colour are not
representable (§1.2).

```rust
struct ScreenDiff {
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
struct Run { start_col: u16, repeat: u16, cells: Vec<Cell> }
```

`base` and `target` are **not** carried here; they live in `Frame` (§7.1) and
nowhere else.

**`Run` semantics, precisely.** The `cells` sequence is emitted **`repeat + 1`
times consecutively**, starting at `start_col`. So `repeat == 0` means the
sequence appears once, and a run of 40 identical blanks is
`Run { start_col, repeat: 39, cells: vec![blank] }`. A run therefore covers
`cells.len() * (repeat + 1)` columns, and a `RowPatch`'s runs must not overlap.

The host retains a ring of its last **32** states so it can diff from whatever
the client last acknowledged. If the client's acknowledged state has fallen out
of the ring, the host sends a full state, which is signalled by `from_state == 0`
in the `Frame` (§8.5).

### 8.3 Client → host: `InputState`

The same machinery, inverted.

```rust
struct InputState {
    seq: u64,
    pending: Vec<u8>,        // user input not yet acknowledged, in order
    size: (u16, u16),        // latest requested terminal size
}

struct InputDiff {
    consumed: u64,           // bytes to drop from the front of base.pending
    appended: Vec<u8>,       // bytes to append afterwards
    size: Option<(u16, u16)>,
}
```

When the host acknowledges state *N*, the client forms state *N+1* with the
consumed prefix removed. Unacknowledged input is therefore retransmitted
automatically, without a retransmission mechanism.

**`apply` is drop-then-append**, in that order: remove the first `consumed` bytes
of the base state's `pending`, then append `appended`. Both operations are needed
because the transition from an untrimmed base to a trimmed target is *not* a pure
append — a diff carrying only `appended` would rebuild `pending` with the
already-consumed bytes still in front, and the host would write them to the PTY a
second time. `consumed` greater than `base.pending.len()` is an `ApplyError`, not
a saturating subtraction.

Mouse events and special keys are encoded into `pending` as the byte sequences
the remote application expects, so the host writes them straight to the PTY.

### 8.4 Purity, and the property that matters

`oxutrm-sync` performs **no I/O whatsoever**. Its entire surface is:

```rust
fn diff(&self, from: &Self) -> Self::Diff;
fn apply(&mut self, base: u64, target: u64, d: &Self::Diff) -> Result<(), ApplyError>;
```

`base` and `target` are parameters rather than fields of `Diff` because `Frame`
owns them (§7.1). `apply` is responsible for checking that `base` matches the
state it is being applied to and that `target` advances; both are `ApplyError`
otherwise.

This makes the highest-risk component of the project exhaustively testable
without sockets. See §12.

### 8.5 Sequence numbering and attach boundaries

State numbers are scoped to a single **attach**, not to the session:

- `seq` **starts at 1** on both sides. **0 is reserved** and is never a valid
  state number.
- `from_state == 0` in a `Frame` means "this is a full state, not a diff". This
  is the ring-miss recovery of §8.2 and the first message of every attach.
- **Both sides reset their counters to 1 at every attach**, and the host bumps
  `attach_id` (§4.2) so the two ends agree on which generation they are in. A
  reattaching client and a host that still remembers the previous attach's
  numbers can therefore never mistake one for the other.
- **The host's first datagram of each attach is a full state.** It does not wait
  for a ring miss to discover that the new client knows nothing; it never assumes
  a fresh client shares history with a previous one.
- `attach_id` is not repeated per frame, because each attach is a distinct QUIC
  connection and datagrams cannot cross between them. It exists so that the
  signalling exchange and the status pane can name the generation unambiguously,
  and so a host that receives a second `--attach` for a session it is already
  serving can tell the two apart.

This is also what makes host-side and client-side updates independent: each
direction has its own counter, its own ring, and its own acknowledgement, so
simultaneous updates in both directions need no arbitration at all.

---

## 9. Host design

### 9.1 Contents of a session

- a PTY with the user's shell (`rustix-openpty`, as in `ansidrama`),
- a `HostTerm`: a `vt100::Parser` and its `Callbacks` (§9.1.1) — together
  **the single source of truth**,
- the current `ScreenState` and a ring of the last 32,
- **zero or one** attached client; detached is a normal state, not an error,
- creation time, last activity, and the child's status.

Per §15, the session holds these terminal states as a **collection**, even though
in phase A+B it always has exactly one member.

### 9.1.1 `HostTerm`

`HostTerm` wraps the patched `vt100::Parser` of §13.2 and feeds it the PTY's
output bytes. **Nothing pre-parses or re-scans that byte stream.** An earlier
draft of this document specified a "sidecar scanner" alongside the parser to
recover what the crate did not report; that design is withdrawn, because the
crate already exposes the information through a callback trait and a second
parser would have been a second source of truth.

The fork provides a `Callbacks` trait. `HostTerm` implements it as a small struct
and constructs the parser with
`Parser::new_with_callbacks(rows, cols, scrollback_len, cb)`, reading the
accumulated state back through `Parser::callbacks()`:

| `ScreenState` field | Callback |
|---|---|
| `title` | `set_window_title` — `OSC 2`, and `OSC 0` which sets both |
| `icon` | `set_window_icon_name` — `OSC 1`, and `OSC 0` which sets both |
| `bell` | `audible_bell` and `visual_bell`, counted monotonically |
| clipboard (§7.2) | `copy_to_clipboard` and `paste_from_clipboard` |

**OSC 52 payloads arrive still base64-encoded.** The crate hands over the
parameter verbatim and does not decode it, so `HostTerm` decodes on the way in
and encodes on the way out. Malformed base64 is dropped rather than forwarded.

Scrollback is the parser's, read by line index through the accessor added in
§13.2, together with that section's monotonic count of lines scrolled off since
the session began. That count is `ScreenState.scrollback_len` and the accessor is
what answers `ScrollbackReq { from, to }` (§7.2). `HostTerm` keeps **no** parallel
scrollback ring: a second copy would have to be kept consistent with the grid
across every scroll and resize, which is exactly the class of bug the single
source of truth exists to prevent.

**`Callbacks::resize` resizes nothing.** It is only the notification that the
application asked for a resize with `CSI 8 ; rows ; cols t`. The handler decides
whether to honour it and, if so, must call `Screen::set_size` itself and resize
the PTY to match. Resize truncates rather than reflows (§1.2).

### 9.2 Registry

A directory per session, mode `0700`, files `0600`, containing:

- `sock` — a Unix domain socket the session listens on,
- `meta.json` — a `SessionMeta`: session id, pid, creation time, shell, terminal
  size, current `attach_id`, and `detachable: bool` (§4.3, §5.5).

**No key material is ever written to the registry, or to disk at all.**

**Where it lives is a decision, not a default.** `$XDG_RUNTIME_DIR` is the
correct location only when it outlives the login session — and on systemd hosts
`/run/user/<uid>` is destroyed when the user's last login session ends. That is
catastrophic here rather than merely untidy: after SSH closes and the host
daemonizes, the process keeps running while its socket and `meta.json` vanish
underneath it, so `--list` reports nothing and the session can never be
reattached. This is precisely the persistence the project exists to provide.

The host therefore **checks** rather than assumes, at session creation:

1. Query `loginctl show-user <uid> --property=Linger`. If it reports
   `Linger=yes`, `$XDG_RUNTIME_DIR/oxutrm/<session-id>/` is used.
2. If lingering is off, or `loginctl` is absent, or `$XDG_RUNTIME_DIR` is unset,
   fall back to **`$HOME/.local/state/oxutrm/<session-id>/`**, which survives
   logout.
3. Falling back is **announced loudly** — on the host's stderr while SSH is still
   attached, and in the client's status pane — naming the chosen directory and
   suggesting `loginctl enable-linger` as the better fix. Silently degrading here
   would hide the exact failure the check exists to prevent.

The fallback directory is on a real filesystem rather than a tmpfs, so `--list`
must prune more carefully: an entry is stale if its pid is gone **or** its pid
belongs to an unrelated process (checked against the recorded creation time), and
stale entries' sockets are removed.

`oxutrm host --list` reads the directory and prunes as above.
`oxutrm host --attach <id>` connects to `sock` and relays SSH signalling into
the running session process, which then bumps `attach_id` and performs a fresh
ICE exchange. A session whose `meta.json` records `detachable: false` is listed
as such and cannot be attached to; it exists only for as long as its own SSH
connection does.

### 9.3 Lifecycle

1. `--serve` creates the session, chooses the registry directory (§9.2), writes
   `HostHello`, completes ICE nomination, establishes the link, then daemonizes
   (§4.3) — unless the link landed on rung 4, which does not daemonize and is
   not detachable (§5.5).
2. Detached: the host keeps draining the PTY into `HostTerm` and **transmits
   nothing**. Staying detached for a week costs no bandwidth. Router port
   mappings are not refreshed while detached; a reattach re-runs the ladder from
   the top, which is the same code path as first connect.
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

**`caps` never reaches the child environment.** The host derives `TERM` and
`COLORTERM` **solely from what `vt100` emulates** — `negotiate_term` takes no
`TerminalCaps` argument and has nothing client-specific to take. All capability
adaptation lives in the client: colours the client cannot show are down-converted
there, at render time.

Two reasons, and both are decisive:

- **Fidelity is not recoverable.** A `TERM` narrowed to the current client's
  intersection makes the *shell* emit degraded output, which is then baked into
  the authoritative `ScreenState` forever. A better client attaching tomorrow
  cannot recover what the application never emitted.
- **`TERM` cannot change under a running shell.** Since connect and reattach are
  one code path (§4.1), a client with different capabilities can attach to a
  session whose shell has been running for a week. Any scheme that derives the
  child environment from client capabilities is undefined at exactly that moment.

`caps` is therefore used for two things only: choosing the client's own
down-conversion strategy, and diagnosis. `term_name` in particular is never
propagated anywhere.

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
oxutrm  SSH tunnel — no UDP path, not detachable  ·  45 ms      [warning]
```

The rung-4 line names the lost property explicitly, because "not detachable"
(§5.5) is the part a user will otherwise discover only by closing the lid. A
registry fallback (§9.2) is reported the same way.

Additionally:

- **`Ctrl-]`** (configurable) opens a locally drawn status pane: current path
  and rung, round-trip time, loss, bytes each way, migration history, session id
  and uptime.
- **Path changes announce themselves** for a few seconds:
  `oxutrm  path migrated → 10.0.0.7 → 192.0.2.44 · 74 ms`. Walking from Wi-Fi to
  mobile should be explained, not mysterious. These are QUIC migrations of the
  client's **own** local address; the rung and the peer address do not change
  within an attach (§5).

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
  delivered over SSH — *and* verifies the TLS 1.3 signature, so pinning proves
  possession of the private key rather than mere knowledge of the certificate
  (§6.1).
- **PSK binding**: ICE checks carry `MESSAGE-INTEGRITY` under keys derived from
  the SSH-delivered PSK, separately per direction (§5.3), so unauthenticated
  peers cannot advance the state machine, a check cannot be replayed back at its
  sender, and oxutrm cannot be used as a reflector or amplifier.
- **Fresh keys per attach.** Each attach generates a new certificate and PSK, so
  a stolen key from an earlier session cannot reattach.
- **No key material on disk**, ever, on either side.
- **Anti-amplification** is QUIC's, by specification: at most 3x the received
  bytes before address validation.
- **Registry permissions**: `0700` directory, `0600` files, under
  `$XDG_RUNTIME_DIR` or, when that does not survive logout, under
  `$HOME/.local/state/oxutrm/` (§9.2). The fallback holds no key material either;
  it holds a socket and metadata.
- **Public STUN servers learn only that an IP is using STUN.** They see no
  session content, no peer address, and no identity. The server list is
  configurable so a user who objects can point at their own.
- **The SSH tunnel fallback is announced**, because its security properties, its
  failure modes on IP change, and its loss of detachability all differ from the
  direct path.

---

## 12. Testing strategy

Testing effort is matched to where the risk actually is.

| Layer | Method | What it proves |
|---|---|---|
| `oxutrm-sync` | **property tests** (`proptest`) | **the one that matters**: for any sequence of terminal output and any subset of resulting diffs — or individual **fragments** (§7.1.1) — dropped, duplicated or reordered, the client state converges to the host state |
| `oxutrm-term` | golden tests over recorded `.ansi` fixtures (reusing `ansidrama`'s corpus), snapshotting `ScreenState` | emulation fidelity, wide characters, wrapping, attributes |
| `oxutrm-proto` | round-trip and version-skew tests | wire compatibility, loud failure on mismatch |
| `oxutrm-net` | **Linux network namespaces** with `nftables` NAT between them | NAT traversal actually works |
| end-to-end | host and client on loopback, scripted shell, compare final screens | the whole pipeline |

### 12.1 NAT testing with network namespaces

This is the only honest way to test rungs 1-3, and it runs unprivileged in CI
with rootless namespaces.

**The topology must contain its own STUN server.** Namespaces have no route to
the internet, so `stun.cloudflare.com` and the rest of §5.3's defaults are
unreachable and rungs 2 and 3 would be untestable — leaving M2's "NAT traversal
actually works" unearned. The harness therefore runs a **minimal STUN responder**
(built on `stun_codec`, the same crate the client uses) in the "internet"
namespace, bound to **two IPs and two ports per IP** so that §5.3's three-probe
NAT typing has something real to classify. The server list is configurable
precisely so the tests can point at it.

Three topologies, built with `ip netns` and `nftables`:

- **port-restricted cone** — a plain `masquerade` rule. Linux conntrack's
  default is endpoint-independent mapping with address-and-port-dependent
  filtering, which is exactly this.
- **symmetric (approximated)** — `masquerade fully-random`. Note the spelling:
  `fully-random` is the nftables flag; `--random-fully` is the iptables one and
  nft will reject it. It varies the external port per destination and therefore
  exercises the same failure mode and the same rung-3 recovery.
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
    ├── oxutrm-term/           HostTerm: patched vt100 + Callbacks, ScreenState, PTY, capabilities
    ├── oxutrm-net/            candidates, STUN, ICE, NAT-PMP/PCP/UPnP, AsyncUdpSocket demux, quinn
    ├── oxutrm-host/           session registry, daemonize, PTY supervision
    └── oxutrm-client/         renderer, speculation (phase C), input
```

The load-bearing boundary is that **`oxutrm-sync` has no sockets**. Everything
risky about the protocol is therefore testable in isolation.

### 13.1 Dependencies

| Crate | Version | Role |
|---|---|---|
| `quinn` | 0.11 | QUIC transport; `new_with_abstract_socket` (§5.3.1) |
| `rustls` | 0.23, via `quinn`'s re-export | TLS 1.3, custom pinning verifier (§6.1) |
| `vt100` | fork `Junyi-99/vt100-rust` at `4bca1b1`, **plus the patch of §13.2** | terminal emulation, both ends |
| `rustix`, `rustix-openpty` | 1 / 0.2 | PTY, termios, process control |
| `stunclient` | 0.4 | pre-QUIC address discovery **only** (§5.3.1) |
| `stun_codec` | 0.4 | ICE checks, nomination, keepalive, test STUN server |
| `hkdf`, `sha2` | 0.12 / 0.10 | directional ICE credentials from the `psk` (§5.3) |
| `crab_nat` | 0.8 | NAT-PMP and PCP (caller supplies the gateway) |
| `igd-next` | 0.17 | UPnP-IGD, including its own SSDP discovery |
| `netdev` | 0.46 | default-gateway lookup for `crab_nat` (§5.2) |
| `tokio` | 1 | async runtime (`quinn` requires one) |
| `postcard`, `serde` | 1 / 1 | wire encoding |
| `serde_json` | 1 | SSH signalling |
| `compact_str` | 0.10 | `Cell.text` (§8.2) |
| `zstd` | 0.13 | opportunistic payload compression |
| `anyhow` | 1 | error handling, matching house style |
| `proptest` | 1 | the convergence property |

The RustCrypto versions above are not free choices: STUN `MESSAGE-INTEGRITY` is
HMAC-SHA1, so `hmac` 0.12 and `sha1` 0.10 are already required, and everything
sharing a `digest` generation with them must stay on the 0.10-era releases.
`hkdf` 0.13 and `sha2` 0.11 belong to the next generation and cannot coexist with
them in one graph.

**`vt100` is pinned by commit hash, not by branch.** A branch name is not a
reproducible dependency — `deck` can move underneath the build, and this is the
crate the entire terminal layer's fidelity rests on. The base is commit
`4bca1b1ec4efbb73b55f6c229e38268dca836825`, declared as
`{ git = "...", rev = "4bca1b1ec4efbb73b55f6c229e38268dca836825" }` against
oxutrm's own fork, which carries the patch described next.

The fork is the same one `ansidrama` uses, keeping one emulator across both
projects — which is why oxutrm patches it rather than switching emulators.

### 13.2 The vt100 patch

oxutrm carries a **small, documented patch** over `4bca1b1`. It adds exactly two
things, both of which the audit of that commit showed to be missing values rather
than missing accessors — no wrapper around the unpatched crate could have
recovered either.

**1. Three attribute bits.** `Attrs::mode` is a `u8` with only five bits used, so
BLINK, STRIKE and HIDDEN fit in the free bits without growing the cell. The
parser is the real change: `Screen::sgr` today lets SGR 5 and 6 (blink), 8
(conceal) and 9 (strikethrough), and the resets 25, 28 and 29, fall through to
`unhandled_csi`, where they are discarded — so these attributes never reach the
grid at all. The patch handles those codes and adds `Cell::blink()`,
`Cell::strikethrough()` and `Cell::hidden()`.

**2. Addressable scrollback.** `Grid` already retains scrolled-off rows in a
private `VecDeque`; the patch exposes an **indexed accessor** over it plus a
**monotonic counter of lines scrolled off since start**. Neither exists today:
`Screen::scrollback()` returns the current *view offset*, not content, and
`Grid::scrollback_len()` is `pub(crate)` and reports the ring's *capacity* rather
than how much of it is filled. §9.1.1 and §14 both depend on this accessor, and
§1.1 lists synced scrollback as a v1 feature, so it is not optional.

The patch deliberately stops there. It does not add reflow, extended underline
styles or underline colour (§1.2). Keeping it this small is what makes carrying a
fork cheaper than adopting a different emulator.

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

**Synced scrollback.** The parser retains N lines and, with the patch of §13.2,
hands them back by line index — the unpatched crate offers only a viewport
offset, which is why the patch exists. `scrollback_len` in `ScreenState` tells the
client how much history exists; the client fetches ranges over a dedicated stream
and caches them. Local scrolling is then instant
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

---

## 18. Review corrections

This document was reviewed against the real crate APIs (crates.io and docs.rs,
2026-08-25) and for internal consistency. Seventeen defects were found and are
resolved above. Recorded here so the changes are auditable.

| # | Defect | Resolution |
|---|---|---|
| 1 | `stunclient` and `quinn` both `recv` on one socket and race | §5.3.1: an `AsyncUdpSocket` wrapper is the sole reader and demultiplexes STUN by the first two bits; `stunclient` is pre-QUIC discovery only; checks are built on `stun_codec`; `grease_quic_bit` off |
| 2 | QUIC migration cannot repoint an established connection at a new peer address | §5: ICE nomination completes before QUIC starts, the peer address is fixed for the attach, late better paths are lost until the next one; §10.3's migration notice is now about the client's own local address |
| 3 | Diffs exceeding `max_datagram_size()` were undeliverable | §7.1: `Frame` gains `frag_index`/`frag_count`; §7.1.1: a state applies only when all fragments arrive, incomplete sets are discarded wholesale |
| 4 | `InputDiff` could not express prefix removal, so input replayed | §8.3: `consumed: u64` added; `apply` is drop-then-append |
| 5 | Rung 4 needs SSH alive, but §4.3 closed every SSH descriptor | §4.3, §5.5, §9.2: rung 4 skips daemonization, is not detachable, `SessionMeta.detachable` records it, registry entry dies with its SSH |
| 6 | 0-RTT is impossible given fresh keys per attach | §6: removed from the table, with the reason stated |
| 7 | `vt100` 0.16 has no title, icon, bell, OSC 52 or addressable scrollback | Resolution revised by §18.1: title, icon, bell and OSC 52 come from the fork's `Callbacks` trait (§9.1.1) and scrollback from the patch (§13.2). The sidecar scanner is withdrawn and §13.1 now pins `4bca1b1` |
| 8 | ICE had no roles, no tie-break, and one shared key for both directions | §5.3: client is deterministically controlling, only it nominates, credentials derived per direction via HKDF-SHA256 |
| 9 | `$XDG_RUNTIME_DIR` is destroyed at logout, killing reattach | §9.2: `loginctl ... Linger` is checked, `$HOME/.local/state/oxutrm/` is the fallback, the fallback is announced loudly |
| 10 | `crab_nat` has no gateway discovery | §5.2: `netdev::get_default_gateway` supplies it; rung 1 is skipped if unavailable |
| 11 | `TERM` from client caps contradicted client-side down-conversion and reattach | §9.4: `TERM`/`COLORTERM` derive solely from what `vt100` emulates, `negotiate_term` takes no caps, all adaptation is client-side |
| 12 | Sequence numbers duplicated in `Frame` and in the diffs | §7.1, §8.2, §8.3, §8.4: `base`/`target` removed from both diffs, `Frame` is the sole carrier, `apply` takes them as parameters |
| 13 | `Run.repeat` was readable two ways | §8.2: `cells` is emitted `repeat + 1` times from `start_col`; `repeat == 0` means once; runs must not overlap |
| 14 | State numbering across reattach was unspecified; `base == 0` collided with a valid seq | §8.5: `seq` starts at 1, 0 reserved as the full-state sentinel, counters reset per attach, `attach_id` added, the host's first datagram of an attach is a full state |
| 15 | netns tests could not reach public STUN; nftables flag misspelled | §12.1: a `stun_codec` responder runs in the "internet" namespace on two IPs and two ports each; `masquerade fully-random` corrected |
| 16 | Two STUN probes cannot separate `AddressDependent` from `Symmetric` | §5.3: three probes, with a second port on the same server IP, and an explicit truth table |
| 17 | The pinning verifier's signature-checking duty was unstated | §6.1: the verifier checks the SPKI hash **and** performs real TLS 1.3 signature verification; `CryptoProvider` installation and TLS-1.3-only noted |

### 18.1 Emulator investigation

Finding 7 was first resolved by assuming the fork could not be changed: a sidecar
scanner would re-parse the PTY byte stream for title, icon, bell and OSC 52, and
`HostTerm` would keep its own scrollback ring. The commit hash was left as a
placeholder pending an audit.

The audit of `Junyi-99/vt100-rust` `deck` at
`4bca1b1ec4efbb73b55f6c229e38268dca836825` changed both halves of that.

**What it found the crate already does.** A `Callbacks` trait carries
`set_window_title`, `set_window_icon_name`, `audible_bell`, `visual_bell`,
`copy_to_clipboard` and `paste_from_clipboard`. Every field the sidecar scanner
was invented to recover is delivered by the emulator that already parsed those
sequences. **The scanner is therefore deleted** — it was a second parser over the
same bytes, and a second source of truth. This part of the correction holds
independently of which emulator is used.

**What it found genuinely missing.** Blink, conceal and strikethrough are
discarded in `Screen::sgr` before reaching the grid, and scrolled-off rows sit in
a private `VecDeque` with no indexed accessor and no monotonic fill counter.
These are missing *values*, not missing getters, so no amount of wrapping
recovers them — and §1.1 commits to both synced scrollback and the full attribute
set.

**Decision: patch the fork.** `alacritty_terminal` 0.26 was considered as an
alternative that supplies all of this natively. It was rejected in favour of a
small patch (§13.2) because the fork is already shared with `ansidrama`, and
keeping one emulator across both projects is worth more than avoiding a
two-feature patch. Reflow is not part of that patch and is recorded as an
accepted limitation (§1.2) rather than a defect.

This document contains **no placeholders**.

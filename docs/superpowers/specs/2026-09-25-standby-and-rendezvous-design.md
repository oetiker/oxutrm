# oxutrm — Standby link and rendezvous mailbox

Status: draft 2026-09-25, awaiting review
Supersedes nothing. Builds on Tier B2
(`docs/superpowers/specs/2026-09-07-tier-b2-client-reattach-design.md`) and
partly retires design spec §18 defect 2 (a better path is lost until the next
attach).

---

## 1. Purpose

Observed setup: the client reaches `thinlinc` over a **split-tunnel VPN**. Only
`thinlinc`'s private subnet goes through the tunnel; everything else on both
ends reaches the internet directly, **outbound only**. `thinlinc`'s public
egress address is stable.

Today, with the VPN up:

- ICE nominates the pair over the tunnel. Host candidates outrank everything
  else (`crates/oxutrm-net/src/candidates.rs:29`), and the split-tunnel route
  carries the one wildcard socket's traffic to `thinlinc`'s private address.
- When the VPN drops, that remote address is unreachable, and QUIC cannot repoint
  a connection at a different remote (`crates/oxutrm-net/src/ice.rs:10`).
- After `REBUILD_AFTER` (20 s) the client rebuilds via `ssh`, which also needs
  the VPN. The session is parked on the host, and nothing can reach it until the
  VPN is back.

A second path existed the whole time: both ends punching through their NATs over
the public internet. It never depended on the VPN. Nothing kept it.

### 1.1 Goals

- **P1 — standby link.** While a link is up, find a second path that leaves the
  client on a *different interface*, and establish a full QUIC connection over
  it. Keep it warm. When the primary goes silent and the standby still answers,
  switch in seconds, with no ssh.
- **P2 — rendezvous mailbox.** When every path is dead *and* ssh is
  unreachable (VPN down, then the client changes network), run the ordinary
  attach exchange through a small HTTPS mailbox that both ends reach outbound.

### 1.2 Non-goals

- **Voluntary path switching.** No switching to a standby because it is faster,
  and no failing back to the VPN when it returns. P1 switches only on failure.
  (The mechanism would allow it later. The policy is out of scope.)
- **Relaying traffic.** The rendezvous server carries signalling only, a few
  kilobytes per reattach. A relay for when no punch succeeds (MASQUE, design
  spec §17) remains future work.
- **Reaching a session from a fresh client process without ssh.** Rendezvous
  credentials live only in the memory of the client that was attached.
- **Full-tunnel VPNs.** There the "public" path still runs through the tunnel,
  and binding around it (`IP_BOUND_IF`) is out of scope.

### 1.3 The rule this obeys

`crates/oxutrm-host/src/attach.rs:3`:

> Reattachment is not a second code path.

Today, one exchange (HostHello, ClientHello, candidates, ICE, QUIC) runs over
two signalling transports: ssh stdio and the session's Unix socket. This design
adds two more:

| Transport | Used by | Authenticated by |
|---|---|---|
| ssh stdio → Unix socket | first connect, B2 rebuild | ssh |
| **control stream on the live link** | P1 standby | the live link's pinned TLS |
| **sealed mailbox** | P2 rendezvous | a grant issued over a live link (§5.2) |

The exchange itself does not change. A standby and a rendezvous reattach
produce a `Link` exactly as an ordinary attach does, and that `Link` flows into
the existing `swap_in` (`src/session.rs:1370`) and `adopt`
(`src/session.rs:602`).

---

## 2. The control stream

After `Established`, the client opens **one bidirectional QUIC stream** on the
primary link, the *control stream*. It carries `Signal` values as
newline-delimited JSON, with the same framing and the same `MAX_SIGNAL_LINE`
cap as ssh signalling (`crates/oxutrm-proto/src/signal.rs`).

It is the only channel between the two ends that exists after ssh has been
severed (`src/serve.rs:108`), and it is at least as well authenticated as ssh:
both ends are pinned by SPKI.

Uses:

- P1: requesting a standby, which runs the attach exchange (§3.2), and probing
  the standby (§3.4). The probe runs on the *standby's* control stream.
- P2: issuing and rotating the rendezvous grant (§5.2).

### 2.1 Feature negotiation

`HostHello` and `ClientHello` gain `features: Vec<String>`, with
`#[serde(default)]`. The `Signal` types do not deny unknown fields, so an older
peer ignores the field and a newer peer treats its absence as "no features". A
client opens the control stream only if the host advertised `"control"`, and
uses P1/P2 only if the host advertised `"standby"` / `"rendezvous"`.
**No `PROTO_VERSION` bump.**

---

## 3. P1 — the standby link

### 3.1 When the client looks for one

- `STANDBY_DELAY` (5 s) after a link is established, so the search never
  competes with the first paint.
- After every swap: the old standby became the primary, or it died with the
  old primary.
- When the standby dies (connection error), with backoff 30 s, 60 s, 120 s,
  then every 300 s.
- When `RouteWatch` reports a route change: an interface that was absent may
  now be present.

At most one standby exists, and at most one search runs at a time.

### 3.2 Establishing it

The client sends `Signal::StandbyRequest` on the primary's control stream. The
host session runs `run_attach_exchange` (`src/attach_exchange.rs`) over that
stream, exactly as `listener::serve_attaches` runs it over a Unix stream:

1. `begin_attach` gives the standby its own attach generation, fresh PSK and
   fresh certificate.
2. A fresh UDP socket and ICE run on both sides.
3. A fresh endpoint and `quic_server` / `quic_client`. It is one connection per
   endpoint, as today; `AcceptPermit` is untouched.

The single difference is at the end. The exchange's `Attached` value carries
`role: Standby`, and the host parks the resulting `Link` in
`HostSession::standby: Option<Link>` **instead of calling `adopt`**. The primary
is untouched.

`begin_attach` bumps `meta.attach_id` for the standby too. That is harmless:
nothing compares `attach_id`. Outside tests it is only written, `Debug`-printed
(`src/rebuild.rs:80`, `src/connect.rs:486`) and shown as "attach N" in the
picker (`src/choose.rs:156`). The picker's number therefore counts standbys as
well. It already means "how many links this session has had", not "how many
times a person attached".

### 3.3 Which pairs a standby may use

The standby exists to survive what kills the primary, so a pair that leaves the
client through the primary's interface is worthless.

The client computes the **egress interface** of a remote address without sending
anything: `connect()` a throwaway UDP socket to the address, read
`getsockname()`, and map the source IP to an interface via `netdev`. This is
portable across Linux and macOS and reads only the routing table.

For the standby's ICE run, the client:

- records `primary_if` = the egress interface of the primary's remote address;
- drops every **remote** candidate whose egress interface is `primary_if` before
  `add_remote`. Filtering local host candidates would not help, because one
  wildcard socket sends wherever the route says.

If nothing is left, there is no standby, and the status line says so (§3.6).
This is a pure function over (remote candidates, egress lookup), tested without
a network.

The client is Controlling and the only side that nominates
(`crates/oxutrm-net/src/ice.rs`), so the filter needs no host cooperation.

Rungs apply as in an ordinary ladder, including the birthday blast. It is noisy,
but it runs once per standby build, not per second.

### 3.4 Keeping it warm, and knowing it works

QUIC keep-alive (10 s, `crates/oxutrm-net/src/quic.rs:97`) holds both NAT
mappings open. No new mechanism is needed.

Keep-alive proves the connection *was* alive recently, not that it is alive now.
So the failover decision uses an active probe: `Signal::Probe { nonce }` on the
**standby's own** control stream, answered by `Signal::ProbeAck { nonce }`. The
host answers without adopting. A sync frame on the standby would trigger
adoption (§3.5), so the probe must not be one.

### 3.5 Failing over

A new phase input in `src/linkstate.rs`, computed purely from facts the session
already holds:

- the primary is `Silent` with a reply owed for `FAILOVER_AFTER` (3 s), **and**
- a `Probe` sent on the standby when the primary entered `Silent` has been
  answered within `PROBE_TIMEOUT` (2 s).

Then the client:

1. `swap_in(standby)`. This resets the sequence counters, closes the old
   primary with a new reason `SWITCHED` (not `REBUILT`, not `TAKEN_OVER`) and
   re-seeds `RouteWatch`.
2. Sends its first frame on the new link.

On the host, the **first valid sync frame arriving on the standby** triggers
`adopt(standby)`. The host resets its counters, then processes that frame. The
order matters: reset first, then feed the frame, so seq 1 is not discarded as
stale. The old primary is closed with `SWITCHED`.

If the probe fails, nothing changes: the client stays `Silent`, and at
`REBUILD_AFTER` it enters `Recovering` exactly as today (B2 ssh rebuild, plus P2
if a grant is held).

A false failover, where the primary only blipped, costs one full-state snapshot
and a new standby search. That is the same price B2 already pays for every
rebuild.

### 3.6 Host-side bookkeeping

- The host holds **one** standby slot. A new standby replaces the old one, whose
  connection is closed.
- A takeover, meaning an ordinary attach from ssh (`HostWake::Attached`), drops
  the standby too, because it belongs to the displaced client.
- Standby liveness does **not** feed `DETACH_AFTER` (`src/session.rs:110`).
  Detachment still means "the primary has been silent".

### 3.7 What the user sees

The one-line path notice gains the standby, once it is established or lost:

```
oxutrm  IPv4 direct  ·  11 ms  ·  standby: IPv4 punched, 38 ms
oxutrm  IPv4 direct  ·  11 ms  ·  no standby path
oxutrm  switched to standby (IPv4 punched)  ·  38 ms
```

"No standby path" is information, not a warning: it tells the user their
session has no fallback.

---

## 4. P2 — the rendezvous server

### 4.1 Shape

A new subcommand, `oxutrm rendezvous --listen <addr> [--stun <addr>]`, in its
own crate `oxutrm-rendezvous`:

- **Plain HTTP** on the listen address. TLS is the job of a reverse proxy in
  front (Caddy, nginx), which is how such services are deployed anyway.
  oxutrm then manages no certificates.
- **Optional STUN responder** on UDP (`--stun`). A self-hoster can then drop
  the public STUN servers too.
- **Memory only.** Nothing is written to disk. A restart loses all mailboxes,
  which costs at most one reattach attempt.

### 4.2 API

A mailbox is an ordered queue of opaque messages under an unguessable id.

```
POST /v1/box/{id}                  body: ≤ 8 KiB opaque bytes     → 204
GET  /v1/box/{id}?after={n}&wait={s}                              → 200 | 204
```

- `{id}` is 32 random bytes, base64url. The server rejects any other shape.
- The server assigns each message a per-box sequence number `n`. `GET` returns
  every message with `seq > after` as JSON `[{"seq":n,"body":"<b64>"}]`. With
  none present it holds the request for up to `wait` seconds (max 60) and then
  returns 204.
- A mailbox keeps at most 32 messages. Each message lives 10 min, and an empty
  mailbox is forgotten.
- Optional `--token-file`: when set, requests must carry
  `Authorization: Bearer <token>`. This stops strangers using a self-hosted
  server; it is not part of oxutrm's security.
- Per-IP rate limits and a global memory cap. On overload the server answers
  503, and clients back off.

### 4.3 What the operator learns

The operator sees the public IPs of both ends, when they poll, and when a client
reattaches. Message contents are sealed (§5.3). The README security section
states this.

---

## 5. P2 — reattaching through the mailbox

### 5.1 Configuration

The client names a server with `--rendezvous <url>`, or with
`OXUTRM_RENDEZVOUS` (and `OXUTRM_RENDEZVOUS_TOKEN` for the bearer token). There
is no default server. With none configured, P2 is off.

The URL must be `https://`. `http://` is accepted only for loopback addresses,
for tests.

### 5.2 The grant

Once a link is established, a client with a configured server sends
`Signal::RendezvousRequest { url, token }` on the control stream. The host
answers `Signal::RendezvousGrant { key }`, 32 random bytes.

From `key`, both ends derive, with HKDF-SHA256 and distinct `info` strings:

- `box_c2h`, `box_h2c`: the two mailbox ids.
- `seal_c2h`, `seal_h2c`: the two AEAD keys.

Lifetime:

- **One grant per session.** A new grant replaces the old one. The host issues
  a fresh grant after every adoption: the client re-requests it on each new
  primary.
- The host drops the grant on takeover by another client and when the session
  ends.
- Both ends keep it **in memory only**, zeroed on drop, as `Psk` is
  (`crates/oxutrm-proto/src/keymat.rs:157`); `tests/no_keys_on_disk.rs` is
  extended to cover it.

Holding the grant is what authorises a mailbox reattach. It was handed over a
pinned link to the client that was attached, so it stands in for "logged in via
ssh" in the same way the rebuild's ssh login does.

### 5.3 Sealing

Each mailbox message is sealed with ChaCha20-Poly1305 under the direction's
key:

- The nonce is a 64-bit per-direction counter, and the receiver rejects any
  counter not above the last one it accepted. That rules out replay and
  reordering.
- The mailbox id is associated data, so a message cannot be moved to another
  box.
- A message that fails to open is dropped silently and does not advance the
  counter.

### 5.4 The sealed stream

`SealedMailbox` implements the same signalling-stream interface the exchange
already runs over. A write is one sealed `POST`. A read is a long-poll `GET`,
opened in order. Each `Signal` line becomes one message.

The exchange therefore runs unchanged: `HostHello` (with a fresh PSK, readable
only by the grant holder), `ClientHello`, `CandidateUpdate`s, `Established`.
Each step costs one HTTPS round trip, so a reattach takes a few seconds, which
is acceptable for the case it serves.

Because the exchange starts with `HostHello`, the host must know a client is
waiting. The client's first message is `Signal::Choose { choice: Attach { id } }`,
reusing B2's type, and the host session starts `run_attach_exchange` over the
mailbox when it reads it. The result is an ordinary attach: `adopt`, a takeover
reason to any lingering primary, then a fresh grant.

### 5.5 The host's poller

While it holds a grant, the host session keeps one long-poll `GET` open on
`box_c2h`. That is about one request a minute when idle. It continues while the
session is **detached**, which is exactly when a client is away.

- Errors back off 5 s, 10 s, 30 s, then every 60 s. A server that is down never
  affects the session.
- The HTTPS client honours `HTTPS_PROXY` / `NO_PROXY` and verifies against the
  **system trust store**, so corporate TLS-inspecting proxies work where the
  user's browser does.
- The long poll is a `tokio::select!` arm in the host loop. It is idle, not
  busy, which keeps the idle-CPU property from commit `19cc001`.

### 5.6 The client's attempt

In `Recovering`, a client holding a grant races two attempts per backoff step:
the B2 ssh rebuild and a mailbox reattach. They go through the same
`AttemptOutcome` channel, and the first `Landed` wins. A mailbox attempt that
gets no answer within 30 s counts as `Retry`.

The old primary and the standby are held throughout, as in B2: whichever path
revives first wins.

---

## 6. What changes where

| Area | Change |
|---|---|
| `oxutrm-proto::signal` | `features` on both hellos; `StandbyRequest`, `Probe`, `ProbeAck`, `RendezvousRequest`, `RendezvousGrant` |
| `oxutrm-proto` | `RendezvousKey` (zeroed, redacted), HKDF labels |
| `src/attach_exchange.rs` | generic over the signalling stream (already true for ssh/Unix); `role: Primary \| Standby` on the result |
| `src/link.rs` | control stream opened/accepted with the link |
| `src/session.rs` (host) | `standby: Option<Link>`; adopt on first frame; probe answers; grant; mailbox poller arm |
| `src/session.rs` (client) | standby search task; failover; grant request; mailbox attempt in `Recovering` |
| `src/linkstate.rs` | `FAILOVER_AFTER`, `PROBE_TIMEOUT`, failover as a pure decision |
| `src/ladder.rs` / new `src/egress.rs` | egress-interface lookup and the standby pair filter |
| new `crates/oxutrm-rendezvous` | server, `SealedMailbox`, HTTPS client |
| `src/main.rs` | `oxutrm rendezvous`, `--rendezvous` |
| README | standby notice, rendezvous, operator metadata in Security |

## 7. Testing

- **Pure functions.** The standby pair filter, the failover decision, the
  mailbox sealing (round trip, replay, tamper, wrong box), and the grant key
  derivation.
- **Exchange over new transports.** The attach exchange over an in-memory
  duplex standing in for the control stream, and over `SealedMailbox` against
  an in-process rendezvous server on loopback `http://`. The same assertions
  as the Unix-socket tests, which proves "not a second code path".
- **Adoption.** A standby's first frame is adopted on the host and seq 1 is
  delivered, not dropped. A takeover drops the standby.
- **netns: the case that motivated this.** Use `crates/oxutrm-net/tests/netns.rs`
  to build a client namespace with two uplinks: a "tunnel" veth that routes
  only the host's private subnet, and a NATed "internet" veth, with the host
  also behind NAT. Connect, confirm the primary uses the tunnel and a standby
  exists on the NAT path, take the tunnel down, and assert the session keeps
  running within `FAILOVER_AFTER + PROBE_TIMEOUT` + 1 s.
- **netns: P2.** Same topology plus a rendezvous server. Take the tunnel down,
  *and* change the client's NATed address. Assert reattach via the mailbox with
  the fake ssh failing every attempt.
- **Hand test** on `thinlinc` with the real VPN.

## 8. Phasing

1. **P1** (§2, §3): control stream, feature flags, standby, failover. It is
   useful alone, and it is what fixes the observed case.
2. **P2** (§4, §5): server, grant, sealed mailbox, host poller, client attempt.

Each gets its own implementation plan.

## 9. Risks and open questions

- **NAT timeouts shorter than 10 s** on the standby path would lose it silently
  until the probe fails. If that shows up in the field, a per-standby shorter
  keep-alive is the fix; do not guess it now.
- **Detecting a network change sooner.** Today a dead primary costs 20 s before
  `Recovering`. A route change while `Silent` could start recovery at once.
  Deferred: it changes B2's behaviour for everyone, not just this feature.
- **The host's HTTPS dependency** adds a TLS client stack to the host binary.
  Pick one that supports the system trust store and proxies; measure the
  binary size cost in the plan.

## 10. Inherited constraints this design must not break

- Reattachment is not a second code path (§1.3).
- Keys are fresh per attach and never on disk. The grant follows the same rule.
- No new trust root. The standby is authorised by the pinned primary, and the
  mailbox by a grant issued over it. The rendezvous server is trusted for
  delivery only.
- The idle-CPU property of `19cc001`: every new wait is event-driven.
- oxutrm never parses `~/.ssh/config`.

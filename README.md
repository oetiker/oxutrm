# oxutrm

A remote terminal that survives bad networks, changing IP addresses, and NAT on
both ends.

It is, deliberately, **Mosh rebuilt in Rust with a real terminal emulator on
both ends**, plus the two things Mosh never solved: NAT traversal and working
scrollback.

> **Status: phase A+B.** The transport, the terminal core and the session loops
> work and are tested. It is not yet something to put on a machine you care
> about. See [What does not work yet](#what-does-not-work-yet) — that section is
> longer than this one, and honestly so.

## Why

`ssh` plus `tmux` is "reconnect and hope". Close a laptop lid, walk between
Wi-Fi and mobile, or move between offices, and the shell is gone or the
connection hangs until a TCP timeout notices.

oxutrm keeps the session. The state is replicated rather than streamed, so a
lost packet costs nothing, and the connection is QUIC, so an IP change is a
migration rather than a disconnection.

## How it differs from Mosh

| | Mosh | oxutrm |
|---|---|---|
| Transport | hand-rolled AES-OCB over UDP | QUIC, key pinned via SSH |
| Roaming | custom address-update logic | QUIC connection migration |
| NAT | none — the server must be reachable | a five-rung ladder; both ends may be behind NAT |
| Firewalls | UDP 60000–61000, widely blocked | UDP/443, the one UDP port usually open |
| Emulation | heuristic prediction overlay | a real `vt`-class emulator on **both** ends |
| Scrollback | broken | synced (phase C) |
| Reattach | none | the same code path as first connect |
| `TERM` | hardcoded `xterm-256color` | negotiated from what the emulator actually supports |

The emulator on both ends is the load-bearing difference. Mosh guesses what the
screen will look like; oxutrm knows, because the client renders from
authoritative state rather than approximating it.

## Using it

```
oxutrm loopback              # both halves in one process, no network
oxutrm loopback --help
```

`loopback` is the piece that works end to end today: a shell on a PTY, through
the emulator, diffed, encoded to bytes, decoded, and painted — everything a
real session does except the transport.

```
oxutrm <ssh-target>          # connect, or reattach   (not yet wired)
oxutrm host --serve          # the remote half         (not yet wired)
oxutrm host --list           # sessions on this machine (not yet wired)
```

oxutrm never parses `~/.ssh/config`. It shells out to `ssh` and assumes
`ssh <target>` already works, by whatever means — jump host, VPN, reverse
tunnel. If you trust `ssh <target>` today, you trust oxutrm: there is no new
trust root, no new credential store, and no additional service exposed.

## How it connects

The host binds **UDP/443** where permitted, because restrictive networks tend to
leave it open — blocking it breaks HTTP/3 for every browser present. Then a
ladder, first validated path wins:

| Rung | Mechanism | Wins when |
|---|---|---|
| 0 | IPv6 direct | both ends have global IPv6 — no NAT to traverse |
| 1 | Router port mapping (NAT-PMP, PCP, UPnP-IGD) | a typical home router |
| 2 | STUN + hole punch | most NATs |
| 3 | Birthday-paradox blast | symmetric NAT |
| 4 | SSH tunnel | UDP blocked entirely |

You are told which one you got, in one line, once:

```
oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452
oxutrm  IPv4 punched (birthday, 312 probes)  ·  61 ms  ·  symmetric NAT
oxutrm  SSH tunnel — no UDP path, not detachable  ·  45 ms      [warning]
```

Rung 4 is a warning rather than a success because that session runs inside the
SSH connection: it cannot detach and cannot be reattached. Degrading to it
silently would remove both properties the project exists to provide.

The packets genuinely **are** QUIC, with ALPN `oxutrm/1`. oxutrm does not forge
anyone else's domain in SNI to look like something it is not.

## What does not work yet

Phase A+B is the transport, the terminal core and the session loops. In
progress or not started:

- **`oxutrm <ssh-target>` is not wired.** The SSH bootstrap, candidate
  signalling, daemonizing and the session registry are phase A's remaining
  work. `oxutrm loopback` is what runs today.
- **Scrollback is not synced.** The host keeps it; the client cannot fetch it
  yet. Phase C.
- **No speculative local echo.** Typing waits for the round trip. Phase C.
- **No `tmux -CC` integration.** Phase D.
- **Bandwidth adaptation is a fixed pacing interval**, `clamp(rtt/2, 8ms,
  100ms)`, not the loss-aware policy phase C describes.

## What it will never do

Named rather than left implicit, so nobody plans around them:

- **No graphics protocols** — no Sixel, no Kitty, no iTerm2 inline images. The
  emulator does not model them and claiming support would be a lie.
- **No Windows.** Unix PTY semantics are assumed throughout.
- **No GUI.** oxutrm renders into a terminal you already have.
- **Not a VPN or a port forwarder.** It moves one terminal session.
- **It will not replace tmux.** Phase D integrates with it instead.

## Known limitations

Real ones, found by testing rather than guessed at:

- **Blink is reconstructed, and scrolling drops it.** The emulator parses
  SGR 5/6/25 and then discards them, so oxutrm recovers blink in a parallel
  plane. A resize reflows the grid and the plane is cleared rather than guessed
  at, so blinking text loses its blink across a resize.
- **All five underline styles render as one.** Double, curly, dotted and dashed
  underlines all arrive as a plain underline. Drawing a curly underline as a
  straight one is a smaller lie than drawing none.
- **No icon name.** OSC 1 is silently dropped by the parser, so there is no
  field for it — an icon field could only ever hold a value oxutrm invented.
- **A better path found after connect is lost until the next attach.** QUIC
  migration lets a client change its own *local* address; there is no mechanism
  to repoint an established connection at a different *remote* address.
- **Under a saturating writer the loop runs slower than its pacing interval.**
  The screen stays current and nothing queues, but a turn costs more than the
  8 ms floor when something like `yes` is running.

## Security

- **The trust root is SSH, unchanged.** No certificate authority, no new
  credential store, no additional listening service.
- The host generates a **fresh self-signed certificate and PSK per attach**. The
  SHA-256 of its SPKI travels over SSH, and the client accepts that fingerprint
  and nothing else — *and still verifies the handshake signature*, because
  pinning without signature verification is not weaker authentication, it is
  none.
- ICE connectivity checks carry `MESSAGE-INTEGRITY` over direction-labelled
  credentials derived from the PSK, so a stranger cannot advance the state
  machine and oxutrm cannot be used as a reflector.
- **No key material is ever written to disk**, on either side.
- Public STUN servers learn only that an IP is using STUN — no session content,
  no peer address, no identity. The server list is configurable.

## Building

Rust 1.85 or newer (edition 2024).

```
make            # fmt-check, clippy, then tests
make test
make lint
```

Every rule caps parallelism at 4 and passes `--workspace`. The `--workspace` is
not decoration: the repo root is both a `[package]` and a `[workspace]`, so a
bare `cargo test` selects only the root binary and reports "ok" while running
none of the crates' tests.

## Layout

```
crates/oxutrm-proto/   wire types, the screen model, framing
crates/oxutrm-sync/    the state/diff engine — NO I/O AT ALL
crates/oxutrm-term/    PTY, emulator, ScreenState conversion
crates/oxutrm-net/     candidates, STUN, NAT traversal, QUIC
crates/oxutrm-host/    session registry, daemonize, PTY supervision
crates/oxutrm-client/  renderer, raw mode, the status line
src/                   the binary: subcommand dispatch and the session loops
```

The load-bearing boundary is that **`oxutrm-sync` has no I/O** — no sockets, no
files, no clocks. That is what lets the riskiest property in the protocol (for
any sequence of states and any subset of frames dropped, duplicated or
reordered, the two ends converge) be tested exhaustively without a network. A
test asserts it against the crate's own manifest, so the boundary is enforced
rather than remembered.

## Licence

Apache-2.0.

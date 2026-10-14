# oxutrm

[![CI](https://github.com/oetiker/oxutrm/actions/workflows/ci.yml/badge.svg)](https://github.com/oetiker/oxutrm/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/oetiker/oxutrm)](https://github.com/oetiker/oxutrm/releases/latest)

A remote terminal that survives bad networks, changing IP addresses, and NAT on
both ends.

oxutrm is Mosh rebuilt in Rust, with a terminal emulator on both ends. The host
owns the authoritative screen and sends the client the differences over QUIC;
ssh is used only to start a session and to rebuild a lost link. It adds NAT
traversal on both ends and a session switcher. Scrollback is kept on the host
but not yet sent to the client.

> **Status:** the transport, the terminal core and the session loops work and
> are tested. oxutrm is young; see [What does not work yet](#what-does-not-work-yet).

The full reference is the man page, [`docs/manual.md`](docs/manual.md)
(`man oxutrm` once installed).

## Installing

oxutrm has to be installed on **both machines**: the one you type on and the
one you connect to. `oxutrm <ssh-target>` runs `ssh <ssh-target> oxutrm host
--connect`, so the far end needs `oxutrm` on a `PATH` that a non-interactive
ssh searches. Check it with:

```sh
ssh <ssh-target> command -v oxutrm
```

The binaries are statically linked, so the far end's distribution and glibc
version do not matter.

Release archives for Linux (`x86_64` and `aarch64`, musl), macOS (`x86_64`
and `aarch64`) and illumos (`x86_64`) are on the
[releases page](https://github.com/oetiker/oxutrm/releases/latest). There is
no Windows build: oxutrm needs a pty, `fork` and Unix sockets.

Debian and Ubuntu:

```sh
sudo install -d -m 0755 /etc/apt/keyrings
sudo curl -fsSL -o /etc/apt/keyrings/gitea-oposs.asc https://gitea.oetiker.ch/api/packages/oposs/debian/repository.key
echo "deb [signed-by=/etc/apt/keyrings/gitea-oposs.asc] https://gitea.oetiker.ch/api/packages/oposs/debian stable main" | sudo tee /etc/apt/sources.list.d/oposs.list
sudo apt update && sudo apt install oxutrm
```

Fedora, RHEL, Rocky and Alma, with dnf4 and dnf5 alike:

```sh
sudo curl -fsSL -o /etc/yum.repos.d/oposs.repo https://gitea.oetiker.ch/api/packages/oposs/rpm.repo
sudo dnf install oxutrm
```

Homebrew:

```sh
brew tap oetiker/oxutrm https://github.com/oetiker/oxutrm
brew trust --formula oetiker/oxutrm/oxutrm
brew install oxutrm
```

`brew trust` exists from Homebrew 6 on; skip it on older versions.

mise:

```sh
mise use -g github:oetiker/oxutrm
```

## Using it

```
oxutrm <ssh-target>                        # pick a session there, or start one
oxutrm --attach <name|id> <ssh-target>     # go straight to that one
oxutrm --new [--name <name>] <ssh-target>  # start a new one regardless
oxutrm host --list                         # sessions on this machine
oxutrm loopback                            # both halves in one process, no network
```

`oxutrm <ssh-target>` drives ssh, tries the connection ladder, brings up QUIC
on the first path that works, and hands you the shell. The session survives the
client going away. Connect again and the far end offers what is running: with
any session there, the **session selector** opens, to switch to one, start a
new one, rename or kill one. Nothing is resumed silently. `--attach` goes
straight to a session by its name or the start of its id (lowercase hex), and
`--new` always starts a new one, named with `--name`. The same selector is `s`
in the status popup, and switching there uses the live link, not ssh. See
[`docs/sessions.md`](docs/sessions.md).

A client whose network dies reconnects by itself. After twenty seconds of
silence it rebuilds the link with a new ssh, back into the same session. The
first attempt starts at once; after that it waits two seconds, then four, then
eight, and every eight seconds for as long as you leave it running. The old
link stays open meanwhile, and whichever link answers first carries the
session. These attempts run `ssh -o BatchMode=yes`, so a key that needs a
passphrase typed cannot be unlocked; the attempt fails and the status popup
says why.

While a link is up, the client also keeps a **standby**: a second connection
to the same session from a different local source address, for example the
open internet next to a split-tunnel VPN. When the primary goes quiet and the
standby still answers, the client fails over to it in about three seconds,
without ssh. It does not switch back. On a machine with only one way out
there is no standby, and the client searches again at intervals that grow from
thirty seconds to five minutes; each search raises the session's attach number.

`Ctrl-\` opens a status popup, and an outage opens it by itself. It shows the
round-trip time and its range over the last minute, loss, throughput, the path
MTU, which link of the session this is, the standby, what the rebuild is
trying, and the last things oxutrm did. Two quick presses send one literal
`Ctrl-\` to the remote program. Input typed during an outage is held, and when
the host answers the popup asks whether to send it (`s`) or drop it (`d`).
Everything oxutrm does to keep the session alive is also appended to
`~/.local/state/oxutrm/client.log`.

The popup key, the timings and the network features are set in
`~/.config/oxutrm/config.toml`, or with `c` in the status popup. See
[`docs/config.md`](docs/config.md).

oxutrm never parses `~/.ssh/config`. It runs `ssh` and expects
`ssh <ssh-target>` to work already, by whatever means: jump host, VPN, reverse
tunnel.

## How it differs from Mosh

| | Mosh | oxutrm |
|---|---|---|
| Transport | AES-OCB over UDP | QUIC, key pinned via SSH |
| Roaming | custom address-update logic | QUIC connection migration |
| NAT | the server must be reachable | a five-rung ladder; both ends may be behind NAT |
| Firewalls | UDP 60000-61000 | UDP/443 first |
| Emulation | prediction overlay | a terminal emulator on both ends |
| Scrollback | none | kept on the host, not yet sent to the client |
| Reattach | none | the same path as a first connect, with a session selector |
| `TERM` | `xterm-256color` | from what the local terminal reports |

## How it connects

The host binds UDP/443 where it can. Then client and host try a ladder, and
the first path that validates wins:

| Rung | Mechanism | Works when |
|---|---|---|
| 0 | IPv6 direct | both ends have global IPv6 |
| 1 | Router port mapping (NAT-PMP, PCP, UPnP-IGD) | a typical home router |
| 2 | STUN and hole punch | most NATs |
| 3 | Birthday-paradox punch | symmetric NAT |
| 4 | SSH tunnel | UDP is blocked |

The result is printed once:

```
oxutrm  IPv6 direct  ·  11 ms  ·  mtu 1452
oxutrm  IPv4 punched (birthday, 312 probes)  ·  61 ms  ·  symmetric NAT
```

A session on rung 4 runs inside the ssh connection: it ends with it and cannot
be reattached, so it is reported as a warning. The packets are QUIC with ALPN
`oxutrm/1`.

## What does not work yet

- **A reconnect cannot ask for a passphrase.** A rebuild runs
  `ssh -o BatchMode=yes`; a key that needs unlocking and is not held by an
  agent makes the attempts fail, and the popup says so.
- **A session taken over by another client is reported as an error**: the
  client says the host closed the session without the shell exiting.
- **Scrollback is not synced.** The host keeps it; the client cannot fetch it.
- **No local echo.** Typing waits for the round trip.
- **No `tmux -CC` integration.**
- **Bandwidth adaptation is a fixed pacing interval**, `clamp(rtt/2, 8ms,
  100ms)`, not a loss-aware policy.

## What it will not do

- **Graphics protocols**: no Sixel, Kitty or iTerm2 inline images.
- **Windows.**
- **A GUI.** oxutrm renders into the terminal you already have.
- **VPN or port forwarding.** It moves one terminal session.
- **Replace tmux.**

## Known limitations

- **Blinking text loses its blink across a resize.**
- **All five underline styles render as a plain underline.**
- **The icon name (OSC 1) is ignored.**
- **Narrowing the window pushes wrapped rows into scrollback**, and widening
  it again does not bring them back, as in a local terminal.
- **Both ends behind NAT is untested.** The network-namespace tests put one end
  behind cone, symmetric and nested double NAT; no test NATs both ends at once.
- **A better path found after connect is used only from the next attach.** QUIC
  migration changes the client's local address, not the remote one; a standby
  is used only when the primary fails.
- **A standby can share the primary's physical link.** It is chosen by source
  address, not by interface, so on a dual-stack or multi-address NIC both
  connections can use one wire and fail together.
- **Under a saturating writer such as `yes`**, a loop turn takes longer than
  the 8 ms pacing floor. The screen stays current and nothing queues.

## Security

- The trust root is SSH. There is no certificate authority, no new credential
  store and no additional listening service.
- The host generates a fresh self-signed certificate and PSK for every attach.
  The SHA-256 of its SPKI travels over SSH; the client accepts that fingerprint
  only, and verifies the handshake signature.
- ICE connectivity checks carry `MESSAGE-INTEGRITY` over credentials derived
  from the PSK, so a stranger cannot advance the state machine or use oxutrm
  as a reflector.
- No key material is written to disk on either side.
- Public STUN servers learn only that an address uses STUN. The server list is
  configurable.

## Building

Rust 1.96 or newer (edition 2024).

```
make            # fmt-check, clippy, then tests
make test
make lint
```

Every rule caps parallelism at 4 and passes `--workspace`.

## Layout

```
crates/oxutrm-proto/   wire types, the screen model, framing
crates/oxutrm-sync/    the state/diff engine, with no I/O
crates/oxutrm-term/    PTY, emulator, ScreenState conversion
crates/oxutrm-net/     candidates, STUN, NAT traversal, QUIC
crates/oxutrm-host/    session registry, daemonize, PTY supervision
crates/oxutrm-client/  renderer, raw mode, the status line
src/                   the binary: subcommand dispatch and the session loops
```

`oxutrm-sync` has no sockets, files or clocks, so convergence under dropped,
duplicated and reordered frames is tested without a network. A test checks
the crate's manifest for this.

## Licence

Apache-2.0.

# oxutrm — Session Recovery and Link State

**Date:** 2026-08-29
**Status:** Approved design. Extends `2026-08-25-oxutrm-design.md`; contradicts
nothing in it. §8.5 of that document ("Sequence numbering and attach
boundaries") is the normative rule this design builds on rather than revises.

---

## 1. Purpose

A user lost their network mid-session and reported two things:

> on the drop, *"no indication of what was happening… I would have expected a
> popover telling about the problem"*; and on reconnecting, they *"just got a
> fresh session instead of a popover with a list of existing sessions showing
> which one was connected, which process they were running and the option to
> either reconnect a connected session or steal an existing connection or open
> a new session"*.

This specifies the first half and the machinery the second half needs: **the
session survives the outage and comes back, and the user can see it happening.**

The starting point is worse than "no indication". Today a network drop freezes
the screen for thirty seconds — `max_idle_timeout` in
`crates/oxutrm-net/src/quic.rs:49` — and then the client exits with a raw error
string, `the link to the host ended without the shell exiting: timed out`. The
shell survives on the host, but nothing reachable can get back to it, because
`oxutrm host --attach` is unimplemented (`src/main.rs:76`).

That thirty seconds is not a design intent. It is a transport default nobody
questioned, and it directly contradicts the project's headline claim.

### 1.1 The principle

**Liveness is an application concern with a visible UI, not a hidden transport
default that silently kills sessions.** The transport's job is to carry bytes;
deciding that a session is over is a decision a user must be able to see, and
ideally make.

### 1.2 Goals

- The user always knows what the link is doing, and what oxutrm is doing about it.
- A short outage costs nothing: the same connection resumes, nothing rebuilt.
- A long outage costs a reconnect, not a session.
- The client never gives up on its own. The user decides when to leave.
- Nothing typed blind is delivered without the user seeing it first.
- `oxutrm host --attach` works, which the session picker (a later piece) needs.

### 1.3 Non-goals

- **The session picker at connect.** It needs `--attach` and `--list`, which
  this delivers, but choosing among sessions is its own piece of work.
- **Speculative local echo.** Phase C. Its absence is why §8 exists in the shape
  it does.
- **Graphics protocols.** Still out of scope, and still at the *emulator*:
  `alacritty_terminal` does not model Sixel, Kitty or iTerm2, so the bytes never
  reach a grid. See `README.md:153`. Nothing in the client's renderer is the
  obstacle, and the client-side widget library adopted in §8 does not change it
  either: it draws oxutrm's own local UI, and has nothing to do with what the
  host's emulator was able to preserve.

---

## 2. Client link state

One state machine. The notice described in §8 is nothing but its rendering, and
carries no state of its own.

| State | Entered when | Notice shown |
|---|---|---|
| `Live` | a frame advanced our ack | none |
| `Silent { since }` | `SILENT_AFTER` (2 s) with a reply owed and none arriving | "no reply from host", with counters |
| `Recovering { attempt, next_try }` | `REBUILD_AFTER` (20 s) of silence | "waiting for the network", backoff and countdown |
| `Confirming { held }` | link restored while held input is non-empty | what was typed offline, and the two keys that resolve it |
| `Displaced { by, at }` | the host reports another client took the session | who took it, and the key that takes it back |

**Any inbound frame that advances our ack returns to `Live` from any state.**
This is why §6 never tears down the old connection while a rebuild is in
flight: whichever path revives first wins, and the loser is discarded.

`Live → Silent` is deliberately quiet below two seconds. A blip that resolves in
400 ms must never paint anything, or the indicator becomes the noise it was
built to remove.

---

## 3. Detecting silence honestly

### 3.1 The clock already on the wire

`Sender::make_frame` (`crates/oxutrm-sync/src/channel.rs:112`) sends a frame
carrying an empty diff whenever it owes the peer an acknowledgement it has not
heard. **Every input the client sends therefore obliges a reply**, whether or
not the shell produced any output. That is a true round-trip liveness test, it
costs nothing, and it needs no protocol change.

The client tracks:

- `last_heard: Instant` — any frame arriving on the link.
- whether a reply is owed — `input_tx.current().seq()` against
  `screen_rx.peer_ack()`, which is what the peer says it has seen of us.

### 3.2 The idle gap, and the heartbeat that closes it

With nothing outstanding, silence and calm are indistinguishable: a user who
walks away, loses the network, and comes back would find a stale screen with no
notice on it.

So when nothing has been sent or received for `HEARTBEAT_IDLE` (5 s), the client
bumps its input sequence with an empty append — precisely what `resize` already
does (`src/session.rs`, `ClientSession::resize`) — which makes `state_moved`
true and obliges the host to answer.

**This is 0.2 Hz.** It is set against the 250 Hz poll removed in `19cc001`, and
it costs a detached session nothing at all, because a detached session has no
client to send it. The idle-CPU property that commit established is preserved.

### 3.3 What the notice may claim

The client can observe these, and only these:

| Condition | Source |
|---|---|
| no reply for *N* | §3.1 |
| screen frames rejected | `Turn.rejected` — `on_frame` returned `BaseMismatch`/decode error |
| cannot send | `SendOutcome::Dropped(reason)` from `FrameSink` |
| path quality | `conn.stats().path` — `lost_packets`, `black_holes_detected`, `rtt` |
| how it ended | quinn's `ConnectionError` at close |

**A dead network and a crashed host are indistinguishable to the client**, and
the notice must not pretend otherwise. In particular it must never say the
session is safe: it does not know that. It reports silence and counters, and
lets the reconnect attempt in §6 be what discovers the truth.

This also retires task #17. The two `eprintln!` sites that corrupt the painted
screen (`src/session.rs:729`, `src/session.rs:1016`) become notice content;
their output is diagnostics about the link, which is exactly what the notice is
for. The host's own `eprintln!` is untouched — it goes up the ssh channel and
`drain_stderr` consumes it.

---

## 4. Tier A — hold the connection

### 4.1 The idle timeout goes

`max_idle_timeout` is removed from `transport_config()`
(`crates/oxutrm-net/src/quic.rs:49`), along with the constant in
`src/accept.rs:53` that exists specifically to match it. Both ends change: the
value is negotiated to the minimum of the two, so a one-sided change would do
nothing.

**quinn's keep-alive stays at 10 s.** It is not redundant with §3.2's
heartbeat: keep-alive is what holds a punched NAT binding open, which rungs 2
and 3 depend on for the life of the connection.

Consequence to be explicit about: `conn.closed()` now fires only on an explicit
close or a transport error, never on silence. The client's own state machine is
what notices silence, which is the point of §1.1.

### 4.2 Following a local address change

QUIC identifies a connection by connection IDs, not addresses, so a changed
local address is survivable by design. `Link::rebind` (`src/session.rs:1064`)
already performs the socket swap and is tested; its doc records what is
missing: *"No caller: roaming is not wired. The mechanism is here and tested;
what is missing is whatever notices that this machine's address changed."*

**The trigger is evidence, not a platform API.** Bind a scratch UDP socket,
connect it to the peer's address, and read its `local_addr()`: that is the
address the kernel would use for this peer right now, obtained without sending
a packet. Differs from the socket we hold → the route moved → rebind. No
netlink, no route sockets, no `cfg`, and both platforms walk the same code —
consistent with the existing rule that `FD_DIRS`, `open_keyboard` and
`second_loopback_ip` use candidate lists rather than `cfg`.

**Only ever attempted while already `Silent`.** A rebind moves our source port,
which invalidates a punched NAT hole; doing it to a working path would break the
path in order to test it. If the rebind does not restore contact, `REBUILD_AFTER`
arrives and §6 re-punches properly.

---

## 5. Tier B — rebuild via reattach

### 5.1 The rule this obeys

`crates/oxutrm-host/src/attach.rs:3` states the constraint, and it is not
negotiable:

> Reattachment is not a second code path. `--attach` connects to the running
> session's Unix socket and relays the same `Signal` traffic a first connect
> would carry, so the session performs a fresh ICE exchange and the client
> cannot tell the two apart. Anything that only worked on reattach would be
> untested by every ordinary connect.

So no reattach-specific handshake is introduced. The existing R4–R11 exchange in
`src/serve.rs` is made generic over its signalling stream, and both the ssh
pipes and the Unix socket feed the same code.

### 5.2 Host side

`connect_to_session` and `begin_attach` already exist. What does not exist is a
listener: `Registry::socket_path()` is computed and registered, and **nothing
has ever bound it.**

1. After detaching, the serving session binds its registered socket path as a
   `UnixListener`.
2. An accepted connection runs the generic exchange of §5.1: `HostHello` (with
   a bumped `attach_id` from `begin_attach`, fresh PSK and certificate),
   `ClientHello`, candidate relay, ICE, `quic_server`.
3. The resulting `Link` reaches the running `HostSession` through an mpsc
   receiver held as a **local** and selected on inside `HostSession::run` —
   never through `self`, per the loop's existing rule that its arms borrow
   locals or the code does not compile.
4. The old link is replaced and its connection closed with a takeover reason
   (§7). Both sync channels reset per §8.5 of the design spec, and
   `screen_stale` is set so the next turn produces a snapshot — the field
   already exists for exactly this, documented as *"The emulator moved while
   nobody was attached… Forces one snapshot on the turn a peer comes back."*

`oxutrm host --attach <id>` then relays stdio to that socket, and `src/main.rs:76`
stops returning an error.

### 5.3 Client side

`connect()`'s L4–L10 (`src/connect.rs`) becomes a reusable function returning a
`Link` and a `PathDescription`. A rebuild runs it against
`ssh <target> oxutrm host --attach <id>`, using the `session_id` the client
already received in `HostHello` (`crates/oxutrm-proto/src/signal.rs:46`).

Backoff: 1, 2, 4, 8 s, then every 8 s indefinitely. The client never stops of
its own accord; §10 is how the user stops it.

**The old connection is held throughout.** A rebuild is an additional attempt,
not a replacement, and if the original path revives mid-attempt the attempt is
abandoned.

### 5.4 What survives the swap

Nothing about the *session* changes — same host, same PTY, same shell. Only the
transport is replaced. Both ends reset their sequence counters to 1 and the
host's first datagram is a full state, exactly as design spec §8.5 requires:

> Both sides reset their counters to 1 at every attach, and the host bumps
> `attach_id` so the two ends agree on which generation they are in. […] The
> host's first datagram of each attach is a full state.

No attempt is made to detect "it is the same client, resume the diff stream".
A full screen is a few compressed kilobytes, once, and the alternative is
cleverness on the one path where being wrong shows the user a corrupted screen.

---

## 6. Takeover, symmetric

Newest attach wins. The arriving client gets the session; the displaced link is
closed with a reason naming the takeover, and the displaced client enters
`Displaced` and shows who took it and when.

**The displaced client is offered the key that takes it back**, which is the
same attach path in the other direction. Takeover is therefore symmetric, and
"steal" — queue item 4 — stops being a feature that needs designing and becomes
a consequence of this one.

The alternative, refusing an attach while the current link still looks alive,
was rejected: "still looks alive" is precisely what is unknowable during an
outage, so it would refuse the returning client in the case this whole document
exists to serve.

Design spec §8.5 already anticipates this case — `attach_id` exists partly *"so
a host that receives a second `--attach` for a session it is already serving can
tell the two apart."*

---

## 7. Input typed while disconnected

Keystrokes typed while not `Live` go to a **holding buffer**, not to `input_tx`
— whose state resets at the attach boundary anyway (§5.4).

On the link returning with a non-empty buffer, the client enters `Confirming`
and shows what was typed, with two keys: send it, or drop it. Nothing is
delivered until the user chooses.

The reason is that the alternative is genuinely dangerous. Delivering blind
input replays it against a screen that moved while the user could not see it, so
a half-typed command can complete into something never intended — and without
speculative echo (Phase C, a non-goal here) there is no way to show the user
what they typed as they type it. Discarding silently is safe but loses work.
Showing it and asking loses neither.

Rules:

- The buffer is capped at `MAX_HELD` (64 KiB). On reaching the cap the client
  **stops accepting and says so**; it never drops the oldest bytes, because the
  oldest bytes are the command and the newest are the newline.
- Rendering is readable, not raw: `↵` for carriage return, `^C` for control
  bytes, and a long buffer summarised (`…and 2.3 KB more`) rather than dumped
  into a box that cannot hold it.
- Keys typed while `Confirming` append to the buffer and update the notice.
  They are still not delivered.
- Quitting while `Confirming` discards the buffer, like any other quit.

---

## 8. The local UI layer

The client paints **two layers**, and separating them is the architectural point
of this section:

- **Layer 0 — the remote framebuffer.** Authoritative, produced by
  `alacritty_terminal` on the host and shipped as `ScreenState` diffs. No
  escape sequence written by the remote application ever reaches the user's
  terminal; only framebuffer content does. Modes are the apparent exception and
  are really the rule — mouse and bracketed paste travel *semantically* as
  `Modes`/`MouseMode`, and the client emits its **own** DECSET for them.
- **Layer 1 — local UI.** Owned entirely by the client, never sent to the host,
  and composited over layer 0. The notice is its first citizen; the session
  picker, a config screen and any later interactive gadget are the reason it is
  a layer rather than a special case.

### 8.1 Compositing

`Renderer::set_overlay(Option<Overlay>)`. Layer 1 is composited into the cell
grid **before** the diff against `Painted`, so drawing it and removing it are
both ordinary diffs.

This is the property the whole approach rests on: no repaint, no
`Renderer::invalidate`, no desync, and it works while the host is unreachable
because the model being diffed against is entirely local. The renderer's module
doc anticipated it (`crates/oxutrm-client/src/renderer.rs:9`):

> Keeping that model of the painted screen is what makes local scrollback and a
> locally drawn status pane possible later: they can be painted over the screen
> and then undone.

It also means layer 1 is **not** in the class of bug that
`ClientSession::announce` must work around by invalidating: nothing is ever
written outside the renderer's model.

Compositing happens on the renderer's internal cell buffer and **never** on a
`ScreenState`. Layer 1 therefore cannot violate the `ScreenState` invariants,
because it never becomes one.

### 8.2 Widgets: ratatui, headless

Layer 1 is built with **`ratatui-core` and `ratatui-widgets`**, rendered into a
bare `ratatui` `Buffer` and converted once into oxutrm cells.

**No backend, no `crossterm`, no `Terminal`, no terminal ownership.** ratatui
0.30 split the umbrella crate precisely this way — `ratatui-core` carries
`Buffer`, `Cell`, `Style`, the layout solver and the `Widget` trait with an
empty default feature set, and `crossterm` is an optional feature of the
umbrella crate that is simply not taken. Its MSRV is 1.88, under this project's
1.96.

What this buys and what it deliberately does not:

- **Buys**: the layout/constraint solver, `Block`, `Paragraph`, wrapping,
  `List`, `Table`, scrollbars — everything the session picker and a config
  screen need, without hand-rolling a layout engine.
- **Does not touch**: the final diff, the colour downgrade through
  `TerminalCaps` and `color::down_convert`, the `Painted` model, raw-mode
  handling in `RawGuard`, or the byte-exact mouse pass-through. ratatui hands
  over a rectangle of styled cells and never learns a terminal exists.

The alternative considered and rejected was adopting ratatui *with* crossterm as
the client's rendering stack. That would replace `Renderer` with a third copy of
the same grid, discard the colour downgrade, and force mouse reports to be
parsed into typed events and re-encoded — a round trip that can only lose
fidelity in a program whose job is to be transparent.

**The conversion is the part to get right**, and it is written once and tested
once: ratatui `Cell`/`Style`/`Color`/`Modifier` into oxutrm `Cell`/`Attrs`/
`Color`, including grapheme clusters against `CellText`'s `MAX_CELL_TEXT` of 32,
and wide characters against the grid's own continuation representation.

### 8.3 Shape

A centred bordered box, clipped to the screen. Below roughly 20x6 it degrades to
a single reverse-video line on row 0, because a box that does not fit is worse
than a line that does. The cursor is hidden while an overlay shows.

A 1 s tick drives the live counters, and it runs **only while an overlay is
up**, so a healthy session gains no wakeups.

### 8.4 Synchronized output

Repaints are wrapped in DECSET 2026 (`\x1b[?2026h` ... `\x1b[?2026l`) so the
terminal shows a repaint atomically instead of mid-tear. Emitted
**unconditionally**: conforming terminals ignore unknown private modes, so this
needs no capability detection, no `TerminalCaps` field, and no negotiation.

It matters most exactly here — a box painted over live content is where tearing
would be visible — but it improves every repaint.

---

## 9. Keys

`Ctrl-\` is a prefix, and it is live **only while a notice is showing**. While
`Live`, every byte belongs to the host and is passed through untouched, which
sidesteps the escape-character collisions Mosh must live with.

| Keys | Where | Effect |
|---|---|---|
| `Ctrl-\` `q` | any notice | close oxutrm on this machine; the shell keeps running on the host |
| `Ctrl-\` `r` | `Displaced` | take the session back |
| `Ctrl-\` `s` | `Confirming` | send the held input |
| `Ctrl-\` `d` | `Confirming` | drop the held input |

Every notice states its own keys, and states them in full sentences: the quit
key must make clear that it closes the *local* client and does not touch the
remote shell. That distinction is the entire content of the sentence.

---

## 10. What changes where

| File | Change |
|---|---|
| `crates/oxutrm-client/Cargo.toml` | new deps: `ratatui-core`, `ratatui-widgets`, both without a backend |
| `crates/oxutrm-client/src/overlay.rs` | new — layer 1: the ratatui `Buffer` to oxutrm `Cell` conversion, and the small-screen fallback |
| `crates/oxutrm-client/src/notice.rs` | new — the notice's widgets, its text, held-input rendering |
| `crates/oxutrm-client/src/renderer.rs` | `set_overlay`, compositing before the diff, DECSET 2026 wrapping |
| `crates/oxutrm-net/src/quic.rs` | `max_idle_timeout` removed; keep-alive unchanged |
| `src/accept.rs` | the constant that matched the idle timeout |
| `src/session.rs` | the state machine, liveness clock, heartbeat, held input, key prefix, link swap; the two `eprintln!` sites become notices |
| `src/connect.rs` | L4–L10 extracted as a reusable handshake; the rebuild loop |
| `src/serve.rs` | R4–R11 made generic over the signalling stream; the `UnixListener` |
| `src/main.rs` | `--attach` wired; the help text it currently contradicts |
| `crates/oxutrm-host/src/attach.rs` | the stdio relay |

---

## 11. Testing

Written test-first, per the project's practice. The traps recorded in the
handoff apply directly, in particular **inject the fault before believing the
test**, and **a test that passes against the injected bug is not a guard**.

- **Overlay conversion**: ratatui `Style` to oxutrm `Attrs`/`Color` for every
  variant; a grapheme cluster longer than `MAX_CELL_TEXT`; a wide character at
  an overlay edge, which is where the continuation representation is easiest to
  corrupt.
- **Renderer**: an overlay paints over the expected cells; removing it restores
  the exact prior content by diff; the small-screen fallback; DECSET 2026
  brackets a repaint.
- **State machine**, on a controlled clock: silence at 2 s enters `Silent`, at
  20 s enters `Recovering`; an arriving frame returns to `Live` from each state,
  including mid-rebuild.
- **Heartbeat**: an idle session with no input still obliges a reply within
  `HEARTBEAT_IDLE`, so silence is detectable with nothing outstanding.
- **Held input**: cap behaviour stops accepting rather than dropping the oldest;
  control bytes render readably; send and drop both leave the session `Live`.
- **Host attach**: a second attach over the Unix socket replaces the link;
  counters reset and the first datagram is a full state; the displaced
  connection closes with the takeover reason.
- **Take-it-back**: attaching again from the displaced client wins.
- **End to end**, on loopback: interrupt the path, assert the notice appears,
  restore it, assert the session resumes without a rebuild.

Both platforms. The fmt trap in the handoff applies — rustc 1.97.1 on the Mac
and 1.96.0 on thinlinc format differently, and CI runs both.

---

## 12. Phasing

Each phase is independently shippable and independently useful.

1. **The notice.** State machine, renderer overlay, heartbeat, DECSET 2026, the
   two `eprintln!` sites. Client only, no host changes. Turns a silent hang into
   a legible one immediately, and stands alone even if nothing follows it.
2. **Tier A.** Idle timeout removed, route probe, `Link::rebind` wired. Short
   outages and local address changes stop costing a session.
3. **Tier B.** Host `UnixListener`, generic signalling exchange,
   `host --attach`, the client rebuild loop. This is queue item 2.
4. **Takeover and take-it-back.** Queue item 4, as a consequence rather than a
   feature.

Phase 1 pays for itself on its own; phases 3 and 4 also unblock the session
picker (queue item 3).

---

## 13. Risks and open questions

- **`REBUILD_AFTER` is a guess.** Twenty seconds is long enough that a rebuild
  is not raced against an outage about to end on its own, and short enough not
  to feel abandoned. It should be revisited against a real bad network, not
  reasoned about further.
- **Removing the idle timeout means a dead connection never errors.** On the
  host that is harmless — newest attach wins, and sessions outlive clients by
  design — but it must not be reintroduced as a "cleanup" without replacing the
  liveness signal it would take away.
- **The route probe is unproven under a VPN.** Binding and connecting a scratch
  socket reports what the kernel would do; whether that tracks a tunnel coming
  and going needs measuring on a real VPN, not asserting.
- **The rebuild path can stall on ssh.** `ssh` may prompt for a passphrase, and
  during a rebuild there is no terminal to prompt on — raw mode is held and the
  screen belongs to the renderer. Tier A exists partly so this is the uncommon
  path, but the failure needs a notice of its own rather than a hang.
- **ratatui is a new dependency in a deliberately lean crate.**
  `crates/oxutrm-client/Cargo.toml` carries a comment listing what must not be
  dragged in and why. `ratatui-core` and `ratatui-widgets` without a backend are
  pure layout and cell manipulation, but the comment must be extended to say so
  explicitly, including that `crossterm` is not taken and why.
- **A takeover notice names a peer address**, which is information about
  whoever attached. It is the user's own session on their own host, so this is
  informative rather than a disclosure, but it is worth stating that the choice
  was made deliberately.

---

## 14. Inherited constraints this design must not break

From the interface contract and the accumulated record, all still normative:

- **No datagram fragmentation.** **QUIC cannot repoint at a new remote
  address** — which is precisely why §6 exists rather than extending §5.
- **A send failure must never end a session, and a rejected frame must never
  disconnect.** The notice reports both; neither becomes fatal.
- `from_state == 0` applies regardless of sequence number; cursor out of range
  is rejected, not clamped.
- **`oxutrm-sync` must never depend on a crate with I/O**, and nothing here
  gives it a reason to.
- **`oxutrm-host` must not depend on `oxutrm-net`.**
- The host loop's descriptors are duplicated before the loop so its arms borrow
  locals, never `self`; the new mpsc arm of §5.2 follows that rule.
- **Do not add a `conn.closed()` arm to the host loop** — a closed connection is
  permanently ready and the arm would spin.
- `oxutrm-client` is `deny(unsafe_code)`; `src/main.rs` is `forbid`. ratatui is
  safe Rust and does not disturb this.
- **`oxutrm-client` must not gain a backend, a `Terminal`, or `crossterm`.**
  ratatui enters as a layout and widget library only; `Renderer` remains the
  only thing in the tree that writes to the user's terminal.
- **`IDLE_POLL` is not to be reintroduced as a pace.**

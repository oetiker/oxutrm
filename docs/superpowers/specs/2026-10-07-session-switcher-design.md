# oxutrm — Session switcher

Status: draft 2026-10-07, design agreed with the user in chat (four sections);
revised 2026-10-08 after a pushback review (13 findings; the user settled the
four that were theirs — rows marked *(review)* in §1.1).
Sub-project **D1** of D: A (no diagnostics on the screen, merged), B (status
popup, merged), C (config file and config screen, merged), **D1 (this)**, D2
(preview, deferred — §1.2). Builds on B
(`docs/superpowers/specs/2026-10-03-status-popup-design.md`), whose key bar
carries the greyed-out `s sessions` entry this fills, and on the session model
of `…/2026-08-29-session-recovery-design.md`,
`…/2026-09-06-registry-root-and-boot-identity-design.md` and
`…/2026-09-07-tier-b2-client-reattach-design.md`.

---

## 1. Purpose

A host may run several oxutrm sessions. Today the only way between them is to
quit and reconnect with `--attach <id>`, and a bare connect to a host with one
detached session silently resumes it.

D1 gives the client one **selector**: a list of the host's sessions, from which
you switch, start a new session, rename or kill one. The same selector opens
from the status popup (`s`) and at connect time, whenever the host already has
a session.

**Success:** with three named sessions on a host, the user connects, picks
`build` from the list, later presses the popup key, `s`, `↓`, `⏎` and is in
`logs` within about one ICE exchange, with no ssh involved; kills `build` from
there; and when they kill the session they are in, the selector stays open
until they pick another.

### 1.1 Decided with the user

| Question | Decision |
|---|---|
| What the switcher is for | Both frequent hopping between shells on one host and picking up sessions left behind. |
| Actions | Switch, new, kill, rename (D1); preview (D2). |
| Transport | **ssh only initiates a connection, never anything after it.** Every switcher request travels over the live link's control stream (approach A of three; a fresh ssh per switch, and a mix of both, were rejected). A future bootstrap — the P2 rendezvous mailbox, or a session-establishment packet sent by email — must be able to replace ssh entirely. |
| Compatibility | None needed: no deployments besides the user's own tests. The wire is designed cleanly and `PROTO_VERSION` is bumped; no feature flags, no fallbacks. |
| Names | Set from the selector (`r`) and with `--new --name <n>`; `--attach <x>` takes an exact name or an id prefix; `host --list` shows them. |
| A session attached to another client | Listed as `in use`; switching to it asks for confirmation, then takes it over as `--attach` does today. |
| Connect without `--attach`/`--new` | If any session exists — even exactly one — the selector opens; nothing is resumed silently. With none, a new session starts as today. |
| Connect-time UI | The same full-screen selector as in the popup (not the line picker). |
| Killing the session you are in | Allowed. Its process stays as a **lobby** feeding the selector; the client ends only when you quit. |
| Killing the last session *(review)* | The client stays in the lobby, with only `+ new session` offered; `q` ends it. Killing never ends the client by itself. |
| How long a silent lobby lives *(review)* | `DETACH_AFTER` (30 s), the silence rule sessions already use. |
| Where new shells start *(review)* | In `$HOME`, as a login shell (`argv[0]` = `-<basename>`), as ssh would give you — for first connects too, which today start `$SHELL` without arguments in `/`. |
| Names vs id prefixes *(review)* | A name must contain at least one character outside `[0-9a-f]`, so it can never be read as an id prefix. No precedence rule. |
| The shell exits on its own (`exit`, crash) | The client ends, as today. Only a kill from the selector leads to the lobby. |

### 1.2 Not in D1

- **Preview (D2).** Showing the selected session's screen needs the target to
  render a snapshot over its socket and the selector to fit it into 72×24; it
  builds on D1's `Open` dispatcher and gets its own spec.
- Moving recovery rebuilds off ssh. They still open a fresh ssh today; under
  the §1.1 transport rule that is a candidate for later work, not a precedent.
- Switching between hosts. The selector lists the host the client is
  connected to.
- Polling the list. It is fetched when the selector opens and after each
  action that changes it.
- Rung 4 (ssh tunnel), which is not implemented (§3.6).

---

## 2. Concepts

### 2.1 Session, lobby

A **session** is one host process with one shell, a registry entry
(`meta.json` + `sock`) and at most one adopted client link — unchanged.

A **lobby** is the same process kind **without a shell**. It runs the ordinary
attach exchange, answers the control stream, and owns no registry entry
(nothing in it outlives its link, so nothing is listed). It exists for two
reasons:

- **Connect time.** A bare connect to a host with sessions answers the offer
  with `Choice::Lobby`; the host starts a lobby; the client opens the selector
  over a blank screen.
- **Kill-self.** A session whose shell was killed *on request* (§3.4) drops its
  registry entry and becomes a lobby, so the selector keeps working.

A lobby ends when its client moves away (a switch, `q`) or when it has heard
nothing from its client for `DETACH_AFTER` (30 s) — the same silence rule a
session uses to detach. (The QUIC idle timeout is off, `oxutrm-net`'s
`max_idle_timeout(None)`, so "the link closed" is never observed for a client
that simply vanished.) It never waits for a client to come back beyond that.

A lobby has a **session id from the start**, minted like any session's and
sent in its `HostHello`. It is not registered under it until it has a shell.

A lobby that receives `New` starts its shell, registers under the id it
already has, and **becomes** the new session: no second attach, no second ICE
exchange, and the client's `Identity` and rebuild target need no new id. The
lobby's control stream is served by the door dispatcher (§2.2) from the start,
so after `New` the link is an ordinary session link; the client arms its
standby search only once it is in a session, never in a lobby.

### 2.2 Doors

A session process (or lobby) has two doors: its QUIC **control stream** (from its
client) and its Unix **socket** (from a sibling session, or from
`oxutrm host --attach` over ssh). Both doors read the same first line, an
`Open` (§4.1), and dispatch it in one place.

**Each connection gets its own task**, which reads the `Open` line under a
short timeout and dispatches it. Only `Open::Attach` enters the session's
existing serial attach loop (`src/listener.rs`, one exchange at a time under
the meta lock, bounded by `ATTACH_TIMEOUT`); only `Attach { role: Primary }`
pre-empts a standby exchange there, as today. `Myself`, `Kill` and `Rename`
are served from state that is not held across an exchange, so they answer
within milliseconds even while the session is mid-attach, and a `Sessions`
fetch never disturbs a sibling's standby search.

A request is always served by the
process that owns what it touches: a session only ever signals its own shell
and writes its own `meta.json`; `Kill` and `Rename` for a sibling are
forwarded to the sibling's socket.

### 2.3 Names

`Name`: 1–24 characters, printable, no leading or trailing whitespace, and
at least one character outside `[0-9a-f]` — so a name can never be read as an
id prefix and `--attach x` needs no precedence rule. Unique per host among live
sessions.

Uniqueness is checked by the session asked to take the name, under an
exclusive lock on a `names.lock` file in the registry directory, against a
fresh registry read; the new `meta.json` is written while the lock is held.
`meta.json` is written atomically (temporary file + rename) — today it is
truncated and rewritten in place, and `Registry::list_in` skips an entry it
cannot parse, so a torn read could hide a name from the check. Stored as
`name` in `meta.json` (`serde(default)`: an entry written by an older binary
must keep parsing, because a running host keeps its old binary).

---

## 3. Behaviour

### 3.1 Connect

`choose::decide` keeps its role as the one pure table:

| Flags | Sessions offered | Choice |
|---|---|---|
| `--new [--name n]` | any | `New { name }` |
| `--attach x` | any | `Attach { id }` of the one session whose name equals `x`, or whose id `x` prefixes (≥ 4 chars, exactly one match); else refused, listing names and ids. A name always has a non-hex character (§2.3), so the two cannot both match. |
| none | none | `New { name: None }` |
| none | one or more | `Lobby` |

`--name` without `--new` is refused. The line picker (`choose::pick`) is
removed: there is no case left that asks on the line.

`Lobby` takes the same path on the host as `New` — `run_host_connect` forks
first, before any runtime exists, then runs the ordinary attach exchange on
ssh's pipes — and differs only in not starting a shell and not registering.

After a `Lobby` connect the splash plays as usual (skippable), then the
selector opens. `q` there ends the client; the lobby exits with its link.

### 3.2 The selector

Drawn through the renderer's overlay like the popup and the config screen,
under the popup's box rules (fits content, capped 72×24, named rules).

```
┌ sessions on thinlinc ─────────────────────────────┐
│▸ build      bash   09:14      120×40   this       │
│  a3f9c01e   fish   Oct 05     80×24    in use     │
│  logs       zsh    11:02      120×40              │
│  + new session                                    │
│───────────────────────────────────────────────────│
│ ⏎ switch  n new  r rename  x kill  q back         │
└───────────────────────────────────────────────────┘
```

- **Row:** name, else the first 8 characters of the id; shell basename;
  start time (local `HH:MM` today, else `Mon DD`); size; mark — `this` (the
  session you are in), `in use` (attached to another client), blank
  (detached), `?` (the sibling did not answer in time), `old version` (the
  sibling runs a binary with another protocol version; it can be killed only
  by ending its shell, and is not offered for switching). Non-detachable sessions are listed dimmed; choosing one is
  refused with the picker's existing reason. Sessions are ordered by start
  time; `+ new session` is always last. The selection starts on `this`, else
  on the first row.
- **Keys:** `↑`/`↓` move; `⏎` switches (or creates, on `+`); `n` creates;
  `r` edits the name in place (`⏎` saves, `Esc` cancels; the config screen's
  line editing); `x` kills after `kill build? y/n`; `q`/`Esc` go back to the
  popup — or, at connect time and in a lobby, end the client (`q back`
  becomes `q quit`).
- **Confirmations:** switching to an `in use` row asks
  `take over a3f9c01e from its other client? y/n`; killing an `in use` row
  asks `kill a3f9c01e (in use elsewhere)? y/n`. ANSWER_GUARD applies as in
  the popup.
- `⏎` on `this` closes the selector.
- **Errors** (a refused rename, a failed switch, an unreachable sibling) show
  as one line under the list and as a popup activity entry; detail goes to
  `client.log`. Nothing is printed.
- **Greyed out:** `s` in the popup is dimmed while the link is not Live
  (outage or rebuild in progress), with that reason.
- **An outage while the selector is open:** it stays, with the outage in its
  header, and its actions are refused until Live again (as the config screen
  does).

### 3.3 Switch

1. The client opens a control stream and sends `Open::Switch { to }`.
2. The current process opens the target's socket and sends
   `Open::Attach { role: Primary }`, then relays the stream both ways
   (`relay_signals`). The target runs the **ordinary attach exchange** — no
   switch-only code path, same rule as `crates/oxutrm-host/src/attach.rs`.
3. **Make before break.** The client keeps the old link until the new one is
   `Established`. Only then does it adopt the new link, close the old one with
   the new close reason `MOVED_AWAY`, drop the old link's parked standby, reset
   the renderer to the new session's screen, and point its rebuild path at the
   new session id. The selector closes; a popup entry records
   `switched to build`.
4. The old session is detached, as after any client loss. A lobby exits.
5. **Failure** (target refuses, ICE fails, target gone, old link drops
   mid-switch): the client is still in its old session; the selector shows
   the reason. If the old link dropped, ordinary recovery restores it. There
   is no resume of a half-done switch.
6. An `in use` target is taken over: its other client receives `TAKEN_OVER`,
   as with `--attach` today. If the switch then fails after the target
   adopted the new link, that other client has been displaced for nothing;
   accepted — it reconnects like after any takeover.

### 3.4 New, kill, rename

- **New from a lobby:** the lobby starts its shell, registers (with the name,
  if given) and becomes the session (§2.1). The client resets its screen and
  rebuild target as after a switch.
- **New from a session:** the session spawns a sibling as the ordinary
  `oxutrm host --serve` (the entry point ssh runs on a first connect), with
  its stdin and stdout on a socket pair the parent holds, and relays the
  client's control stream into that pair exactly as ssh carries a first
  connect. The sibling forks, runs the attach exchange, settles
  detachability, severs from its pipes (R12), registers and starts its shell
  in the order `serve()` already uses. **One startup path** makes every
  session process, whoever asks. The parent is a relay only, as in a switch.
- **Which binary a sibling runs:** the running session's own — on Linux
  `/proc/self/exe`, which still opens after the file was replaced by a
  rebuild; elsewhere the path the session was started from, recorded at
  start. The path is passed in, never looked up inside the spawning code
  (tests inject the built binary). If that path now holds another protocol
  version, the attach is refused like any version mismatch and the selector
  shows the reason.
- **Shells** start in `$HOME` as login shells, whoever starts them (first
  connect, lobby `New`, sibling).
- **How a kill is done:** the owning session hangs up its shell the way the
  kernel does when a terminal goes away (SIGHUP to the pty's foreground
  process group and to the shell, then closing the pty master), and sends
  SIGKILL to the shell's process group if it is still there after 3 s. It
  answers `Done` only after the shell was reaped and its registry entry
  removed, so the refetched list never shows it.
- **Kill a sibling:** forwarded to its socket; the sibling then ends the
  ordinary shell-exited way (its own client, if any, sees the shell exit).
  The list is refetched.
- **Kill this session:** the session kills its shell as above, marks the
  exit as *on request*, removes its registry entry and becomes a lobby (same
  id, unregistered). The selector stays open without the `this` row — with
  only `+ new session` if it was the last one. The client's rebuild switches
  to `Choice::Lobby` (§3.5).
- A shell exiting on its own takes today's `SHELL_EXITED` path; the client
  ends.
- **Rename:** forwarded to the owning session, which validates the name
  (§2.3) and rewrites its `meta.json`. An empty name clears it.

### 3.5 Recovery

- The rebuild path aims at the client's *current* session — a session id
  (updated by a switch or a sibling `New`) or, while the client is in a lobby
  (connect-time or after kill-self), `Choice::Lobby`.
- A lobby is never reattached (it ended after `DETACH_AFTER` of silence, or
  will): the rebuild answers the offer with `Choice::Lobby`, so the client
  lands in a fresh lobby with the selector still open. The old lobby times
  out on its own.
- A rebuild finding its session killed meanwhile (by another client) ends the
  client as today.

### 3.6 Rung 4 (note)

Rung 4 (QUIC tunnelled through ssh) is not implemented today
(`src/ladder.rs`, `crates/oxutrm-host/src/lib.rs`); D1 adds nothing for it.
When it lands, its spec has to settle the selector on a rung-4 link — at
least `New` in a rung-4 lobby, since a connect-time lobby is chosen before the
rung is known.

---

## 4. Wire

`PROTO_VERSION` is bumped. No compatibility with the previous version.

### 4.1 `Open` — the first line of every door

```rust
struct Open { proto: u32, req: Request }        // proto = PROTO_VERSION
enum Request {
    Attach { role: Role },                      // the attach exchange follows
    Probe { .. },                               // unchanged semantics
    Sessions,                                   // -> Reply::Sessions(Vec<SessionEntry>)
    Myself,                                     // -> Reply::Entry(SessionEntry)  (sibling-to-sibling)
    Switch { to: SessionId },                   // relayed as Attach { Primary }; then the attach exchange
    New { name: Option<Name> },                 // -> Reply::Entry, then (sibling) the attach exchange
    Kill { id: SessionId },                     // -> Reply::Done
    Rename { id: SessionId, name: Option<Name> }, // -> Reply::Entry
}
enum Reply {
    Sessions(Vec<SessionEntry>),
    Entry(SessionEntry),
    Done,
    Refused(String),                            // any request; the reason is for the user
}
struct SessionEntry {
    id: SessionId, name: Option<Name>, shell: String, created_unix: u64,
    size: TermSize, detachable: bool, attached: Attached, this: bool,
}
enum Attached { No, Here, Elsewhere, Unknown, OtherVersion }
```

- **Every request has exactly one reply before anything else on the stream.**
  For `Switch` and a sibling `New` that reply is the first line of the attach
  exchange (`HostHello`) or a `Refused`; a lobby `New` replies
  `Entry` (the lobby, now a session, same id) and the stream ends.
- `proto` lets a door refuse a request from another version with a reason
  instead of misreading it; an asker that gets an attach-exchange line or a
  version refusal back from `Myself` lists that sibling as `OtherVersion`.
- `SessionId` and `Name` are validated newtypes with private fields; parsing
  is the only way to make one. (Today's `SessionId(pub [u8; 16])` loses its
  `pub`.)
- `Sessions` is answered by reading the registry and asking each live entry
  `Myself` over its socket, each bounded by a short timeout; a sibling that
  does not answer is listed from its `meta.json` as `Unknown` (shown `?`).
  `this` and `Here` are for the asker's own entry.
- **Names never travel as references.** The client resolves a name to an id
  from the list it already has; the wire carries ids only.
- `StandbyRequest` and `Probe` leave `Signal` and become `Request` variants.
  `Signal` is purely the attach exchange.

### 4.2 The ssh offer

`Signal::Sessions { list: Vec<OfferEntry> }`, where `OfferEntry` is
`SessionEntry` without `attached` and `this`: the offer is built by
`run_host_connect` with blocking I/O before its fork, where no runtime may
exist, so it reads `meta.json` only and never asks a sibling. The lobby's
`Sessions` fills in the rest once the selector opens.
`Choice = Attach { id: SessionId } | New { name: Option<Name> } | Lobby`.

### 4.3 Close reasons

New: `MOVED_AWAY` (the client moved to another session; the session detaches
quietly). `SWITCHED` already exists and keeps its meaning (a primary replaced
by its own client's standby). `TAKEN_OVER` and `SHELL_EXITED` are
unchanged.

---

## 5. Code layout

- **Task 1** splits `HostSession` out of `src/session.rs` (~10.3k lines)
  before anything grows it; further cuts only where D1 touches.
- `src/door.rs` — the one `Open` dispatcher both doors call (Sessions,
  Myself, Switch relay, New, Kill, Rename).
- The lobby — a state of the host session, not a second process type.
- `src/listener.rs` — accept loop split: a task per connection reads `Open`;
  only `Attach` enters the serial exchange loop.
- `crates/oxutrm-host/src/registry.rs` — atomic `meta.json` writes;
  `names.lock`.
- The binary path for siblings is resolved once at startup and passed in.
- `src/selector.rs` — the selector's pure state and key handling; drawn from
  `src/view.rs` like the popup and config screen.
- `crates/oxutrm-proto` — `Open`, `Request`, `Reply`, `SessionEntry`,
  `OfferEntry`, `SessionId`, `Name`; the reshaped `Signal`/`Choice`.
- `crates/oxutrm-host/src/registry.rs` — `name` in `SessionMeta`; name column
  in `--list`.
- Every module that runs while a session owns the screen carries
  `#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]`.
- Root-crate constraints unchanged (`forbid(unsafe_code)`, no unused `pub`,
  paths passed in).

---

## 6. Testing

- **Pure:** selector state and keys (navigation, confirmations, in-place
  rename, `q` back vs quit, greyed states), `choose::decide`'s table, `Name`
  and `SessionId` validation, round trips of `Open`, `SessionEntry`, `Choice`.
  UI fixtures use realistic names, ids, shells and sizes — tiny dummy values
  hid a cut-off column in C.
- **Snapshots:** the selector at connect time, from the popup, with an
  `in use` row, with an error line, at 72×24 and at a narrow terminal.
- **Loopback integration:**
  - switch between two sessions; the old one stays detached and listed;
  - switch to a dead target: still in the old session, reason shown;
  - switch to an `in use` session: confirm, the other client gets `TAKEN_OVER`;
  - new from a live session and from a lobby (named);
  - kill a sibling; kill self into a lobby; kill the last session (still in
    the lobby); kill a shell that ignores SIGHUP (escalates, then `Done`);
  - `Myself`/`Kill`/`Rename` answered promptly while the sibling is
    mid-attach; a `Sessions` fetch does not cancel a sibling's standby
    search;
  - a vanished client's lobby exits after `DETACH_AFTER`;
  - a rebuild after kill-self lands in a fresh lobby;
  - rename, a rename to a taken name refused, an all-hex name refused;
  - new shells start in `$HOME` as login shells;
  - bare connect with one session lands in a lobby;
  - a lobby's link lost: rebuild lands in a fresh lobby;
  - `--attach <name>` and `--attach <prefix>`.
- **Hand test** on thinlinc and the Mac: the version bump means every old host
  session must be ended first (an old host refuses the new client). Push
  before building on thinlinc.
- Changelog entry; README and `docs/` updated for `--name`, `--attach <name>`
  and the new connect behaviour.

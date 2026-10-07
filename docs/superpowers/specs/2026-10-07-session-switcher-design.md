# oxutrm — Session switcher

Status: draft 2026-10-07, design agreed with the user in chat (four sections).
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
| Killing the session you are in | Allowed. Its process stays as a **lobby** feeding the selector; the client ends only when no session is left, or when you quit. |
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
- Sessions on rung 4 (ssh tunnel): they cannot be switched from or to (§3.6).

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

A lobby ends when its link closes for any reason — a switch away, `q`, or an
outage. It never waits for a client to come back.

A lobby that receives `New` starts its shell, registers and **becomes** the
new session: no second attach, no second ICE exchange.

### 2.2 Doors

A session process has two doors: its QUIC **control stream** (from its
client) and its Unix **socket** (from a sibling session, or from
`oxutrm host --attach` over ssh). Both doors read the same first line, an
`Open` (§4.1), and dispatch it in one place. A request is always served by the
process that owns what it touches: a session only ever signals its own shell
and writes its own `meta.json`; `Kill` and `Rename` for a sibling are
forwarded to the sibling's socket.

### 2.3 Names

`Name`: 1–24 characters, printable, no leading or trailing whitespace, not
32 hex characters (it must never be mistakable for an id). Unique per host
among live sessions; uniqueness is checked by the session asked to take the
name, against a fresh registry read. Stored as `name` in `meta.json`
(`serde(default)`: an entry written by an older binary must keep parsing,
because a running host keeps its old binary).

---

## 3. Behaviour

### 3.1 Connect

`choose::decide` keeps its role as the one pure table:

| Flags | Sessions offered | Choice |
|---|---|---|
| `--new [--name n]` | any | `New { name }` |
| `--attach x` | any | `Attach(Id)` if `x` is an id prefix (≥ 4 chars) of exactly one; `Attach(Name)` if it equals a name; else refused, listing names and ids |
| none | none | `New { name: None }` |
| none | one or more | `Lobby` |

`--name` without `--new` is refused. The line picker (`choose::pick`) is
removed: there is no case left that asks on the line.

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
  (detached). Non-detachable sessions are listed dimmed; choosing one is
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
  (outage or rebuild in progress) and on rung 4, each with its reason.
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
   the new close reason `SWITCHED`, drop the old link's parked standby, reset
   the renderer to the new session's screen, and point its rebuild path at the
   new session id. The selector closes; a popup entry records
   `switched to build`.
4. The old session is detached, as after any client loss. A lobby exits.
5. **Failure** (target refuses, ICE fails, target gone, old link drops
   mid-switch): the client is still in its old session; the selector shows
   the reason. If the old link dropped, ordinary recovery restores it. There
   is no resume of a half-done switch.
6. An `in use` target is taken over: its other client receives `TAKEN_OVER`,
   as with `--attach` today.

### 3.4 New, kill, rename

- **New from a lobby:** the lobby starts its shell, registers (with the name,
  if given) and becomes the session (§2.1). The client resets its screen and
  rebuild target as after a switch.
- **New from a session:** the session spawns a sibling — its own executable
  (`current_exe`, so the same version) as a detached `oxutrm host` process
  that starts the same login shell in `$HOME` a first connect gets, registers,
  and listens on its socket — then proceeds exactly as `Switch` to it. One
  code path makes a session process, whoever asks.
- **Kill a sibling:** forwarded to its socket; it SIGHUPs its shell's process
  group and ends the ordinary shell-exited way (its own client, if any, sees
  the shell exit). The list is refetched.
- **Kill this session:** the session SIGHUPs its shell's process group, marks
  the exit as *on request*, removes its registry entry and becomes a lobby.
  The selector stays open without the `this` row. If no session is left, the
  lobby says so and the client ends with `last session ended`.
- A shell exiting on its own takes today's `SHELL_EXITED` path; the client
  ends.
- **Rename:** forwarded to the owning session, which validates the name
  (§2.3) and rewrites its `meta.json`. An empty name clears it.

### 3.5 Recovery

- The rebuild path aims at the client's *current* session id, which a switch
  or a lobby `New` updates.
- A lobby's rebuild finds no lobby to return to (it ended with its link). The
  rebuild answers the offer with `Choice::Lobby` instead of refusing, so the
  client lands in a fresh lobby with the selector still open.
- A rebuild finding its session killed meanwhile (by another client) ends the
  client as today.

### 3.6 Rung 4

A rung-4 link tunnels QUIC through the ssh that created the session; nothing
else on the host is reachable without a UDP path. The selector is greyed out
on rung 4. An attach relayed over a socket never nominates rung 4 (there is no
ssh to tunnel through), as with `host --attach` today.

---

## 4. Wire

`PROTO_VERSION` is bumped. No compatibility with the previous version.

### 4.1 `Open` — the first line of every door

```rust
enum Open {
    Attach { role: Role },                      // the attach exchange follows
    Probe { .. },                               // unchanged semantics
    Sessions,                                   // -> SessionList
    Myself,                                     // -> SessionEntry (sibling-to-sibling)
    Switch { to: SessionRef },                  // relayed as Attach { Primary }
    New { name: Option<Name> },
    Kill { id: SessionId },                     // -> Done | Refused(String)
    Rename { id: SessionId, name: Option<Name> },
}
enum SessionRef { Id(SessionId), Name(Name) }
struct SessionEntry {
    id: SessionId, name: Option<Name>, shell: String, created_unix: u64,
    size: TermSize, detachable: bool, attached: Attached, this: bool,
}
enum Attached { No, Here, Elsewhere }
```

- `SessionId` and `Name` are validated newtypes; parsing is the only way to
  make one.
- `Sessions` is answered by reading the registry and asking each live entry
  `Myself` over its socket (bounded by a short timeout per sibling; a sibling
  that does not answer is listed from its `meta.json` with `attached` unknown,
  shown as `?`). `this` and `Attached::Here` are for the asker's own entry.
- `StandbyRequest` and `Probe` leave `Signal` and become `Open` variants.
  `Signal` is purely the attach exchange.

### 4.2 The ssh offer

`Signal::Sessions { list: Vec<SessionEntry> }`;
`Choice = Attach(SessionRef) | New { name: Option<Name> } | Lobby`. The client
resolves prefixes in `choose::decide`; the host only ever receives an exact id
or an exact name.

### 4.3 Close reasons

New: `SWITCHED` (the client moved to another session; the session detaches
quietly). Existing `TAKEN_OVER` and `SHELL_EXITED` are unchanged.

---

## 5. Code layout

- **Task 1** splits `HostSession` out of `src/session.rs` (~10.3k lines)
  before anything grows it; further cuts only where D1 touches.
- `src/door.rs` — the one `Open` dispatcher both doors call (Sessions,
  Myself, Switch relay, New, Kill, Rename).
- The lobby — a state of the host session, not a second process type.
- `src/selector.rs` — the selector's pure state and key handling; drawn from
  `src/view.rs` like the popup and config screen.
- `crates/oxutrm-proto` — `Open`, `SessionEntry`, `SessionId`, `Name`; the
  reshaped `Signal`/`Choice`.
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
  - kill a sibling; kill self into a lobby; kill the last session;
  - rename, and a rename to a taken name refused;
  - bare connect with one session lands in a lobby;
  - a lobby's link lost: rebuild lands in a fresh lobby;
  - `--attach <name>` and `--attach <prefix>`.
- **Hand test** on thinlinc and the Mac: the version bump means every old host
  session must be ended first (an old host refuses the new client). Push
  before building on thinlinc.
- Changelog entry; README and `docs/` updated for `--name`, `--attach <name>`
  and the new connect behaviour.

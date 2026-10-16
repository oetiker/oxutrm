# oxutrm — OSC 52 copy and OSC 8 hyperlinks

Status: draft 2026-10-16. All design decisions delegated to the controller by
the owner ("just work on it"); each one is recorded in §1.1. Items the owner
may want to change are listed in §9 with the default this spec uses.

Two features, one spec, **two independent PRs** (§2): OSC 52 first, then
OSC 8. Both bump the protocol to the next `PROTO_VERSION` (D2 also bumps it;
the plan rebases onto whatever `main` holds).

---

## 1. Purpose

A remote program can do two things today that never reach the user:

1. **Copy to the clipboard** (OSC 52). vim with `set clipboard=unnamedplus`
   and an OSC 52 provider, tmux with `set-clipboard on`, or plain
   `printf '\e]52;c;%s\a' "$(printf hello | base64)"`. The host's emulator
   already decodes the request (`Signals::clipboard`,
   `crates/oxutrm-term/src/listener.rs`), and `HostTerm::poll`
   (`crates/oxutrm-term/src/host.rs`) never reads that field, so the copy is
   dropped.
2. **Print a hyperlink** (OSC 8). `ls --hyperlink=auto`, `gcc`
   diagnostics, `systemctl`, `delta`, `eza`. The emulator stores the link on
   every cell it covers, but the wire `Cell`
   (`crates/oxutrm-proto/src/cell.rs`) has no field for it, so the text
   arrives as plain text.

Over plain `ssh` both work, because the escape sequences go straight to the
local terminal. oxutrm re-renders the screen, so it has to carry both
explicitly.

**Success:**

- In a session, `printf '\e]52;c;%s\a' "$(printf hello | base64)"` puts
  `hello` on the local clipboard (subject to the local terminal's own OSC 52
  policy), exactly once, also when the datagram that first carried it was
  lost.
- `ls --hyperlink=auto` in a session shows clickable file names in a terminal
  that supports OSC 8, and a link never lets the host write any escape
  sequence other than one well-formed OSC 8.

### 1.1 Decided

All rows: (controller, 2026-10-16).

| # | Question | Decision | Reason |
|---|---|---|---|
| 1 | One plan or two | One spec, two plans, two PRs: OSC 52 first, then OSC 8. | They share no code; OSC 52 is smaller and ships value alone. |
| 2 | How OSC 52 travels | A field in `ScreenState`, not a separate stream message (§3.2). | The state already survives loss, reattach and ring misses; a new message type would need its own reliability, demux and replay rules. mosh 1.4 made the same choice. |
| 3 | Delivery guarantee | At most once per client process and session; repeated by ordinary diffs until acked; latest copy wins. | A clipboard holds one value; an older copy that was superseded before delivery is worth nothing. |
| 4 | Replay on attach | A client never writes a copy that was made before its first state of that session; a client that reattaches in the same process receives copies it has not seen yet. | A new client must not overwrite the local clipboard with stale remote content; a client that lost its link for 30 s must still get the copy the user made in that time. |
| 5 | Text kept after delivery | The host drops the copy's text once the client has acked a state that carries it (§3.4). | Keeps every later full state small and stops stale clipboard content from going to later clients. |
| 6 | Size cap | `MAX_CLIPBOARD` = 256 KiB of UTF-8 text. A larger copy is dropped whole on the host, never truncated. | Big enough for a vim yank of a large source file; after zstd about the size of one truecolor full state. A truncated paste is silent data corruption. |
| 7 | Read requests (`OSC 52 ; c ; ?`) | Never answered. The host emulator is set to `Osc52::OnlyCopy`. | Answering would send the user's local clipboard to the remote host. |
| 8 | Selection targets | Two: `Clipboard` (`c`) and `Selection` (`p`, `s`). The client writes `c` or `p`. | These are the two the emulator distinguishes; `p` is the one local terminals implement for the X primary selection. |
| 9 | OSC 52 default | On, setting `terminal.clipboard`, takes effect now. | Matches plain ssh; the local terminal stays the policy point (it may still ask or refuse). |
| 10 | Popup, config screen, selector open | The clipboard is written anyway. | It changes nothing on the screen, and holding it would delay a copy the user just made. |
| 11 | Session switching | The client remembers the last written serial per session id. | Each session has its own counter; switching back must not replay. |
| 12 | Hyperlink representation | `Cell.link: u16` (0 = none) plus a per-state table `links: BTreeMap<u16, Arc<str>>`; the diff carries table changes as upserts and removals (§4.2). | One varint byte per unlinked cell; a URI travels once per state, not once per cell. |
| 13 | Link id allocation | Host interns each emulator link to the lowest free id; ids of links no longer on screen are freed at snapshot time. | Stable ids keep diffs small; freeing keeps the table bounded. |
| 14 | Link table cap | `MAX_LINKS` = 512 entries per state. On the host, a link that does not fit is dropped (its text stays). | Far above any real screen; bounds a state's table to about 1 MiB. |
| 15 | URI cap and characters | `MAX_LINK_URI` = 2083 bytes, every byte in `0x20..=0x7E`. On the host, a link with any other URI is dropped. | 2083 is VTE's limit, the most common local terminal's; the OSC 8 spec allows only bytes 32–126 in a URI, and that set can contain no ESC, BEL or C1. |
| 16 | The remote `id=` parameter | Not carried. The client emits `id=<wire id>`. | The host's interning already preserves link identity; the remote string is unbounded and would need its own validation. |
| 17 | Remote `file://` links | Passed through unchanged. | The OSC 8 spec puts the host name in `file://` URIs for exactly this case; the local terminal compares it with its own host name. |
| 18 | Links in scrollback | Dropped (`link = 0`) in rows fetched with `ScrollbackReq`. | Scrollback rows have no link table; v1 keeps them plain. |
| 19 | Links in overlays and the D2 preview | Always stripped: the renderer clears `link` on every overlay cell. | An overlay cell copied from another session's state would resolve against the wrong table and point at the wrong URL. |
| 20 | OSC 8 default | On, setting `terminal.hyperlinks`, takes effect now; also off when `$TERM` is `dumb`. | Terminals without OSC 8 ignore the sequence; the setting is the escape hatch for one that does not. |
| 21 | String terminator | BEL for both OSC 52 and OSC 8. | Same as the existing OSC 0 title; every terminal that implements either feature accepts BEL. |
| 22 | Hostile values | Rejected, never repaired: a frame that breaks a new invariant is refused like any I8 violation. | Same rule as `text.rs`: a refusal is visible in a log, a silent repair is visible nowhere. |

### 1.2 Not in scope

- Paste from the local clipboard to the remote (OSC 52 read). See row 7.
- Telling anyone about a dropped over-cap copy. It is dropped silently
  (§10).
- Rewriting `file://` links (to `sftp://` or similar).
- Links in scrollback rows (row 18).
- Any OSC 8 capability detection beyond `$TERM`.

---

## 2. Delivery: two PRs

**PR 1 — OSC 52** (§3): `ScreenState.clipboard`, its diff field and
invariants, host capture and ack-driven clearing, the client's
`ClipboardRelay`, the `terminal.clipboard` setting, CHANGES entry.

**PR 2 — OSC 8** (§4): `Cell.link`, `ScreenState.links`, the diff's link
table changes, I10, host interning, renderer emission, the
`terminal.hyperlinks` setting, CHANGES entry.

Each PR bumps `PROTO_VERSION` once. They touch the same structs
(`ScreenState`, `ScreenDiff`) but different fields, so the second rebases on
the first without design changes. Neither depends on D2 code; §4.8 names the
one interface point.

---

## 3. OSC 52 copy

### 3.1 What the emulator gives us

`alacritty_terminal` parses `OSC 52 ; <targets> ; <base64>`, takes the first
target byte (`c` → `ClipboardType::Clipboard`, `p` or `s` →
`ClipboardType::Selection`, anything else is ignored), base64-decodes the
data and drops it unless it is valid UTF-8. The result arrives as
`Event::ClipboardStore(kind, text)` and `EventSink` pushes it to
`Signals::clipboard`. An empty data field arrives as an empty string; it is
forwarded like any other copy (it clears the local clipboard on terminals
that implement that).

Read requests (`?`) arrive as `Event::ClipboardLoad`, which `EventSink`
already ignores. `HostTerm::spawn_with` sets `Osc52::CopyPaste`; it changes
to `Osc52::OnlyCopy`, so the emulator refuses reads itself and the comment
that justified `CopyPaste` goes.

### 3.2 Wire

In `oxutrm-proto`:

```rust
pub enum ClipboardTarget { Clipboard, Selection }

pub struct ClipboardCopy {
    pub target: ClipboardTarget,
    pub text: Arc<str>,
}

pub struct Clipboard {
    /// How many copies this session has made. Monotonic, like `bell`.
    pub serial: u32,
    /// The latest copy, until the client has acknowledged a state that
    /// carries it (§3.4). `None` before the first copy and after delivery.
    pub copy: Option<ClipboardCopy>,
}
```

`ScreenState` gets `pub clipboard: Clipboard` (appended; `serial: 0,
copy: None` in `ScreenState::blank`). `Arc<str>` because the sender keeps a
ring of 32 states and every clone would otherwise copy up to 256 KiB; this
needs serde's `rc` feature in the workspace.

`ScreenDiff` gets `pub clipboard: Option<Clipboard>`: `Some` when the field
differs from the base (`diff_from`), always `Some` in `full_diff`. The diff
compares the whole `Clipboard`; `Arc<str>` equality checks the pointer
before the bytes, so an unchanged copy costs nothing to compare.

No `Cell` changes. The text never enters a cell, so the I8 rule that keeps
OSC 52 out of cells (`crates/oxutrm-proto/src/text.rs`) stays as it is.

### 3.3 Invariants

- **I8, extended:** `copy.text.len() <= MAX_CLIPBOARD`. Checked in
  `ScreenState::validate`, and in `ScreenState::apply` **before anything is
  applied**, exactly as the title is. Control characters are allowed: the
  client base64-encodes the text, so it cannot reach the terminal as
  control. New `TextField::Clipboard` for the error.
- **I9 (transition):** `clipboard.serial` never goes backwards
  (`ApplyError::ClipboardWentBackwards`), checked in `validate_transition`
  beside I5 and I6.

### 3.4 Host

`HostTerm` keeps `clipboard: Clipboard`. In `poll`, for each entry of
`signals.clipboard`, in order:

- text longer than `MAX_CLIPBOARD`: dropped silently, the serial does not
  move (§10 says why nothing is printed).
- otherwise: `serial += 1` (saturating), `copy = Some(...)`, and `poll`
  reports a change so a frame goes out.

Several copies in one poll leave only the last one in the state; the serial
still counts all of them. `snapshot` copies `clipboard` into the state
(an `Arc` clone).

**Ack-driven clearing.** `HostTerm::clipboard_delivered(serial)` sets
`copy = None` when `serial` is still the current one. The host session's
sender loop calls it when the client's ack reaches a state whose
`clipboard.copy` is `Some` with that serial: `Sender::acked()` returns the
acknowledged state while the ring holds it, `HostSession::turn_at` checks it
right after taking inbound frames, and sets `screen_stale` so the text-less
snapshot goes out even on a quiet pty. From then on, states carry
the serial and no text. Consequences:

- A copy is repeated by every diff until the client acks it. A lost
  datagram costs one pacing interval, as for any other change.
- A copy made while no client is attached, or one the client never acked,
  stays in the state and is offered to the next client; the client-side rule
  in §3.5 decides whether it is written.
- A later full state (reattach, ring miss) carries no stale text once the
  copy was delivered.

### 3.5 Client

New `ClipboardRelay` in `oxutrm-client`, outside the renderer (the renderer
also paints things that must never write the clipboard, §4.8):

```rust
impl ClipboardRelay {
    pub fn new(osc52: bool) -> ClipboardRelay;
    /// Bytes to write to the terminal for this newly applied state, if any.
    pub fn on_state(&mut self, session: Option<SessionId>, c: &Clipboard, enabled: bool) -> Option<Vec<u8>>;
}
```

`session` is `None` for a session with no identity (test fixtures).

It is built with the terminal's `caps.osc52` and holds
`HashMap<SessionId, u32>`: the last serial seen per session, for the life of
the client process.

- First state of a session in this process: remember `c.serial`, write
  nothing. (Row 4: a copy older than this client is stale.) A consequence:
  a copy made before the client's first state of a session arrives, such as
  one in the first milliseconds of a brand-new session, is not written.
- `c.serial > remembered`: remember it; if `c.copy` is `Some` and `enabled`
  and `caps.osc52`, return
  `ESC ] 52 ; <c|p> ; <base64(text)> BEL`.
- Otherwise nothing.

`enabled = false` still advances the remembered serial, so turning the
setting on later does not replay an old copy.

The client session calls `on_state` each time `Receiver::on_frame` applies
a new state, and writes the bytes straight to the terminal, before the next
render. This happens whether or not the popup, config screen or selector is
open (row 10). A write error is handled like the render write error next
to it.

A session switch changes the session id passed in; the map makes switching
back free of replays (row 11).

---

## 4. OSC 8 hyperlinks

### 4.1 What the emulator gives us

`alacritty_terminal` 0.26 parses `OSC 8 ; params ; URI`, takes `id=` from
params, and sets the link on the cursor template, so every cell printed
afterwards carries it: `VteCell::hyperlink() -> Option<Hyperlink>` with
`id()` and `uri()`. A link without an explicit `id=` gets a generated unique
id (`"<n>_alacritty"`), so two prints of one URI without `id=` are two
links, as the OSC 8 spec requires. `Hyperlink` is `Eq + Hash` over
`(id, uri)`. `crates/oxutrm-term/src/blink.rs` already forwards
`set_hyperlink` to the term.

### 4.2 Wire

- `Cell` gets `pub link: u16` (appended; 0 = no link; `Cell::blank()` has 0).
  postcard writes it as one byte for 0..=127.
- `ScreenState` gets `pub links: BTreeMap<u16, Arc<str>>`: wire id → URI.
- `ScreenDiff` gets `pub links: Vec<(u16, Option<Arc<str>>)>`: `Some` sets
  or replaces an entry, `None` removes it. `diff_from` emits only entries that
  differ from the base; `full_diff` emits every entry, and `apply` with base 0
  clears the table first.

### 4.3 Invariants

**I10 — a link is a link, not control, and it exists.** Checked in
`ScreenState::validate`:

- `links.len() <= MAX_LINKS`;
- no key is 0;
- every URI is 1..=`MAX_LINK_URI` bytes, each in `0x20..=0x7E`
  (`ApplyError::BadLinkUri`, `TextTooLong { field: TextField::LinkUri }`);
- every cell's `link` is 0 or a key of `links` (`ApplyError::UnknownLink`).

In `ScreenState::apply`, the URI and count checks on the diff's link entries
run **before** anything is applied, as the title check does; the
"every cell's link exists" check runs on the result through `validate`.
Length is tested before the byte scan (the I8 ordering rule).

### 4.4 Host

`HostTerm` keeps a `LinkTable`: `HashMap<Hyperlink, u16>` plus the reverse
map. `screen_state_of` (which needs `&mut` access to it, or the table is
passed in) does for each visible cell:

1. no hyperlink → `link = 0`;
2. URI fails the I10 byte or length rule → `link = 0` (the text stays; row 15);
3. already interned → its id;
4. table has fewer than `MAX_LINKS` entries → intern at the lowest free id;
5. otherwise → `link = 0` (row 14).

After the pass, entries no cell referenced are freed, and `links` in the
state is exactly the referenced set. A `WIDE_CONT` cell takes the same link
as its lead cell.

`HostTerm::scrollback` writes `link = 0` in every cell (row 18).

### 4.5 Renderer

The renderer's output is the only place OSC 8 is written.

**Opening and closing.** `write_cells` tracks the link currently open on the
terminal (`open: u16`, 0 = none), like the SGR pen. Before each cell's text:

- if `cell.link != open`: when `open != 0` emit the close
  `ESC ] 8 ; ; BEL`; when `cell.link != 0` emit the open
  `ESC ] 8 ; id=<link> ; <uri> BEL` with `uri = links[cell.link]`;
  set `open = cell.link`.

A link is closed (`open != 0` → close, `open = 0`):

- at the end of every changed run, before the next cursor move, so a link
  never spans a jump;
- at the end of the pass;
- consequently at every right screen edge, since a run never crosses a row.

A link that covers several runs or rows is emitted as several open/close
pairs with the same `id=`, so the local terminal treats the pieces as one link
(hover highlights all of them). `WIDE_CONT` cells are skipped as today; the
lead cell's link covers both columns.

**Repaint.** A full repaint starts with a close before `ESC[H ESC[2J`, so a
link left open by an interrupted earlier write cannot attach to the repaint.
`RawGuard`'s restore on exit also writes a close.

**What counts as changed.** `Painted` keeps the link table it painted
(`BTreeMap` of `Arc<str>`, cheap to clone). A cell counts as changed when the
cell differs **or** its link id is nonzero and the URI it resolves to differs
between the painted table and the new one. Comparing ids alone would miss an
id that was freed and reused for another URI between two painted states.

**Overlays** (row 19): `composite` sets `link = 0` on every overlay cell it
stamps. Overlay content therefore never carries a link and closes any link
beneath it.

**Off.** With `terminal.hyperlinks = false`, or `caps.osc8 == false`, no OSC 8
is written and cells are painted as plain text. Changing the setting
invalidates the renderer, so the next render repaints with or without links.

### 4.6 Remote `file://` links

Passed through unchanged (row 17). `ls --hyperlink` writes
`file://<remote-hostname>/path`; a local terminal that follows the OSC 8 spec
sees the foreign host name and refuses or warns. Nothing local is opened by
accident.

### 4.7 Capability

`TerminalCaps` gets `osc8: bool`, set as `osc52` is (`term != "dumb"` in
`crates/oxutrm-term/src/caps.rs`).

### 4.8 Interface with D2 (session preview)

D2 (feat/d2-preview) shows a cell-for-cell crop of another session's
`ScreenState` in the selector. Its cells carry link ids from **that**
session's table. If the preview is painted as an overlay, row 19 already
strips them and D2 needs nothing. If D2 paints through another path, it must
set `link = 0` on the cells it shows and must never pass preview states to
`ClipboardRelay`. The plans for D2 and this PR note this contract; neither
depends on the other's code.

---

## 5. Settings

Two rows in the client's settings table (`src/config.rs`), a new section
`terminal`; `docs/config.md` is regenerated from the table.

| Key | Kind | Default | Takes effect | Description |
|---|---|---|---|---|
| `terminal.clipboard` | bool | true | now | let remote programs copy to the local clipboard (OSC 52) |
| `terminal.hyperlinks` | bool | true | now | show remote hyperlinks as links (OSC 8) |

Host side: none. The host always sends the clipboard and links; the client
decides what to write.

---

## 6. Changelog

PR 1, for CHANGES.md:

- Text a remote program copies to the clipboard (vim, tmux `set-clipboard`,
  OSC 52) now reaches the local clipboard; the `terminal.clipboard` setting
  turns it off. Reading the local clipboard from the remote is refused.

PR 2:

- Hyperlinks printed by remote programs (`ls --hyperlink`, compiler
  messages) are now clickable in terminals that support OSC 8; the
  `terminal.hyperlinks` setting turns them off.

---

## 7. Testing

Every test that drives a shell or an emulator with generated input runs under
the project's usual memory cap (`systemd-run --user --scope -p MemoryMax=2G`).

### 7.1 Unit — OSC 52 (PR 1)

- `oxutrm-proto`: a state whose copy is `MAX_CLIPBOARD` bytes validates; one
  byte more is `TextTooLong { field: Clipboard }`; text with ESC and BEL
  validates (it is base64'd on output); a serial that goes backwards fails
  `validate_transition`.
- `oxutrm-sync`: a diff carries `clipboard` only when serial or presence
  changed; `full_diff` always carries it; an over-cap copy in a diff is
  refused **before** any row is applied (pattern of
  `a_hostile_title_is_refused_before_any_row_is_painted`).
- `oxutrm-term`: `printf '\e]52;c;aGVsbG8=\a'` into a `HostTerm` gives
  serial 1 and text `hello`; two copies in one poll give serial 2 and the
  second text; an over-cap copy leaves the serial unchanged; `\e]52;c;?\a`
  produces no PTY reply; `clipboard_delivered(1)` clears the text, and
  `clipboard_delivered(0)` after a newer copy does not.
- `ClipboardRelay`: first state writes nothing; a higher serial writes
  exactly `\x1b]52;c;aGVsbG8=\x07`; the same serial again writes nothing;
  `Selection` writes `p`; `enabled = false` writes nothing and a later
  enable does not replay; two sessions keep separate serials; `osc52 = false`
  writes nothing.

### 7.2 Unit — OSC 8 (PR 2)

- I10: unknown link id, key 0, `MAX_LINKS + 1` entries, a URI of
  `MAX_LINK_URI + 1` bytes, and URIs containing `\x1b`, `\x07`, `\x9c`
  (as UTF-8) and `\n` are each refused; a link entry in a diff is refused
  before any row is applied.
- Sync: adding one link to a screen of 1000 unchanged cells produces a diff
  with one table upsert and only the linked cells; removing it produces one
  `None` entry; ids stay stable while unrelated links come and go.
- Host: `printf '\e]8;;http://a\e\\x\e]8;;\e\\'` gives one cell with a link
  and a one-entry table; the same URI printed twice without `id=` gives two
  ids; a URI with a control byte or non-ASCII gives `link = 0` and the text;
  link `MAX_LINKS + 1` gives `link = 0`; a link scrolled off frees its id;
  scrollback rows carry `link = 0`.

### 7.3 Renderer — OSC 8 run boundaries (PR 2)

Exact-bytes tests in `crates/oxutrm-client/src/renderer.rs`:

- one linked word: open, text, close, in that order, once;
- a link followed directly by another link: close then open between them;
- a link to the last column: closed before the next row's cursor move;
- a change inside a long link repaints only the changed cells, wrapped in
  their own open/close with the same `id=`;
- a full repaint begins with a close;
- same id, URI changed: the cells are repainted;
- an overlay over a link: no OSC 8 inside the overlay's cells;
- `hyperlinks` off: no `\x1b]8` anywhere in the output;
- a wide character inside a link: one open, the glyph, no output for the
  continuation cell.

### 7.4 End to end

Loopback session (`src/loopback.rs`: real shell, real round trip through
`Frame` bytes, real renderer):

- PR 1: the shell runs `printf '\e]52;c;aGVsbG8=\a'`; the client's output
  contains `\x1b]52;c;aGVsbG8=\x07` **exactly once**, over enough further
  ticks (with screen changes) to prove it is not repeated; then the test
  simulates a reattach in the same client (a full state of the same session)
  and checks nothing more is written.
- PR 1, loss: the first frame carrying the copy is dropped; the copy still
  arrives exactly once.
- PR 2: the shell prints a hyperlink; the client's output contains
  `\x1b]8;id=1;http://example.com/\x07` followed by the text and
  `\x1b]8;;\x07`, once; no other OSC appears.

---

## 8. Build order

PR 1: proto types and I8/I9 → sync diff field → host capture and
`OnlyCopy` → ack-driven clearing in the host session → `ClipboardRelay`
and the client session hook → setting → loopback test → CHANGES.

PR 2: proto `Cell.link`, `links`, I10 → sync table diff → host `LinkTable`
→ renderer emission and change detection → overlay stripping and repaint
close → caps and setting → loopback test → CHANGES.

---

## 9. Open for the owner

Each item works as decided above unless the owner says otherwise.

| Item | Default chosen |
|---|---|
| OSC 52 on by default | On (`terminal.clipboard = true`); the local terminal's own OSC 52 policy still applies. |
| Clipboard size cap | 256 KiB; larger copies dropped whole. |
| Telling the user about a dropped over-cap copy | Not done; the copy is dropped silently. |
| Delivering copies made while no client was attached | Not written by a client that attaches fresh (stale); written by a client that reattaches in the same process. |
| OSC 52 reads | Never answered. |
| OSC 8 on by default | On (`terminal.hyperlinks = true`). |
| Remote `file://` links | Passed through unchanged; the local terminal decides. |
| Links in scrollback rows | Dropped. |
| `MAX_LINKS` / `MAX_LINK_URI` | 512 entries / 2083 bytes. |

---

## 10. Judgement calls

Made while building PR 1 (OSC 52), all (controller, 2026-10-16). Where they
contradict an earlier section, that section has been corrected.

- The ack hook (§3.4) is `Sender::acked()` read in `HostSession::turn_at`.
- serde's `rc` feature is on workspace-wide, for `Arc<str>`.
- The diff compares the whole `Clipboard` (§3.2), not serial and presence.
- An over-cap copy is dropped silently (§3.4). The host daemon reopens its
  stderr on `/dev/null` when it severs from ssh, so a printed line reached no
  one, and `HostTerm` cannot print either: in `oxutrm loopback` its stderr is
  the painted terminal. The host has no log to write it to instead.
- `ClipboardRelay` takes `Option<SessionId>` and is built with
  `caps.osc52` (§3.5).
- A clipboard write error is handled like the render write error beside it
  (§3.5).
- The first-state baseline drops a copy made before the client's first state
  of a session (§3.5).
- `terminal.clipboard` adds a fourth section, `terminal`, to the config
  screen.
- `oxutrm loopback` does not write copies; it has no client session.
- The end-to-end tests let the client apply a first state before the shell
  copies (`read go`), because of the baseline rule above.
- For D2: a preview `window()` returns `Clipboard::default()` (clearing a
  copy is `state.clipboard = Clipboard::default()`, or `without_text()` to
  keep the serial), and only the client session's own applied states feed
  `ClipboardRelay` (§4.8 applies to the clipboard as well as to links).

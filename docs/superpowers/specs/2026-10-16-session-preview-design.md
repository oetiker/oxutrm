# oxutrm — Session preview in the selector

Status: implemented 2026-10-16; the deviations of the implementation are
folded in below and listed in §10. The owner delegated every design decision ("just
work on it"); the controller decided each one, listed in §1.1 with its reason.
What the owner may still want a say in is listed in §8, each with the default
this spec takes.
Sub-project **D2** of D. Builds on D1, the session switcher
(`docs/superpowers/specs/2026-10-07-session-switcher-design.md`, plan
`docs/superpowers/plans/2026-10-08-session-switcher.md`), whose rulings stand
unchanged: ssh only initiates; no wire compatibility, `PROTO_VERSION` is
bumped freely; the one full-screen selector at connect time and in the popup;
a request is served by the process that owns what it touches.

---

## 1. Purpose

The D1 selector lists a host's sessions by name, shell, start time and size.
That is often not enough to tell `a3f9c01e` from `logs`: what you remember
about a session is what was on its screen. D2 shows the selected session's
screen in the selector, under the list, while you move through it.

**Success:** with three sessions on a host, the user opens the selector on an
80×24 terminal, presses `↓` twice, and within one round trip sees the last
lines of `logs`' screen (its prompt and the output above it, in its colours)
under the list; on `+ new session` the pane says what `⏎` would start; on a
36-column terminal the selector looks exactly as in D1. Nothing the session
printed reaches the user's terminal as an escape sequence, and the session's
own client (if another one is attached) sees nothing of it.

### 1.1 Decided

All rows: *(controller, 2026-10-16)*.

| Question | Decision | Why |
|---|---|---|
| What the preview shows | A **crop** of the session's current screen, cell for cell, in its colours and attributes. Never scaled. | Scaling text by dropping rows and columns makes it unreadable; a crop of real cells is legible and costs nothing to render, because the overlay already is cells. |
| Which part of the screen | The **leftmost columns**, and the **rows ending at the cursor's row** while the cursor is visible; the **top rows** while it is hidden (§3.2). | In a shell the prompt is at the cursor, with the latest output above it; full-screen programs that hide the cursor (`htop`, `top`) keep their header at the top. |
| Where | Under the list, inside the same box, below a rule titled `preview`; the box then takes its full cap (`min(cols−4, 72)` × `min(rows−2, 24)`). | One box, no new overlay geometry; a fixed box size does not jump while the selection moves between sessions of different sizes. |
| Small terminals | No room, no preview: the pane needs 30 columns and 4 rows inside the box after everything D1 shows (§3.3); below that the selector is exactly D1's. | The list and the question are what the selector is for; the preview is the first thing to give way. |
| When it is fetched | When the selection **arrives** on a row, unless that row's shot is under 2 s old. One preview request in flight; arriving on another row meanwhile asks for that one when the answer is in. No debounce timer, no refresh while the selection rests. | "One in flight, then the latest" throttles held arrows by itself without a timer; a resting refresh would need a new wake source in the client loop for a feature whose purpose is recognition (§8 item 1). |
| Cache | Per session id, for as long as the selector stays open; a shot is shown at once on return, and refetched behind it when older than 2 s. Dropped on close, on a switch, and for ids gone from a refetched list. | Moving back and forth costs no request and no flicker. |
| The session you are in (`this`) | Cropped locally from the client's own screen, every lap, the same way: no request. | The client already holds that screen; it is live for free. |
| `+ new session` | The pane says `a new login shell in your home directory`. | There is no screen yet; the pane says what `⏎` does. |
| Lobby | A lobby has no entry, so it is never a row; a lobby's door forwards previews of siblings like any session's. A preview asked of a lobby's own id is refused. | Nothing new to decide: a lobby lists siblings and `+ new session` only. |
| `in use` | Previewed like any other, read-only: no question, no takeover, nothing told to the other client. | A snapshot changes nothing; the same user could take the session over anyway. |
| `?` (did not answer `Myself`) | Asked anyway; a timeout shows its reason in the pane. | It may answer now, and the wait is bounded. |
| `old version`, not detachable | Not asked; the pane says why (`another version of oxutrm: no preview`, `no preview: it dies with its ssh`). | An old binary cannot parse the request; a non-detachable session has no socket (`door::forward`). |
| Wire | `Request::Preview { id }` → `Reply::Preview(ScreenState)`, at most `PREVIEW_MAX` = 68×18. `PROTO_VERSION` 4. | Reusing `ScreenState` gives the reply I1–I3, I7 and I8 checks through the existing `validate()`. One fixed size keeps the request free of layout and the reply bounded. |
| How the target renders it | `LoopCmd::Preview` to its own loop, served by a `&self` method: a fresh `HostTerm::snapshot`, cropped. | `&self` makes it impossible for the handler to touch the sync sender, `screen_stale` or presence, so the attached client cannot see it. |
| Timeouts | Door to loop and door to sibling: `SIBLING_TIMEOUT` (2 s). Client: `PREVIEW_WITHIN` (5 s) for the whole request. | Every stream wait gets its own bound (QUIC idle timeout is off); a preview is as cheap as `Myself` and gets its bound. |
| Hostile screens | The client validates every received shot (`ScreenState::validate`, size ≤ `PREVIEW_MAX`) and rejects, never repairs; only cells are used: title, modes, bell are ignored. The renderer's existing `repair_wide_pairs` keeps a wide glyph at the pane edge from shifting the box. | Same rule and same code path as the main screen (`crates/oxutrm-proto/src/text.rs`, I8). |
| Failures | Shown in the pane, dimmed: why there is no preview. Not in the activity log, not in `client.log`. | Moving through a list is not an action; holding `↓` past a dead sibling must not flood the log. |
| Keys | None new. | The preview is automatic where there is room. |
| Splitting `session.rs` / `host_session.rs` first | **No split.** D2's code goes into new modules; it adds about 60 lines to `session.rs` and 30 to `host_session.rs` (§6). | A split of the areas D2 touches would move D1's switcher plumbing without making D2's diff smaller or clearer. |

### 1.2 Not in D2

- A live preview, refreshed while the selection rests (§8 item 1).
- Scrolling the preview, or a preview of scrollback.
- Previewing sessions of another host.
- A config key for the preview. There is nothing to configure that the
  terminal size does not already decide (§8 item 4).
- Splitting the 8k-line test module of `src/session.rs`. It is the bulk of
  that file (lines 2786–10964 of 10964) and a candidate for its own
  behaviour-preserving step, but D2 adds to it only one test (§7).

---

## 2. Concepts

A **shot** is one preview: a `ScreenState` of at most `PREVIEW_MAX`
(68 columns, 18 rows), cut from a session's screen by the **window** rule
(§3.2), or the reason there is none. 68 is `MAX_BOX.cols` less the box frame
(`FRAME_COLS`); 18 is `MAX_BOX.rows` less the border (2), key bar, rule,
preview rule and one list row: the largest pane the box can ever hold.

The **pane** is the part of the selector's box a shot is drawn in. Its size
follows the terminal and the list (§3.3), so a shot is windowed once more,
by the same rule, to the pane at layout time. The host never knows the pane's
size: the request carries none.

---

## 3. Behaviour

### 3.1 The selector with a preview

(Shortened: on an 80×24 terminal the pane has 13 rows.)

```
╭ sessions on thinlinc ─────────────────────────────────────────────╮
│   a3f9c01e  fish  Oct 05  80×24   in use                          │
│   build     bash  09:14   120×40  this                            │
│ ▸ logs      zsh   11:02   120×40                                  │
│   + new session                                                   │
├ preview ──────────────────────────────────────────────────────────┤
│ Oct 16 11:41:07 thinlinc sshd[4121]: Accepted publickey for oetike│
│ Oct 16 11:41:07 thinlinc sshd[4121]: pam_unix(sshd:session): sess │
│ Oct 16 11:42:13 thinlinc CRON[4188]: (root) CMD (command -v debian│
│ ^C                                                                │
│ oetiker@thinlinc:~$ journalctl -f -u ssh                          │
├───────────────────────────────────────────────────────────────────┤
│ ⏎ switch  n new  r rename  x kill  q back                         │
╰───────────────────────────────────────────────────────────────────╯
```

Top to bottom inside the box: the header (an outage, as in D1), the list, the
`preview` rule, the pane, the line under the list (question, note, refusal),
the plain rule, the key bar. The pane's cells are the shot's cells, top-left
aligned; a shot smaller than the pane leaves the rest blank. A **note** in
place of a shot (§3.5) is dimmed text, wrapped at the pane's width and
centred in the pane both ways.

### 3.2 The window rule

`ScreenState::window(&self, max: TermSize) -> ScreenState`, in
`crates/oxutrm-proto/src/screen.rs`, pure, used by the host to make a shot and
by the client to fit a shot to the pane and to preview its own session:

- `rows' = min(rows, max.rows)`, `cols' = min(cols, max.cols)`.
- `top = min((cursor.row + 1).saturating_sub(rows'), rows − rows')` when the
  cursor is visible, else `0`. The cursor's row is then always inside.
- Cells: rows `top .. top + rows'`, columns `0 .. cols'`. A wide glyph in
  column `cols' − 1` whose continuation was cut off becomes a space with its
  colours kept and no attributes, as `repair_wide_pairs` does it. The cut
  lead is found by its neighbour: the cell in column `cols'` of the source
  row carries `WIDE_CONT`. Unicode width is not consulted, so the rule
  agrees with the cells the emulator made, whatever width tables either
  side uses.
- Cursor: `row − top` (hidden: clamped to `rows' − 1`), column clamped to
  `cols' − 1`, `visible` and `shape` kept. A 0-sized result puts it at 0,0.
- `seq` 1, `title` empty, `bell` 0, `scrollback_len` 0, `modes` default.

**Invariant:** the window of a state that validates validates. Pinned by
tests over wide glyphs at the cut, hidden and visible cursors at the first
and last row, sessions smaller and larger than `max`, and 0×0.

### 3.3 Room

`layout_sessions` lays out exactly as D1 when `SessionsView::preview` is
`None`, or when there is no room. With a preview:

1. The box takes its cap: `box_cols = min(cols − 4, 72)`,
   `box_rows = min(rows − 2, 24)`.
2. Inside: the border, key bar and plain rule as D1; the header if any; the
   line under the list at its wrapped height; the list at
   `min(lines, 8)` rows (`LIST_ROWS_WITH_PREVIEW`), scrolling as D1 does; the
   `preview` rule; the pane gets what is left.
3. The pane is drawn only when it gets at least `PREVIEW_MIN` = 30 columns ×
   4 rows. Otherwise step 1 is undone and the box is D1's.

With three sessions and no header or line, that is: 80×24 → pane 68×13;
120×40 → 68×15; 40×24 → 32×13; 38×24 → 30×13; 37×24 → hidden (29
columns); 80×16 → 68×5; 80×15 → 68×4; 80×14 → hidden (3 rows). Below `MIN_BOX` the one-line form is unchanged.

The pane gives way before the list, the header and the question: a question
on a short screen keeps its row (D1 §3.2) and the pane loses its rows first.

### 3.4 Fetching

`src/preview.rs` holds the client's pure preview state (`Previews`); the
session calls it each lap, as it calls the selector:

- **Arrival.** When the selected row changes (or the selector opens), and the
  row is previewable — not `this`, not `+ new session`, detachable, not
  `OtherVersion` — and has no shot under `PREVIEW_FRESH` (2 s), it becomes
  the **wanted** id. Any other row replaces the wanted id with none: the
  latest selection wins, and a row left before it was asked is not asked.
  Nothing arrives while the list is still on its way; the pane is left out
  until then.
- **Asking.** On a live link, with no preview request out, the wanted id is
  asked: `Request::Preview { id }` on a fresh control stream, from a task the
  loop holds as `AbortOnDrop`, answered on a channel the loop's `select!`
  reads (a new `Wake::Previewed` arm, borrowing a local, C1). Preview
  requests have their own slot: they never make the selector `busy`, and a
  switch, kill or rename never waits for one.
- **Answer.** An answer is dropped unless its id is the request out. A shot
  is checked (§4.2) and cached under its id with the time; a refusal or
  failure is cached the same way, as its reason, and replaces an older
  shot of that id. If the selection moved meanwhile and the new row is
  wanted, it is asked next.
- **Not live.** Nothing is asked; cached shots still show; a row with none
  shows `the link is down`.
- **Close, switch.** The cache and any wanted id are dropped; the task in
  flight is aborted with its `AbortOnDrop`. A refetched list drops shots of
  ids it no longer lists. The session notices the close on the next lap
  (`preview_step` sees the mode is no longer `Sessions`), whatever closed
  it. A link swapped by a rebuild drops nothing: a request out on the old
  link fails and its reason shows until the row is asked again.
- **`this`.** The pane shows `self.screen_rx.state().window(PREVIEW_MAX)`, taken every
  lap the selector's view is built; the view comparison in `layer_one`
  repaints only when it changed.

### 3.5 What the pane says when there is no shot

All dimmed, wrapped at the pane's width and centred both ways, through
`legible`:

| Row | Pane |
|---|---|
| `+ new session` | `a new login shell in your home directory` |
| `old version` | `another version of oxutrm: no preview` |
| not detachable | `no preview: it dies with its ssh` |
| asked, no shot yet | `asking logs for its screen…` |
| refused or failed | the reason, `summarised` (e.g. `session 5d2e8a7c did not answer in time`) |
| a shot that failed the check | `logs sent a screen that is not valid` |
| link down, no shot | `the link is down` |

### 3.6 On the host

- **The door** (`src/door.rs`) gets one arm. `Preview { id }` for its own id
  asks its loop, `LoopCmd::Preview { reply }`, and waits up to
  `SIBLING_TIMEOUT`; a door with no loop behind it refuses with `this
  session cannot show its screen`. For any other id it forwards like
  `Kill`/`Rename` but under `SIBLING_TIMEOUT`, not `FORWARD_TIMEOUT` (which
  is sized for a kill's grace): `forward()` takes its bound as a
  parameter, and `Kill` and `Rename` pass `FORWARD_TIMEOUT`. The forwarding door refuses a reply larger than
  `PREVIEW_MAX` rather than pass it on. Both doors (control stream and
  socket) serve it, so a sibling's preview reaches a lobby and a session
  alike.
- **The loop** (`src/host_session.rs`) answers `LoopCmd::Preview` from
  `fn preview(&self) -> Result<ScreenState, String>`: in a lobby,
  `Err("this session has no shell")`, which is what a preview of a
  lobby's own id is refused with; else
  `term.snapshot(1).window(PREVIEW_MAX)`. A detached session's emulator is
  fed while nobody is attached (`DETACH_AFTER`'s doc), so the shot is
  current either way. The loop does not poll the pty for it.
- **While a kill runs** the loop takes no commands (`if killing.is_none()`),
  so a preview of a session being killed times out after 2 s with that
  reason. Accepted: the kill ends within 5 s and the list refetch removes the
  row.
- **Cost.** One snapshot of the whole screen and a copy of at most 1224
  cells, per request. The client has one in flight at a time; a local
  process flooding a socket with requests is the same user and bounded by
  the one-deep command channel and each request's timeout.

---

## 4. Wire

`PROTO_VERSION` becomes 4. No compatibility with 3: a sibling of version 3
is listed `old version` and not asked.

### 4.1 Types

```rust
pub const PREVIEW_MAX: TermSize = TermSize { cols: 68, rows: 18 };

enum Request {
    ...,
    Preview { id: SessionId },        // -> Reply::Preview, or Refused
}
enum Reply {
    ...,
    Preview(ScreenState),             // window(PREVIEW_MAX) of the session's screen
}
```

- One request, one reply, as every D1 request.
- **Size bound.** At most 68×18 = 1224 cells. A test builds the worst case
  every cell can be — 32 bytes of `"` (each escaped to two), RGB colours on
  both sides, every attribute — and asserts that `encode_line` of the reply
  stays under `MAX_SIGNAL_LINE` (1 MiB). `read_line_async` already refuses
  any longer line, so a hostile peer cannot make the client allocate more.

### 4.2 The client's check

`Previews::answered` keeps a shot only if `validate()` passes,
`rows ≤ 18`, `cols ≤ 68`, and every cell's text measures at most two
columns (§10 item 13); anything else is the note in §3.5, never a repaired
screen. Only `cells`, `rows`, `cols` and `cursor` are used; title, modes,
bell and scrollback are never applied to anything.

### 4.3 Why a hostile screen cannot reach the terminal

1. **I8** (`text.rs`): every cell's text is checked by `validate()` before
   the shot is kept, so no C0, DEL or C1 scalar survives. An OSC 52 the
   session printed was consumed by the emulator on the host; one forged into
   a cell is a control scalar and is refused.
2. The shot's cells are stamped into the overlay as `Cell`s; the renderer
   paints overlay cells exactly as remote cells, through `down_convert` and
   `write_cells`.
3. `repair_wide_pairs` runs over every composited row, so a wide glyph or an
   orphaned `WIDE_CONT` at the pane's edge becomes a space instead of
   shifting the box's border.
4. Nothing of the shot is interpolated into an escape sequence: no title, no
   mode is mirrored.

---

## 5. On the client

- `crates/oxutrm-client/src/popup.rs`: `pub enum PreviewPane { Screen(ScreenState), Note(String) }`
  and `SessionsView::preview: Option<PreviewPane>`. `layout_sessions`
  computes room (§3.3), draws the `preview` rule with the existing
  `rule(…, Some("preview"))`, draws a note with ratatui as dimmed text, and
  stamps a screen's `window(pane)` cells into the `Overlay` after
  `overlay_from_buffer`, at the pane's position. `preview: None` keeps every
  D1 snapshot unchanged.
- `src/view.rs`: `SessionsFacts` gains `preview: Option<PreviewPane>`, built by
  the session from `Previews::pane` and passed through.
- `src/preview.rs` (new): `Previews` — `arrive(selected, now)`,
  `ask_next(live) -> Option<SessionId>`, `answered(id, Result<ScreenState, String>, now)`,
  `keep(&[SessionEntry])`, `clear()`,
  `pane(selected, own: &ScreenState, live: bool) -> PreviewPane`. `live`
  picks `the link is down` over `asking …`; the session wraps the pane in
  `Some` once the list is in.
  Pure: no I/O, no clock of its own. Carries
  `#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]`.
- `src/switcher.rs`: `preview(conn, id) -> Result<ScreenState, String>`,
  on its own control stream, with one `PREVIEW_WITHIN` (5 s) bound over
  opening the stream, sending and reading the answer.
- `src/session.rs`: a `previews: Previews` field; on each lap, before the
  popup is laid out, `preview_step` (`arrive` with the selected row, then
  `ask_next`) and the spawn; the `Wake::Previewed` arm; `clear` on close
  and on `Landed`; `keep` on a refetched list. No other change.

---

## 6. Code layout, and the split question

| Where | What D2 adds |
|---|---|
| `crates/oxutrm-proto` `open.rs`, `screen.rs`, `lib.rs` | `Request::Preview`, `Reply::Preview`, `PREVIEW_MAX`, `ScreenState::window`, `PROTO_VERSION` 4 |
| `src/door.rs` | the `Preview` arm, own and forwarded (~40 lines) |
| `src/host_session.rs` | `LoopCmd::Preview`, `fn preview(&self)`, one `HostWake::Cmd` arm (~30 lines) |
| `src/preview.rs` (new) | `Previews` and its tests |
| `src/switcher.rs` | `preview()` (~25 lines) |
| `src/session.rs` | field, lap steps, wake arm (~60 lines) |
| `src/view.rs` | one field through `SessionsFacts` |
| `crates/oxutrm-client/src/popup.rs` | `PreviewPane`, room, pane drawing (~120 lines) |
| `docs/sessions.md`, `CHANGES.md` | a `Preview` section; one changelog entry |

**No preliminary split.** The handoff suggested splitting `session.rs`
(10,964 lines) and `host_session.rs` (1,420) before D2. Measured against
where D2 lands: in `host_session.rs` it is one enum variant, one `&self`
method and one match arm in a file D1 already split out for exactly this
kind of growth; in `session.rs` it is a field and about 60 lines next to
D1's switcher plumbing (`ask`/`answered`, lines ~705–900, and the spawn at
~2500), which already calls out to `src/switcher.rs` and `src/selector.rs`.
Every piece of D2 with logic of its own goes to a new or existing small
module (`preview.rs`, `screen.rs`, `door.rs`, `popup.rs`). Moving the
switcher plumbing into `src/session/switching.rs` would be a diff of its own
that D2 would then add 60 lines to instead of 60 lines in place; it is not
clearly easier, so it is not done. The one real weight in `session.rs`, its
8k-line test module, is outside D2 (§1.2).

Constraints that bind, unchanged: `#![forbid(unsafe_code)]`; `oxutrm-host`
does not depend on `oxutrm-net` (D2 adds nothing there); every stream wait
has its own bound; `select!` arms borrow locals; nothing is printed on the
client.

---

## 7. Testing

UI fixtures use realistic values (D1's rule): ids such as
`3ff1218f5e0c4b7d9a1c2e3f40516273`, names `build`/`logs`, shells
`/bin/bash`, sizes 120×40, and screens with real text — a prompt, log lines,
a CJK line — not single letters.

- **`ScreenState::window`** (proto, pure): the invariant in §3.2; the cursor
  row kept at the bottom; hidden cursor shows the top; a wide glyph cut at
  the right edge becomes a space with its colours; a screen smaller than
  `max` is copied whole; 0×0.
- **Size bound:** the worst-case reply encodes under `MAX_SIGNAL_LINE`;
  `Request::Preview` and `Reply::Preview` round-trip.
- **`Previews`** (pure): arrival asks once; a fresh shot is not re-asked;
  a stale one is shown and re-asked; one in flight, the latest selection
  asked next; not live asks nothing; `clear` and `keep`; `this`,
  `+ new session`, `old version`, not detachable never asked; a shot with a
  control scalar in a cell, or larger than `PREVIEW_MAX`, becomes the
  not-valid note.
- **Layout** (`popup.rs`, unit + `insta` snapshots): 80×24, 120×40, 40×24
  and 38×24 with the pane; 37×24 and 80×14 without it, equal to the D1 layout of the
  same view; 12 sessions at 80×24 (list scrolls at 8 rows, pane 9 rows);
  a question on 80×16 keeps its row and the pane shrinks; each note of §3.5;
  a session smaller than the pane (80×24 shown in 68×13 is cropped; a 40×10
  one is whole, top-left); a wide glyph in the pane's last column does not
  move the right border; a `Some(preview)` on a screen below `MIN_BOX` is
  the one line.
- **Door, loopback** (`src/serve.rs` tests, real session processes from
  `fixtures::process`, real door, real control stream):
  - a sibling whose shell prints a marker line, an OSC 52 and a C1 CSI and
    sleeps: `ask_over(me, Preview { id: LOGS })` returns a shot holding the
    marker on the cursor's row or above, no control scalar in any cell,
    at most 68×18;
  - the sibling's `Myself` answer is the same before and after (its
    presence untouched);
  - `Preview` of the asker's own id over its control stream;
  - `Preview` from a connect-time lobby's door reaches a sibling; `Preview`
    of the lobby's own id is refused;
  - an unknown id is refused with its short id;
  - a socket at a sibling's path that accepts and never answers is refused
    with `did not answer in time` within `SIBLING_TIMEOUT` plus slack.
- **Session wiring** (`src/session.rs`, one test): with the selector open and
  a fetched list, `↓` onto a sibling makes one preview request, its answer
  shows in the laid-out overlay, and closing the selector drops the cache.
  The test drives `preview_step` and `switcher::preview` against real
  session processes, as `ask_now` drives the switcher, rather than the
  whole loop.
- **Hand test** on thinlinc and the Mac: three sessions, one running
  `journalctl -f`, one `htop`, one with a CJK prompt; move through the
  selector at 80×24 and at 40×20; a narrow terminal shows no pane.
- **Docs:** a `Preview` section in `docs/sessions.md`; a changelog entry
  (§9). The version bump means old sessions must be ended first (already
  in `docs/sessions.md`'s "Versions").

---

## 8. Open for the owner

Each is decided as above; the owner may overrule.

1. **Snapshot, not live.** The preview is taken when the selection arrives
   (and refreshed on return after 2 s), not refreshed while it rests. A live
   preview (say once a second) needs a timer wake in the client loop and
   one request per second per open selector. Default: snapshot.
2. **Crop, anchored at the cursor.** Rows ending at the cursor, left
   columns; the top when the cursor is hidden. Alternatives: always the
   bottom rows; the rows around the cursor. Default: as decided.
3. **Colours kept.** The pane shows the session's colours and attributes.
   Alternative: plain text, or the whole pane dimmed so it reads as "not
   here yet". Default: colours kept.
4. **No way to turn it off.** There is no key or config key to hide the
   preview; it shows whenever there is room. Default: none.
5. **Previewing a session in use elsewhere** shows another client's screen
   without telling it. Same user and host, and taking it over is already
   possible. Default: allowed, silently.

---

## 9. Changelog entry (draft)

Under `### New`:

- **The session selector shows the selected session's screen.** Under the
  list, the last lines of the session's screen up to its cursor appear in
  its colours as you move through the list; `+ new session`, a session of
  another oxutrm version and one that cannot be reattached say why there is
  none. The preview needs a terminal of at least 38 columns and, with a
  few sessions, 15 rows, and is left out below that; this release changes
  the protocol version, so end host sessions started by an older oxutrm
  before connecting with this one.

---

## 10. Judgement calls

Made while implementing, where the spec above was silent or wrong; each is
folded into the section named.

1. **Notes wrap** (§3.1, §3.5). A reason such as a sibling's timeout is
   longer than a 30-column pane; one cut line would lose its end.
2. **Two refusals for "no screen"** (§3.6). A door with no loop and a
   lobby's loop are different cases and say so in different words.
3. **`forward()` takes its bound** (§3.6) instead of a second forwarding
   function for the shorter preview timeout.
4. **A cut wide glyph is found by its `WIDE_CONT` neighbour** (§3.2), so the
   window agrees with the emulator's cells rather than with a width table.
5. **`to_ask` is `ask_next`** (§5): clippy's `wrong_self_convention` refuses
   a `to_*` method that takes `&mut self`.
6. **`pane` takes `live` and returns a `PreviewPane`** (§5): the note for a
   row with no shot depends on the link, and the only time there is no
   pane, while the list loads, is the session's to decide.
7. **Failures are cached like shots** (§3.4), so holding `↓` over a dead
   sibling does not re-ask it on every pass within 2 s; a failure replaces
   an older shot, so the pane says what is true now.
8. **A non-previewable row clears the wanted id** (§3.4): moving on to
   `+ new session` while a request is out asks nothing more afterwards.
9. **No pane while the list loads** (§3.4): the cursor sits on
   `+ new session` then, and its note would describe a row nobody chose.
10. **A rebuild drops no previews** (§3.4): the request on the old link
    fails on its own within `PREVIEW_WITHIN` and the row is asked again on
    the next arrival.
11. **One bound for the whole client request** (§5): `PREVIEW_WITHIN` covers
    opening the stream, writing and reading, which bounds each of those
    waits too.
12. **A close is found by the mode, once a lap** (§3.4). The selector
    cannot close and open again between two laps: a read ends at the key
    that closes it (the rest of the read is dropped), and every read is
    followed by a lap, which then sees it closed. So no counter of
    openings is kept; a test pins the premise.
13. **A preview cell may measure at most two columns** (§4.2). I8 bounds a
    cell's bytes, not its width, and a forged cell of 32 letters would
    push the box's border out. A two-column glyph without its `WIDE_CONT`
    is not refused but left to `repair_wide_pairs`, as on the main screen,
    so a real screen from an emulator with another width table still
    shows.

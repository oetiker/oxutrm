# oxutrm — Status popup and activity log

Status: draft 2026-10-03; popup content and layout revised 2026-10-04 after
the hand test (§6, §5.1)
Builds on P1, the standby link
(`docs/superpowers/specs/2026-09-25-standby-and-rendezvous-design.md`).
Retires `crates/oxutrm-client/src/notice.rs` and the mid-session lines of P1
§3.7. Sub-project **B** of four: A (no diagnostics on the screen, merged),
**B (this)**, C (config file and config screen), D (session switcher).

---

## 1. Purpose

The P1 hand test (2026-10-03, Mac client to `thinlinc` over a split-tunnel VPN)
showed the failover working and the user learning almost nothing about it:

- The notice box says "no reply from host" and little else; it cannot say that
  a standby is being probed, that the session switched, or why a search failed.
- The P1 status lines ("standby: …", "switched to standby …") are written with
  `say_mid_session` and wiped by the full repaint that follows
  (`src/session.rs` ~:1130). They survive only in scrollback.
- Network diagnostics were written raw over the screen (fixed by A: they are
  now returned as reasons, and a clippy deny guards the in-session modules).

B gives the client one place where the state of the connection can be seen at
any time: a popup, opened by a key and opened by itself on connection loss,
showing connection quality and what oxutrm is doing to keep the session alive.
Every event it shows is also kept in a small, size-capped log file.

**Success:** during an outage like the hand test's, a glance at the screen tells
the user what oxutrm is trying, how long it has been at it, and that it
worked — and nothing is ever written over the shell.

### 1.1 Decided with the user

| Question | Decision |
|---|---|
| Ctrl-\ while the link is healthy | Opens the popup. A quick double press sends one literal Ctrl-\. The key becomes configurable (or off) in C. |
| Layout | A centred box, status on top, activity log below, key bar at the bottom. |
| When the link comes back | The popup lingers ~3 s showing the outcome, then closes — unless the user pressed a key in it. |
| Quality facts | Live RTT with min/avg/max over 60 s, loss over 60 s, an RTT sparkline, throughput. Revised 2026-10-04: the popup shows the RTT range (min–max) beside its sparkline; avg and the cumulative sent/lost counts are dropped. |
| Popup structure (2026-10-04, after the hand test found it "too much text, too little structure") | Named sections under rules drawn into the border, one fact per line, one log line per outage, local `HH:MM` times in the log (§6). |
| Activity log | In memory (last ~200) and in a file, capped at 2 MiB on disk. |
| Architecture | A view model plus a modal state machine, rendered into the existing `Overlay` (approach 1 of 3). |

### 1.2 Not in B

- The config file and config screen (C). B hard-codes the key and the
  timings; it leaves a greyed-out `c config` entry in the key bar.
- The session switcher (D). Greyed-out `s sessions` entry.
- atuin's "cursor position could not be read" during an outage: the remote
  asked the terminal for the cursor position and the answer was delayed by the
  outage. Not addressed here.

---

## 2. Units

| Unit | Crate | One job | Depends on |
|---|---|---|---|
| `popup.rs` | oxutrm-client | `layout_popup(&PopupView, TermSize) -> Overlay` — pure layout with ratatui | ratatui, `overlay` |
| `quality.rs` | oxutrm (src) | A 60-sample ring of link measurements and the numbers derived from it | nothing (fed plain values) |
| `activity.rs` | oxutrm (src) | The event log: in-memory ring plus size-capped file | std only |
| `ui.rs` | oxutrm (src) | The popup's modal state and key routing | `linkstate::Phase` |

`ClientSession` owns one of each (except `popup.rs`, which is a function),
builds a `PopupView` each lap, and feeds `quality` and `activity`. Each unit is
testable without a terminal or a network.

Constraints that bind every unit (unchanged from earlier work):

- **No new timer that fires while nothing is happening** (`19cc001`). Sampling,
  lingering and repaint decisions run on laps the client loop already takes
  (it re-arms its deadline at `now + pacing_interval()`, at most 100 ms).
- **Nothing on the in-session path writes to the terminal** except the
  renderer (A's clippy deny covers these modules; `ui.rs`, `quality.rs`,
  `activity.rs` get the same deny).
- **The loop's `select!` arms borrow locals, never `self`** (constraint C1).
- **Text that came from a remote side is escaped** before it is shown or
  written to the file (the existing `legible`/`summarised` helpers).

---

## 3. Behaviour and keys (`ui.rs`)

### 3.1 States

```
Closed ──Ctrl-\──▶ Open ──Esc / Ctrl-\──▶ Closed
Closed ──outage──▶ Auto ──link Live──▶ Lingering ──3 s──▶ Closed
                    │                    │
                    └──any key──▶ Open ◀─┘ (any key)
```

- `Closed`: every byte goes to the host while the link is `Live`; during an
  outage it is held (§3.2). A popup closed by hand during an outage is
  `Closed` with the outage marked dismissed, so it does not re-open itself
  until a new outage begins.
- `Open`: opened by hand. Stays until closed.
- `Auto`: opened because the phase became an outage (`Phase::is_outage`, i.e.
  after `SILENT_AFTER`, 2 s). An `Open` popup stays `Open` when an outage
  starts — nothing jumps.
- `Lingering`: the link is `Live` again after an `Auto` popup. Shows
  "● LIVE again   outage <N.N> s · <path> · up <age> · link <N>" and closes
  `LINGER` (3 s) after
  it entered this state. Any key other than a closing or quitting key turns it
  (or an `Auto` popup) into `Open`, which stays.
- `Confirming` is not a popup state: while the link phase is `Confirming`
  (held input waiting), the popup is forced open with the held section (§6)
  and cannot close until the user sends or drops the input. Then the normal
  rules resume.

### 3.2 Keys

- **Ctrl-\ while `Closed`** opens the popup (`Open`).
- **Double press:** a second Ctrl-\ that arrives within `DOUBLE_PRESS`
  (500 ms) of the one that opened the popup closes it and sends one literal
  `0x1c` to the host. The time is taken from the key reads' timestamps; no
  timer is armed. A Ctrl-\ after that window just closes the popup.
- **While the popup is shown, every key belongs to it** (user decision
  2026-10-04), whatever opened it and whatever the link is doing. Nothing
  typed into the popup is held or sent to the host.
  - `Esc` or Ctrl-\ closes it (outside the double-press window);
  - `q` quits the client;
  - `c` and `s` are shown greyed out and do nothing in B;
  - under `Confirming`, `s` sends and `d` drops the held input;
  - **answer guard:** under `Confirming` only, for `ANSWER_GUARD` (500 ms)
    after the question first appears (the first lap or read that saw
    `Confirming`), `s`, `d` and `q` do nothing, like every other key: keys
    already in flight when the question appears are somebody's typing, not
    an answer. Measured from the reads' timestamps; nothing is armed;
  - any other key does nothing.
- **Closing during an outage** is allowed. The popup then stays closed until
  that outage ends (it does not re-open itself for the same outage), and what
  the user types meanwhile is held exactly as today (`hold_keys`,
  `MAX_HELD`). Blind typing is thus a deliberate choice; when the link returns
  with held input, the popup opens in `Confirming` (§3.1).
- **Closed and the link `Live`:** every byte goes to the host, as today.
- **Esc ambiguity:** a lone `0x1b` byte in a read is Esc; `0x1b` followed by
  more bytes in the same read is an escape sequence (arrow key etc.) and is
  treated as "any other key" (and so does nothing while the popup is shown).

The existing `PREFIX` handling in `linkstate.rs` (`hold_keys`,
`prefix_pending`, `Command`) is folded into `ui.rs`; `hold_keys` keeps only
the holding of bytes. While the popup is closed, Ctrl-\ opens it in every
phase, so no other prefix commands remain.

---

## 4. Connection quality (`quality.rs`)

A ring of 60 samples, one per second at most, taken on a lap where at least
1 s has passed since the last sample. Each sample holds what the primary
link's quinn connection reports:

- `rtt()` (quinn's smoothed estimate),
- `stats().path.sent_packets`, `stats().path.lost_packets`,
- `stats().udp_tx.bytes`, `stats().udp_rx.bytes`,
- a `segment` number (which link of this session it came from),
- an `outage` flag (the phase was an outage when it was taken).

Derived, all pure functions over the ring:

| Shown | From |
|---|---|
| RTT now | the newest sample, or `—` for the whole outage |
| RTT min–max | non-outage samples of the current segment in the window |
| Sparkline | one cell per second; outage seconds are gaps |
| Loss | Δlost / Δsent over the current segment's samples in the window |
| Throughput | ↑ and ↓ bytes per second averaged over the last 5 s of the current segment |

**A link swap** (failover or a landed rebuild) starts a new segment: the new
connection's counters start at zero, so no delta is ever taken across two
connections. The header reads "<path> · up <age> · link <N> · mtu <N>"
(the MTU last: a header too wide for the box is cut at its end).
The MTU is the newest sample's `stats().path.current_mtu`, not
`PathDescription::mtu`: that one is read at attach, before path MTU
discovery has run, and is always the 1200 floor. A rise in the link's
`black_holes_detected` between two samples of one segment is logged as
"path MTU reduced to <N>"; a segment's first sample is only its baseline,
because a failover adopts a standby whose counters have been running since
its own handshake (loss and throughput, which only take deltas within a
segment, never noticed). Only the client's send direction is measured;
screen updates travel the host's direction, which the client cannot see.

**Standby section**, from new read-only accessors on `Standby`:

- the standby's path label (`rung_label`) and its own `rtt()`,
- "searching…" or "next search in <N> s" / "<m> m <s> s" (from
  `next_search`),
- after a failed search, `last search: no second path found`. The reason
  (which rung hit what) goes to client.log only: it is ladder vocabulary,
  and the popup says what it means to the user.

Its probe state (`ProbeState`) is shown in the outage's attempts block
instead (§6).

**Identity:** `ClientSession` keeps the ssh target and `session_id`, taken
from `Established`. The `attach_id` is not shown any more (2026-10-04) and is
not kept.

---

## 5. Activity log (`activity.rs`)

```rust
struct Entry {
    at: SystemTime, since: SystemTime, kind: Kind,
    text: String,   // the file's wording
    shown: String,  // the popup's wording; `text` unless given
    detail: bool,   // in the file, not in the popup
    repeats: u32,
}
struct Activity { ring: VecDeque<Entry>, file: Option<LogFile> }
```

### 5.1 Recording

`Activity::record(kind, text)` records an entry the popup shows as written;
`record_detail(kind, text)` one it leaves out; `record_shown(kind, text,
shown)` one it shows in other words. All three go through one function, which
escapes and shortens both texts, and every entry, detail or not, is written
to the file exactly as before.

- If the newest entry has the same `kind`, `text`, `shown` and `detail`, its
  `repeats` is incremented and its time updated; nothing is added.
- Otherwise a new entry is pushed; beyond `RING` (200) the oldest detail is
  dropped, or the oldest entry when no detail but the newest is left. A long
  outage's steps are never shown, so they must not push out what is.

**One line per outage** (2026-10-04). Every step of an outage is a detail:
`link silent`, `link live again …`, the `failover` entries, the `rebuild`
entries, and `standby search started`. When an outage ends, the session
records one `outage` entry from what it noted while the outage lasted -- how
it ended (the failover's or the landed rebuild's path, or neither) and how
many ssh attempts failed:

```
outage 8.1 s → switched to standby (IPv4 punched)
outage 56.7 s → rebuilt over ssh (IPv4 punched)
outage 96.3 s → switched to standby (IPv4 punched) after 1 failed ssh attempt
outage 3.2 s → came back by itself
```

A path label with its own parenthesis is not nested: `switched to standby
(IPv4 punched, birthday, 412 probes)`. An ssh attempt begun before a switch
to the standby may still run and fail after it (ruling B1); that failure is
not counted, because the outage did not wait through it -- unless a new
attempt has to begin after the switch, or a rebuild lands, in which case the
switch did not end the outage and every failure counts.

The file's line carries the kind once (`… outage 8.1 s → …`). An outage that
never ends (the session dies in it) leaves only its details.

The network crates never log (A): they return reasons, and the session
records them. Events:

| Kind | Text (shape); *details in italics*; [popup wording] where it differs |
|---|---|
| `link` | *silent* · *live again via <path>, outage <N.N> s* · path migrated → <path> |
| `standby` | *search started* · found <path>, <N> ms [standby found …] · not found: <reason> [no second path found] · lost: <reason> [standby lost: <first clause>] |
| `failover` | *probing standby* · *probe answered / failed* · *switched to standby (<path>)* |
| `rebuild` | *attempt <N> started* · *attempt <N> failed: <reason>* · *landed via <path>* |
| `input` | held input sent (<N> bytes) · held input dropped (<N> bytes) |
| `log` | log file off: <reason> (once) |
| `outage` | <N.N> s → <how it ended>[ after <N> failed ssh attempt(s)] [outage <N.N> s → …] |

The blast-miss numbers from A arrive as the `not found` reason.

### 5.2 Retired screen writes

`say_mid_session`, `announce_standby`, `announce_failover` and the "path
migrated" branch of `announce` stop writing to the terminal; each becomes a
`record`. The only line still printed is the connect banner (`announce`'s
first call), which is written before the session owns the screen.

### 5.3 The file

- **Where:** `$XDG_STATE_HOME/oxutrm/client.log`, else
  `~/.local/state/oxutrm/client.log`, on Linux and macOS alike. The directory
  is created if missing.
- **Line:** `<RFC 3339 UTC time, e.g. 2026-10-04T09:02:44Z> <target> <session-id prefix> <kind> <text>`;
  when a folded run ends (a different entry arrives, or the client exits),
  the run is written as one line with ` (repeated <N>× since <HH:MM>)`.
- **Writing:** opened once with `O_APPEND`; each line is one `write` call, so
  two clients appending at once do not interleave inside a line. Events are
  rare; the write is synchronous on the loop.
- **Size cap:** before a write that would take the file past `LOG_CAP`
  (1 MiB), rename `client.log` to `client.log.1` (replacing it) and start a new
  file. At most 2 MiB on disk. With two clients, a lost race can cost a
  rotation, never the cap.
- **Failure is quiet:** if the file cannot be opened or written (read-only
  home, full disk), `file` becomes `None`, one `log` entry says so, and the
  popup log goes on. A logging failure never ends or disturbs a session.

---

## 6. Rendering (`popup.rs`)

Revised 2026-10-04 after the hand test. A live view after a failover, and
the top of an outage with an ssh attempt running, at 80×24 (both pinned as
snapshots in `src/view.rs`). The box is as tall as its content:

```
╭ oxutrm · thinlinc · 3ff1218f ────────────────────────────────────────╮
│ ● LIVE   IPv4 punched · up 1m · link 2                               │
│ rtt   140 ms  (70–171)  ▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆ │
│ loss  0.0 %  ↑ 0 B/s  ↓ 0 B/s                                        │
├ standby ─────────────────────────────────────────────────────────────┤
│ none · next search in 23 s                                           │
│ last search: no second path found                                    │
├ recent ──────────────────────────────────────────────────────────────┤
│ 22:53  outage 96.3 s → switched to standby (IPv4 punched) after 1    │
│        failed ssh attempt                                            │
│ 22:59  outage 8.1 s → switched to standby (IPv4 punched)             │
│ 22:59  no second path found                                          │
├──────────────────────────────────────────────────────────────────────┤
│ Esc close  q quit  c config  s sessions                              │
╰──────────────────────────────────────────────────────────────────────╯

│ ● RECOVERING   silent 34 s                                           │
│ standby probe   no answer · next probe in 3 s                        │
│ ssh rebuild     attempt 1 · running 14 s                             │
│ 11 bytes typed since - kept, not sent                                │
│ rtt   —  (70–171)  █▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆█▅▄▃▄▅▆ │
```

`PopupView` is plain data, `PartialEq`:

- **title**, in the top border: `oxutrm · <target> · <session-id prefix>`
  (8 characters, escaped);
- **header**, the first row: the state marker in its colour and bold --
  `● LIVE` (green), `● SILENT` (amber), `● RECOVERING` (red), `● LIVE again`
  while lingering -- then, after three spaces, `<path> · up <age> · link <N> · mtu <N>`
  (lingering: `outage <N.N> s · ` in front), or during an outage
  `silent <N> s` (from `Silent`'s `since`, or `last_heard` in `Recovering`;
  truncated);
- **attempts**, during an outage only: one row per means of recovery, label
  and state in two columns, each with its own clock:
  - `standby probe` (only when the host offered a standby): `probing…`,
    `answered`, `no answer · next probe in <N> s` (`PROBE_RETRY` from the
    failure), `not probed`, or `no standby` when there is none now;
  - `ssh rebuild` (only for a session that can rebuild): `in <N> s` while
    `Silent` (`REBUILD_AFTER` minus the silence); `attempt <N> · running
    <N> s` while one is in flight (its start from `Rebuild::running_since`);
    `attempt <N> failed · next in <N> s` between attempts, then a row with
    no label carrying the reason (`summarised`); `attempt 1 in <N> s` before
    the first; `not needed · switched to standby` once the standby has been
    switched in, since an attempt still running then was begun before the
    switch. Attempt numbers are 1-based;
- **held**: the held text (`render_held`) with `s send` / `d drop` -- the
  Confirming question -- or, during an outage, `<N> bytes typed since -
  kept, not sent`;
- **rtt** row: `rtt   <N> ms  (<min>–<max>)`, `—` for the whole outage, with
  the sparkline filling the rest of the same row, the newest sample at the
  right edge (with fewer samples than cells, the gap is on the left);
- **quality** rows: `loss  <N.N> %  ↑ <rate>  ↓ <rate>`, and
  `screen frames rejected: <N>` when there are any;
- **standby** section (§4), when the host offered a standby;
- **log**: the activity log without its details, each line `HH:MM` local
  wall-clock time and the popup wording. Consecutive identical lines (once
  the details are gone) fold into one with ` ×N`, summing the ring's own
  repeats, at the latest time. The zone is read once per session
  (`jiff::tz::TimeZone::system()`, the root crate forbids `unsafe` and std
  has no time zone), so a laptop that changes zone mid-session shows the old
  zone's times until it reattaches; the view takes it as a parameter so tests
  use UTC or a fixed offset. client.log stays UTC;
- the **key bar**.

`ClientSession` builds it each lap. As with today's notice
(`src/session.rs` ~:2053), the overlay is rebuilt and repainted only when the
view differs from the one shown. Every number in it that moves by itself is
in whole seconds (the log's times in whole minutes), so a static popup
repaints at most about once a second and costs one comparison per lap
otherwise.

`layout_popup(&PopupView, TermSize) -> Overlay` uses ratatui `Block`,
`Paragraph` and `Sparkline`:

- width `min(cols − 4, 72)`; height what the content needs, at most
  `min(rows − 2, 24)`, so a short log leaves no empty rows; centred;
- top to bottom: header, attempts, held, rtt, quality, then `├ standby ─┤`
  and its rows, `├ recent ─┤` and the log, and a plain `├───┤` above the key
  bar, which has the last row. The rules are drawn into the border;
- the attempts and the log are two-column rows: a wrapped text hangs under
  its own text, so the label or time column stays clear;
- the log starts right under its rule, newest last; entries that do not fit
  give way oldest first;
- **short screens** give way in this order of priority: header, held (the
  question survives), attempts, rtt, quality, the plain rule, the standby
  section, the log. While there is held text the header keeps to one row,
  cut rather than wrapped, so a long live header cannot push the typed bytes
  out from under the Confirming question. A rule costs a row and is drawn
  only with at least one row of its section under it;
- below `MIN_BOX` (20×6): today's single reverse-video line on row 0,
  reading `oxutrm: <state marker text>`, or under `Confirming` the question
  and its keys.

The renderer is unchanged: `set_overlay` + `render` composite the rectangle
over the remote screen and hide the cursor while it is up; `resize()`
re-lays-out the shown view. `notice.rs` is removed; `recovering_notice`'s
content moves into the view builder, and `legible`/`summarised` move to
`popup.rs`.

---

## 7. Testing

The project's test discipline applies: every guard gets an injection step
(break it, watch the test fail, restore), and every assertion is checked
against "what else could produce this value?".

- **`quality.rs`:** synthetic rings — window min/max, loss over a window,
  throughput over 5 s, outage samples become gaps and are excluded from RTT
  stats, no delta across a segment boundary, sparkline scaling.
- **`activity.rs`:** fold of repeats, ring cap, line format, escaping of
  control bytes; in a temp dir: append, rotation at an injected small cap,
  total size never above two caps, a read-only directory gives `file: None`
  and one `log` entry rather than an error.
- **`ui.rs`:** table tests of every transition — Ctrl-\ opens; double press
  within 500 ms sends one `0x1c`, after 500 ms only closes; outage opens
  `Auto`, an `Open` popup stays `Open`; `Lingering` closes after 3 s, a key
  keeps it open; while the popup is shown every key is its own and none
  reaches the host or the held buffer; closed in an outage, keys are held;
  `Confirming` forces the popup open and its keys do nothing for
  `ANSWER_GUARD`; lone Esc versus an escape sequence.
- **`layout_popup`:** ratatui buffer snapshots at 80×24, 40×12 and 19×5;
  rules in the border, a rule dropped with its section, hanging indents, the
  sparkline filling the rtt row, the held section surviving a short screen.
- **`view.rs`:** every row's wording per phase; the whole popup, built and
  laid out, as snapshots of a live view and an outage view at 80×24.
- **Session level** (existing `pair_through_relay`, `keyboard()`, `drive`):
  blackholing the link raises the popup overlay, and no byte reaches `out`
  outside the renderer's output; after a failover the ring and client.log
  hold "switched to standby" as a detail, the popup's log shows the outage
  line, and the popup lingers, then closes. An outage driven from its silence
  to its end -- by a failover, a landed rebuild, or by itself -- leaves one
  shown `outage` line and every step before it a detail.

---

## 8. Changelog

One entry under `## Unreleased`, `### New`: the status popup (key, auto-open,
contents) and the log file with its location and size cap; and under
`### Changed`: the mid-session status lines are gone, their content is in the
popup and the log.

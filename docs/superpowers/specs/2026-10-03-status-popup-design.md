# oxutrm — Status popup and activity log

Status: draft 2026-10-03, awaiting review
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
| Quality facts | Live RTT with min/avg/max over 60 s, loss over 60 s, an RTT sparkline, throughput. |
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

- `Closed`: every byte goes to the host, as today.
- `Open`: opened by hand. Stays until closed.
- `Auto`: opened because the phase became an outage (`Phase::is_outage`, i.e.
  after `SILENT_AFTER`, 2 s). An `Open` popup stays `Open` when an outage
  starts — nothing jumps.
- `Lingering`: the link is `Live` again after an `Auto` popup. Shows
  "● LIVE again via <path> · outage <N.N> s" and closes `LINGER` (3 s) after
  it entered this state. Any key turns it into `Open`.
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
- **While open:** `Esc` or Ctrl-\ closes; `q` quits the client (as today's
  Ctrl-\ q); `c` and `s` are shown greyed out and do nothing in B; under
  `Confirming`, `s` sends and `d` drops the held input (as today's Ctrl-\ s /
  Ctrl-\ d).
- **Any other key:**
  - link `Live`: closes the popup and the key goes to the host — typing never
    disappears into a popup by accident;
  - link in an outage: the key is held exactly as today (`hold_keys`,
    `MAX_HELD`), and shown in the held section.
- **Esc ambiguity:** a lone `0x1b` byte in a read is Esc; `0x1b` followed by
  more bytes in the same read is an escape sequence (arrow key etc.) and is
  treated as "any other key".

The existing `PREFIX` handling in `linkstate.rs` (`hold_keys`,
`prefix_pending`, `Command`) is folded into `ui.rs`; `hold_keys` keeps only
the holding of bytes.

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
| RTT now | the newest sample, or `—` while in an outage |
| RTT min/avg/max | non-outage samples of the current segment in the window |
| Sparkline | one cell per second; outage seconds are gaps |
| Loss | Δlost / Δsent over the current segment's samples in the window; cumulative sent/lost for this link |
| Throughput | ↑ and ↓ bytes per second averaged over the last 5 s of the current segment |

**A link swap** (failover or a landed rebuild) starts a new segment: the new
connection's counters start at zero, so no delta is ever taken across two
connections. The status line reads "link <N> of this session, since <time>".

**Standby block**, from new read-only accessors on `Standby`:

- the standby's path label (`rung_label`) and its own `rtt()`,
- its probe state (`ProbeState`),
- "searching…" or "next search in <m> m <s> s" (from `next_search`),
- `last_failure`, shortened and escaped.

**Identity:** `ClientSession` keeps the ssh target, `session_id` and
`attach_id`, taken from `Established` and updated on every swap.

---

## 5. Activity log (`activity.rs`)

```rust
struct Entry { at: SystemTime, kind: Kind, text: String, repeats: u32 }
struct Activity { ring: VecDeque<Entry>, file: Option<LogFile> }
```

### 5.1 Recording

`Activity::record(kind, text)` is the only way in.

- If the newest entry has the same `kind` and `text`, its `repeats` is
  incremented and its time updated; nothing is added.
- Otherwise a new entry is pushed; beyond `RING` (200) the oldest is dropped.

The network crates never log (A): they return reasons, and the session
records them. Events:

| Kind | Text (shape) |
|---|---|
| `link` | silent · live again via <path>, outage <N.N> s · path migrated → <path> |
| `standby` | search started · found <path>, <N> ms · not found: <reason> · lost: <reason> |
| `failover` | probing standby · probe answered / failed · switched to standby (<path>) |
| `rebuild` | attempt <N> started · attempt <N> failed: <reason> · landed via <path> |
| `input` | held input sent (<N> bytes) · held input dropped (<N> bytes) |
| `log` | log file off: <reason> (once) |

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
- **Line:** `<RFC 3339 local time> <target> <session-id prefix> <kind> <text>`;
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

`PopupView` is plain data, `PartialEq`:

- title: `oxutrm · <target>`;
- state marker: `● LIVE` (green), `● SILENT` (amber), `● RECOVERING` (red),
  `● LIVE again …` while lingering;
- status rows (§4), standby block, sparkline series;
- the last log lines (as many as fit);
- an optional held section: the held text (`render_held`) with `s send` /
  `d drop` — today's Confirming notice;
- an optional recovering line: attempt, next try, last failure — today's
  Recovering notice;
- the key bar.

`ClientSession` builds it each lap. As with today's notice
(`src/session.rs` ~:2053), the overlay is rebuilt and repainted only when the
view differs from the one shown. Time-varying numbers are rounded to whole
seconds, so a static popup repaints at most once a second and costs one
comparison per lap otherwise.

`layout_popup(&PopupView, TermSize) -> Overlay` uses ratatui `Block`,
`Paragraph` and `Sparkline`:

- width `min(cols − 4, 72)`, height `min(rows − 2, 24)`, centred;
- status block on top, then the log filling the remaining rows, newest at
  the bottom, long entries wrapped; key bar last;
- below `MIN_BOX` (20×6): today's single reverse-video line on row 0,
  reading `oxutrm: <state marker text>`.

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

- **`quality.rs`:** synthetic rings — window min/avg/max, loss over a window,
  throughput over 5 s, outage samples become gaps and are excluded from RTT
  stats, no delta across a segment boundary, sparkline scaling.
- **`activity.rs`:** fold of repeats, ring cap, line format, escaping of
  control bytes; in a temp dir: append, rotation at an injected small cap,
  total size never above two caps, a read-only directory gives `file: None`
  and one `log` entry rather than an error.
- **`ui.rs`:** table tests of every transition — Ctrl-\ opens; double press
  within 500 ms sends one `0x1c`, after 500 ms only closes; outage opens
  `Auto`, an `Open` popup stays `Open`; `Lingering` closes after 3 s, a key
  keeps it open; with the link `Live` a non-popup key closes and passes
  through; in an outage it is held; `Confirming` forces the popup open; lone
  Esc versus an escape sequence.
- **`layout_popup`:** ratatui buffer snapshots at 80×24, 40×12 and 19×5.
- **Session level** (existing `pair_through_relay`, `keyboard()`, `drive`):
  blackholing the link raises the popup overlay, and no byte reaches `out`
  outside the renderer's output; after a failover the log holds
  "switched to standby", the popup lingers, then closes.

---

## 8. Changelog

One entry under `## Unreleased`, `### New`: the status popup (key, auto-open,
contents) and the log file with its location and size cap; and under
`### Changed`: the mid-session status lines are gone, their content is in the
popup and the log.

# B — Status popup and activity log — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** One place where the state of the connection can be seen at any time — a popup opened by `Ctrl-\` and by itself on connection loss, showing link quality and what oxutrm is doing to keep the session alive — with every event also kept in a size-capped log file, and nothing ever written over the shell.

**Architecture:** Four units. `popup.rs` (oxutrm-client) lays a plain-data `PopupView` out as an `Overlay` with ratatui. `activity.rs` keeps the event ring and the capped file. `quality.rs` keeps 60 one-second samples of the primary's quinn counters and derives the numbers. `ui.rs` is the popup's modal state machine and decides where every keystroke goes. A fifth, `view.rs`, is a pure function from the session's facts to a `PopupView`, so the popup's words are tested without a network. `ClientSession` owns one of each, builds the view on laps the loop already takes, and composites it through the existing renderer. `notice.rs` and the mid-session status lines are retired.

**Tech Stack:** Rust, edition 2024, MSRV 1.96. `ratatui` 0.30.2 (`Block`, `Paragraph`, `Sparkline` — all available under the workspace's `std`-only feature set), `quinn` 0.11.11 with the vendored `quinn-proto` 0.11.17, `tokio`, `insta` (dev, already a workspace dependency), `tempfile` (dev, already in the root crate).

**Spec:** `docs/superpowers/specs/2026-10-03-status-popup-design.md` (all of it). Read it before Task 1. Where this plan and the spec disagree, the spec wins, except for the deviations listed below. Background: `docs/superpowers/specs/2026-09-25-standby-and-rendezvous-design.md` §3.7 (the lines this retires).

**Baseline:** `make check` on `main` at `d1ab924`. It was not run while this plan was written. Run it first; if it fails, stop and report before Task 1.

**Verified while planning** (against `Cargo.lock` and `~/.cargo/registry/src`):

- `quinn::Connection::rtt() -> Duration` and `stats() -> ConnectionStats`; the fields are `stats.path.sent_packets`, `stats.path.lost_packets`, `stats.udp_tx.bytes`, `stats.udp_rx.bytes` (all `u64`) in `vendor/quinn-proto/src/connection/stats.rs`.
- `ratatui::widgets::{Sparkline, SparklineBar}` is re-exported unconditionally in ratatui 0.30.2. `Sparkline::data` takes any `IntoIterator<Item: Into<SparklineBar>>`, and `Option<u64>` converts, `None` being an absent bar drawn with `absent_value_symbol` (default `" "`). `max(u64)` sets the scale.
- `quinn::ConnectionError::TimedOut` displays as `timed out`.

**Deviations from the spec, decided while planning:**

1. **While typing is being held, the popup's commands take the `Ctrl-\` prefix** (spec §3.2 says bare `q`, and bare `s`/`d` under `Confirming`). §3.2 also says that in an outage "the key is held exactly as today", and the two cannot both hold: someone typing blind into a dead screen types `q`, `s`, `d` and `Esc` as ordinary letters, and a bare `q` would quit the client in the middle of `squeue`, a bare `s` would answer `Confirming`'s question with half a word (the exact defect the old `heard` comment records), and a lone `Esc` from vim would close the popup instead of being kept. So the rule is: **bare letters are popup commands only while the link is `Live`** — nothing typed can be lost then, because any other key closes the popup and goes to the host. While bytes are held (`Silent`, `Recovering`, `Confirming`) every bare byte is typing, and commands are `Ctrl-\ q`, and under `Confirming` `Ctrl-\ s` / `Ctrl-\ d` — today's keys, unchanged. The key bar says so in each phase.
2. **The popup cannot be closed by hand during an outage.** It follows from 1: `Esc` and a lone `Ctrl-\` are typing then. The popup is the only thing saying the screen is frozen; it stays up until the link answers.
3. **`Lingering` and keys** (§3.1 says any key turns it into `Open`; §3.2 says that with the link `Live` a non-popup key closes the popup and goes to the host). Resolved in favour of §3.2's "typing never disappears into a popup by accident": in `Lingering`, `Esc` and `Ctrl-\` close, `q` quits, the popup's own letters `c` and `s` turn it into `Open` (it stops counting down), and any other key closes it and goes to the host. A key typed during the outage itself turns `Auto` into `Open` (§3.1's "any key"), so a popup the user touched does not close on its own afterwards.
4. **Times in the log file are UTC (`2026-10-04T12:34:56Z`), not local** (§5.3 says "RFC 3339 local time"). The root crate is `#![forbid(unsafe_code)]`, `activity.rs` is "std only", and std has no time zone: local time needs `localtime_r` (unsafe) or a new dependency outside the contract's table. UTC is valid RFC 3339 and unambiguous. The popup shows **ages** instead of clock times (`12s`, `4m`, `3h`), so no time zone is needed on screen either; the status row reads "link 2 of this session, up 14m" rather than "since <time>". The fold suffix is `(repeated N× since HH:MM)` in UTC.
5. **A folded run in the file:** the first occurrence is written at once (a crash loses nothing); when the run ends — a different entry arrives, or the `Activity` is dropped at session end — one more line is written for it, `<time of last> <tag> <kind> <text> (repeated N× since HH:MM)`, where N counts the repeats after the first.
6. **Where the held and recovering sections go** (§6 lists them; its layout paragraph does not place them): directly under the marker, above the status block. They are what the old notice said and what the user may have to act on, so on a short screen they are the last body sections to be clipped. The key bar always keeps the last row.
7. **The standby's "say it once per dry spell" (`told_none`) is retired.** It existed for the mid-session line. `Standby::not_found` now returns whether the result was for the search still wanted, and every such failure is recorded; identical consecutive reasons fold. `Standby::lost` returns whether the loss is worth recording (not when the session is ending).
8. **`activity.rs` uses `oxutrm_client::summarised`** to escape and shorten what it records (§2 says "std only"). §2 also says remote text is escaped "with the existing `legible`/`summarised` helpers", which live in the client crate; one escaper is better than two.
9. **`c config` / `s sessions` appear dimmed only in the healthy-link key bar.** In the outage and `Confirming` bars, `c` and `s` are typing (deviation 1), and showing them there would invite pressing them.

## Global Constraints

Every task's requirements include these.

- **`make check` is the gate** (fmt check, clippy `-D warnings`, tests, parallelism capped at 4). Never a bare `cargo test`. A single test: `cargo test --workspace --jobs 4 <name> -- --test-threads 4`. Run the gate as `make check > "${TMPDIR:-/tmp}/oxutrm-check.log" 2>&1; echo "make check exit: $?"` and then read the log (piping into `tail` loses the exit code).
- **Every task ends with an injection step:** break the thing deliberately, watch the new test fail, restore. Of each test ask "what else could produce the value this test asserts?", and put a before-assertion beside every after-assertion where it matters.
- **No `#[allow(dead_code)]`.** An item used only by a later task gets `#[cfg_attr(not(test), allow(dead_code))]` with a comment naming the wiring task, which removes it; test-only helpers are `#[cfg(test)]`. Such an item must still be called from a test, or `clippy --all-targets` fails. In this plan the new root-crate modules carry the attribute on their `mod` line in `src/main.rs`.
- **No new timer that fires while nothing is happening** (commit `19cc001`). Sampling, lingering and repaint decisions ride on laps the client loop already takes (it re-arms its deadline at `now + pacing_interval()`, at most 100 ms).
- **The loops' `select!` arms borrow locals, never `self`** (constraint C1).
- **In-session client modules carry `#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]`** (commits `674bed7`, `668a73a`). `popup.rs`, `activity.rs`, `quality.rs`, `ui.rs` and `view.rs` get it too.
- **Nothing on the in-session path writes to the terminal except the renderer.** The connect banner, printed by `announce` before the session owns the screen, is the one line left.
- **Text that came from a remote side is escaped** (`legible` / `summarised`) before it is shown or written to the file.
- **`src/main.rs` is `#![forbid(unsafe_code)]`; `oxutrm-host` must not depend on `oxutrm-net`.**
- **A rejected frame must never disconnect, a send failure must never end a session, and a logging failure never ends or disturbs a session.**
- **`max_idle_timeout` is `None`:** any wait on a stream or connection needs its own bound.
- **English for all code and comments. Comments must stay true** — a comment that outlives its truth is this project's signature defect — and **describe the code, not the task that wrote it** (no "Task 6" in a comment, except the dead-code attributes, which the named task deletes).
- **Commits:** conventional subjects; every message ends with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.
- **One `CHANGES.md` entry for the feature, under `## Unreleased`**, using the file's real headings (`### New`, `### Changed`, `### Fixed`, `### Compatibility`), in the last task.
- **Tests use a STUN-free `NetConfig`** (`crate::attach_exchange::fixtures::stun_free()`, or the session tests' `stunless()`). Session-level tests use the existing fixtures (`pair`, `pair_through_relay`, `crate::link::fixtures::link_pair`, `keyboard()`, `drive`, `SharedOut`, with `run_carries_typing_to_the_shell_and_its_exit_code_back` as the model). **Retransmission heals ordering bugs in end-to-end tests — prove ordering synchronously** (call the method, inspect the state) and use the e2e tests for composition.

## Review Focus

The five inputs the spec implies but does not spell out that are most likely to bite a person using this. Each has a test in the task named.

1. **Someone typing blind into a dead screen** — `q`, `s`, `d`, a lone `Esc` from vim, arrow keys. Everything must be held, nothing must quit, send or close. *Task 4: `bare_letters_and_esc_are_typing_while_the_link_is_down`, `an_arrow_key_in_an_outage_is_held_whole`.*
2. **An arrow key or other escape sequence pressed while the popup is open on a healthy link** — it must reach the remote program whole and close the popup, not be eaten as `Esc` plus stray bytes. *Task 4: `a_lone_esc_closes_but_an_escape_sequence_passes_through`.*
3. **The terminal resized while the popup is up, including below the minimum box** — the popup is laid out again for the new size, down to the one-line fallback. *Task 6: `a_resize_lays_the_popup_out_again_for_the_new_screen` (24×8 and 19×5).*
4. **A log file that cannot be written** — read-only or missing home, a path component that is a file, the file replaced under a running client. The session goes on, the popup says so once. *Task 2: `a_log_file_that_cannot_be_opened_leaves_one_entry_and_the_ring_goes_on`, `a_log_file_that_fails_mid_session_is_dropped_quietly`.*
5. **Remote text with control bytes or of any length** — a search's failure reason or ssh stderr reaching the popup and the file. *Task 2: `a_recorded_text_is_escaped_and_cut_to_one_line`; Task 5: `a_search_failure_from_the_far_end_is_shown_escaped_and_short`.*

A sixth that is close behind and also has a test: **two clients appending to the same log and rotating it** (Task 2: `a_rotation_by_another_client_is_followed_and_the_cap_holds`).

## File Structure

| File | Responsibility |
|---|---|
| `crates/oxutrm-client/src/popup.rs` | **New.** `PopupView`, `Marker`, `KeyHint`, `layout_popup`, `MIN_BOX`, `MAX_BOX`; `legible`, `summarised`, `wrapped_row_count` moved here from `notice.rs`. |
| `crates/oxutrm-client/src/notice.rs` | Modified in Task 1 (imports the moved helpers), **deleted** in Task 6. |
| `crates/oxutrm-client/src/lib.rs` | Modified. `pub mod popup` and its re-exports; `notice` removed in Task 6. |
| `crates/oxutrm-client/src/overlay.rs` | Modified (module doc only). |
| `crates/oxutrm-client/Cargo.toml` | Modified. `insta` as a dev-dependency. |
| `src/activity.rs` | **New.** `Activity`, `Entry`, `Kind`, `LogFile`, `state_path`, `rfc3339_utc`. |
| `src/quality.rs` | **New.** `Quality`, `Reading`, `Sample`, `RttStats`, `Loss`, `Throughput`. |
| `src/ui.rs` | **New.** `Ui`, `Mode`, `Command`, `Routed`, `LinkChange`, `PREFIX`, `ESC`, `DOUBLE_PRESS`, `LINGER`. |
| `src/view.rs` | **New.** `Facts`, `StandbyFacts`, `Identity`, `build`, `path_label`, `age`. |
| `src/linkstate.rs` | Modified in Task 6. `PREFIX`, `Command` and the prefix handling leave; `hold_keys` only holds. |
| `src/standby.rs` | Modified. Read-only accessors (Task 6); holds an `Established`, recording semantics (Task 7). |
| `src/session.rs` | Modified. The popup replaces the notice (Task 6); events are recorded, mid-session lines retired (Task 7); e2e tests (Task 8). |
| `src/connect.rs` | Modified. Identity (Task 6) and the log file (Task 7). |
| `src/loopback.rs` | Modified (Task 7). `replay` becomes a shared test fixture. |
| `src/main.rs` | Modified. `mod activity; mod quality; mod ui; mod view;`. |
| `README.md`, `CHANGES.md` | Modified (Task 9). |

---

### Task 1: `popup.rs` — the view model and its layout

**Files:**
- Create: `crates/oxutrm-client/src/popup.rs`
- Modify: `crates/oxutrm-client/src/notice.rs` (remove `REASON_SHOWN`, `summarised`, `legible`, `wrapped_row_count`; import them)
- Modify: `crates/oxutrm-client/src/lib.rs`
- Modify: `crates/oxutrm-client/Cargo.toml`

**Interfaces:**
- Consumes: `crate::overlay::{Overlay, overlay_from_buffer}`, `oxutrm_proto::TermSize`.
- Produces (all re-exported from `oxutrm_client`):
  - `pub struct PopupView { pub title: String, pub marker: Marker, pub marker_text: String, pub held: Vec<String>, pub recovering: Vec<String>, pub status: Vec<String>, pub standby: Vec<String>, pub spark: Vec<Option<u64>>, pub log: Vec<String>, pub keys: Vec<KeyHint> }` — `Clone, PartialEq, Eq, Debug, Default`.
  - `pub enum Marker { Live, Silent, Recovering, LiveAgain }` — `Clone, Copy, PartialEq, Eq, Debug, Default` (`Live`).
  - `pub struct KeyHint { pub key: String, pub label: String, pub enabled: bool }` — `Clone, PartialEq, Eq, Debug`.
  - `pub fn layout_popup(v: &PopupView, size: TermSize) -> Overlay`
  - `pub fn legible(line: &str) -> String`, `pub fn summarised(reason: &str) -> String`
  - `pub const MIN_BOX: TermSize` (20×6), `pub const MAX_BOX: TermSize` (72×24), `pub const REASON_SHOWN: usize` (120)
  - `pub(crate) fn wrapped_row_count(lines: &[Line<'static>], width: u16, height: u16) -> u16`

- [ ] **Step 1: Add `insta` to the client's dev-dependencies**

Append to `crates/oxutrm-client/Cargo.toml`:

```toml

[dev-dependencies]
# Snapshots of the status popup's layout, as `oxutrm-term`'s golden tests do
# for the emulator: a layout regression shows up as a readable text diff.
insta.workspace = true
```

- [ ] **Step 2: Write `popup.rs` with its tests, layout functions stubbed**

Create `crates/oxutrm-client/src/popup.rs` with the module below, but with the body of `layout_popup` replaced by `todo!()` for now (Step 4 fills it in). Everything else is final.

```rust
//! The status popup, laid out as layer-1 cells.
//!
//! [`PopupView`] is what the popup says, as plain data the session builds on
//! every lap the popup is open; [`layout_popup`] turns it into an [`Overlay`]
//! the renderer composites over the remote screen. Pure: no clock, no
//! terminal, no session.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use oxutrm_proto::TermSize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Padding, Paragraph, Sparkline, Widget as _, Wrap,
};

use crate::overlay::{Overlay, overlay_from_buffer};

/// Below this the box is dropped for a single line: a box that does not fit
/// is worse than a line that does.
pub const MIN_BOX: TermSize = TermSize { cols: 20, rows: 6 };

/// The largest box, whatever the screen: wide enough for a log line, small
/// enough to leave the shell visible around it.
pub const MAX_BOX: TermSize = TermSize { cols: 72, rows: 24 };

/// How much of a reason is shown.
///
/// A reason is an error chain built partly from a remote's stderr, so its
/// length is not ours to choose. The popup wraps and clips, so a long one
/// cannot overflow anything -- but it can push everything else out of the
/// box. The useful part of an ssh failure is at the front.
pub const REASON_SHOWN: usize = 120;

/// The label in front of the RTT sparkline.
const SPARK_LABEL: &str = "rtt ";

/// The state the popup reports, and the colour it reports it in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Marker {
    /// The host is answering.
    #[default]
    Live,
    /// A reply is owed and none has come.
    Silent,
    /// Silent long enough that the client is rebuilding the link.
    Recovering,
    /// The host answers again after an outage the popup opened for.
    LiveAgain,
}

impl Marker {
    fn color(self) -> Color {
        match self {
            Marker::Live | Marker::LiveAgain => Color::Green,
            Marker::Silent => Color::Yellow,
            Marker::Recovering => Color::Red,
        }
    }
}

/// One entry of the key bar.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KeyHint {
    pub key: String,
    pub label: String,
    /// Drawn dimmed when false: a key that is coming, not one that works.
    pub enabled: bool,
}

/// What the popup says, as content rather than as cells.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct PopupView {
    /// Drawn into the top border.
    pub title: String,
    pub marker: Marker,
    /// The first line inside the box, e.g. `● SILENT`.
    pub marker_text: String,
    /// What was typed while the link was down, and what can be done with it.
    pub held: Vec<String>,
    /// What the rebuild loop is doing, while it runs.
    pub recovering: Vec<String>,
    /// Link quality and identity, one row each.
    pub status: Vec<String>,
    /// The standby, when the host offered one.
    pub standby: Vec<String>,
    /// Round-trip time in milliseconds, oldest first; `None` is a gap.
    pub spark: Vec<Option<u64>>,
    /// The activity log, oldest first.
    pub log: Vec<String>,
    pub keys: Vec<KeyHint>,
}

/// Lay the popup out for this screen, as cells ready to composite.
///
/// `min(cols - 4, 72)` by `min(rows - 2, 24)`, centred. Top to bottom: the
/// marker, the held section, the recovering section, the status block, the
/// standby block, the sparkline, then the log filling whatever is left with
/// the newest entry at the bottom. The key bar always has the last row inside
/// the border, because it is how the popup is left.
pub fn layout_popup(v: &PopupView, size: TermSize) -> Overlay {
    todo!()
}

fn plain(lines: &[String]) -> Vec<Line<'static>> {
    lines.iter().map(|l| Line::from(l.clone())).collect()
}

/// Render `lines`, wrapped, at the top of `area`; return the part of `area`
/// left below them.
fn place(lines: &[Line<'static>], area: Rect, buf: &mut Buffer) -> Rect {
    if lines.is_empty() || area.height == 0 {
        return area;
    }
    let used = wrapped_row_count(lines, area.width, area.height);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(Rect { height: used, ..area }, buf);
    Rect {
        y: area.y + used,
        height: area.height - used,
        ..area
    }
}

/// One row: a label, then one cell per sample, the newest at the right.
fn place_spark(spark: &[Option<u64>], area: Rect, buf: &mut Buffer) -> Rect {
    let label = SPARK_LABEL.len() as u16;
    if area.height == 0 || area.width <= label || spark.iter().all(Option::is_none) {
        return area;
    }
    Paragraph::new(SPARK_LABEL).render(
        Rect {
            width: label,
            height: 1,
            ..area
        },
        buf,
    );
    let width = (area.width - label) as usize;
    let shown = &spark[spark.len().saturating_sub(width)..];
    let max = shown.iter().flatten().copied().max().unwrap_or(1).max(1);
    Sparkline::default()
        .data(shown.to_vec())
        .max(max)
        .absent_value_symbol(" ")
        .render(
            Rect {
                x: area.x + label,
                width: area.width - label,
                height: 1,
                ..area
            },
            buf,
        );
    Rect {
        y: area.y + 1,
        height: area.height - 1,
        ..area
    }
}

/// The log fills `area` from the bottom up, newest last; entries that do not
/// fit give way oldest first. A long entry wraps and takes the rows it needs.
fn place_log(log: &[String], area: Rect, buf: &mut Buffer) {
    if area.height == 0 {
        return;
    }
    let mut kept: Vec<(Line<'static>, u16)> = Vec::new();
    let mut used = 0u16;
    for entry in log.iter().rev() {
        let line = Line::from(entry.clone());
        let rows = wrapped_row_count(std::slice::from_ref(&line), area.width, area.height).max(1);
        if used + rows > area.height {
            break;
        }
        used += rows;
        kept.push((line, rows));
    }
    let mut y = area.bottom() - used;
    for (line, rows) in kept.into_iter().rev() {
        Paragraph::new(line)
            .wrap(Wrap { trim: false })
            .render(Rect { y, height: rows, ..area }, buf);
        y += rows;
    }
}

fn key_line(keys: &[KeyHint]) -> Line<'static> {
    let mut spans = Vec::new();
    for (i, k) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        let (key, label) = if k.enabled {
            (Style::default().add_modifier(Modifier::BOLD), Style::default())
        } else {
            let dim = Style::default().add_modifier(Modifier::DIM);
            (dim, dim)
        };
        spans.push(Span::styled(k.key.clone(), key));
        spans.push(Span::styled(format!(" {}", k.label), label));
    }
    Line::from(spans)
}

/// The fallback for a screen too small for a box.
///
/// Reverse video and the top row, because the bottom rows are where the
/// cursor usually is and covering those is what the box was centred to avoid.
fn single_line(v: &PopupView, size: TermSize) -> Overlay {
    let area = Rect::new(0, 0, size.cols.max(1), 1);
    let mut buf = Buffer::empty(area);
    Paragraph::new(Line::from(Span::styled(
        format!("oxutrm: {}", v.marker_text),
        Style::default().add_modifier(Modifier::REVERSED),
    )))
    .render(area, &mut buf);
    overlay_from_buffer(&buf, 0, 0)
}

/// How many rows `lines` needs when wrapped at `width`, capped at `height`.
///
/// `Paragraph::line_count` would answer this directly, but sits behind
/// ratatui's `unstable-rendered-line-info` feature, and enabling an unstable
/// feature to size a box is a worse trade than this: render the same wrap
/// into a scratch buffer and read back the last row it touched. By
/// construction it cannot disagree with ratatui's own wrapping -- it *is*
/// ratatui's own wrapping. Callers never end a section on a blank line, so a
/// run of untouched rows at the bottom means "wrapping stopped here".
pub(crate) fn wrapped_row_count(lines: &[Line<'static>], width: u16, height: u16) -> u16 {
    if width == 0 || height == 0 {
        return 0;
    }
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    Paragraph::new(lines.to_vec())
        .wrap(Wrap { trim: false })
        .render(area, &mut buf);
    (0..height)
        .rev()
        .find(|&y| (0..width).any(|x| buf[(x, y)].symbol() != " "))
        .map_or(0, |y| y + 1)
}
```

Then move `summarised` and `legible` from `notice.rs` into `popup.rs`, **verbatim including their doc comments**, below `wrapped_row_count`, changing only `fn summarised` to `pub fn summarised` and `fn legible` to `pub fn legible`. In `legible`'s doc, the sentence "the alternative was to sanitise at the call site in the root crate, before the reason reaches [`recovering_notice`]" now names a function that will go away in Task 6; reword that paragraph's two references to `recovering_notice` to "the popup", e.g. "...before the reason reaches the popup. This is the better half of that trade: it leaves the escaping owned by the function that already owns \"make this reason fit\", so no future caller can reintroduce the hole by not knowing about it."

Append the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{Attrs, Color as PColor};

    fn view() -> PopupView {
        PopupView {
            title: "oxutrm \u{b7} bastion".to_string(),
            marker: Marker::Silent,
            marker_text: "\u{25cf} SILENT".to_string(),
            held: vec![],
            recovering: vec![],
            status: vec![
                "silent for 6s".to_string(),
                "rtt \u{2014} \u{b7} min 30 / avg 35 / max 52 ms".to_string(),
            ],
            standby: vec!["standby: searching\u{2026}".to_string()],
            spark: vec![Some(30), Some(40), None, Some(52)],
            log: (1..=30).map(|i| format!("event-{i:02}")).collect(),
            keys: vec![
                KeyHint {
                    key: "Ctrl-\\ q".to_string(),
                    label: "quit".to_string(),
                    enabled: true,
                },
                KeyHint {
                    key: "c".to_string(),
                    label: "config".to_string(),
                    enabled: false,
                },
            ],
        }
    }

    fn row(o: &Overlay, r: u16) -> String {
        (0..o.cols)
            .map(|c| o.cells[r as usize * o.cols as usize + c as usize].text.to_string())
            .collect()
    }

    fn text_of(o: &Overlay) -> String {
        (0..o.rows).map(|r| row(o, r)).collect::<Vec<_>>().join("\n")
    }

    fn cell_of<'a>(o: &'a Overlay, r: u16, ch: &str) -> &'a oxutrm_proto::Cell {
        let start = r as usize * o.cols as usize;
        o.cells[start..start + o.cols as usize]
            .iter()
            .find(|c| c.text == ch)
            .unwrap_or_else(|| panic!("no {ch:?} on row {r}: {:?}", row(o, r)))
    }

    #[test]
    fn a_popup_is_centred_and_capped() {
        for (cols, rows, want) in [
            (80u16, 24u16, (72u16, 22u16, 1u16, 4u16)),
            (200, 60, (72, 24, 18, 64)),
            (40, 12, (36, 10, 1, 2)),
        ] {
            let o = layout_popup(&view(), TermSize { cols, rows });
            assert_eq!((o.cols, o.rows, o.row, o.col), want, "on {cols}x{rows}");
        }
    }

    #[test]
    fn a_popup_never_exceeds_the_screen() {
        for (cols, rows) in [(80u16, 24u16), (20, 6), (21, 7), (200, 60), (24, 8)] {
            let o = layout_popup(&view(), TermSize { cols, rows });
            assert!(o.cols <= cols && o.rows <= rows, "{o:?} exceeds {cols}x{rows}");
            assert!(o.row + o.rows <= rows && o.col + o.cols <= cols);
            assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
        }
    }

    /// The marker is the first row inside the border, the key bar the last:
    /// asserted together, so a layout that put everything on one row could
    /// not pass either.
    #[test]
    fn the_marker_leads_and_the_key_bar_closes_the_box() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        assert!(row(&o, 1).contains("\u{25cf} SILENT"), "{}", text_of(&o));
        assert!(!row(&o, 1).contains("quit"), "{}", text_of(&o));
        assert!(row(&o, o.rows - 2).contains("Ctrl-\\ q quit"), "{}", text_of(&o));
        assert!(row(&o, o.rows - 2).contains("c config"), "{}", text_of(&o));
    }

    #[test]
    fn the_newest_log_entry_is_at_the_bottom_and_the_oldest_give_way() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);
        assert!(row(&o, o.rows - 3).contains("event-30"), "{text}");
        assert!(row(&o, o.rows - 4).contains("event-29"), "{text}");
        assert!(
            !text.contains("event-01"),
            "thirty entries fitted into a box with fewer rows than that: {text}"
        );
    }

    /// On a short screen the held section -- the question the user has to
    /// answer -- survives, and the status block is what gives way.
    #[test]
    fn the_held_section_survives_a_short_screen() {
        let mut v = view();
        v.held = vec![
            "You typed 10 bytes while offline:".to_string(),
            "make test\u{21b5}".to_string(),
        ];
        let tall = text_of(&layout_popup(&v, TermSize { cols: 80, rows: 24 }));
        assert!(tall.contains("silent for 6s"), "the fixture shows no status: {tall}");

        let short = text_of(&layout_popup(&v, TermSize { cols: 40, rows: 8 }));
        assert!(short.contains("make test"), "{short}");
        assert!(short.contains("Ctrl-\\ q"), "the key bar was clipped: {short}");
        assert!(!short.contains("silent for 6s"), "nothing gave way: {short}");
    }

    #[test]
    fn the_marker_is_coloured_by_state() {
        for (marker, want) in [
            (Marker::Live, PColor::Idx(2)),
            (Marker::LiveAgain, PColor::Idx(2)),
            (Marker::Silent, PColor::Idx(3)),
            (Marker::Recovering, PColor::Idx(1)),
        ] {
            let v = PopupView { marker, ..view() };
            let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
            assert_eq!(cell_of(&o, 1, "\u{25cf}").fg, want, "for {marker:?}");
        }
    }

    #[test]
    fn a_key_not_offered_yet_is_dimmed_and_one_that_works_is_not() {
        let o = layout_popup(&view(), TermSize { cols: 80, rows: 24 });
        let bar = o.rows - 2;
        assert!(cell_of(&o, bar, "c").attrs.contains(Attrs::DIM));
        let ctrl = cell_of(&o, bar, "C");
        assert!(!ctrl.attrs.contains(Attrs::DIM));
        assert!(ctrl.attrs.contains(Attrs::BOLD));
    }

    /// An outage second has no RTT, and must read as a gap rather than as a
    /// zero-height bar, which would claim a very fast link.
    #[test]
    fn an_outage_second_is_a_gap_in_the_sparkline() {
        let v = PopupView {
            spark: vec![Some(10), None, Some(10)],
            ..view()
        };
        let o = layout_popup(&v, TermSize { cols: 80, rows: 24 });
        // The status row starts with "rtt " too; it is the one with "ms".
        let line = (0..o.rows)
            .map(|r| row(&o, r))
            .find(|l| l.contains(SPARK_LABEL) && !l.contains("ms"))
            .expect("no sparkline row");
        let at = line.find(SPARK_LABEL).unwrap() + SPARK_LABEL.len();
        let bars: Vec<char> = line[at..].chars().take(3).collect();
        assert_ne!(bars[0], ' ', "{line:?}");
        assert_eq!(bars[1], ' ', "the outage second was drawn: {line:?}");
        assert_ne!(bars[2], ' ', "{line:?}");
    }

    #[test]
    fn a_screen_below_the_minimum_gets_one_line() {
        let o = layout_popup(&view(), TermSize { cols: 19, rows: 5 });
        assert_eq!((o.rows, o.row, o.col, o.cols), (1, 0, 0, 19));
        assert!(text_of(&o).contains("oxutrm: \u{25cf} SILENT"), "{}", text_of(&o));
        assert!(o.cells[0].attrs.contains(Attrs::INVERSE));
    }

    /// A terminal reports 1x1 transiently while some emulators tear down.
    #[test]
    fn a_one_by_one_screen_does_not_panic() {
        let o = layout_popup(&view(), TermSize { cols: 1, rows: 1 });
        assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
    }

    #[test]
    fn snapshot_80x24() {
        insta::assert_snapshot!(text_of(&layout_popup(
            &view(),
            TermSize { cols: 80, rows: 24 }
        )));
    }

    #[test]
    fn snapshot_40x12() {
        insta::assert_snapshot!(text_of(&layout_popup(
            &view(),
            TermSize { cols: 40, rows: 12 }
        )));
    }

    #[test]
    fn snapshot_19x5() {
        insta::assert_snapshot!(text_of(&layout_popup(
            &view(),
            TermSize { cols: 19, rows: 5 }
        )));
    }

    /// The reason is a REMOTE's stderr. A terminal must never act on it.
    #[test]
    fn control_scalars_in_a_reason_are_shown_rather_than_emitted() {
        let shown = summarised("ssh said: \u{1b}[2Jcleared\u{9b}31m and \u{7} rang");
        assert!(!shown.chars().any(char::is_control), "{shown:?}");
        assert!(shown.contains("^[") && shown.contains("^G"), "{shown:?}");
        assert!(shown.contains("<9B>"), "{shown:?}");
        assert!(shown.contains("ssh said:") && shown.contains("cleared"), "{shown:?}");
    }

    /// ssh says why it failed on its first line and pads afterwards.
    #[test]
    fn only_the_first_line_of_a_reason_is_shown() {
        let shown = summarised("Permission denied (publickey).\nbanner line nobody needs");
        assert!(shown.contains("Permission denied"), "{shown}");
        assert!(!shown.contains("banner line"), "{shown}");
    }

    /// Against `REASON_SHOWN` itself, plus the ellipsis: a loose bound would
    /// pass for every cap between the two and pin none of them.
    #[test]
    fn a_long_reason_is_cut_at_the_cap_and_keeps_its_front() {
        let shown = summarised(&format!("ssh said: {}", "noise ".repeat(400)));
        assert_eq!(shown.chars().count(), REASON_SHOWN + "...".len(), "{shown}");
        assert!(shown.starts_with("ssh said:"), "{shown}");
        let short = summarised("ssh said: short");
        assert_eq!(short, "ssh said: short", "a short reason was cut");
    }
}
```

In `crates/oxutrm-client/src/lib.rs` add `pub mod popup;` after `pub mod overlay;`, and after the `pub use overlay::…` line:

```rust
pub use popup::{KeyHint, Marker, PopupView, layout_popup, legible, summarised};
```

In `crates/oxutrm-client/src/notice.rs`: delete `const REASON_SHOWN`, `fn summarised`, `fn legible` and `fn wrapped_row_count` (they now live in `popup.rs`); add `use crate::popup::{summarised, wrapped_row_count};` beside the other `use crate::…` line; and inside its `mod tests` add `use crate::popup::REASON_SHOWN;` (only the test `a_very_long_failure_reason_is_summarised_rather_than_shown_whole` uses it). Its `wrapped_row_count` doc paragraph referring to `notice_lines` goes with the function; nothing in `notice.rs` refers to it any more.

- [ ] **Step 3: Run the popup tests to see them fail**

Run: `cargo test --workspace --jobs 4 popup:: -- --test-threads 4`
Expected: the layout tests FAIL with `not yet implemented` (the `todo!()`); the three `summarised` tests PASS (moved code); the notice tests still PASS.

- [ ] **Step 4: Implement `layout_popup`**

Replace the `todo!()` body:

```rust
pub fn layout_popup(v: &PopupView, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        return single_line(v, size);
    }
    // At least 16x4 here, so the inner area below is at least 12x2.
    let cols = (size.cols - 4).min(MAX_BOX.cols);
    let rows = (size.rows - 2).min(MAX_BOX.rows);
    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .padding(Padding::horizontal(1))
        .title(format!(" {} ", v.title));
    let inner = block.inner(area);
    block.render(area, &mut buf);

    let bar = Rect {
        y: inner.bottom() - 1,
        height: 1,
        ..inner
    };
    Paragraph::new(key_line(&v.keys)).render(bar, &mut buf);

    let mut body = Rect {
        height: inner.height - 1,
        ..inner
    };
    let marker = vec![Line::from(Span::styled(
        v.marker_text.clone(),
        Style::default()
            .fg(v.marker.color())
            .add_modifier(Modifier::BOLD),
    ))];
    body = place(&marker, body, &mut buf);
    for section in [&v.held, &v.recovering, &v.status, &v.standby] {
        body = place(&plain(section), body, &mut buf);
    }
    body = place_spark(&v.spark, body, &mut buf);
    place_log(&v.log, body, &mut buf);

    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}
```

- [ ] **Step 5: Generate the snapshots, read them, and run the tests**

Run: `INSTA_UPDATE=always cargo test --workspace --jobs 4 popup:: -- --test-threads 4`
Expected: PASS, and three new files under `crates/oxutrm-client/src/snapshots/` (`oxutrm_client__popup__tests__snapshot_80x24.snap` etc.).

Open each `.snap` and check by eye before committing — a snapshot accepted unread guards nothing: 80×24 is a rounded 72×22 box titled ` oxutrm · bastion `, `● SILENT` on its first inner row, the two status rows, the standby row, an `rtt ` row with three bars and a gap, `event-NN` lines ending with `event-30`, and `Ctrl-\ q quit  c config` on the last inner row; 40×12 is the same, narrower, with fewer log lines; 19×5 is one row reading `oxutrm: ● SILENT`.

Then run without `INSTA_UPDATE`: `cargo test --workspace --jobs 4 popup:: -- --test-threads 4` — Expected: PASS.

- [ ] **Step 6: Injection**

1. In `layout_popup`, move the `place_log` call above the `for section` loop (log before status). Run the popup tests: `the_held_section_survives_a_short_screen` and the snapshots must FAIL. Restore.
2. In `place_spark`, change `.absent_value_symbol(" ")` to `.absent_value_symbol("_")`. `an_outage_second_is_a_gap_in_the_sparkline` must FAIL. Restore.
3. In `key_line`, make the disabled style `Style::default()`. `a_key_not_offered_yet_is_dimmed_and_one_that_works_is_not` must FAIL. Restore.

- [ ] **Step 7: Gate and commit**

Run `make check > "${TMPDIR:-/tmp}/oxutrm-check.log" 2>&1; echo "make check exit: $?"` and read the log. Expected: exit 0.

```bash
git add crates/oxutrm-client/Cargo.toml crates/oxutrm-client/src/popup.rs crates/oxutrm-client/src/notice.rs crates/oxutrm-client/src/lib.rs crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_80x24.snap crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_40x12.snap crates/oxutrm-client/src/snapshots/oxutrm_client__popup__tests__snapshot_19x5.snap Cargo.lock
git commit -m "$(cat <<'EOF'
feat(client): lay out a status popup from a plain view

PopupView is what the popup says; layout_popup turns it into layer-1
cells: marker, held and recovering sections, status, standby, an RTT
sparkline with gaps for outage seconds, the log filling what is left,
and the key bar on the last row. legible and summarised move here from
notice.rs so the root crate can escape remote text with them too.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

(`Cargo.lock` only if `git status` shows it changed.)

---

### Task 2: `activity.rs` — the event ring and the capped file

**Files:**
- Create: `src/activity.rs`
- Modify: `src/main.rs` (`mod activity;`)

**Interfaces:**
- Consumes: `oxutrm_client::{legible, summarised}` (Task 1).
- Produces:
  - `pub(crate) const RING: usize = 200; pub(crate) const LOG_CAP: u64 = 1024 * 1024;`
  - `pub(crate) enum Kind { Link, Standby, Failover, Rebuild, Input, Log }` with `pub(crate) fn name(self) -> &'static str` (`"link"`, `"standby"`, …).
  - `pub(crate) struct Entry { pub(crate) at: SystemTime, pub(crate) since: SystemTime, pub(crate) kind: Kind, pub(crate) text: String, pub(crate) repeats: u32 }`
  - `pub(crate) struct Activity` with `new() -> Activity`, `with_file(file: std::io::Result<LogFile>, target: &str, session_id: &str) -> Activity`, `record(&mut self, kind: Kind, text: &str)`, `record_at(&mut self, kind: Kind, text: &str, at: SystemTime)`, `entries(&self) -> std::collections::vec_deque::Iter<'_, Entry>`; `Drop` writes a pending folded run.
  - `pub(crate) struct LogFile` with `open(path: PathBuf, cap: u64) -> std::io::Result<LogFile>`. (`open_default`, which reads the real environment, is added in Task 7 where `connect` calls it: no test may call it, because it would write into the developer's own home, and an item no test calls fails `clippy --all-targets`.)
  - `pub(crate) fn state_path(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf>`
  - `pub(crate) fn rfc3339_utc(t: SystemTime) -> String`

- [ ] **Step 1: Declare the module**

In `src/main.rs`, after `mod accept;`:

```rust
// Fed by the session in Task 6 (link events) and Task 7 (everything else,
// and the file); Task 7 removes this attribute.
#[cfg_attr(not(test), allow(dead_code))]
mod activity;
```

- [ ] **Step 2: Write the tests**

Create `src/activity.rs` containing only the module doc, the deny attribute, the `use` lines and the public signatures from the Interfaces block with `todo!()` bodies, plus this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 2026-09-21T14:13:20Z.
    const T: u64 = 1_790_000_000;

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn texts(a: &Activity) -> Vec<String> {
        a.entries().map(|e| e.text.clone()).collect()
    }

    #[test]
    fn record_stamps_the_entry_with_the_wall_clock() {
        let before = SystemTime::now();
        let mut a = Activity::new();
        a.record(Kind::Link, "silent");
        let e = a.entries().next().unwrap();
        assert!(e.at >= before && e.at <= SystemTime::now(), "{:?}", e.at);
    }

    #[test]
    fn every_kind_has_the_name_the_log_shows() {
        let names: Vec<&str> = [Kind::Link, Kind::Standby, Kind::Failover, Kind::Rebuild, Kind::Input, Kind::Log]
            .into_iter()
            .map(Kind::name)
            .collect();
        assert_eq!(names, ["link", "standby", "failover", "rebuild", "input", "log"]);
    }

    #[test]
    fn utc_timestamps_are_rfc_3339() {
        assert_eq!(rfc3339_utc(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(at(951_782_400)), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(at(1_000_000_000)), "2001-09-09T01:46:40Z");
        assert_eq!(rfc3339_utc(at(4_102_444_799)), "2099-12-31T23:59:59Z");
        assert_eq!(rfc3339_utc(at(T)), "2026-09-21T14:13:20Z");
    }

    /// The same thing happening again is one entry with a count, not a
    /// screenful of copies -- and its time is the latest one.
    #[test]
    fn a_repeat_folds_into_the_entry_before_it() {
        let mut a = Activity::new();
        a.record_at(Kind::Standby, "not found: no path", at(T));
        a.record_at(Kind::Standby, "not found: no path", at(T + 30));
        a.record_at(Kind::Standby, "not found: no path", at(T + 90));

        assert_eq!(a.entries().len(), 1);
        let e = a.entries().next().unwrap();
        assert_eq!(e.repeats, 2);
        assert_eq!(e.at, at(T + 90), "the time was not updated");
        assert_eq!(e.since, at(T), "the run forgot when it began");
    }

    /// Same text, different kind: two things, not one.
    #[test]
    fn only_the_same_kind_and_text_fold() {
        let mut a = Activity::new();
        a.record_at(Kind::Link, "x", at(T));
        a.record_at(Kind::Standby, "x", at(T));
        a.record_at(Kind::Standby, "y", at(T));
        assert_eq!(texts(&a), ["x", "x", "y"]);
    }

    #[test]
    fn the_ring_keeps_the_newest_two_hundred() {
        let mut a = Activity::new();
        for i in 0..250 {
            a.record_at(Kind::Link, &format!("e-{i:03}"), at(T + i));
        }
        assert_eq!(a.entries().len(), RING);
        assert_eq!(a.entries().next().unwrap().text, "e-050");
        assert_eq!(a.entries().next_back().unwrap().text, "e-249");
    }

    /// A reason is a remote's stderr. Escaped and cut to its first line when
    /// recorded, so neither the popup nor the file can be handed a control
    /// sequence.
    #[test]
    fn a_recorded_text_is_escaped_and_cut_to_one_line() {
        let mut a = Activity::new();
        a.record_at(Kind::Standby, "not found: \u{1b}[2Jgone\u{9b}1m\nsecond line", at(T));
        let text = &a.entries().next().unwrap().text;
        assert!(!text.chars().any(char::is_control), "{text:?}");
        assert!(text.contains("^[[2Jgone") && text.contains("<9B>"), "{text:?}");
        assert!(!text.contains("second line"), "{text:?}");
    }

    #[test]
    fn the_state_path_prefers_xdg_and_falls_back_to_home() {
        let p = |x: Option<&str>, h: Option<&str>| {
            state_path(x.map(Into::into), h.map(Into::into))
        };
        assert_eq!(
            p(Some("/x/state"), Some("/home/u")),
            Some(PathBuf::from("/x/state/oxutrm/client.log"))
        );
        assert_eq!(
            p(None, Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/oxutrm/client.log"))
        );
        // The XDG spec: a relative path in an XDG variable is invalid and
        // must be ignored, not resolved against wherever oxutrm was started.
        assert_eq!(
            p(Some("rel/state"), Some("/home/u")),
            Some(PathBuf::from("/home/u/.local/state/oxutrm/client.log"))
        );
        assert_eq!(p(Some(""), Some("")), None);
        assert_eq!(p(None, None), None);
    }

    fn opened(dir: &Path, cap: u64) -> (Activity, PathBuf) {
        let path = dir.join("state/oxutrm/client.log");
        let a = Activity::with_file(LogFile::open(path.clone(), cap), "bastion", "f00dcafe0123");
        (a, path)
    }

    #[test]
    fn every_new_entry_is_appended_as_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "silent", at(T));
        a.record_at(Kind::Standby, "search started", at(T + 5));

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "2026-09-21T14:13:20Z bastion f00dcafe link silent\n\
             2026-09-21T14:13:25Z bastion f00dcafe standby search started\n"
        );
    }

    /// The first occurrence is on disk at once; the repeats are one line
    /// when the run ends.
    #[test]
    fn a_folded_run_is_written_once_when_it_ends() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "silent", at(T));
        a.record_at(Kind::Link, "silent", at(T + 60));
        a.record_at(Kind::Link, "silent", at(T + 120));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            1,
            "a repeat was written as a line of its own"
        );

        a.record_at(Kind::Standby, "search started", at(T + 130));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines,
            [
                "2026-09-21T14:13:20Z bastion f00dcafe link silent",
                "2026-09-21T14:15:20Z bastion f00dcafe link silent (repeated 2\u{d7} since 14:13)",
                "2026-09-21T14:15:30Z bastion f00dcafe standby search started",
            ]
        );
    }

    #[test]
    fn a_run_still_folding_when_the_session_ends_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "silent", at(T));
        a.record_at(Kind::Link, "silent", at(T + 60));
        drop(a);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("(repeated 1\u{d7} since 14:13)"), "{text}");
    }

    /// The cap holds on disk: client.log at most one cap, client.log.1 at
    /// most one more, and the newest line in client.log.
    #[test]
    fn the_file_rotates_before_it_passes_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let cap = 300;
        let (mut a, path) = opened(dir.path(), cap);
        for i in 0..40 {
            a.record_at(Kind::Link, &format!("entry number {i:02}"), at(T + i));
        }
        let rotated = dir.path().join("state/oxutrm/client.log.1");
        let live = std::fs::metadata(&path).unwrap().len();
        let old = std::fs::metadata(&rotated).expect("never rotated").len();
        assert!(live <= cap && old <= cap, "{live} + {old} against a cap of {cap}");
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("entry number 39\n"));
    }

    /// Another client rotated the shared file under us. Our handle now
    /// points at client.log.1; appending there would grow it past the cap.
    #[test]
    fn a_rotation_by_another_client_is_followed_and_the_cap_holds() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "first", at(T));
        let rotated = dir.path().join("state/oxutrm/client.log.1");
        std::fs::rename(&path, &rotated).unwrap();
        let before = std::fs::metadata(&rotated).unwrap().len();

        a.record_at(Kind::Link, "second", at(T + 1));

        assert_eq!(std::fs::metadata(&rotated).unwrap().len(), before, "wrote into the rotated file");
        assert!(std::fs::read_to_string(&path).unwrap().ends_with("link second\n"));
    }

    /// A read-only home, a missing one, a path component that is a file: the
    /// session goes on, and the popup says once that there is no file.
    /// A file in the way rather than a read-only directory, so this also
    /// fails as root, where permissions do not.
    #[test]
    fn a_log_file_that_cannot_be_opened_leaves_one_entry_and_the_ring_goes_on() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-directory");
        std::fs::write(&blocker, b"").unwrap();
        let mut a = Activity::with_file(
            LogFile::open(blocker.join("oxutrm/client.log"), LOG_CAP),
            "bastion",
            "f00d",
        );
        let first: Vec<(Kind, String)> = a.entries().map(|e| (e.kind, e.text.clone())).collect();
        assert_eq!(first.len(), 1, "{first:?}");
        assert_eq!(first[0].0, Kind::Log);
        assert!(first[0].1.starts_with("log file off: "), "{first:?}");

        a.record_at(Kind::Link, "silent", at(T));
        assert_eq!(texts(&a).last().map(String::as_str), Some("silent"));
    }

    /// The file vanished and something else took its name mid-session.
    #[test]
    fn a_log_file_that_fails_mid_session_is_dropped_quietly() {
        let dir = tempfile::tempdir().unwrap();
        let (mut a, path) = opened(dir.path(), LOG_CAP);
        a.record_at(Kind::Link, "before", at(T));
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();

        a.record_at(Kind::Link, "during", at(T + 1));
        a.record_at(Kind::Link, "after", at(T + 2));

        let offs = a.entries().filter(|e| e.kind == Kind::Log).count();
        assert_eq!(offs, 1, "the failure was reported {offs} times: {:?}", texts(&a));
        assert!(texts(&a).contains(&"during".to_string()), "the entry that hit the failure was lost");
        assert_eq!(texts(&a).last().map(String::as_str), Some("after"));
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test --workspace --jobs 4 activity:: -- --test-threads 4`
Expected: FAIL with `not yet implemented`.

- [ ] **Step 4: Implement**

The whole of `src/activity.rs` above the test module:

```rust
//! The activity log: what oxutrm did to keep the session alive.
//!
//! Kept twice: a ring of the last [`RING`] entries for the status popup, and
//! a size-capped file for afterwards. The network crates never log -- they
//! return reasons -- and the session records them here. [`Activity::record`]
//! is the only way in.
//!
//! Times in the file are UTC. The root crate forbids `unsafe`, std has no
//! time zone, and UTC is unambiguous on any machine that reads the file.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::collections::VecDeque;
use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// How many entries the popup can scroll back through.
pub(crate) const RING: usize = 200;

/// The size at which `client.log` is rotated to `client.log.1`, so the two
/// together never hold more than twice this.
pub(crate) const LOG_CAP: u64 = 1024 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    Link,
    Standby,
    Failover,
    Rebuild,
    Input,
    Log,
}

impl Kind {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Link => "link",
            Kind::Standby => "standby",
            Kind::Failover => "failover",
            Kind::Rebuild => "rebuild",
            Kind::Input => "input",
            Kind::Log => "log",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Entry {
    /// When it last happened.
    pub(crate) at: SystemTime,
    /// When its run of identical entries began; `at` until it repeats.
    pub(crate) since: SystemTime,
    pub(crate) kind: Kind,
    /// Legible already: escaped and cut to one line when it was recorded.
    pub(crate) text: String,
    /// How many times it happened again after the first.
    pub(crate) repeats: u32,
}

pub(crate) struct Activity {
    ring: VecDeque<Entry>,
    file: Option<LogFile>,
    /// `<target> <session-id prefix>`: what every file line carries after
    /// its time, so two sessions sharing the file can be told apart.
    tag: String,
}

impl Activity {
    /// A log with no file: the ring alone.
    pub(crate) fn new() -> Activity {
        Activity {
            ring: VecDeque::new(),
            file: None,
            tag: "- -".to_string(),
        }
    }

    /// A log that also appends to `file`. A file that could not be opened
    /// costs one `log` entry and nothing else.
    pub(crate) fn with_file(
        file: std::io::Result<LogFile>,
        target: &str,
        session_id: &str,
    ) -> Activity {
        let prefix: String = session_id.chars().take(8).collect();
        let prefix = if prefix.is_empty() { "-".to_string() } else { oxutrm_client::legible(&prefix) };
        let mut a = Activity {
            ring: VecDeque::new(),
            file: None,
            tag: format!("{} {prefix}", oxutrm_client::legible(target)),
        };
        match file {
            Ok(f) => a.file = Some(f),
            Err(e) => a.file_off(&e),
        }
        a
    }

    pub(crate) fn record(&mut self, kind: Kind, text: &str) {
        self.record_at(kind, text, SystemTime::now());
    }

    /// [`Activity::record`], at a given time.
    pub(crate) fn record_at(&mut self, kind: Kind, text: &str, at: SystemTime) {
        let text = oxutrm_client::summarised(text);
        if let Some(last) = self.ring.back_mut()
            && last.kind == kind
            && last.text == text
        {
            last.repeats = last.repeats.saturating_add(1);
            last.at = at;
            return;
        }
        self.end_run();
        let line = self.line(at, kind, &text, None);
        self.push(Entry {
            at,
            since: at,
            kind,
            text,
            repeats: 0,
        });
        self.write(&line);
    }

    /// Oldest first.
    pub(crate) fn entries(&self) -> std::collections::vec_deque::Iter<'_, Entry> {
        self.ring.iter()
    }

    fn push(&mut self, e: Entry) {
        self.ring.push_back(e);
        while self.ring.len() > RING {
            self.ring.pop_front();
        }
    }

    /// The newest entry's repeats, written as one line now that its run is
    /// over. Its first occurrence is on disk already.
    fn end_run(&mut self) {
        let Some(last) = self.ring.back() else {
            return;
        };
        if last.repeats == 0 {
            return;
        }
        let line = self.line(last.at, last.kind, &last.text, Some((last.repeats, last.since)));
        self.write(&line);
    }

    fn line(&self, at: SystemTime, kind: Kind, text: &str, run: Option<(u32, SystemTime)>) -> String {
        let mut l = format!("{} {} {} {text}", rfc3339_utc(at), self.tag, kind.name());
        if let Some((n, since)) = run {
            l.push_str(&format!(" (repeated {n}\u{d7} since {})", hh_mm_utc(since)));
        }
        l
    }

    fn write(&mut self, line: &str) {
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(e) = file.write_line(line) {
            self.file = None;
            self.file_off(&e);
        }
    }

    /// The file is gone for the rest of the session: said once, in the ring.
    fn file_off(&mut self, e: &std::io::Error) {
        let at = SystemTime::now();
        self.push(Entry {
            at,
            since: at,
            kind: Kind::Log,
            text: oxutrm_client::summarised(&format!("log file off: {e}")),
            repeats: 0,
        });
    }
}

impl Drop for Activity {
    /// A run still folding when the session ends is written out, so the
    /// file says how the session ended.
    fn drop(&mut self) {
        self.end_run();
    }
}

/// `client.log`, opened for appending, rotated at a cap.
pub(crate) struct LogFile {
    path: PathBuf,
    file: File,
    cap: u64,
}

impl LogFile {
    pub(crate) fn open(path: PathBuf, cap: u64) -> std::io::Result<LogFile> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = append(&path)?;
        Ok(LogFile { path, file, cap })
    }

    fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        // Another client sharing the file may have rotated it, leaving our
        // handle on client.log.1. Appending there would take it past the
        // cap, so the name is followed rather than the handle.
        let ours = self.file.metadata()?;
        let named = std::fs::metadata(&self.path).ok();
        if named.is_none_or(|m| m.dev() != ours.dev() || m.ino() != ours.ino()) {
            self.file = append(&self.path)?;
        }
        let bytes = format!("{line}\n");
        let len = self.file.metadata()?.len();
        if len > 0 && len + bytes.len() as u64 > self.cap {
            std::fs::rename(&self.path, rotated(&self.path))?;
            self.file = append(&self.path)?;
        }
        // One write of the whole line: with O_APPEND, two clients appending
        // at once cannot interleave inside it.
        self.file.write_all(bytes.as_bytes())
    }
}

fn append(path: &Path) -> std::io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotated(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".1");
    PathBuf::from(name)
}

/// Where the log lives, from the two variables that decide it. A relative
/// or empty value is ignored, as the XDG base-directory spec requires.
pub(crate) fn state_path(xdg: Option<OsString>, home: Option<OsString>) -> Option<PathBuf> {
    let absolute = |v: Option<OsString>| v.map(PathBuf::from).filter(|p| p.is_absolute());
    let base = match absolute(xdg) {
        Some(p) => p,
        None => absolute(home)?.join(".local/state"),
    };
    Some(base.join("oxutrm").join("client.log"))
}

/// `t` as RFC 3339 in UTC, to the second.
pub(crate) fn rfc3339_utc(t: SystemTime) -> String {
    let secs = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) as i64;
    let (y, m, d) = civil(secs.div_euclid(86_400));
    let s = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        s / 3600,
        s / 60 % 60,
        s % 60
    )
}

fn hh_mm_utc(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()) % 86_400;
    format!("{:02}:{:02}", s / 3600, s / 60 % 60)
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace --jobs 4 activity:: -- --test-threads 4`
Expected: PASS.

- [ ] **Step 6: Injection**

1. In `LogFile::write_line`, delete the `if named.is_none_or(…) { … }` block. `a_rotation_by_another_client_is_followed_and_the_cap_holds` must FAIL. Restore.
2. In `Activity::write`, delete `self.file = None;`. `a_log_file_that_fails_mid_session_is_dropped_quietly` must FAIL (the failure is reported twice). Restore.
3. In `record_at`, replace `oxutrm_client::summarised(text)` with `text.to_string()`. `a_recorded_text_is_escaped_and_cut_to_one_line` must FAIL. Restore.
4. In `impl Drop for Activity`, empty the body. `a_run_still_folding_when_the_session_ends_is_written` must FAIL. Restore.

- [ ] **Step 7: Gate and commit**

Run the gate as in the Global Constraints; expected exit 0.

```bash
git add src/activity.rs src/main.rs
git commit -m "$(cat <<'EOF'
feat(client): an activity log in a ring and a capped file

Activity keeps the last 200 events for the status popup, folds repeats,
escapes what it is given, and appends each new event as one O_APPEND
line to $XDG_STATE_HOME/oxutrm/client.log, rotating to client.log.1
before 1 MiB. A file that cannot be written costs one entry, never the
session. Times are UTC.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 3: `quality.rs` — a minute of link measurements

**Files:**
- Create: `src/quality.rs`
- Modify: `src/main.rs` (`mod quality;`)

**Interfaces:**
- Consumes: nothing (plain values).
- Produces:
  - `pub(crate) const WINDOW: usize = 60; pub(crate) const SAMPLE_EVERY: Duration = 1 s; pub(crate) const THROUGHPUT_SPAN: Duration = 5 s;`
  - `pub(crate) struct Reading { pub(crate) rtt: Duration, pub(crate) sent: u64, pub(crate) lost: u64, pub(crate) tx_bytes: u64, pub(crate) rx_bytes: u64 }` — `Clone, Copy, PartialEq, Eq, Debug, Default`.
  - `pub(crate) struct RttStats { pub(crate) min: Duration, pub(crate) avg: Duration, pub(crate) max: Duration }`
  - `pub(crate) struct Loss { pub(crate) window_sent: u64, pub(crate) window_lost: u64, pub(crate) total_sent: u64, pub(crate) total_lost: u64 }` with `pub(crate) fn percent(&self) -> Option<f64>`
  - `pub(crate) struct Throughput { pub(crate) up: u64, pub(crate) down: u64 }` (bytes per second)
  - `pub(crate) struct Quality` with `new(now: Instant)`, `due(&self, now: Instant) -> bool`, `push(&mut self, at: Instant, reading: Reading, outage: bool)`, `new_segment(&mut self, now: Instant)`, `segment(&self) -> u32` (1-based), `segment_since(&self) -> Instant`, `rtt_now(&self) -> Option<Duration>`, `rtt_stats(&self) -> Option<RttStats>`, `loss(&self) -> Option<Loss>`, `throughput(&self) -> Option<Throughput>`, `sparkline(&self) -> Vec<Option<u64>>`.

- [ ] **Step 1: Declare the module**

In `src/main.rs`, after `mod loopback;`:

```rust
// Sampled and shown by the session in Task 6, which removes this attribute.
#[cfg_attr(not(test), allow(dead_code))]
mod quality;
```

- [ ] **Step 2: Write the tests**

Create `src/quality.rs` with the signatures stubbed (`todo!()`) and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn r(rtt_ms: u64, sent: u64, lost: u64, tx: u64, rx: u64) -> Reading {
        Reading {
            rtt: Duration::from_millis(rtt_ms),
            sent,
            lost,
            tx_bytes: tx,
            rx_bytes: rx,
        }
    }

    fn secs(t: Instant, s: u64) -> Instant {
        t + Duration::from_secs(s)
    }

    #[test]
    fn a_sample_is_due_at_most_once_a_second() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        assert!(q.due(t), "the first sample waited");
        q.push(t, r(30, 0, 0, 0, 0), false);
        assert!(!q.due(t + Duration::from_millis(999)));
        assert!(q.due(secs(t, 1)));
    }

    #[test]
    fn the_ring_holds_one_window() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        for i in 0..100 {
            q.push(secs(t, i), r(i, 0, 0, 0, 0), false);
        }
        let spark = q.sparkline();
        assert_eq!(spark.len(), WINDOW);
        assert_eq!(spark.first(), Some(&Some(40)), "the oldest kept is not the 41st");
    }

    #[test]
    fn rtt_statistics_cover_the_answered_seconds_of_this_link() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), r(40, 0, 0, 0, 0), false);
        q.push(secs(t, 2), r(900, 0, 0, 0, 0), true);
        q.push(secs(t, 3), r(50, 0, 0, 0, 0), false);

        let s = q.rtt_stats().expect("no statistics");
        assert_eq!(s.min, Duration::from_millis(30));
        assert_eq!(s.max, Duration::from_millis(50), "an outage second's RTT counted");
        assert_eq!(s.avg, Duration::from_millis(40));
        assert_eq!(q.rtt_now(), Some(Duration::from_millis(50)));
    }

    /// While the link is down quinn's estimate is a stale number, and
    /// showing it as "now" would claim a link that is not there.
    #[test]
    fn there_is_no_rtt_now_during_an_outage_and_its_seconds_are_gaps() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 0, 0, 0, 0), false);
        assert_eq!(q.rtt_now(), Some(Duration::from_millis(30)));
        q.push(secs(t, 1), r(30, 0, 0, 0, 0), true);
        assert_eq!(q.rtt_now(), None);
        assert_eq!(q.sparkline(), vec![Some(30), None]);
    }

    #[test]
    fn loss_is_measured_across_the_window_and_counted_for_the_link() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 1_000, 10, 0, 0), false);
        q.push(secs(t, 1), r(30, 1_100, 12, 0, 0), false);
        q.push(secs(t, 2), r(30, 1_200, 14, 0, 0), false);
        let l = q.loss().unwrap();
        assert_eq!((l.window_sent, l.window_lost), (200, 4));
        assert_eq!((l.total_sent, l.total_lost), (1_200, 14));
        assert_eq!(l.percent(), Some(2.0));
    }

    /// A swapped-in connection starts its counters at zero. A delta taken
    /// across the swap would be a huge negative -- saturated to nonsense --
    /// or the old link's loss charged to the new one.
    #[test]
    fn no_delta_is_ever_taken_across_two_links() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 5_000, 900, 9_000_000, 9_000_000), false);
        q.push(secs(t, 1), r(30, 5_100, 950, 9_100_000, 9_100_000), false);
        assert_eq!(q.segment(), 1);
        q.new_segment(secs(t, 2));
        assert_eq!(q.segment(), 2);
        assert_eq!(q.segment_since(), secs(t, 2));
        assert!(q.loss().is_none() && q.rtt_stats().is_none(), "the new link inherited samples");

        q.push(secs(t, 2), r(20, 10, 0, 1_000, 2_000), false);
        q.push(secs(t, 3), r(20, 20, 1, 2_000, 4_000), false);
        let l = q.loss().unwrap();
        assert_eq!((l.window_sent, l.window_lost, l.total_sent), (10, 1, 20));
        assert_eq!(q.throughput(), Some(Throughput { up: 1_000, down: 2_000 }));
        assert_eq!(q.rtt_stats().unwrap().max, Duration::from_millis(20));
    }

    #[test]
    fn throughput_is_averaged_over_the_last_five_seconds() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        // A burst ten seconds ago that must not count.
        q.push(t, r(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), r(30, 0, 0, 50_000_000, 0), false);
        for i in 2..=11 {
            let base = 50_000_000 + (i - 6).max(0) as u64 * 1_000;
            q.push(secs(t, i as u64), r(30, 0, 0, base, (i as u64) * 10_000), false);
        }
        // tx grows 1000 B/s from second 6 on; rx 10000 B/s throughout.
        assert_eq!(q.throughput(), Some(Throughput { up: 1_000, down: 10_000 }));
    }

    #[test]
    fn one_sample_is_not_a_rate() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        assert_eq!(q.throughput(), None);
        q.push(t, r(30, 0, 0, 10, 10), false);
        assert_eq!(q.throughput(), None);
        assert_eq!(q.loss().map(|l| l.percent()), Some(None));
    }

    #[test]
    fn the_sparkline_is_in_milliseconds() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(38, 0, 0, 0, 0), false);
        q.push(secs(t, 1), r(1_250, 0, 0, 0, 0), false);
        assert_eq!(q.sparkline(), vec![Some(38), Some(1_250)]);
    }
}
```

The throughput fixture, worked: samples at seconds 2..=11 have `tx = 50_000_000 + max(i-6, 0) * 1000`, so from second 6 to 11 tx rises by 5 000 over 5 s; `rx = i * 10_000`, rising 50 000 over the same 5 s. The newest sample is second 11; the oldest current-segment sample within 5 s of it is second 6.

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test --workspace --jobs 4 quality:: -- --test-threads 4`
Expected: FAIL with `not yet implemented`.

- [ ] **Step 4: Implement**

```rust
//! Link quality: a minute of measurements of the primary link, and the
//! numbers the status popup derives from them.
//!
//! Fed plain values -- the session reads them off quinn -- so nothing here
//! needs a network to test. At most one sample a second, taken on a lap the
//! client loop already runs.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Samples kept: one minute at one a second.
pub(crate) const WINDOW: usize = 60;

/// The least time between two samples.
pub(crate) const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// What throughput is averaged over.
pub(crate) const THROUGHPUT_SPAN: Duration = Duration::from_secs(5);

/// What one connection reports at one moment. Counters are cumulative for
/// that connection and start at zero with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Reading {
    /// quinn's smoothed estimate.
    pub(crate) rtt: Duration,
    pub(crate) sent: u64,
    pub(crate) lost: u64,
    pub(crate) tx_bytes: u64,
    pub(crate) rx_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sample {
    at: Instant,
    reading: Reading,
    /// Which link of the session it came from.
    segment: u32,
    /// Taken while the phase was an outage.
    outage: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RttStats {
    pub(crate) min: Duration,
    pub(crate) avg: Duration,
    pub(crate) max: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Loss {
    /// Sent and lost across the window, on the current link.
    pub(crate) window_sent: u64,
    pub(crate) window_lost: u64,
    /// Sent and lost over the current link's whole life.
    pub(crate) total_sent: u64,
    pub(crate) total_lost: u64,
}

impl Loss {
    /// Lost as a share of sent across the window; `None` when nothing was
    /// sent, which is not the same as nothing lost.
    pub(crate) fn percent(&self) -> Option<f64> {
        (self.window_sent > 0).then(|| self.window_lost as f64 * 100.0 / self.window_sent as f64)
    }
}

/// Bytes per second, up and down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Throughput {
    pub(crate) up: u64,
    pub(crate) down: u64,
}

pub(crate) struct Quality {
    ring: VecDeque<Sample>,
    segment: u32,
    segment_since: Instant,
}

impl Quality {
    pub(crate) fn new(now: Instant) -> Quality {
        Quality {
            ring: VecDeque::with_capacity(WINDOW + 1),
            segment: 1,
            segment_since: now,
        }
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.ring
            .back()
            .is_none_or(|s| now.saturating_duration_since(s.at) >= SAMPLE_EVERY)
    }

    pub(crate) fn push(&mut self, at: Instant, reading: Reading, outage: bool) {
        self.ring.push_back(Sample {
            at,
            reading,
            segment: self.segment,
            outage,
        });
        while self.ring.len() > WINDOW {
            self.ring.pop_front();
        }
    }

    /// A different connection carries the session from `now` on: a failover
    /// or a landed rebuild. Its counters start at zero, so nothing is ever
    /// subtracted across the boundary.
    pub(crate) fn new_segment(&mut self, now: Instant) {
        self.segment = self.segment.saturating_add(1);
        self.segment_since = now;
    }

    /// Which link of this session the current one is, counting from 1.
    pub(crate) fn segment(&self) -> u32 {
        self.segment
    }

    pub(crate) fn segment_since(&self) -> Instant {
        self.segment_since
    }

    fn current(&self) -> impl DoubleEndedIterator<Item = &Sample> + '_ {
        let segment = self.segment;
        self.ring.iter().filter(move |s| s.segment == segment)
    }

    /// The newest sample's RTT, or `None` while it was taken in an outage.
    pub(crate) fn rtt_now(&self) -> Option<Duration> {
        self.ring
            .back()
            .filter(|s| s.segment == self.segment && !s.outage)
            .map(|s| s.reading.rtt)
    }

    pub(crate) fn rtt_stats(&self) -> Option<RttStats> {
        let rtts: Vec<Duration> = self
            .current()
            .filter(|s| !s.outage)
            .map(|s| s.reading.rtt)
            .collect();
        let min = *rtts.iter().min()?;
        let max = *rtts.iter().max()?;
        let avg = rtts.iter().sum::<Duration>() / rtts.len() as u32;
        Some(RttStats { min, avg, max })
    }

    pub(crate) fn loss(&self) -> Option<Loss> {
        let first = self.current().next()?;
        let last = self.current().next_back()?;
        Some(Loss {
            window_sent: last.reading.sent.saturating_sub(first.reading.sent),
            window_lost: last.reading.lost.saturating_sub(first.reading.lost),
            total_sent: last.reading.sent,
            total_lost: last.reading.lost,
        })
    }

    pub(crate) fn throughput(&self) -> Option<Throughput> {
        let last = self.current().next_back()?;
        let first = self
            .current()
            .find(|s| last.at.saturating_duration_since(s.at) <= THROUGHPUT_SPAN)?;
        let span = last.at.saturating_duration_since(first.at);
        if span.is_zero() {
            return None;
        }
        let per_second =
            |from: u64, to: u64| (to.saturating_sub(from) as f64 / span.as_secs_f64()) as u64;
        Some(Throughput {
            up: per_second(first.reading.tx_bytes, last.reading.tx_bytes),
            down: per_second(first.reading.rx_bytes, last.reading.rx_bytes),
        })
    }

    /// One value a sample, in milliseconds, oldest first; an outage second
    /// is `None`, drawn as a gap.
    pub(crate) fn sparkline(&self) -> Vec<Option<u64>> {
        self.ring
            .iter()
            .map(|s| (!s.outage).then(|| s.reading.rtt.as_millis() as u64))
            .collect()
    }
}
```

`Sample` is private; the Interfaces block's consumers never name it.

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace --jobs 4 quality:: -- --test-threads 4`
Expected: PASS.

- [ ] **Step 6: Injection**

1. In `current`, change the filter's condition to `true || s.segment == segment`. `no_delta_is_ever_taken_across_two_links` must FAIL. Restore.
2. In `rtt_stats`, drop `.filter(|s| !s.outage)`. `rtt_statistics_cover_the_answered_seconds_of_this_link` must FAIL. Restore.
3. In `throughput`, replace the `find(…)` with `self.current().next()?`. `throughput_is_averaged_over_the_last_five_seconds` must FAIL. Restore.

- [ ] **Step 7: Gate and commit**

```bash
git add src/quality.rs src/main.rs
git commit -m "$(cat <<'EOF'
feat(client): keep a minute of link quality samples

Quality holds sixty one-second readings of the primary link -- RTT,
packets sent and lost, bytes each way -- tagged with the link they came
from and whether the link was down, and derives RTT min/avg/max, loss,
throughput over five seconds and a sparkline with gaps. No delta is ever
taken across a failover or a landed rebuild.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 4: `ui.rs` — the popup's state machine and key routing

**Files:**
- Create: `src/ui.rs`
- Modify: `src/main.rs` (`mod ui;`)

**Interfaces:**
- Consumes: `crate::linkstate::Phase` (`Live`, `Silent { since }`, `Recovering { attempt, next_try }`, `Confirming`; `Phase::is_outage()`).
- Produces:
  - `pub(crate) const PREFIX: u8 = 0x1c; pub(crate) const ESC: u8 = 0x1b; pub(crate) const DOUBLE_PRESS: Duration = 500 ms; pub(crate) const LINGER: Duration = 3 s;`
  - `pub(crate) enum Mode { Closed, Open { pressed: Option<Instant> }, Auto, Lingering { since: Instant, outage: Duration } }` — `Clone, Copy, PartialEq, Eq, Debug`.
  - `pub(crate) enum Command { Quit, SendHeld, DropHeld }`
  - `pub(crate) struct Routed { pub(crate) to_host: Vec<u8>, pub(crate) to_hold: Vec<u8>, pub(crate) command: Option<Command> }` — `Default, PartialEq, Eq, Debug`.
  - `pub(crate) enum LinkChange { WentSilent, Back { outage: Duration } }`
  - `pub(crate) struct Ui` with `new() -> Ui`, `mode(&self) -> Mode`, `visible(&self, phase: Phase) -> bool`, `tick(&mut self, phase: Phase, now: Instant) -> Option<LinkChange>`, `keys(&mut self, bytes: &[u8], phase: Phase, now: Instant) -> Routed`.

`linkstate.rs` is **not** touched in this task; its own prefix handling is folded away in Task 6, when the session switches over. Until then both exist, and only `linkstate`'s is live.

- [ ] **Step 1: Declare the module**

In `src/main.rs`, after `mod standby;`:

```rust
// Routes the keyboard and drives the popup from Task 6, which removes this
// attribute.
#[cfg_attr(not(test), allow(dead_code))]
mod ui;
```

- [ ] **Step 2: Write the tests**

Create `src/ui.rs` with the items from the Interfaces block (`Ui` methods as `todo!()`), and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn silent(t: Instant) -> Phase {
        Phase::Silent { since: t }
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    fn open_at(t: Instant) -> Ui {
        let mut ui = Ui::new();
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: Some(t) });
        ui
    }

    fn auto_at(t: Instant) -> Ui {
        let mut ui = Ui::new();
        ui.tick(silent(t), t);
        assert_eq!(ui.mode(), Mode::Auto);
        ui
    }

    fn host(bytes: &[u8]) -> Routed {
        Routed { to_host: bytes.to_vec(), ..Routed::default() }
    }

    fn hold(bytes: &[u8]) -> Routed {
        Routed { to_hold: bytes.to_vec(), ..Routed::default() }
    }

    fn command(c: Command) -> Routed {
        Routed { command: Some(c), ..Routed::default() }
    }

    // ---- a healthy link -------------------------------------------------

    #[test]
    fn typing_on_a_healthy_link_passes_through_untouched() {
        let mut ui = Ui::new();
        assert_eq!(ui.keys(b"ls -l\r", Phase::Live, Instant::now()), host(b"ls -l\r"));
        assert_eq!(ui.mode(), Mode::Closed);
        assert!(!ui.visible(Phase::Live));
    }

    #[test]
    fn ctrl_backslash_opens_the_popup_and_is_not_sent() {
        let ui = open_at(Instant::now());
        assert!(ui.visible(Phase::Live));
    }

    #[test]
    fn what_comes_before_ctrl_backslash_goes_to_the_host_and_what_follows_to_the_popup() {
        let mut ui = Ui::new();
        let r = ui.keys(b"ls\x1cq", Phase::Live, Instant::now());
        assert_eq!(r, Routed { to_host: b"ls".to_vec(), command: Some(Command::Quit), ..Routed::default() });
    }

    #[test]
    fn a_double_press_within_the_window_sends_one_literal() {
        let t = Instant::now();
        let mut ui = open_at(t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, ms(t, 499)), host(&[PREFIX]));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_second_press_after_the_window_only_closes() {
        let t = Instant::now();
        let mut ui = open_at(t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, ms(t, 500)), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_double_press_in_one_read_sends_one_literal() {
        let mut ui = Ui::new();
        assert_eq!(ui.keys(&[PREFIX, PREFIX], Phase::Live, Instant::now()), host(&[PREFIX]));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    /// Only the press that OPENED the popup starts the window: one opened
    /// by an outage has none, so a single `Ctrl-\` only closes it.
    #[test]
    fn a_popup_not_opened_by_ctrl_backslash_has_no_double_press() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        ui.tick(Phase::Live, ms(t, 100));
        assert!(matches!(ui.mode(), Mode::Lingering { .. }));
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, ms(t, 150)), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);
    }

    /// Typing never disappears into a popup by accident.
    #[test]
    fn any_other_key_closes_the_popup_and_reaches_the_host() {
        let mut ui = open_at(Instant::now());
        assert_eq!(ui.keys(b"xyz", Phase::Live, Instant::now()), host(b"xyz"));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_lone_esc_closes_but_an_escape_sequence_passes_through() {
        let mut ui = open_at(Instant::now());
        assert_eq!(ui.keys(&[ESC], Phase::Live, Instant::now()), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);

        let mut ui = open_at(Instant::now());
        assert_eq!(ui.keys(b"\x1b[A", Phase::Live, Instant::now()), host(b"\x1b[A"));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn q_quits_while_open() {
        let mut ui = open_at(Instant::now());
        assert_eq!(ui.keys(b"q", Phase::Live, Instant::now()), command(Command::Quit));
    }

    #[test]
    fn c_and_s_are_offered_later_and_do_nothing_now() {
        for key in [b'c', b's'] {
            let mut ui = open_at(Instant::now());
            assert_eq!(ui.keys(&[key], Phase::Live, Instant::now()), Routed::default());
            assert!(matches!(ui.mode(), Mode::Open { .. }), "{}", key as char);
        }
    }

    // ---- outages -------------------------------------------------------

    #[test]
    fn an_outage_opens_the_popup_by_itself_once() {
        let t = Instant::now();
        let mut ui = Ui::new();
        assert_eq!(ui.tick(Phase::Live, t), None);
        assert_eq!(ui.mode(), Mode::Closed);
        assert_eq!(ui.tick(silent(t), ms(t, 2_000)), Some(LinkChange::WentSilent));
        assert_eq!(ui.mode(), Mode::Auto);
        assert_eq!(ui.tick(silent(t), ms(t, 2_100)), None, "the outage started twice");
    }

    /// Nothing jumps.
    #[test]
    fn an_open_popup_stays_open_when_an_outage_starts() {
        let t = Instant::now();
        let mut ui = open_at(t);
        ui.tick(silent(t), ms(t, 10));
        assert_eq!(ui.mode(), Mode::Open { pressed: Some(t) });
    }

    #[test]
    fn the_popup_lingers_after_the_link_returns_then_closes() {
        let t = Instant::now();
        let mut ui = Ui::new();
        ui.tick(silent(t), ms(t, 2_000));
        let back = ms(t, 4_200);
        assert_eq!(
            ui.tick(Phase::Live, back),
            Some(LinkChange::Back { outage: Duration::from_millis(4_200) }),
            "the outage was not measured from when the host went quiet"
        );
        assert_eq!(ui.mode(), Mode::Lingering { since: back, outage: Duration::from_millis(4_200) });
        ui.tick(Phase::Live, back + LINGER - Duration::from_millis(1));
        assert!(ui.visible(Phase::Live), "closed early");
        ui.tick(Phase::Live, back + LINGER);
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_new_outage_while_lingering_reopens_it_as_an_outage() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        ui.tick(Phase::Live, ms(t, 100));
        assert_eq!(ui.tick(silent(ms(t, 200)), ms(t, 2_200)), Some(LinkChange::WentSilent));
        assert_eq!(ui.mode(), Mode::Auto);
    }

    #[test]
    fn in_an_outage_typing_is_held_not_sent() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"make test\r", silent(t), t), hold(b"make test\r"));
    }

    /// Review focus 1. Someone typing blind types `q`, `s`, `d` and `Esc` as
    /// letters; none of them may quit, send, drop or close.
    #[test]
    fn bare_letters_and_esc_are_typing_while_the_link_is_down() {
        let t = Instant::now();
        for phase in [silent(t), Phase::Recovering { attempt: 0, next_try: t }] {
            let mut ui = auto_at(t);
            assert_eq!(ui.keys(b"squeue -d", phase, t), hold(b"squeue -d"), "{phase:?}");
            assert_eq!(ui.keys(&[ESC], phase, t), hold(&[ESC]), "{phase:?}");
            assert!(ui.visible(phase), "the popup closed under {phase:?}");
        }
        let mut ui = Ui::new();
        assert_eq!(ui.keys(b"sd", Phase::Confirming, t), hold(b"sd"));
    }

    #[test]
    fn an_arrow_key_in_an_outage_is_held_whole() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"\x1b[A", silent(t), t), hold(b"\x1b[A"));
    }

    /// The popup a user touched does not close on its own afterwards.
    #[test]
    fn a_key_typed_during_the_outage_keeps_the_popup_open_after_it() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        ui.keys(b"x", silent(t), t);
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
        ui.tick(Phase::Live, ms(t, 100));
        ui.tick(Phase::Live, ms(t, 100) + LINGER);
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
    }

    #[test]
    fn ctrl_backslash_q_quits_in_an_outage_and_keeps_what_came_before() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        let r = ui.keys(b"ab\x1cq", silent(t), t);
        assert_eq!(r, Routed { to_hold: b"ab".to_vec(), command: Some(Command::Quit), ..Routed::default() });
    }

    #[test]
    fn send_and_drop_are_commands_only_under_confirming() {
        let t = Instant::now();
        for (key, want) in [(b's', Command::SendHeld), (b'd', Command::DropHeld)] {
            let mut ui = Ui::new();
            assert_eq!(ui.keys(&[PREFIX, key], Phase::Confirming, t), command(want));

            let mut ui = auto_at(t);
            assert_eq!(
                ui.keys(&[PREFIX, key], silent(t), t),
                hold(&[PREFIX, key]),
                "Ctrl-\\ {} was honoured under an outage",
                key as char
            );
        }
        let mut ui = Ui::new();
        assert_eq!(ui.keys(&[PREFIX, b'q'], Phase::Confirming, t), command(Command::Quit));
    }

    #[test]
    fn a_prefix_split_across_two_reads_still_commands() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"x\x1c", silent(t), t), hold(b"x"));
        assert_eq!(ui.keys(b"q", silent(t), t), command(Command::Quit));
    }

    #[test]
    fn an_unknown_key_after_the_prefix_is_held_with_the_prefix() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"\x1cz", silent(t), t), hold(b"\x1cz"));
    }

    /// A `Ctrl-\` whose letter never came belongs to what was showing when
    /// it was typed. Carried into `Confirming`, it would let the first key
    /// typed there answer the question.
    #[test]
    fn a_half_typed_prefix_does_not_survive_into_a_different_section() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        ui.keys(b"make test\r\x1c", silent(t), t);
        assert_eq!(ui.keys(b"send it", Phase::Confirming, t), hold(b"send it"));
    }

    /// And within the same section it does survive: a frame landing between
    /// the prefix and its letter must not eat the command.
    #[test]
    fn a_half_typed_prefix_survives_within_the_same_section() {
        let t = Instant::now();
        let mut ui = Ui::new();
        assert_eq!(ui.keys(&[PREFIX], Phase::Confirming, t), Routed::default());
        assert_eq!(ui.keys(b"d", Phase::Confirming, t), command(Command::DropHeld));
    }

    /// The link came back with nothing held: the prefix is gone and the next
    /// key is ordinary typing on a healthy link.
    #[test]
    fn a_half_typed_prefix_does_not_survive_the_link_coming_back() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        ui.keys(&[PREFIX], silent(t), t);
        assert_eq!(ui.keys(b"d", Phase::Live, t), host(b"d"));
    }

    #[test]
    fn confirming_forces_the_popup_visible() {
        let ui = Ui::new();
        assert!(!ui.visible(Phase::Live));
        assert!(ui.visible(Phase::Confirming));
    }

    #[test]
    fn a_lingering_popup_is_kept_by_its_own_keys_and_closed_by_typing() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        ui.tick(Phase::Live, ms(t, 100));
        assert_eq!(ui.keys(b"c", Phase::Live, ms(t, 200)), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: None });

        let mut ui = auto_at(t);
        ui.tick(Phase::Live, ms(t, 100));
        assert_eq!(ui.keys(b"ls", Phase::Live, ms(t, 200)), host(b"ls"));
        assert_eq!(ui.mode(), Mode::Closed);
    }
}
```

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test --workspace --jobs 4 ui:: -- --test-threads 4`
Expected: FAIL with `not yet implemented`.

- [ ] **Step 4: Implement**

```rust
//! The status popup's modal state, and where every keystroke goes.
//!
//! Pure, like `linkstate`: every method takes the current `Instant`, so the
//! whole machine is tested without sleeping, and nothing here holds a timer.
//! The loop calls [`Ui::tick`] on laps it already takes.
//!
//! The rule that decides the keys: **bare letters are popup commands only
//! while the link is `Live`.** Nothing typed can be lost then, because any
//! key that is not the popup's own closes it and goes to the host. While
//! typing is being held -- an outage, or the question afterwards -- every
//! bare byte is typing, and the popup's commands take the [`PREFIX`].

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant};

use crate::linkstate::Phase;

/// `Ctrl-\`. Opens the popup while the link is healthy; while typing is
/// held it is the prefix for the popup's commands.
pub(crate) const PREFIX: u8 = 0x1c;

/// A read that is exactly this byte is the Esc key. Followed by more bytes
/// in the same read it begins an escape sequence -- an arrow key, a function
/// key -- which is typing like any other.
pub(crate) const ESC: u8 = 0x1b;

/// How soon a second `Ctrl-\` must follow the one that opened the popup for
/// the pair to mean one literal `Ctrl-\` for the host. Measured between the
/// two reads' timestamps; nothing is armed.
pub(crate) const DOUBLE_PRESS: Duration = Duration::from_millis(500);

/// How long the popup stays up showing the outcome after the link it opened
/// for comes back.
pub(crate) const LINGER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Closed,
    /// Opened by hand, or touched by a key; stays until closed.
    Open {
        /// When the `Ctrl-\` that opened it arrived, for the double press.
        pressed: Option<Instant>,
    },
    /// Opened because the link went quiet.
    Auto,
    /// The link answers again after an `Auto` popup; closes `LINGER` after
    /// `since`.
    Lingering { since: Instant, outage: Duration },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Command {
    Quit,
    SendHeld,
    DropHeld,
}

/// Where one read's bytes go. A command ends the read: anything after it
/// belongs to whatever the command leads to, not to the buffer before it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Routed {
    pub(crate) to_host: Vec<u8>,
    pub(crate) to_hold: Vec<u8>,
    pub(crate) command: Option<Command>,
}

/// What a lap's phase changed, for the activity log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkChange {
    WentSilent,
    Back { outage: Duration },
}

/// What is being held, which decides the commands the prefix leads to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Holding {
    Outage,
    Confirming,
}

fn holding(phase: Phase) -> Option<Holding> {
    match phase {
        Phase::Silent { .. } | Phase::Recovering { .. } => Some(Holding::Outage),
        Phase::Confirming => Some(Holding::Confirming),
        Phase::Live => None,
    }
}

pub(crate) struct Ui {
    mode: Mode,
    /// A `Ctrl-\` typed while holding, whose letter has not arrived yet --
    /// it may come in the next read -- and what was held when it was typed.
    prefix: Option<Holding>,
    /// When the current outage began: the phase's own `Silent { since }`.
    outage_since: Option<Instant>,
    /// How long the last outage lasted, for the lingering popup.
    last_outage: Duration,
}

impl Ui {
    pub(crate) fn new() -> Ui {
        Ui {
            mode: Mode::Closed,
            prefix: None,
            outage_since: None,
            last_outage: Duration::ZERO,
        }
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the popup is on the screen. `Confirming` forces it: the held
    /// input is waiting on an answer, and the question is in the popup.
    pub(crate) fn visible(&self, phase: Phase) -> bool {
        self.mode != Mode::Closed || phase == Phase::Confirming
    }

    /// One lap's phase. Opens the popup when an outage begins, starts the
    /// linger when the link answers again, and ends it.
    pub(crate) fn tick(&mut self, phase: Phase, now: Instant) -> Option<LinkChange> {
        if phase.is_outage() {
            if matches!(self.mode, Mode::Closed | Mode::Lingering { .. }) {
                self.mode = Mode::Auto;
            }
            if self.outage_since.is_some() {
                return None;
            }
            // `Recovering` is only ever entered from `Silent`, so `now` is
            // never actually used -- but a lap that somehow saw `Recovering`
            // first still gets an outage that starts somewhere.
            self.outage_since = Some(match phase {
                Phase::Silent { since } => since,
                _ => now,
            });
            return Some(LinkChange::WentSilent);
        }

        let change = self.outage_since.take().map(|since| {
            self.last_outage = now.saturating_duration_since(since);
            LinkChange::Back {
                outage: self.last_outage,
            }
        });
        match self.mode {
            Mode::Auto if phase == Phase::Live => {
                self.mode = Mode::Lingering {
                    since: now,
                    outage: self.last_outage,
                };
            }
            Mode::Lingering { since, .. } if now.saturating_duration_since(since) >= LINGER => {
                self.mode = Mode::Closed;
            }
            _ => {}
        }
        change
    }

    /// One read from the keyboard: what goes to the host, what is held, and
    /// the command, if one was typed.
    pub(crate) fn keys(&mut self, bytes: &[u8], phase: Phase, now: Instant) -> Routed {
        let mut routed = Routed::default();
        let held = holding(phase);
        if self.prefix.is_some() && self.prefix != held {
            self.prefix = None;
        }
        let lone_esc = bytes == [ESC];
        for &b in bytes {
            let done = match held {
                Some(h) => self.held_key(b, h, &mut routed),
                None => self.live_key(b, lone_esc, now, &mut routed),
            };
            if done {
                break;
            }
        }
        routed
    }

    /// A key while typing is held. Returns whether the read is over.
    fn held_key(&mut self, b: u8, h: Holding, r: &mut Routed) -> bool {
        // Touched: it will not close on its own once the link is back.
        if self.mode == Mode::Auto {
            self.mode = Mode::Open { pressed: None };
        }
        if self.prefix.take().is_some() {
            let command = match b {
                b'q' => Some(Command::Quit),
                b's' if h == Holding::Confirming => Some(Command::SendHeld),
                b'd' if h == Holding::Confirming => Some(Command::DropHeld),
                _ => None,
            };
            if command.is_some() {
                r.command = command;
                return true;
            }
            // Not a command here, so the user meant to type both bytes.
            r.to_hold.extend_from_slice(&[PREFIX, b]);
            return false;
        }
        if b == PREFIX {
            self.prefix = Some(h);
        } else {
            r.to_hold.push(b);
        }
        false
    }

    /// A key while the link is healthy. Returns whether the read is over.
    fn live_key(&mut self, b: u8, lone_esc: bool, now: Instant, r: &mut Routed) -> bool {
        if self.mode == Mode::Closed {
            if b == PREFIX {
                self.mode = Mode::Open { pressed: Some(now) };
            } else {
                r.to_host.push(b);
            }
            return false;
        }
        match b {
            PREFIX => {
                if let Mode::Open { pressed: Some(at) } = self.mode
                    && now.saturating_duration_since(at) < DOUBLE_PRESS
                {
                    r.to_host.push(PREFIX);
                }
                self.mode = Mode::Closed;
            }
            ESC if lone_esc => self.mode = Mode::Closed,
            b'q' => {
                r.command = Some(Command::Quit);
                return true;
            }
            // Offered by a later version; pressing one still counts as
            // touching the popup.
            b'c' | b's' => self.mode = Mode::Open { pressed: None },
            _ => {
                self.mode = Mode::Closed;
                r.to_host.push(b);
            }
        }
        false
    }
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace --jobs 4 ui:: -- --test-threads 4`
Expected: PASS.

- [ ] **Step 6: Injection**

1. In `held_key`, change `b'q' => Some(Command::Quit)` so bare `q` quits: add, before the `if self.prefix.take()…` line, `if b == b'q' { r.command = Some(Command::Quit); return true; }`. `bare_letters_and_esc_are_typing_while_the_link_is_down` must FAIL. Restore.
2. In `keys`, compute `let lone_esc = bytes.first() == Some(&ESC);`. `a_lone_esc_closes_but_an_escape_sequence_passes_through` must FAIL. Restore.
3. In `keys`, delete the `if self.prefix.is_some() && self.prefix != held { … }` block. `a_half_typed_prefix_does_not_survive_into_a_different_section` must FAIL. Restore.
4. In `live_key`, change `< DOUBLE_PRESS` to `<= DOUBLE_PRESS`. `a_second_press_after_the_window_only_closes` must FAIL. Restore.

- [ ] **Step 7: Gate and commit**

```bash
git add src/ui.rs src/main.rs
git commit -m "$(cat <<'EOF'
feat(client): the status popup's states and key routing

Ui decides whether the popup is closed, open, opened by an outage or
lingering after one, and where each keystroke goes: on a healthy link
Ctrl-\ opens it, a quick second press sends one literal, Esc closes, q
quits and any other key closes it and reaches the host; while typing is
held every bare byte is kept and commands take the Ctrl-\ prefix.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 5: `view.rs` — the session's facts as a `PopupView`

**Files:**
- Create: `src/view.rs`
- Modify: `src/main.rs` (`mod view;`)

**Interfaces:**
- Consumes: `oxutrm_client::{KeyHint, Marker, PopupView, legible, rung_label, summarised}` (Task 1); `crate::activity::Activity` and `Entry` fields (Task 2); `crate::quality::Quality` (Task 3); `crate::linkstate::{Phase, ProbeState, render_held}`.
- Produces:
  - `pub(crate) struct Identity { pub(crate) target: String, pub(crate) session_id: String, pub(crate) attach_id: u64 }` — `Clone, Debug, PartialEq, Eq`.
  - `pub(crate) struct StandbyFacts<'a> { pub(crate) path: Option<&'a PathDescription>, pub(crate) rtt: Option<Duration>, pub(crate) probe: ProbeState, pub(crate) searching: bool, pub(crate) next_search: Instant, pub(crate) last_failure: Option<&'a str> }`
  - `pub(crate) struct Facts<'a> { pub(crate) identity: Option<&'a Identity>, pub(crate) phase: Phase, pub(crate) lingering: Option<Duration>, pub(crate) last_heard: Instant, pub(crate) path: Option<&'a PathDescription>, pub(crate) quality: &'a Quality, pub(crate) rejected: u64, pub(crate) standby: Option<StandbyFacts<'a>>, pub(crate) rebuild_failure: Option<&'a str>, pub(crate) held: &'a [u8], pub(crate) held_full: bool, pub(crate) activity: &'a Activity, pub(crate) now: Instant, pub(crate) wall: SystemTime }`
  - `pub(crate) fn build(f: &Facts<'_>) -> PopupView`
  - `pub(crate) fn path_label(path: Option<&PathDescription>) -> String` (`rung_label`, or `"this link"`)
  - `pub(crate) fn age(d: Duration) -> String` (`"12s"`, `"4m"`, `"3h"`)
  - `pub(crate) const MAX_LOG_LINES: usize = 22;`
  - `#[cfg(test)] pub(crate) fn words(v: &PopupView) -> String` — every section's text except the log, for tests here and in `session.rs`.

- [ ] **Step 1: Declare the module**

In `src/main.rs`, after `mod ui;`:

```rust
// Built by the session from Task 6, which removes this attribute.
#[cfg_attr(not(test), allow(dead_code))]
mod view;
```

- [ ] **Step 2: Write the tests**

Create `src/view.rs` with the Interfaces items (`build` as `todo!()`, the helpers as `todo!()`, `words` final as below) and this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Kind;
    use oxutrm_proto::{NatType, Rung};

    fn path() -> PathDescription {
        PathDescription {
            rung: Rung::StunPunch,
            local: "127.0.0.1:1".parse().unwrap(),
            remote: "203.0.113.7:443".parse().unwrap(),
            probes_sent: 0,
            nat_type: NatType::Unknown,
            rtt_ms: 38,
            mtu: 1400,
        }
    }

    fn facts<'a>(q: &'a Quality, a: &'a Activity, phase: Phase, now: Instant) -> Facts<'a> {
        Facts {
            identity: None,
            phase,
            lingering: None,
            last_heard: now,
            path: None,
            quality: q,
            rejected: 0,
            standby: None,
            rebuild_failure: None,
            held: &[],
            held_full: false,
            activity: a,
            now,
            wall: SystemTime::now(),
        }
    }

    fn reading(rtt_ms: u64, sent: u64, lost: u64, tx: u64, rx: u64) -> crate::quality::Reading {
        crate::quality::Reading {
            rtt: Duration::from_millis(rtt_ms),
            sent,
            lost,
            tx_bytes: tx,
            rx_bytes: rx,
        }
    }

    fn secs(t: Instant, s: u64) -> Instant {
        t + Duration::from_secs(s)
    }

    #[test]
    fn the_title_names_the_target_and_a_row_names_the_session() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let bare = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(bare.title, "oxutrm");
        assert!(!words(&bare).contains("session "), "{}", words(&bare));

        let id = Identity {
            target: "bastion".to_string(),
            session_id: "f00dcafe0123".to_string(),
            attach_id: 3,
        };
        let v = build(&Facts { identity: Some(&id), ..facts(&q, &a, Phase::Live, t) });
        assert_eq!(v.title, "oxutrm \u{b7} bastion");
        assert!(v.status.contains(&"session f00dcafe \u{b7} attach 3".to_string()), "{:?}", v.status);
    }

    #[test]
    fn the_marker_follows_the_phase() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        for (phase, marker, text) in [
            (Phase::Live, Marker::Live, "\u{25cf} LIVE"),
            (Phase::Confirming, Marker::Live, "\u{25cf} LIVE"),
            (Phase::Silent { since: t }, Marker::Silent, "\u{25cf} SILENT"),
            (Phase::Recovering { attempt: 0, next_try: t }, Marker::Recovering, "\u{25cf} RECOVERING"),
        ] {
            let v = build(&facts(&q, &a, phase, t));
            assert_eq!((v.marker, v.marker_text.as_str()), (marker, text), "{phase:?}");
        }
    }

    #[test]
    fn a_lingering_popup_says_how_the_link_came_back_and_how_long_it_was_gone() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        let v = build(&Facts {
            lingering: Some(Duration::from_millis(4_240)),
            path: Some(&p),
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.marker, Marker::LiveAgain);
        assert_eq!(v.marker_text, "\u{25cf} LIVE again via IPv4 punched \u{b7} outage 4.2 s");

        // A new outage while lingering is an outage, whatever came before.
        let v = build(&Facts {
            lingering: Some(Duration::from_secs(4)),
            ..facts(&q, &a, Phase::Silent { since: t }, t)
        });
        assert_eq!(v.marker, Marker::Silent);
    }

    /// Truncated: "at least this long", the way every stopwatch reads. A
    /// rounded counter would overstate an outage the user may act on.
    #[test]
    fn the_silence_is_counted_in_whole_seconds_from_when_the_host_went_quiet() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let v = build(&facts(&q, &a, Phase::Silent { since: t }, t + Duration::from_millis(6_900)));
        assert_eq!(v.status.first().map(String::as_str), Some("silent for 6s"));
    }

    #[test]
    fn rtt_is_a_dash_while_the_link_is_down() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        q.push(t, reading(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), reading(40, 0, 0, 0, 0), false);
        let up = build(&facts(&q, &a, Phase::Live, secs(t, 1)));
        assert!(up.status.contains(&"rtt 40 ms \u{b7} min 30 / avg 35 / max 40 ms".to_string()), "{:?}", up.status);

        q.push(secs(t, 2), reading(40, 0, 0, 0, 0), true);
        let down = build(&facts(&q, &a, Phase::Silent { since: t }, secs(t, 2)));
        assert!(down.status.contains(&"rtt \u{2014} \u{b7} min 30 / avg 35 / max 40 ms".to_string()), "{:?}", down.status);
    }

    #[test]
    fn loss_and_throughput_have_a_row_each_once_there_is_something_to_say() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        let empty = build(&facts(&q, &a, Phase::Live, t));
        assert!(!words(&empty).contains("loss"), "{}", words(&empty));

        for i in 0..=5 {
            q.push(secs(t, i), reading(30, 100 + i * 20, i / 5, 1_000 + i * 1_000, 5_000 + i * 10_000), false);
        }
        let v = build(&facts(&q, &a, Phase::Live, secs(t, 5)));
        assert!(v.status.contains(&"loss 1.0% in the last minute \u{b7} sent 200 lost 1".to_string()), "{:?}", v.status);
        assert!(v.status.contains(&"\u{2191} 1.0 kB/s \u{2193} 10.0 kB/s".to_string()), "{:?}", v.status);
    }

    #[test]
    fn the_path_row_counts_links_and_their_age() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        q.new_segment(t);
        let p = path();
        let v = build(&Facts { path: Some(&p), ..facts(&q, &a, Phase::Live, secs(t, 125)) });
        assert!(v.status.contains(&"IPv4 punched \u{b7} link 2 of this session, up 2m".to_string()), "{:?}", v.status);
    }

    #[test]
    fn rejected_frames_are_reported_only_when_there_are_any() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        assert!(!words(&build(&facts(&q, &a, Phase::Live, t))).contains("rejected"));
        let v = build(&Facts { rejected: 3, ..facts(&q, &a, Phase::Live, t) });
        assert!(v.status.contains(&"screen frames rejected: 3".to_string()));
    }

    fn standby<'a>(path: Option<&'a PathDescription>, t: Instant) -> StandbyFacts<'a> {
        StandbyFacts {
            path,
            rtt: path.map(|_| Duration::from_millis(41)),
            probe: ProbeState::Idle,
            searching: false,
            next_search: secs(t, 270),
            last_failure: None,
        }
    }

    #[test]
    fn the_standby_block_says_what_the_standby_is_doing() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        let row = |s: StandbyFacts<'_>| build(&Facts { standby: Some(s), ..facts(&q, &a, Phase::Live, t) }).standby;

        assert_eq!(row(standby(Some(&p), t)), ["standby IPv4 punched \u{b7} 41 ms"]);
        assert_eq!(
            row(StandbyFacts { probe: ProbeState::Pending { sent: t }, ..standby(Some(&p), t) }),
            ["standby IPv4 punched \u{b7} 41 ms \u{b7} probing"]
        );
        assert_eq!(
            row(StandbyFacts { probe: ProbeState::Answered { sent: t }, ..standby(Some(&p), t) }),
            ["standby IPv4 punched \u{b7} 41 ms \u{b7} probe answered"]
        );
        assert_eq!(
            row(StandbyFacts { probe: ProbeState::Failed { at: t }, ..standby(Some(&p), t) }),
            ["standby IPv4 punched \u{b7} 41 ms \u{b7} probe failed"]
        );
        assert_eq!(row(StandbyFacts { searching: true, ..standby(None, t) }), ["standby: searching\u{2026}"]);
        assert_eq!(row(standby(None, t)), ["standby: none \u{b7} next search in 4 m 30 s"]);
        assert!(build(&facts(&q, &a, Phase::Live, t)).standby.is_empty(), "a host with no standby got a block");
    }

    /// Review focus 5: the reason is the far end's own words.
    #[test]
    fn a_search_failure_from_the_far_end_is_shown_escaped_and_short() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let s = StandbyFacts { last_failure: Some("\u{1b}[2Jgone\u{9b}1m\nsecond line"), ..standby(None, t) };
        let v = build(&Facts { standby: Some(s), ..facts(&q, &a, Phase::Live, t) });
        let last = v.standby.last().unwrap();
        assert!(last.starts_with("last search: ^[[2Jgone"), "{last:?}");
        assert!(!last.chars().any(char::is_control), "{last:?}");
        assert!(!last.contains("second line"), "{last:?}");
    }

    #[test]
    fn the_recovering_section_reports_quiet_time_attempt_countdown_and_reason() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let now = secs(t, 23);
        let phase = Phase::Recovering { attempt: 2, next_try: now + Duration::from_secs(5) };
        let bare = build(&Facts { last_heard: t, ..facts(&q, &a, phase, now) });
        // `attempt` is zero-based; the user reads the third attempt as 3.
        assert_eq!(bare.recovering, ["host quiet for 23s", "reconnect attempt 3", "next try in 5s"]);

        let why = build(&Facts {
            last_heard: t,
            rebuild_failure: Some("ssh exited with status 255: Permission denied (publickey)"),
            ..facts(&q, &a, phase, now)
        });
        assert_eq!(
            why.recovering.last().map(String::as_str),
            Some("last attempt: ssh exited with status 255: Permission denied (publickey)")
        );
        assert!(build(&facts(&q, &a, Phase::Silent { since: t }, now)).recovering.is_empty());
    }

    #[test]
    fn held_typing_is_reported_during_the_outage_and_asked_about_after() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let typed = b"make test\r";
        let silent = Phase::Silent { since: t };
        assert!(build(&facts(&q, &a, silent, t)).held.is_empty(), "a held section with nothing held");

        let v = build(&Facts { held: typed, ..facts(&q, &a, silent, t) });
        assert_eq!(v.held, ["10 bytes typed since - kept, not sent"]);
        let full = build(&Facts { held: typed, held_full: true, ..facts(&q, &a, silent, t) });
        assert_eq!(full.held.last().map(String::as_str), Some("The buffer is full; later keys are not being kept."));

        let ask = build(&Facts { held: typed, ..facts(&q, &a, Phase::Confirming, t) });
        assert_eq!(
            ask.held,
            [
                "the host is answering again - deliver what you typed?",
                "You typed 10 bytes while offline:",
                "make test\u{21b5}",
            ]
        );
    }

    #[test]
    fn the_key_bar_offers_what_works_in_each_phase() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let bar = |phase| -> Vec<(String, bool)> {
            build(&facts(&q, &a, phase, t))
                .keys
                .into_iter()
                .map(|k| (format!("{} {}", k.key, k.label), k.enabled))
                .collect()
        };
        let own = |s: &str, on: bool| (s.to_string(), on);
        assert_eq!(
            bar(Phase::Live),
            [own("Esc close", true), own("q quit", true), own("c config", false), own("s sessions", false)]
        );
        assert_eq!(bar(Phase::Silent { since: t }), [own("Ctrl-\\ q quit", true)]);
        assert_eq!(
            bar(Phase::Confirming),
            [own("Ctrl-\\ s send", true), own("Ctrl-\\ d drop", true), own("Ctrl-\\ q quit", true)]
        );
    }

    #[test]
    fn the_log_shows_the_newest_entries_with_their_age_and_count() {
        let t = Instant::now();
        let q = Quality::new(t);
        let mut a = Activity::new();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        for i in 0..30u64 {
            a.record_at(Kind::Link, &format!("e-{i:02}"), base + Duration::from_secs(i));
        }
        a.record_at(Kind::Link, "e-29", base + Duration::from_secs(29));
        let v = build(&Facts { wall: base + Duration::from_secs(29), ..facts(&q, &a, Phase::Live, t) });
        assert_eq!(v.log.len(), MAX_LOG_LINES);
        assert_eq!(v.log.first().map(String::as_str), Some(" 21s link e-08"));
        assert_eq!(v.log.last().map(String::as_str), Some("  0s link e-29 (\u{d7}2)"));
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(age(Duration::from_millis(59_999)), "59s");
        assert_eq!(age(Duration::from_secs(60)), "1m");
        assert_eq!(age(Duration::from_secs(3_599)), "59m");
        assert_eq!(age(Duration::from_secs(7_300)), "2h");
    }

    /// In `Silent` and `Confirming` nothing is reconnecting -- the rebuild
    /// starts only in `Recovering` -- and from here a dead network and a
    /// crashed host look the same, so those views may not vouch for the far
    /// end, even hedged.
    #[test]
    fn the_views_of_an_outage_claim_nothing_the_client_cannot_see() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        for phase in [Phase::Silent { since: t }, Phase::Confirming] {
            let shown = words(&build(&Facts { held: b"x", ..facts(&q, &a, phase, t) })).to_lowercase();
            for claim in ["safe", "reconnect", "retry", "keeps running", "still running", "is running"] {
                assert!(!shown.contains(claim), "{phase:?} says {claim:?}: {shown}");
            }
        }
    }
}
```

The loss fixture, worked: six samples `i = 0..=5`, `sent = 100 + 20i` (100→200, Δ100), `lost = i / 5` (0→1), `tx = 1000 + 1000i` (Δ5000 over 5 s = 1000 B/s), `rx = 5000 + 10000i` (Δ50000 over 5 s = 10000 B/s).

- [ ] **Step 3: Run the tests to see them fail**

Run: `cargo test --workspace --jobs 4 view:: -- --test-threads 4`
Expected: FAIL with `not yet implemented`.

- [ ] **Step 4: Implement**

```rust
//! What the status popup says, assembled from what the session knows.
//!
//! Pure: [`build`] takes plain facts and returns a [`PopupView`], so every
//! word of the popup is tested without a network or a terminal. Every number
//! in it that moves by itself is in whole seconds, which is what lets the
//! loop compare a freshly built view with the one on the screen and repaint
//! at most once a second while nothing else changes.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant, SystemTime};

use oxutrm_client::{KeyHint, Marker, PopupView, legible, rung_label, summarised};
use oxutrm_proto::PathDescription;

use crate::activity::Activity;
use crate::linkstate::{Phase, ProbeState, render_held};
use crate::quality::Quality;

/// The most log lines a view carries: the tallest box has fewer rows than
/// this, so building more would only be work for the layout to throw away.
pub(crate) const MAX_LOG_LINES: usize = 22;

/// Which session this is: the ssh target typed, and the host's names for the
/// session and for this attach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) target: String,
    pub(crate) session_id: String,
    pub(crate) attach_id: u64,
}

pub(crate) struct StandbyFacts<'a> {
    pub(crate) path: Option<&'a PathDescription>,
    pub(crate) rtt: Option<Duration>,
    pub(crate) probe: ProbeState,
    pub(crate) searching: bool,
    pub(crate) next_search: Instant,
    pub(crate) last_failure: Option<&'a str>,
}

pub(crate) struct Facts<'a> {
    pub(crate) identity: Option<&'a Identity>,
    pub(crate) phase: Phase,
    /// How long the outage was, while the popup lingers after it.
    pub(crate) lingering: Option<Duration>,
    pub(crate) last_heard: Instant,
    /// The primary's path, as announced or as last swapped in.
    pub(crate) path: Option<&'a PathDescription>,
    pub(crate) quality: &'a Quality,
    pub(crate) rejected: u64,
    /// `None` for a host that offered no standby.
    pub(crate) standby: Option<StandbyFacts<'a>>,
    /// Why the last rebuild attempt failed.
    pub(crate) rebuild_failure: Option<&'a str>,
    pub(crate) held: &'a [u8],
    pub(crate) held_full: bool,
    pub(crate) activity: &'a Activity,
    pub(crate) now: Instant,
    /// The wall clock, for the log's ages.
    pub(crate) wall: SystemTime,
}

pub(crate) fn build(f: &Facts<'_>) -> PopupView {
    let (marker, marker_text) = marker(f);
    PopupView {
        title: f.identity.map_or_else(
            || "oxutrm".to_string(),
            |i| format!("oxutrm \u{b7} {}", legible(&i.target)),
        ),
        marker,
        marker_text,
        held: held(f),
        recovering: recovering(f),
        status: status(f),
        standby: f
            .standby
            .as_ref()
            .map_or_else(Vec::new, |s| standby(s, f.now)),
        spark: f.quality.sparkline(),
        log: log(f.activity, f.wall),
        keys: keys(f.phase),
    }
}

/// How the primary is reached, in the words the connect line used.
pub(crate) fn path_label(path: Option<&PathDescription>) -> String {
    path.map_or_else(|| "this link".to_string(), rung_label)
}

/// A duration in its largest whole unit: `12s`, `4m`, `3h`.
pub(crate) fn age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

fn marker(f: &Facts<'_>) -> (Marker, String) {
    match (f.phase, f.lingering) {
        (Phase::Live, Some(outage)) => (
            Marker::LiveAgain,
            format!(
                "\u{25cf} LIVE again via {} \u{b7} outage {:.1} s",
                path_label(f.path),
                outage.as_secs_f64()
            ),
        ),
        (Phase::Silent { .. }, _) => (Marker::Silent, "\u{25cf} SILENT".to_string()),
        (Phase::Recovering { .. }, _) => (Marker::Recovering, "\u{25cf} RECOVERING".to_string()),
        _ => (Marker::Live, "\u{25cf} LIVE".to_string()),
    }
}

fn held(f: &Facts<'_>) -> Vec<String> {
    let n = f.held.len();
    match f.phase {
        // What was observed is a frame arriving; nothing "reconnected" --
        // the connection may never have dropped.
        Phase::Confirming => {
            let mut rows = vec![
                "the host is answering again - deliver what you typed?".to_string(),
                format!("You typed {n} bytes while offline:"),
                render_held(f.held),
            ];
            if f.held_full {
                rows.push("The buffer is full; later keys were not kept.".to_string());
            }
            rows
        }
        // Someone typing into a dead screen cannot tell "kept" from
        // "discarded" until the question comes; this is the only place
        // that can tell them while it still matters. Present tense for the
        // cap: one they hear about afterwards is one they could not act on.
        phase if phase.is_outage() && n > 0 => {
            let mut rows = vec![format!("{n} bytes typed since - kept, not sent")];
            if f.held_full {
                rows.push("The buffer is full; later keys are not being kept.".to_string());
            }
            rows
        }
        _ => Vec::new(),
    }
}

fn recovering(f: &Facts<'_>) -> Vec<String> {
    let Phase::Recovering { attempt, next_try } = f.phase else {
        return Vec::new();
    };
    // `attempt` is zero-based, which is right where it indexes the backoff;
    // a person reading "reconnect attempt 0" would read it as a bug.
    let mut rows = vec![
        format!("host quiet for {}s", f.now.saturating_duration_since(f.last_heard).as_secs()),
        format!("reconnect attempt {}", attempt.saturating_add(1)),
        format!("next try in {}s", next_try.saturating_duration_since(f.now).as_secs()),
    ];
    if let Some(why) = f.rebuild_failure {
        rows.push(format!("last attempt: {}", summarised(why)));
    }
    rows
}

fn status(f: &Facts<'_>) -> Vec<String> {
    let q = f.quality;
    let mut rows = Vec::new();
    if let Phase::Silent { since } = f.phase {
        // Truncated: "at least this long", as every stopwatch reads.
        rows.push(format!("silent for {}s", f.now.saturating_duration_since(since).as_secs()));
    }
    let mut rtt = match q.rtt_now() {
        Some(r) => format!("rtt {} ms", r.as_millis()),
        None => "rtt \u{2014}".to_string(),
    };
    if let Some(s) = q.rtt_stats() {
        rtt.push_str(&format!(
            " \u{b7} min {} / avg {} / max {} ms",
            s.min.as_millis(),
            s.avg.as_millis(),
            s.max.as_millis()
        ));
    }
    rows.push(rtt);
    if let Some(l) = q.loss() {
        let pct = l
            .percent()
            .map_or_else(|| "\u{2014}".to_string(), |p| format!("{p:.1}%"));
        rows.push(format!(
            "loss {pct} in the last minute \u{b7} sent {} lost {}",
            l.total_sent, l.total_lost
        ));
    }
    if let Some(t) = q.throughput() {
        rows.push(format!("\u{2191} {} \u{2193} {}", rate(t.up), rate(t.down)));
    }
    rows.push(format!(
        "{} \u{b7} link {} of this session, up {}",
        path_label(f.path),
        q.segment(),
        age(f.now.saturating_duration_since(q.segment_since()))
    ));
    if let Some(id) = f.identity {
        let prefix: String = id.session_id.chars().take(8).collect();
        rows.push(format!("session {} \u{b7} attach {}", legible(&prefix), id.attach_id));
    }
    if f.rejected > 0 {
        rows.push(format!("screen frames rejected: {}", f.rejected));
    }
    rows
}

fn standby(s: &StandbyFacts<'_>, now: Instant) -> Vec<String> {
    let mut rows = Vec::new();
    match s.path {
        Some(p) => {
            let rtt = s
                .rtt
                .map_or_else(|| "\u{2014}".to_string(), |r| format!("{} ms", r.as_millis()));
            let probe = match s.probe {
                ProbeState::Idle => "",
                ProbeState::Pending { .. } => " \u{b7} probing",
                ProbeState::Answered { .. } => " \u{b7} probe answered",
                ProbeState::Failed { .. } => " \u{b7} probe failed",
            };
            rows.push(format!("standby {} \u{b7} {rtt}{probe}", rung_label(p)));
        }
        None if s.searching => rows.push("standby: searching\u{2026}".to_string()),
        None => {
            let wait = s.next_search.saturating_duration_since(now).as_secs();
            rows.push(format!(
                "standby: none \u{b7} next search in {} m {} s",
                wait / 60,
                wait % 60
            ));
        }
    }
    if s.path.is_none()
        && let Some(why) = s.last_failure
    {
        rows.push(format!("last search: {}", summarised(why)));
    }
    rows
}

fn log(a: &Activity, wall: SystemTime) -> Vec<String> {
    let skip = a.entries().len().saturating_sub(MAX_LOG_LINES);
    a.entries()
        .skip(skip)
        .map(|e| {
            let age = age(wall.duration_since(e.at).unwrap_or_default());
            let count = if e.repeats > 0 {
                format!(" (\u{d7}{})", e.repeats.saturating_add(1))
            } else {
                String::new()
            };
            format!("{age:>4} {} {}{count}", e.kind.name(), e.text)
        })
        .collect()
}

fn keys(phase: Phase) -> Vec<KeyHint> {
    let hint = |key: &str, label: &str, enabled: bool| KeyHint {
        key: key.to_string(),
        label: label.to_string(),
        enabled,
    };
    match phase {
        Phase::Confirming => vec![
            hint("Ctrl-\\ s", "send", true),
            hint("Ctrl-\\ d", "drop", true),
            hint("Ctrl-\\ q", "quit", true),
        ],
        phase if phase.is_outage() => vec![hint("Ctrl-\\ q", "quit", true)],
        _ => vec![
            hint("Esc", "close", true),
            hint("q", "quit", true),
            hint("c", "config", false),
            hint("s", "sessions", false),
        ],
    }
}

fn rate(bytes_per_second: u64) -> String {
    match bytes_per_second {
        b if b < 1_000 => format!("{b} B/s"),
        b if b < 1_000_000 => format!("{:.1} kB/s", b as f64 / 1e3),
        b => format!("{:.1} MB/s", b as f64 / 1e6),
    }
}

/// Every word of `v` except the log, which is history and carries words
/// from earlier phases legitimately.
#[cfg(test)]
pub(crate) fn words(v: &PopupView) -> String {
    std::iter::once(v.marker_text.clone())
        .chain([&v.held, &v.recovering, &v.status, &v.standby].into_iter().flatten().cloned())
        .chain(v.keys.iter().map(|k| format!("{} {}", k.key, k.label)))
        .collect::<Vec<_>>()
        .join(" | ")
}
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --workspace --jobs 4 view:: -- --test-threads 4`
Expected: PASS.

- [ ] **Step 6: Injection**

1. In `recovering`, show `attempt` instead of `attempt.saturating_add(1)`. `the_recovering_section_reports_quiet_time_attempt_countdown_and_reason` must FAIL. Restore.
2. In `standby`, replace `summarised(why)` with `why.to_string()`. `a_search_failure_from_the_far_end_is_shown_escaped_and_short` must FAIL. Restore.
3. In `keys`, add `hint("q", "quit", true)` to the outage bar. `the_key_bar_offers_what_works_in_each_phase` must FAIL. Restore.
4. In `log`, drop `.skip(skip)`. `the_log_shows_the_newest_entries_with_their_age_and_count` must FAIL. Restore.

- [ ] **Step 7: Gate and commit**

```bash
git add src/view.rs src/main.rs
git commit -m "$(cat <<'EOF'
feat(client): build the status popup's view from the session's facts

view::build turns the phase, the quality ring, the standby, the rebuild
loop's last failure, held typing and the activity log into a PopupView:
marker, held and recovering sections, status rows, the standby block,
the sparkline, the newest log lines with their ages, and a key bar that
offers what works in each phase. Pure, so every word is tested here.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 6: The popup replaces the notice in the session

**Files:**
- Modify: `src/session.rs` (client half and its tests)
- Modify: `src/linkstate.rs` (fold the prefix out; tests)
- Modify: `src/standby.rs` (read-only accessors)
- Modify: `src/connect.rs` (identity)
- Modify: `src/main.rs` (drop the attributes on `mod quality; mod ui; mod view;`)
- Delete: `crates/oxutrm-client/src/notice.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`, `crates/oxutrm-client/src/overlay.rs` (module doc)

**Interfaces:**
- Consumes: everything Tasks 1–5 produce.
- Produces (in `session.rs`, used by Tasks 7 and 8):
  - `ClientSession` fields `ui: Ui`, `quality: Quality`, `activity: Activity`, `identity: Option<Identity>`, `path: Option<PathDescription>` (renamed from `announced`), `shown: Option<PopupView>`; `built` and `NOTICE_REFRESH` are gone.
  - `pub(crate) fn with_identity(self, id: Identity) -> ClientSession`
  - `fn popup_at(&mut self, now: Instant) -> Option<PopupView>` (replaces `notice_at`)
  - `fn sample_quality(&mut self, now: Instant)`
  - `fn view(&self, phase: Phase, now: Instant) -> PopupView`
  - `fn route_keys<W: Write>(&mut self, keys: &[u8], out: &mut W) -> Result<Option<i32>>` (same signature, new body)
  - `#[cfg(test)] fn outage_at(&mut self, now: Instant) -> bool`
  - free fn `fn reading_of(conn: &quinn::Connection) -> crate::quality::Reading`
- Produces (in `standby.rs`): `pub(crate) fn path(&self) -> Option<&PathDescription>`, `pub(crate) fn rtt(&self) -> Option<Duration>`, `pub(crate) fn probe(&self) -> ProbeState`, `pub(crate) fn searching(&self) -> bool`; `next_search()` loses `#[cfg(test)]`, `last_failure()` loses its `cfg_attr`.
- Produces (in `linkstate.rs`): `pub fn hold_keys(&mut self, bytes: &[u8])` (no return value); `PREFIX`, `Command` and `prefix_pending` removed.

- [ ] **Step 1: Write the new session tests**

Add to `session.rs`'s test module (near the other popup tests, after `with_confirming_popup` from Step 6):

```rust
    /// The whole point of the key on a healthy link: it opens the popup and
    /// is not sent.
    #[tokio::test]
    async fn ctrl_backslash_opens_the_popup_on_a_healthy_session() {
        let (_host, mut session) = pair("/bin/sh").await;
        let now = Instant::now();
        assert!(session.popup_at(now).is_none(), "the popup was up before the key");
        let before = spoken(&session);
        let mut out = Vec::new();

        assert_eq!(session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap(), None);

        assert_eq!(spoken(&session), before, "the key reached the host");
        let v = session.popup_at(now).expect("the key opened nothing");
        assert_eq!(v.marker, Marker::Live);
        assert!(crate::view::words(&v).contains("Esc close"), "{}", crate::view::words(&v));
    }

    #[tokio::test]
    async fn typing_into_an_open_popup_on_a_healthy_session_closes_it_and_reaches_the_host() {
        let (_host, mut session) = pair("/bin/sh").await;
        let mut out = Vec::new();
        session.route_keys(&[CTRL_BACKSLASH], &mut out).unwrap();
        assert!(session.popup_at(Instant::now()).is_some());

        session.route_keys(b"ls\r", &mut out).unwrap();

        assert!(spoken(&session).ends_with(b"ls\r"), "{:?}", spoken(&session));
        assert!(session.popup_at(Instant::now()).is_none(), "the popup swallowed the typing");
    }

    /// The popup an outage opened stays up for `LINGER` once the link is
    /// back, saying so, and then closes by itself.
    #[tokio::test]
    async fn returning_to_live_lingers_and_then_closes() {
        let (_host, mut session) = with_popup().await;
        // The host keeps answering throughout: a `note_heard` before each
        // lap is a frame arriving. Without it the fixture's unacknowledged
        // input would make the link `Silent` again two seconds in, which is
        // inside `LINGER`.
        let now = Instant::now();
        session.note_heard(now);
        let v = session.popup_at(now).expect("the popup closed at once");
        assert_eq!(v.marker, Marker::LiveAgain);

        let almost = now + crate::ui::LINGER - Duration::from_millis(1);
        session.note_heard(almost);
        assert!(session.popup_at(almost).is_some(), "closed before LINGER");

        let then = now + crate::ui::LINGER;
        session.note_heard(then);
        assert_eq!(session.popup_at(then), None, "it never closed");
    }

    #[tokio::test]
    async fn an_outage_and_its_end_are_recorded() {
        let t = Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.popup_at(t).is_none());
        assert_eq!(session.activity.entries().len(), 0, "something was recorded before the outage");

        session.popup_at(t + Duration::from_secs(3));
        session.note_heard(t + Duration::from_millis(4_500));
        session.popup_at(t + Duration::from_millis(4_500));

        let texts: Vec<String> = session.activity.entries().map(|e| e.text.clone()).collect();
        assert_eq!(texts, ["silent", "live again via this link, outage 4.5 s"]);
    }

    #[tokio::test]
    async fn quality_is_sampled_once_a_second_and_marks_outage_seconds() {
        let t = Instant::now();
        let (_host, mut session) = with_popup().await;
        assert!(session.quality.sparkline().is_empty());

        session.sample_quality(t);
        session.sample_quality(t + Duration::from_millis(500));
        session.sample_quality(t + Duration::from_secs(1));

        assert_eq!(session.quality.sparkline(), vec![None, None], "with_popup is Silent: both are outage seconds");
    }

    #[tokio::test]
    async fn a_swap_starts_a_new_quality_segment() {
        let (_host, mut client) = pair("").await;
        let (_rebuilt_host, rebuilt) = crate::link::fixtures::link_pair().await;
        assert_eq!(client.quality.segment(), 1);

        client.swap_in(rebuilt, Instant::now()).expect("swapping in");

        assert_eq!(client.quality.segment(), 2);
    }
```

And add to `standby.rs`'s test module:

```rust
    #[tokio::test]
    async fn the_popup_can_read_what_the_standby_is_doing() {
        let t0 = Instant::now();
        let mut fresh = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        assert!(fresh.path().is_none() && fresh.rtt().is_none() && !fresh.searching());
        search_at(&mut fresh, t0 + STANDBY_DELAY);
        assert!(fresh.searching());

        let (s, _host) = with_a_standby(t0).await;
        assert_eq!(s.path().map(|p| p.rtt_ms), Some(38));
        assert!(s.rtt().is_some());
        assert_eq!(s.probe(), ProbeState::Idle);
        assert!(!s.searching());
    }
```

- [ ] **Step 2: Run them to see them fail**

Run: `cargo test --jobs 4 --bin oxutrm -- --test-threads 4`
Expected: FAIL to compile (`popup_at`, `with_popup`, `Marker`, `path()` … not found).

- [ ] **Step 3: `standby.rs` accessors**

Remove the `#[cfg(test)]` from `next_search`, and the doc lines plus `#[cfg_attr(not(test), allow(dead_code))]` from `last_failure` (its doc becomes "Why the last search failed, for the popup's standby block."). Add beside them:

```rust
    /// The standby's path, while there is one.
    pub(crate) fn path(&self) -> Option<&PathDescription> {
        self.link.as_ref().map(|(_, p)| p)
    }

    /// quinn's round-trip estimate on the standby's own connection.
    pub(crate) fn rtt(&self) -> Option<std::time::Duration> {
        self.link.as_ref().map(|(l, _)| l.sink.connection().rtt())
    }

    pub(crate) fn probe(&self) -> ProbeState {
        self.probe
    }

    /// Whether a search is running now.
    pub(crate) fn searching(&self) -> bool {
        self.searching
    }
```

Also reword the two docs that still say "the coming status popup" (`StandbyEvent::NotFound`, the `last_failure` field) to "the popup's standby block".

- [ ] **Step 4: `linkstate.rs` — only the holding stays**

1. Delete `const PREFIX` and its doc, `enum Command`, the `prefix_pending` field (and its initialiser in `new`), and `fn push_held`.
2. Replace `heard`'s body after `self.owed_since = None;` (and the prefix comment block) with:

```rust
        // Coming back with something typed blind is a question, not a
        // resumption. Delivering it silently would replay it against a screen
        // that moved while the user could not watch.
        self.phase = if self.held.is_empty() {
            Phase::Live
        } else {
            Phase::Confirming
        };
```

3. Replace `hold_keys` (doc and body) with:

```rust
    /// Keep keystrokes typed while the link is not answering.
    ///
    /// Only the holding: which keystrokes are commands is decided by the
    /// popup (`ui.rs`) before anything reaches here. Beyond [`MAX_HELD`] the
    /// buffer stops accepting -- see there for why it never drops the oldest.
    pub fn hold_keys(&mut self, bytes: &[u8]) {
        let room = MAX_HELD.saturating_sub(self.held.len());
        self.held.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }
```

4. `take_held`: delete the doc sentences about the half-typed prefix and `self.prefix_pending = false;`. `drop_held`: doc becomes "Discard the held input." and delete `self.prefix_pending = false;`.
5. `Phase::is_outage`'s doc: replace "such as the notice box's rebuild check" with "such as the popup's key routing". `last_heard`'s doc: replace "a caller building the notice for `Recovering`" with "the popup, reporting how long the host has been quiet in `Recovering`,".
6. Tests: delete `the_prefix_and_a_letter_are_a_command_and_are_not_held`, `every_command_key_is_recognised`, `send_and_drop_are_not_commands_under_the_silent_notice`, `quit_is_offered_in_every_phase`, `a_prefix_split_across_two_reads_still_commands`, `an_unknown_key_after_the_prefix_is_held_with_the_prefix`, `a_half_typed_prefix_does_not_survive_the_notice_it_was_typed_into` and `resolving_the_buffer_drops_a_half_typed_prefix_with_it` — `ui.rs` now has each of them (Task 4). In `keys_typed_offline_are_held_not_delivered`, replace `assert_eq!(s.hold_keys(b"make test"), None);` with `s.hold_keys(b"make test");`.

- [ ] **Step 5: `session.rs` — the client half**

1. Imports:

```rust
use std::time::{Duration, Instant, SystemTime};

use oxutrm_client::{PopupView, Renderer, layout_popup, status_line, terminal_size_of};
...
use crate::activity::{Activity, Kind};
use crate::linkstate::{LinkState, Phase};
use crate::quality::{Quality, Reading};
use crate::ui::{Command, LinkChange, Mode, Ui};
use crate::view::{Facts, Identity, StandbyFacts};
```

2. Delete `NOTICE_REFRESH` and its doc.
3. In `ClientSession`: rename `announced` to `path` with the doc "The primary's path: the one `announce` printed, until a failover or a landed rebuild replaces it."; replace the `shown` and `built` fields with:

```rust
    /// What is drawn as layer 1, so an unchanged popup is not laid out again
    /// every lap. Mirrors the overlay exactly: `None` means no overlay.
    shown: Option<PopupView>,
    /// The popup's state, and where keystrokes go.
    ui: Ui,
    /// A minute of the primary link's measurements, for the popup.
    quality: Quality,
    /// What oxutrm did to keep the session alive.
    activity: Activity,
    /// Which session this is. `None` where it was not reached over ssh --
    /// the fixtures in this file's tests.
    identity: Option<Identity>,
```

In `new`, initialise `path: None, shown: None, ui: Ui::new(), quality: Quality::new(Instant::now()), activity: Activity::new(), identity: None` (and drop `announced`, `built`). Update the three uses of `self.announced` in `announce`, `announce_standby` and `announce_failover` to `self.path`.

4. Beside `with_standby`:

```rust
    /// Which session this is, for the popup's title and status rows.
    pub(crate) fn with_identity(mut self, id: Identity) -> ClientSession {
        self.identity = Some(id);
        self
    }
```

5. Replace `route_keys` (doc and body):

```rust
    /// One read from the keyboard, sent wherever it belongs.
    ///
    /// The popup decides ([`Ui::keys`]): on a healthy link with the popup
    /// closed every byte goes to the host untouched; while the link is down,
    /// or the host is answering again with typing held, bytes are held and
    /// the popup's commands take the `Ctrl-\` prefix.
    ///
    /// `Some(code)` means the user asked to close oxutrm, which is the one
    /// answer that ends the loop. A method rather than the body of the
    /// `Wake::Keys` arm because the arm cannot be reached from a test without
    /// a real terminal; the arm is a single call to this, so what is tested
    /// is what ships.
    fn route_keys<W: Write>(&mut self, keys: &[u8], out: &mut W) -> Result<Option<i32>> {
        let phase = self.link_state.phase_now();
        let routed = self.ui.keys(keys, phase, Instant::now());
        if !routed.to_host.is_empty() {
            self.turn(&routed.to_host, out)?;
        }
        if !routed.to_hold.is_empty() {
            self.link_state.hold_keys(&routed.to_hold);
        }
        match routed.command {
            Some(Command::Quit) => return Ok(Some(0)),
            Some(Command::SendHeld) => {
                let held = self.link_state.take_held();
                self.turn(&held, out)?;
            }
            Some(Command::DropHeld) => self.link_state.drop_held(),
            None => {}
        }
        Ok(None)
    }
```

6. Replace `notice_at` (doc and body) with:

```rust
    /// One lap of layer 1: decide the phase, move the popup on, and return
    /// what it should show at `now` -- `None` while it is closed.
    ///
    /// Built afresh on every lap it is open and compared by the loop with
    /// what is on the screen. Every number in it that moves by itself is in
    /// whole seconds, so an open popup with nothing happening repaints at
    /// most once a second and costs one comparison otherwise; a change of
    /// phase is reported the lap it happens.
    fn popup_at(&mut self, now: Instant) -> Option<PopupView> {
        let owed = self.input_tx.current().seq() != self.screen_rx.peer_ack();
        let phase = self.link_state.evaluate(now, owed);
        match self.ui.tick(phase, now) {
            Some(LinkChange::WentSilent) => self.activity.record(Kind::Link, "silent"),
            Some(LinkChange::Back { outage }) => {
                let text = format!(
                    "live again via {}, outage {:.1} s",
                    crate::view::path_label(self.path.as_ref()),
                    outage.as_secs_f64()
                );
                self.activity.record(Kind::Link, &text);
            }
            None => {}
        }
        self.ui.visible(phase).then(|| self.view(phase, now))
    }

    /// What the popup says at `now`.
    fn view(&self, phase: Phase, now: Instant) -> PopupView {
        let lingering = match self.ui.mode() {
            Mode::Lingering { outage, .. } => Some(outage),
            _ => None,
        };
        let standby = self.standby.as_ref().map(|s| StandbyFacts {
            path: s.path(),
            rtt: s.rtt(),
            probe: s.probe(),
            searching: s.searching(),
            next_search: s.next_search(),
            last_failure: s.last_failure(),
        });
        crate::view::build(&Facts {
            identity: self.identity.as_ref(),
            phase,
            lingering,
            last_heard: self.link_state.last_heard(),
            path: self.path.as_ref(),
            quality: &self.quality,
            rejected: self.rejected_total,
            standby,
            rebuild_failure: self.last_failure.as_deref(),
            held: self.link_state.held(),
            held_full: self.link_state.held_is_full(),
            activity: &self.activity,
            now,
            wall: SystemTime::now(),
        })
    }

    /// One sample of the primary link, if a second has passed since the
    /// last. Rides on the loop's laps; nothing is armed for it.
    fn sample_quality(&mut self, now: Instant) {
        if !self.quality.due(now) {
            return;
        }
        let outage = self.link_state.phase_now().is_outage();
        self.quality
            .push(now, reading_of(self.link.sink.connection()), outage);
    }

    /// Whether the popup at `now` is reporting an outage: the question the
    /// tests that predate the popup asked of the notice, which closed the
    /// moment the host answered. The popup lingers instead, so "is it up"
    /// no longer means "is the host silent".
    #[cfg(test)]
    fn outage_at(&mut self, now: Instant) -> bool {
        self.popup_at(now).is_some_and(|v| {
            matches!(
                v.marker,
                oxutrm_client::Marker::Silent | oxutrm_client::Marker::Recovering
            )
        })
    }
```

And at module level, beside `exit_code`:

```rust
/// What `conn` reports right now, as plain values for [`Quality`].
fn reading_of(conn: &quinn::Connection) -> Reading {
    let stats = conn.stats();
    Reading {
        rtt: conn.rtt(),
        sent: stats.path.sent_packets,
        lost: stats.path.lost_packets,
        tx_bytes: stats.udp_tx.bytes,
        rx_bytes: stats.udp_rx.bytes,
    }
}
```

7. In `swap_in_as`, after `self.link_state.rebuilt(now);`:

```rust
        // A new connection, whose counters start at zero.
        self.quality.new_segment(now);
```

8. In `resize`, replace the `if let Some(n) = self.shown.as_ref() { … layout_notice … }` block and its comment with:

```rust
        // Layer 1 was laid out for the screen that just went away, and the
        // loop rebuilds it only when the VIEW changes -- which a resize does
        // not. The same view, laid out again, rather than `self.shown =
        // None`: `shown` has to keep mirroring what the overlay is, or a lap
        // whose view is also `None` would never clear a box stranded on the
        // screen.
        if let Some(v) = self.shown.as_ref() {
            self.renderer.set_overlay(Some(layout_popup(v, size)));
        }
```

9. In `run_on`, replace the "Layer 1" block with:

```rust
            // Layer 1. Laid out again only when the view actually changed, so
            // an open popup with nothing new costs one comparison per lap.
            let now = Instant::now();
            let view = self.popup_at(now);
            self.sample_quality(now);
            if view != self.shown {
                self.renderer
                    .set_overlay(view.as_ref().map(|v| layout_popup(v, self.size)));
                self.shown = view;
                self.renderer
                    .render(out, self.screen_rx.state())
                    .context("painting the popup")?;
                out.flush().context("flushing the terminal")?;
            }
```

10. Comment sweep. Run `grep -n -i 'notice\|the box\|box ' src/session.rs src/linkstate.rs src/standby.rs src/rebuild.rs src/connect.rs` and reword every comment or doc that names the notice so it names the popup, keeping each one true. Known sites: `take_frames`' long comment ("the box saying nobody was answering", "the `Silent` box is the only place the rejected count is reported" → "the popup"), `rejected_total`'s doc ("For tests and for the notice." → "For tests and for the popup."), the `rejected_total` field doc, `rebuild_step`'s doc ("with the same `now` the notice was built from" → "the popup was built from"), `swap_in`'s doc ("typed while the notice was up" → "while the popup was holding it"), the `last_failure` field doc ("for the notice" → "for the popup"), the `follow_route` call-site comment and the `rebuild_step` call-site comment ("After the notice, so the box describing the silence…" → "After the popup, so what describes the silence is on the screen…"), the `pacing_interval` deadline comment (drop the sentence about `NOTICE_REFRESH`; keep that the loop wakes at least ten times a second), `Rebuild`'s `Drop` doc in `rebuild.rs` ("the key the notice on screen is offering" → "the key the popup is offering"), and in `rebuild.rs` the `Retry` variant doc ("kept for the notice" → "kept for the popup"). `grep` again afterwards: the only hits left should be inside test names you are about to migrate in Step 6.

11. `crates/oxutrm-client`: delete `src/notice.rs` (`git rm crates/oxutrm-client/src/notice.rs`); in `lib.rs` remove `pub mod notice;` and `pub use notice::{…};`. In `overlay.rs`'s module doc, replace "a notice, and later a session picker or a config screen" with "the status popup, and later a session picker or a config screen".

12. `src/main.rs`: delete the attribute and its comment on `mod quality;`, `mod ui;` and `mod view;` (keep the one on `mod activity;` — Task 7 removes it).

13. `src/connect.rs`: chain the identity onto the session it builds:

```rust
    let mut session = ClientSession::new(size, detect_caps(), established.link, Some(rebuild))
        .context("preparing the client session")?
        .with_identity(Identity {
            target: target.to_owned(),
            session_id: established.session_id.clone(),
            attach_id: established.attach_id,
        });
```

with `use crate::view::Identity;`.

- [ ] **Step 6: Migrate the existing session tests**

Imports in the test module: add `use oxutrm_client::Marker;` and `use crate::view::words;`; the `layout_notice` import goes.

**Mechanical rule** for every remaining `notice_at` in the test module (the ignored experiments `blackout_recovery_curve` and `does_a_rebind_rescue_a_backed_off_connection`, `drive_to_recovering`, `a_session_outlives_a_silence_that_used_to_kill_it`, `a_short_silence_raises_and_clears_the_notice_on_a_real_clock`, `an_answer_that_applies_nothing_still_clears_the_notice`, `a_heartbeating_session_that_is_answered_never_raises_a_notice`):
- `X.notice_at(T).is_none()` → `!X.outage_at(T)`
- `X.notice_at(T).is_some()` → `X.outage_at(T)`
- `let _ = X.notice_at(T);` → `let _ = X.outage_at(T);`
- `assert_eq!(X.notice_at(T), None, msg…)` → `assert!(!X.outage_at(T), msg…)`
and fix the comments in those tests that say "`run`'s own loop calls `notice_at`" to say `popup_at`. The doc on `reply_owed` ("The caller's half of `notice_at`'s question") → `popup_at`'s. `drive_to_recovering`'s doc names `the_recovering_notice_reports_the_wired_numbers` → `the_recovering_section_reports_the_wired_numbers`.

**Renames** throughout the module: `with_notice` → `with_popup`, `with_confirming_notice` → `with_confirming_popup`.

**Replacements**, by test (old name → new code):

```rust
    /// Every word the popup puts on the screen outside its log. The log is
    /// history and carries words from earlier phases legitimately; a guard
    /// that read only the body is how a claim once sat in a key list
    /// unnoticed.
    fn painted_words(v: &PopupView) -> String {
        words(v)
    }

    /// What the popup may not say while `Silent` or `Confirming`, wherever
    /// in it it says it.
    ///
    /// Nothing is reconnecting in those phases -- the rebuild loop starts
    /// only in `Recovering` -- so they may not name it. And from here a dead
    /// network and a crashed host are indistinguishable, so the popup may not
    /// vouch for the far end at all, not even hedged. What a key DOES stays
    /// true either way.
    fn assert_claims_nothing_it_cannot_see(shown: &str) {
        // body unchanged
    }

    /// Ctrl-\, the key layer 1 listens for. `ui`'s own copy is the code
    /// under test, and a test that reached for it would be asserting the
    /// constant rather than the keystroke.
    const CTRL_BACKSLASH: u8 = 0x1c;

    /// A client with the popup up for a real `Silent` phase, left exactly as
    /// the loop leaves it: the phase decided by `popup_at`, and `shown`
    /// mirroring what the overlay is.
    async fn with_popup() -> (HostSession, ClientSession) {
        let t = std::time::Instant::now();
        let (host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        // The lap the owing begins on, and it is not decoration: the grace
        // period is measured from when the reply started being owed, so a
        // fixture that jumped straight to three seconds would be asking about
        // an owing three seconds long that had only just started.
        assert!(session.popup_at(t).is_none());
        let view = session.popup_at(t + Duration::from_secs(3));
        assert!(view.is_some(), "the fixture raised no popup");
        session.shown = view;
        (host, session)
    }

    /// A client whose popup asks about `held`, typed blind: the popup went
    /// up, the user typed into it, and the host started answering again.
    /// The only phase that offers `Ctrl-\ s` and `Ctrl-\ d`.
    async fn with_confirming_popup(held: &[u8]) -> (HostSession, ClientSession) {
        let (host, mut session) = with_popup().await;
        let mut out = Vec::new();
        session.route_keys(held, &mut out).expect("hold the typing");

        let now = std::time::Instant::now();
        session.note_heard(now);
        let view = session.popup_at(now);
        assert!(view.is_some(), "the fixture asked the user nothing");
        session.shown = view;
        (host, session)
    }
```

`silence_raises_a_notice_and_a_frame_clears_it` → `silence_raises_the_popup_and_a_frame_ends_the_outage`:

```rust
    #[tokio::test]
    async fn silence_raises_the_popup_and_a_frame_ends_the_outage() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        // (keep the existing comment about moving the origin onto `t`)
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.popup_at(t + Duration::from_secs(1)).is_none());

        let v = session.popup_at(t + Duration::from_secs(3)).expect("no popup after three seconds of silence");
        assert_eq!(v.marker, Marker::Silent);

        session.note_heard(t + Duration::from_secs(4));
        assert!(!session.outage_at(t + Duration::from_secs(4)));
    }
```

`the_notice_names_the_counters_it_can_actually_observe` → `the_popup_says_how_long_the_host_has_been_silent`: same body with `popup_at`, and the assertion `assert!(shown.contains("silent for 6s"), "no silence duration: {shown}");` where `shown = painted_words(&v)`.

`the_silent_notice_says_that_blind_typing_is_being_kept` and `a_full_buffer_is_reported_while_it_is_still_filling`: `notice_at` → `popup_at`; delete the `session.shown = …` lines and the `assert!(session.shown.is_some())` (holding now follows the phase, not what is drawn); `painted_words(&n)` works unchanged on the view.

`the_silence_counters_are_rebuilt_at_most_once_a_second` → replace whole test:

```rust
    /// Every number in the popup that moves by itself is in whole seconds,
    /// so an open popup is the same view for a second at a time and the
    /// loop's comparison spares the repaint. A view carrying quinn's raw
    /// packet counters would change on every lap of an outage -- up to 125
    /// times a second -- in a box whose whole job is to be read.
    #[tokio::test]
    async fn an_open_popup_is_the_same_view_for_a_second_at_a_time() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_heard(t);
        session.note_sent(t);
        assert!(session.popup_at(t).is_none());

        let first = session.popup_at(t + Duration::from_secs(3)).expect("no popup after three seconds of silence");
        assert_eq!(
            session.popup_at(t + Duration::from_millis(3_900)),
            Some(first.clone()),
            "the view changed within the same second"
        );
        let later = session.popup_at(t + Duration::from_secs(4)).expect("the popup vanished");
        assert_ne!(later, first, "the silence counter never moved");
        assert!(painted_words(&later).contains("silent for 4s"), "{}", painted_words(&later));
    }
```

`a_change_of_phase_repaints_at_once_however_recent_the_refresh` → `a_change_of_phase_is_shown_the_lap_it_happens`: `with_popup`, `popup_at(now)`, and assert `v.held.first().is_some_and(|l| l.contains("answering again"))` with the old message.

`returning_to_live_clears_the_box_at_once` → delete; `returning_to_live_lingers_and_then_closes` (Step 1) replaces it.

`the_confirming_notice_states_only_what_the_client_can_observe` → `the_confirming_popup_states_only_what_the_client_can_observe`: `with_popup`, `popup_at(now)`, body otherwise unchanged.

`the_recovering_notice_reports_the_wired_numbers` → `the_recovering_section_reports_the_wired_numbers`: `notice_at` → `popup_at`; `let shown = n.recovering.join(" | ");`; drop `waiting for the network` (that headline is gone; assert `n.marker == Marker::Recovering` instead); keep the three number assertions (`host quiet for 20s`, `reconnect attempt 1`, `next try in 0s`) and their comments; update the doc's first paragraph to say "`view.rs`'s test for the recovering rows calls `build` with numbers already computed; nothing there exercises `popup_at`'s own derivation of them".

`a_scavenged_frame_takes_the_notice_down` → `a_scavenged_frame_ends_the_outage_the_popup_reports`:

```rust
    #[tokio::test]
    async fn a_scavenged_frame_ends_the_outage_the_popup_reports() {
        let (mut host, mut session) = with_popup().await;
        let mut out = Vec::new();

        host.turn().expect("the host takes a turn");
        wait_for_frame(&mut session).await;
        session.turn(&[], &mut out).expect("a pacing lap");

        let v = session.popup_at(Instant::now()).expect("the popup closed instead of lingering");
        assert_eq!(v.marker, Marker::LiveAgain, "a frame was applied and the popup still reports silence");
    }
```

`a_healthy_session_paints_no_notice_across_several_heartbeats` → `a_healthy_session_raises_no_popup_across_several_heartbeats`: replace the `painted.contains("reply from host")` assertion with

```rust
        assert!(
            !client.activity.entries().any(|e| e.text == "silent"),
            "a healthy session went silent"
        );
```

keeping `assert!(client.shown.is_none(), "the session ended with a popup still up");`. Its doc's last paragraph (about the headline's bytes) is replaced by: "The assertion reads the activity log, which records every outage the popup opens for: painted bytes are a diff, and a diff can split a word wherever a cell happened to be unchanged."

`a_resize_lays_the_notice_out_again_for_the_new_screen` → `a_resize_lays_the_popup_out_again_for_the_new_screen` (Review focus 3), looping over two sizes:

```rust
    #[tokio::test]
    async fn a_resize_lays_the_popup_out_again_for_the_new_screen() {
        for small in [TermSize { cols: 24, rows: 8 }, TermSize { cols: 19, rows: 5 }] {
            let t = std::time::Instant::now();
            let (_host, mut session) = pair("/bin/sh").await;
            session.note_heard(t);
            session.note_sent(t);
            assert!(session.popup_at(t).is_none());
            let view = session.popup_at(t + Duration::from_secs(3)).unwrap();
            session.renderer.set_overlay(Some(layout_popup(&view, session.size)));
            session.shown = Some(view.clone());
            let mut painted = Vec::new();
            session.renderer.render(&mut painted, session.screen_rx.state()).unwrap();

            session.resize(small);
            let mut after = Vec::new();
            session.renderer.render(&mut after, session.screen_rx.state()).unwrap();

            let mut fresh = Renderer::new(small, caps());
            fresh.set_overlay(Some(layout_popup(&view, small)));
            let mut expected = Vec::new();
            fresh.render(&mut expected, session.screen_rx.state()).unwrap();

            assert_eq!(
                String::from_utf8_lossy(&after),
                String::from_utf8_lossy(&expected),
                "the popup kept the geometry of the screen that went away ({small:?})"
            );
        }
    }
```

The key tests (`typing_into_a_notice_is_held_and_not_sent` → `typing_into_the_popup_during_an_outage_is_held_and_not_sent`, `the_quit_key_ends_the_client_with_a_status_of_zero`, `the_send_key_delivers_what_was_typed_blind`, `the_drop_key_throws_the_blind_typing_away`, `the_silent_notice_does_not_honour_the_keys_it_does_not_offer` → `the_outage_popup_does_not_honour_the_keys_it_does_not_offer`, `the_quit_key_works_under_the_confirming_notice_too` → `…_confirming_popup_too`, `a_frame_between_the_prefix_and_its_letter_does_not_eat_the_command`, `a_scavenged_frame_clears_the_notice` → `a_scavenged_frame_ends_the_silence`) keep their bodies with the fixture renames: their keystrokes are `Ctrl-\`-prefixed, which is still how commands work while typing is held. `a_rejected_frame_is_counted_rather_than_printed`: message "the count did not reach the notice" → "the popup". The route-probe tests that use `with_notice` only need the rename.

- [ ] **Step 7: Run the session, linkstate and standby tests**

Run: `cargo test --jobs 4 --bin oxutrm -- --test-threads 4`
Expected: PASS (the `#[ignore]`d experiments compile and stay ignored).

- [ ] **Step 8: Injection**

1. In `popup_at`, replace the last line with `(phase != Phase::Live).then(|| self.view(phase, now))` (the old "only while not live" rule). `ctrl_backslash_opens_the_popup_on_a_healthy_session` and `returning_to_live_lingers_and_then_closes` must FAIL. Restore.
2. In `resize`, delete the `if let Some(v) = self.shown.as_ref() { … }` block. `a_resize_lays_the_popup_out_again_for_the_new_screen` must FAIL. Restore.
3. In `swap_in_as`, delete `self.quality.new_segment(now);`. `a_swap_starts_a_new_quality_segment` must FAIL. Restore.
4. In `route_keys`, pass `Phase::Live` instead of `phase` to `self.ui.keys`. `typing_into_the_popup_during_an_outage_is_held_and_not_sent` must FAIL. Restore.

- [ ] **Step 9: Gate and commit**

```bash
git add src/session.rs src/linkstate.rs src/standby.rs src/connect.rs src/rebuild.rs src/main.rs crates/oxutrm-client/src/lib.rs crates/oxutrm-client/src/overlay.rs
git rm crates/oxutrm-client/src/notice.rs
git commit -m "$(cat <<'EOF'
feat(client): the status popup replaces the notice

The session builds a PopupView on every lap the popup is open and
repaints only when it changed. Ctrl-\ opens it on a healthy link, an
outage opens it by itself, and it lingers three seconds after the link
comes back. Keys are routed by ui.rs; linkstate only holds bytes now.
Link quality is sampled once a second on the loop's own laps, and each
outage's start and end go to the activity log.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 7: Record every event; retire the mid-session lines

**Files:**
- Modify: `src/session.rs`
- Modify: `src/standby.rs`
- Modify: `src/connect.rs`
- Modify: `src/loopback.rs` (`replay` becomes a shared fixture)
- Modify: `src/main.rs` (drop the attribute on `mod activity;`)

**Interfaces:**
- Consumes: `ClientSession` fields and methods from Task 6; `Activity::{with_file, record}`, `LogFile::open_default`, `Kind` (Task 2).
- Produces (`session.rs`):
  - `pub(crate) fn with_activity(self, activity: Activity) -> ClientSession`
  - `fn on_standby_event(&mut self, event: crate::standby::StandbyEvent, now: Instant) -> Option<quinn::Connection>`
  - `fn on_standby_closed(&mut self, reason: &quinn::ConnectionError, now: Instant)`
  - `fn fail_over<W: Write>(&mut self, now: Instant, out: &mut W) -> Result<bool>`
  - `fn rebuild_landed(&mut self, e: crate::connect::Established, now: Instant) -> Result<()>`
  - `fn rebuild_failed(&mut self, why: String, now: Instant)`
  - `announce_standby`, `announce_failover` and `say_mid_session` are deleted.
  - Test fixtures: `const BIG: TermSize` (80×24), `async fn pair_sized(shell: &str, size: TermSize) -> (HostSession, ClientSession)`, `async fn pair_through_relay_sized(shell: &str, size: TermSize) -> (HostSession, ClientSession, Relay)`, `SharedOut::bytes(&self) -> Vec<u8>`, `fn screen_of(out: &SharedOut, size: TermSize) -> String`, `async fn wait_for_screen(out: &SharedOut, size: TermSize, want: &str, budget: Duration)`, `fn outside_renderer(out: &[u8]) -> Vec<u8>`, `fn last_entry(c: &ClientSession) -> Option<(Kind, String)>`.
- Produces (`activity.rs`): `LogFile::open_default() -> std::io::Result<LogFile>`.
- Produces (`standby.rs`): `link: Option<Established>`; `take_for_failover(&mut self, now: Instant) -> Option<Established>`; `not_found(…) -> bool` = "the result was for the search still wanted"; `lost(…) -> bool` = "worth recording"; `probed(&mut self, answered: bool, now: Instant) -> bool` = "it was for the probe in flight"; `told_none` and `dry_spell_news` deleted.
- Produces (`loopback.rs`): `#[cfg(test)] pub(crate) mod fixtures { pub(crate) fn replay(ansi: &[u8], size: TermSize) -> Vec<String> }`.

- [ ] **Step 1: Shared test fixtures**

`src/loopback.rs`: move `fn replay` out of `mod tests` into a new module above it, unchanged apart from visibility, and have the tests use it:

```rust
#[cfg(test)]
pub(crate) mod fixtures {
    use alacritty_terminal::Term;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::index::{Column, Line, Point};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;
    use oxutrm_proto::TermSize;

    /// Replay the bytes the renderer emitted through a real emulator, and
    /// read back what a terminal would be showing.
    pub(crate) fn replay(ansi: &[u8], size: TermSize) -> Vec<String> {
        // body moved verbatim from `tests::replay`
    }
}
```

In `loopback.rs`'s `mod tests`, delete `replay` and the `alacritty_terminal` imports only it used, and add `use super::fixtures::replay;`.

`src/session.rs` test module — add beside `pair_on` / `pair_through_relay`:

```rust
    /// Big enough for the popup's log to have rows: at the fixtures' 40x10 the
    /// status block fills the whole box.
    const BIG: TermSize = TermSize { cols: 80, rows: 24 };

    /// `pair`, with host and client both at `size` from the start, so no
    /// resize is in flight when a test begins.
    async fn pair_sized(shell: &str, size: TermSize) -> (HostSession, ClientSession) {
        let listening = crate::link::fixtures::listening().await;
        let addr = listening.addr;
        let (host_link, client_link) = listening.dial("127.0.0.1:0", addr).await;
        let mut host = HostSession::spawn("/bin/sh", size, 200, host_link).unwrap();
        let client = ClientSession::new(size, caps(), client_link, None).unwrap();
        host.term.write_input(shell.as_bytes()).unwrap();
        (host, client)
    }

    /// `pair_through_relay` at `size`.
    async fn pair_through_relay_sized(shell: &str, size: TermSize) -> (HostSession, ClientSession, Relay) {
        let listening = crate::link::fixtures::listening().await;
        let relay = relay_to(listening.addr).await;
        let (host_link, client_link) = listening.dial("127.0.0.1:0", relay.addr).await;
        let mut host = HostSession::spawn("/bin/sh", size, 200, host_link).unwrap();
        let client = ClientSession::new(size, caps(), client_link, None).unwrap();
        host.term.write_input(shell.as_bytes()).unwrap();
        (host, client, relay)
    }
```

and make `pair_through_relay(shell)` a one-liner calling `pair_through_relay_sized(shell, size()).await`.

Beside `SharedOut`:

```rust
    impl SharedOut {
        fn bytes(&self) -> Vec<u8> {
            self.0.lock().unwrap().clone()
        }
    }

    /// What a terminal of `size` fed everything the client wrote would be
    /// showing. Painted bytes are a diff, and a diff can split a word
    /// wherever a cell happened to be unchanged, so tests read the screen
    /// and not the bytes.
    fn screen_of(out: &SharedOut, size: TermSize) -> String {
        crate::loopback::fixtures::replay(&out.bytes(), size).join("\n")
    }

    async fn wait_for_screen(out: &SharedOut, size: TermSize, want: &str, budget: Duration) {
        let deadline = Instant::now() + budget;
        while !screen_of(out, size).contains(want) {
            assert!(
                Instant::now() < deadline,
                "{want:?} never reached the screen; it showed:\n{}",
                screen_of(out, size)
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// What reached the terminal outside the renderer. `Renderer::render`
    /// wraps every non-empty write in synchronized output
    /// (`ESC[?2026h` ... `ESC[?2026l`), so anything between those blocks
    /// was written by something else.
    fn outside_renderer(out: &[u8]) -> Vec<u8> {
        const BEGIN: &[u8] = b"\x1b[?2026h";
        const END: &[u8] = b"\x1b[?2026l";
        let find = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).position(|w| w == needle);
        let mut rest = out;
        let mut stray = Vec::new();
        while !rest.is_empty() {
            let Some(begin) = find(rest, BEGIN) else {
                stray.extend_from_slice(rest);
                break;
            };
            stray.extend_from_slice(&rest[..begin]);
            let inside = &rest[begin + BEGIN.len()..];
            let Some(end) = find(inside, END) else {
                stray.extend_from_slice(&rest[begin..]);
                break;
            };
            rest = &inside[end + END.len()..];
        }
        stray
    }

    /// The helper has to be able to see a stray, or every assertion made
    /// with it is a guard that cannot fail.
    #[test]
    fn outside_renderer_finds_what_is_written_between_the_renderers_blocks() {
        assert_eq!(outside_renderer(b"x\x1b[?2026hA\x1b[?2026ly\r\n"), b"xy\r\n");
        assert!(outside_renderer(b"\x1b[?2026hA\x1b[?2026l\x1b[?2026hB\x1b[?2026l").is_empty());
    }

    fn last_entry(c: &ClientSession) -> Option<(crate::activity::Kind, String)> {
        c.activity.entries().next_back().map(|e| (e.kind, e.text.clone()))
    }
```

In `standby_established`, change `attach_id: 0` to `attach_id: 2` (so a test can see it carried over; nothing else reads it).

- [ ] **Step 2: Write the failing tests**

Replace `a_found_standby_is_added_to_the_path_line`, `no_standby_is_said_on_the_path_line` and `a_failover_announces_the_standby_it_switched_to` with:

```rust
    /// A standby, found by a search the client started.
    fn standby_searching(now: Instant) -> (crate::standby::Standby, u64) {
        let mut s = crate::standby::Standby::new(
            crate::attach_exchange::fixtures::stun_free(),
            now.checked_sub(crate::linkstate::STANDBY_DELAY).expect("a clock this young"),
        );
        let crate::standby::StandbyAction::Search { search } = s.step(Phase::Live, now, false) else {
            panic!("no search started");
        };
        (s, search)
    }

    /// Spec §5.2: what used to be a line written over the session is an
    /// entry in the activity log. The handler has no terminal to write to.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_found_standby_is_recorded() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let now = Instant::now();
        let (standby, search) = standby_searching(now);
        client.standby = Some(standby);
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;

        let watch = client.on_standby_event(
            crate::standby::StandbyEvent::Found { search, e: Box::new(standby_established(standby_client)) },
            now,
        );

        assert!(watch.is_some(), "the loop was not handed the standby to watch");
        assert_eq!(last_entry(&client), Some((crate::activity::Kind::Standby, "found IPv4 punched, 38 ms".to_string())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_search_is_recorded_with_its_reason_and_a_stale_one_is_not() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let now = Instant::now();
        let (standby, search) = standby_searching(now);
        client.standby = Some(standby);

        client.on_standby_event(
            crate::standby::StandbyEvent::NotFound { search: search + 7, reason: "stale".to_string() },
            now,
        );
        assert_eq!(client.activity.entries().len(), 0, "a stale search's failure was recorded");

        client.on_standby_event(
            crate::standby::StandbyEvent::NotFound { search, reason: "no path".to_string() },
            now,
        );
        assert_eq!(last_entry(&client), Some((crate::activity::Kind::Standby, "not found: no path".to_string())));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_lost_standby_is_recorded() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));

        client.on_standby_closed(&quinn::ConnectionError::TimedOut, Instant::now());

        assert_eq!(last_entry(&client), Some((crate::activity::Kind::Standby, "lost: timed out".to_string())));
        assert!(!client.standby.as_ref().is_some_and(|s| s.has_link()));
    }

    /// The failover is recorded, writes nothing to the terminal but the
    /// renderer's output, and the path it went to becomes the session's.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failover_is_recorded_and_the_path_follows_it() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        client
            .announce(&path_of(Rung::Ipv6Direct, 11, 1452, 0, NatType::None), &mut out)
            .expect("announce");
        out.clear();
        client.identity = Some(Identity { target: "bastion".into(), session_id: "f0".repeat(16), attach_id: 5 });
        let (_standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));

        assert!(client.fail_over(Instant::now(), &mut out).expect("failing over"));

        assert_eq!(
            last_entry(&client),
            Some((crate::activity::Kind::Failover, "switched to standby (IPv4 punched)".to_string()))
        );
        assert!(outside_renderer(&out).is_empty(), "{:?}", String::from_utf8_lossy(&outside_renderer(&out)));
        assert_eq!(client.identity.as_ref().map(|i| i.attach_id), Some(2), "the attach id stayed the old link's");
        let standby = path_of(Rung::StunPunch, 38, 1400, 0, NatType::Unknown);
        assert!(
            !client.announce(&standby, &mut out).expect("announce"),
            "the failover did not record the path it switched to"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failover_with_no_standby_does_nothing() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        assert!(!client.fail_over(Instant::now(), &mut out).expect("failing over"));
        assert_eq!(client.activity.entries().len(), 0);
    }

    /// The migration branch of `announce` used to print; the session owns the
    /// screen by then.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_migration_is_recorded_not_printed() {
        let (_host, mut client) = pair("sleep 30\n").await;
        let mut out = Vec::new();
        client
            .announce(&path_of(Rung::StunPunch, 38, 1392, 0, NatType::EndpointIndependent), &mut out)
            .expect("first");
        assert!(!out.is_empty(), "the connect line was not printed");
        out.clear();

        let better = path_of(Rung::Ipv6Direct, 11, 1452, 0, NatType::None);
        assert!(client.announce(&better, &mut out).expect("second"));

        assert!(out.is_empty(), "the migration was written over the session: {:?}", String::from_utf8_lossy(&out));
        assert_eq!(
            last_entry(&client),
            Some((crate::activity::Kind::Link, "path migrated \u{2192} IPv6 direct".to_string()))
        );
    }

    #[tokio::test]
    async fn sending_or_dropping_held_input_is_recorded() {
        let (_host, mut session) = with_confirming_popup(b"make test\r").await;
        let mut out = Vec::new();
        session.route_keys(&[CTRL_BACKSLASH, b's'], &mut out).unwrap();
        assert_eq!(last_entry(&session), Some((crate::activity::Kind::Input, "held input sent (10 bytes)".to_string())));

        let (_host, mut session) = with_confirming_popup(b"rm -rf /tmp/x\r").await;
        session.route_keys(&[CTRL_BACKSLASH, b'd'], &mut out).unwrap();
        assert_eq!(last_entry(&session), Some((crate::activity::Kind::Input, "held input dropped (14 bytes)".to_string())));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_rebuild_attempt_and_its_failure_are_recorded() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        let pidfile = dir.path().join("ssh.pid");
        let rebuild = Rebuild::new("bastion.example.net".to_owned(), "f0".repeat(16))
            .via(hanging_ssh(dir.path(), &pidfile), stunless());
        let (_host, mut session) = pair_on("127.0.0.1:0", "/bin/sh", Some(rebuild)).await;
        let entered = drive_to_recovering(&mut session);
        let (tx, _rx) = tokio::sync::mpsc::channel(1);

        session.rebuild_step(entered, &tx);
        assert_eq!(last_entry(&session), Some((crate::activity::Kind::Rebuild, "attempt 1 started".to_string())));

        session.rebuild_failed("ssh exited with status 255".to_string(), entered + Duration::from_secs(1));
        assert_eq!(
            last_entry(&session),
            Some((crate::activity::Kind::Rebuild, "attempt 1 failed: ssh exited with status 255".to_string()))
        );
        assert!(matches!(session.link_state.phase_now(), Phase::Recovering { attempt: 1, .. }));
        if let Some(r) = session.rebuild.as_mut() {
            r.cancel();
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_landed_rebuild_is_recorded_and_its_path_and_attach_become_the_sessions() {
        let (_host, mut client) = pair("").await;
        client.identity = Some(Identity { target: "bastion".into(), session_id: "f0".repeat(16), attach_id: 5 });
        let (_rebuilt_host, rebuilt) = crate::link::fixtures::link_pair().await;

        client.rebuild_landed(standby_established(rebuilt), Instant::now()).expect("landing");

        assert_eq!(last_entry(&client), Some((crate::activity::Kind::Rebuild, "landed via IPv4 punched".to_string())));
        assert_eq!(client.path.as_ref().map(|p| p.rtt_ms), Some(38));
        assert_eq!(client.identity.as_ref().map(|i| i.attach_id), Some(2));
    }
```

(`standby_established` is the shape a rebuild's `Established` has too; reusing it keeps the fixture count down.)

Add `use crate::view::Identity;` to the test module imports if Task 6 did not.

Migrate `a_standby_closed_under_a_running_session_is_announced_and_forgotten` → `a_standby_closed_under_a_running_session_is_recorded_and_forgotten`:

```rust
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_closed_under_a_running_session_is_recorded_and_forgotten() {
        let (mut host, mut client) = pair_sized("", BIG).await;
        let (standby_host, standby_client) = crate::link::fixtures::link_pair().await;
        client.standby = Some(standby_holding(standby_client));

        let (_attach_tx, mut attach_rx) = tokio::sync::mpsc::channel(1);
        let host_loop = tokio::spawn(async move { host.run_with_attaches(&mut attach_rx).await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        typing.write_all(b"printf 'ready-%s\\n' ok\n").expect("type");
        wait_for_screen(&out, BIG, "ready-ok", Duration::from_secs(10)).await;

        // Losing a standby is not an outage, so nothing opens the popup by
        // itself: open it by hand, as a curious user would.
        typing.write_all(&[CTRL_BACKSLASH]).expect("type");
        wait_for_screen(&out, BIG, "Esc close", Duration::from_secs(10)).await;
        assert!(
            !screen_of(&out, BIG).contains("lost:"),
            "the standby was reported lost before anything closed it"
        );

        standby_host.sink.connection().close(quinn::VarInt::from_u32(0), SUPERSEDED);
        wait_for_screen(&out, BIG, "lost:", Duration::from_secs(10)).await;

        // `e` closes the popup and goes to the shell with the rest.
        typing.write_all(b"exit 7\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(15), client_loop)
            .await
            .expect("the client never finished")
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 7);
        assert!(!client.standby.as_ref().is_some_and(|s| s.has_link()), "the closed standby is still held");
        let stray = outside_renderer(&out.bytes());
        assert!(stray.is_empty(), "written outside the renderer: {:?}", String::from_utf8_lossy(&stray));
        assert_eq!(host_loop.await.expect("host task").expect("host loop"), 7);
    }
```

Migrate `a_dead_primary_fails_over_onto_an_answering_standby`: use `pair_through_relay_sized("", BIG)`; replace `tokio::time::sleep(Duration::from_secs(6)).await;` with

```rust
        wait_for_screen(&out, BIG, "switched to standby", Duration::from_secs(15)).await;
        // And answering on it. Typed before the first frame arrives on the
        // standby, `exit 7` would be held for the `Confirming` question and
        // never reach the shell.
        wait_for_screen(&out, BIG, "LIVE again", Duration::from_secs(15)).await;
```

replace the final `out.text().contains("switched to standby")` assertion with

```rust
        let stray = outside_renderer(&out.bytes());
        assert!(stray.is_empty(), "written outside the renderer: {:?}", String::from_utf8_lossy(&stray));
```

and add to its doc: "The popup opens for the outage and shows the failover in its log; the test reads that off the replayed screen, then types `exit 7`."

`SharedOut::text` is now unused — delete it.

`src/standby.rs` tests:

```rust
    #[test]
    fn every_current_failed_search_counts_and_backs_off() {
        let t0 = Instant::now();
        let mut s = Standby::new(crate::attach_exchange::fixtures::stun_free(), t0);
        let first = search_at(&mut s, t0 + STANDBY_DELAY);
        let t1 = t0 + STANDBY_DELAY;
        assert!(s.not_found(first, t1, "no path".to_string()));
        assert_eq!(s.next_search(), t1 + standby_backoff(0));
        let second = search_at(&mut s, t1 + standby_backoff(0));
        let t2 = t1 + standby_backoff(0);
        assert!(s.not_found(second, t2, "no path".to_string()), "a second current failure did not count");
        assert_eq!(s.next_search(), t2 + standby_backoff(1));
    }

    #[tokio::test]
    async fn losing_the_standby_counts_and_so_does_the_next_failure() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert!(s.lost(t0, &quinn::ConnectionError::TimedOut), "losing the standby went unrecorded");
        let search = search_at(&mut s, t0 + standby_backoff(0));
        assert!(s.not_found(search, t0 + standby_backoff(0), "no path".to_string()));
    }

    #[tokio::test]
    async fn a_probe_result_after_its_outage_ended_does_not_count() {
        let t0 = Instant::now();
        let (mut s, _host) = with_a_standby(t0).await;
        assert!(matches!(s.step(silent(t0), t0, false), StandbyAction::Probe { .. }));
        s.step(Phase::Live, t0 + Duration::from_millis(100), false);
        assert!(!s.probed(true, t0 + Duration::from_millis(200)), "a stale answer counted");

        assert!(matches!(s.step(silent(t0), t0 + Duration::from_secs(1), false), StandbyAction::Probe { .. }));
        assert!(s.probed(true, t0 + Duration::from_secs(1)), "a current answer did not count");
    }
```

These replace `a_failed_search_is_reported_once_and_backs_off` and `losing_the_standby_is_reported_once`. In `a_standby_closed_by_the_session_ending_goes_unannounced` (rename `…_goes_unrecorded`), keep `!s.lost(…)` and change the second assertion's message to "a current failure after the close went unrecorded"; its doc drops the sentence about the dry spell's one announcement. In `a_search_over_a_replaced_primary_changes_nothing`, the message "announced a dry spell" → "counted as current"; the doc's "must neither say \"no standby path\" nor" → "must neither be recorded nor".

- [ ] **Step 3: Run them to see them fail**

Run: `cargo test --jobs 4 --bin oxutrm -- --test-threads 4`
Expected: FAIL to compile (`on_standby_event`, `fail_over`, `rebuild_landed`, … not found).

- [ ] **Step 4: `standby.rs`**

1. `link: Option<(Link, PathDescription)>` → `link: Option<Established>`, with the doc "The standby, as its search established it."
2. `connection`: `self.link.as_ref().map(|e| e.link.sink.connection().clone())`. `path`: `self.link.as_ref().map(|e| &e.path)`. `rtt`: `self.link.as_ref().map(|e| e.link.sink.connection().rtt())`.
3. `found`: `self.link = Some(e);` and delete `self.told_none = false;`.
4. Delete the `told_none` field, its initialiser and `fn dry_spell_news`.
5. `not_found`:

```rust
    /// A search finished without one. Returns whether the result was for the
    /// search still wanted -- the one worth recording.
    ///
    /// A stale search changes nothing, `reason` included: it most likely
    /// failed because the primary it ran over was closed under it, which says
    /// nothing about the path the session has now.
    pub(crate) fn not_found(&mut self, search: u64, now: Instant, reason: String) -> bool {
        if search != self.search {
            return false;
        }
        self.searching = false;
        self.last_failure = Some(reason);
        self.backoff(now);
        true
    }
```

6. `probed`:

```rust
    /// Whatever a probe found. Returns whether it was for the probe in
    /// flight: a result whose outage already ended is dropped, because
    /// `step` reset `probe` to `Idle`, and `Idle` is not `Pending`.
    pub(crate) fn probed(&mut self, answered: bool, now: Instant) -> bool {
        self.probing = false;
        let ProbeState::Pending { sent } = self.probe else {
            return false;
        };
        self.probe = if answered {
            ProbeState::Answered { sent }
        } else {
            ProbeState::Failed { at: now }
        };
        true
    }
```

7. `lost`: doc "The standby's connection closed on its own. Returns whether that is worth recording: not when the shell exited or another client took the session over -- the session is ending, and the primary's close is about to say why." Body ends with `!ending` instead of `!ending && self.dry_spell_news()`.
8. `take_for_failover(&mut self, now: Instant) -> Option<Established>` (body unchanged; `self.link.take()` now yields an `Established`). `forget`: `if let Some(e) = self.link.take() { close(&e.link, REBUILT); }`.
9. Module and `StandbyEvent::NotFound` docs: the user is no longer "told only that there is no standby (spec §3.7)"; say the reason is recorded in the activity log and kept on `last_failure` for the popup's standby block.

- [ ] **Step 5: `session.rs`**

1. Delete `announce_standby`, `announce_failover` and `say_mid_session` (and their docs).
2. `announce` — replace the body after the `same` early return:

```rust
        if self.path.is_some() {
            // A migration, not a fresh connect. The session owns the screen
            // by now, so it goes to the activity log, not the terminal.
            self.activity.record(
                Kind::Link,
                &format!("path migrated \u{2192} {}", oxutrm_client::rung_label(path)),
            );
            self.path = Some(path.clone());
            return Ok(true);
        }

        writeln!(out, "{}", status_line(path)).context("announcing the path")?;
        out.flush().context("flushing the terminal")?;

        // The line was written outside the renderer's model of the screen, so
        // that model is now wrong by one row. Anything less than a full
        // repaint would leave the terminal and the model disagreeing.
        self.renderer.invalidate();
        self.path = Some(path.clone());
        Ok(true)
```

and in its doc replace "Called again with a different path it announces the migration briefly, because walking from Wi-Fi to mobile should be explained rather than mysterious." with "Called again with a different path it records the migration in the activity log, where the popup shows it: walking from Wi-Fi to mobile should be explained rather than mysterious, and the session owns the screen by then."

3. Beside `with_identity`:

```rust
    /// Keep the activity log in `activity` -- the one with the file, which
    /// `connect` opens -- instead of the ring-only log `new` starts with.
    pub(crate) fn with_activity(mut self, activity: Activity) -> ClientSession {
        self.activity = activity;
        self
    }
```

4. In `route_keys`, the two held-input arms:

```rust
            Some(Command::SendHeld) => {
                let held = self.link_state.take_held();
                self.activity
                    .record(Kind::Input, &format!("held input sent ({} bytes)", held.len()));
                self.turn(&held, out)?;
            }
            Some(Command::DropHeld) => {
                let n = self.link_state.held().len();
                self.link_state.drop_held();
                self.activity
                    .record(Kind::Input, &format!("held input dropped ({n} bytes)"));
            }
```

5. In `rebuild_step`, bind the attempt (`let Phase::Recovering { attempt, next_try } = phase else {`) and after `self.link_state.begin_attempt(now);`:

```rust
        self.activity
            .record(Kind::Rebuild, &format!("attempt {} started", attempt.saturating_add(1)));
```

6. New methods, after `rebuild_step`:

```rust
    /// A rebuild attempt failed for a reason worth retrying.
    fn rebuild_failed(&mut self, why: String, now: Instant) {
        if let Phase::Recovering { attempt, .. } = self.link_state.phase_now() {
            self.activity.record(
                Kind::Rebuild,
                &format!("attempt {} failed: {why}", attempt.saturating_add(1)),
            );
        }
        self.link_state.attempt_failed(now);
        // Kept for the popup's recovering section.
        self.last_failure = Some(why);
    }

    /// A rebuild attempt landed: its link replaces the one that stopped
    /// answering, and its path and attach are the session's from here.
    fn rebuild_landed(&mut self, e: crate::connect::Established, now: Instant) -> Result<()> {
        self.swap_in(e.link, now)?;
        if let Some(id) = self.identity.as_mut() {
            id.attach_id = e.attach_id;
        }
        self.activity.record(
            Kind::Rebuild,
            &format!("landed via {}", oxutrm_client::rung_label(&e.path)),
        );
        self.path = Some(e.path);
        Ok(())
    }

    /// Swap the standby in for a primary that stopped answering (spec §3.5).
    /// Returns whether there was one to swap in.
    fn fail_over<W: Write>(&mut self, now: Instant, out: &mut W) -> Result<bool> {
        let Some(e) = self.standby.as_mut().and_then(|s| s.take_for_failover(now)) else {
            return Ok(false);
        };
        self.swap_in_as(e.link, now, SWITCHED)
            .context("failing over to the standby")?;
        // Spec §3.5 step 2: the host adopts the standby on our first frame
        // on it, so that frame goes now.
        self.turn(&[], out)?;
        if let Some(id) = self.identity.as_mut() {
            id.attach_id = e.attach_id;
        }
        self.activity.record(
            Kind::Failover,
            &format!("switched to standby ({})", oxutrm_client::rung_label(&e.path)),
        );
        self.path = Some(e.path);
        Ok(true)
    }

    /// A standby search or probe reported back. Returns the connection the
    /// loop should watch for closing, when a search found one that was kept.
    fn on_standby_event(
        &mut self,
        event: crate::standby::StandbyEvent,
        now: Instant,
    ) -> Option<quinn::Connection> {
        let s = self.standby.as_mut()?;
        match event {
            crate::standby::StandbyEvent::Found { search, e } => {
                let path = e.path.clone();
                if !s.found(search, *e) {
                    return None;
                }
                let watch = s.connection();
                self.activity.record(
                    Kind::Standby,
                    &format!("found {}, {} ms", oxutrm_client::rung_label(&path), path.rtt_ms),
                );
                watch
            }
            crate::standby::StandbyEvent::NotFound { search, reason } => {
                let text = format!("not found: {reason}");
                if s.not_found(search, now, reason) {
                    self.activity.record(Kind::Standby, &text);
                }
                None
            }
            crate::standby::StandbyEvent::Probed { answered } => {
                if s.probed(answered, now) {
                    self.activity.record(
                        Kind::Failover,
                        if answered { "probe answered" } else { "probe failed" },
                    );
                }
                None
            }
        }
    }

    /// The standby's connection closed. Whatever the reason, the primary is
    /// untouched: a standby's close is information, not a reason to end
    /// anything.
    fn on_standby_closed(&mut self, reason: &quinn::ConnectionError, now: Instant) {
        if self.standby.as_mut().is_some_and(|s| s.lost(now, reason)) {
            self.activity.record(Kind::Standby, &format!("lost: {reason}"));
        }
    }
```

7. In `run_on`:
   - `Wake::Rebuilt`: `Landed(established)` → `self.rebuild_landed(*established, Instant::now()).context("swapping in the rebuilt link")?;` followed by the existing local updates (`conn = …; takeover_expected = false; standby_conn = None; _search_task = None;`) and their comments. `Retry(why)` → `self.rebuild_failed(why, Instant::now()),` (the comment "The loop keeps its cadence and the popup explains the last try." stays above it).
   - The three `Wake::Standby(…)` arms become one:

```rust
                Wake::Standby(event) => {
                    if let Some(watch) = self.on_standby_event(event, Instant::now()) {
                        standby_conn = Some(watch);
                    }
                }
```

   - `Wake::StandbyClosed(reason)`:

```rust
                // Whatever the reason, the standby is gone; `lost` decides
                // whether it is worth recording.
                Wake::StandbyClosed(reason) => {
                    standby_conn = None;
                    self.on_standby_closed(&reason, Instant::now());
                }
```

   - `StandbyAction::Search { search }`: first line `self.activity.record(Kind::Standby, "search started");`. `StandbyAction::Probe { nonce }`: inside the `if let Some(standby)`, first line `self.activity.record(Kind::Failover, "probing standby");`.
   - `StandbyAction::FailOver`:

```rust
                crate::standby::StandbyAction::FailOver => {
                    if self.fail_over(now, out)? {
                        conn = self.link.sink.connection().clone();
                        standby_conn = None;
                        // `take_for_failover` disowned any search running
                        // over the old primary; this stops it.
                        _search_task = None;
                        takeover_expected = false;
                    }
                }
```

8. `src/main.rs`: delete the attribute and comment on `mod activity;`.

9. `src/activity.rs`: add to `impl LogFile`, after `open`:

```rust
    /// `$XDG_STATE_HOME/oxutrm/client.log`, else
    /// `~/.local/state/oxutrm/client.log`, capped at [`LOG_CAP`].
    pub(crate) fn open_default() -> std::io::Result<LogFile> {
        let path = state_path(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"))
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "neither XDG_STATE_HOME nor HOME names a directory",
                )
            })?;
        LogFile::open(path, LOG_CAP)
    }
```

No test calls it: it would write into the home of whoever runs the tests. It is three lines over `state_path` and `open`, which are tested; `connect` is its caller.

10. `src/connect.rs`: chain the file-backed log after `with_identity`:

```rust
        .with_activity(Activity::with_file(
            LogFile::open_default(),
            target,
            &established.session_id,
        ));
```

with `use crate::activity::{Activity, LogFile};`. `established.link` has moved into `ClientSession::new` by then, but `Established` has no `Drop`, so its other fields stay usable after that partial move — as `announce(&established.path, …)` a few lines further down already relies on.

- [ ] **Step 6: Run the tests**

Run: `cargo test --jobs 4 --bin oxutrm -- --test-threads 4`
Expected: PASS.

- [ ] **Step 7: Injection**

1. In `fail_over`, add `write!(out, "oxutrm  switched\r\n")?;` before `self.turn(&[], out)?;`. `a_failover_is_recorded_and_the_path_follows_it` and `a_dead_primary_fails_over_onto_an_answering_standby` must FAIL on `outside_renderer`. Restore.
2. In `on_standby_event`'s `NotFound` arm, record unconditionally (drop the `if`). `a_failed_search_is_recorded_with_its_reason_and_a_stale_one_is_not` must FAIL. Restore.
3. In `rebuild_landed`, delete the `identity` update. `a_landed_rebuild_is_recorded_and_its_path_and_attach_become_the_sessions` must FAIL. Restore.
4. In `run_on`'s `Wake::StandbyClosed` arm, delete the `self.on_standby_closed(…)` call. `a_standby_closed_under_a_running_session_is_recorded_and_forgotten` must FAIL (the screen never shows `lost:`). Restore.

- [ ] **Step 8: Gate and commit**

```bash
git add src/session.rs src/standby.rs src/connect.rs src/loopback.rs src/activity.rs src/main.rs
git commit -m "$(cat <<'EOF'
feat(client): record what oxutrm does; stop writing over the session

Standby searches, finds, failures and losses, probes and the failover,
rebuild attempts, held input sent or dropped, and path migrations go to
the activity log -- and through it to the popup and to client.log --
instead of lines written over the screen. The connect banner is the only
line left. The session's path and attach id follow a failover or a
landed rebuild.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 8: End-to-end — the popup through the real loop

**Files:**
- Modify: `src/session.rs` (test module only)

**Interfaces:**
- Consumes: `pair`, `pair_through_relay_sized`, `BIG`, `keyboard()`, `SharedOut`, `screen_of`, `wait_for_screen`, `outside_renderer` (Task 7), `ClientSession::run_on`, `client.activity`, `client.screen()`, `text()`.
- Produces: two tests. No production code: if either fails, the defect is in Tasks 4–7, and the fix goes there with a test of its own at that level.

- [ ] **Step 1: Write the tests**

```rust
    /// Spec §7, end to end: the link goes dark under a running session, the
    /// popup opens by itself and says so on the screen, and when the link
    /// comes back it says that too -- with nothing written to the terminal
    /// outside the renderer at any point.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_outage_raises_the_popup_and_its_end_is_reported() {
        let (mut host, mut client, relay) = pair_through_relay_sized("", BIG).await;
        let host_loop = tokio::spawn(async move { host.run().await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        typing.write_all(b"printf 'ready-%s\\n' ok\n").expect("type");
        wait_for_screen(&out, BIG, "ready-ok", Duration::from_secs(10)).await;
        assert!(!screen_of(&out, BIG).contains("SILENT"), "the popup was up before the outage");

        relay.blackhole(true);
        // Sent while the phase is still `Live`, so a reply is owed and the
        // silence is noticed.
        typing.write_all(b"true\n").expect("type");
        wait_for_screen(&out, BIG, "\u{25cf} SILENT", Duration::from_secs(10)).await;

        relay.blackhole(false);
        wait_for_screen(&out, BIG, "LIVE again", Duration::from_secs(20)).await;

        // `e` closes the lingering popup and goes to the shell with the rest.
        typing.write_all(b"exit 4\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(20), client_loop)
            .await
            .unwrap_or_else(|_| panic!("the client never finished; the screen was:\n{}", screen_of(&out, BIG)))
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 4);
        assert_eq!(host_loop.await.expect("host task").expect("host loop"), 4);

        let link: Vec<String> = client
            .activity
            .entries()
            .filter(|e| e.kind == crate::activity::Kind::Link)
            .map(|e| e.text.clone())
            .collect();
        assert_eq!(link.first().map(String::as_str), Some("silent"), "{link:?}");
        assert!(link.get(1).is_some_and(|t| t.starts_with("live again via this link, outage ")), "{link:?}");
        let stray = outside_renderer(&out.bytes());
        assert!(stray.is_empty(), "written outside the renderer: {:?}", String::from_utf8_lossy(&stray));
    }

    /// A quick double `Ctrl-\` is one literal `Ctrl-\` for the remote
    /// program, through the real keyboard path. `stty -isig -echo` makes the
    /// shell's `cat -v` print exactly what arrives, once: two literals would
    /// read `^\^\`, none would read nothing.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_quick_double_ctrl_backslash_reaches_the_shell_as_one_literal() {
        let (mut host, mut client) = pair("").await;
        let host_loop = tokio::spawn(async move { host.run().await });
        let (keys, mut typing) = keyboard();
        let out = SharedOut::default();
        let client_loop = tokio::spawn({
            let mut out = out.clone();
            async move {
                let code = client.run_on(keys, &mut out).await;
                (code, client)
            }
        });

        // `ready-ok` is printed after `stty` has run, so once it is on the
        // screen nothing typed afterwards can be eaten as a signal.
        typing
            .write_all(b"stty -isig -echo; printf 'ready-%s\\n' ok; cat -v\n")
            .expect("type");
        wait_for_screen(&out, size(), "ready-ok", Duration::from_secs(10)).await;
        assert!(!screen_of(&out, size()).contains("^\\"), "a literal arrived before it was typed");

        // One read: both presses carry the same timestamp, well inside the
        // window.
        typing.write_all(&[CTRL_BACKSLASH, CTRL_BACKSLASH, b'\n']).expect("type");
        wait_for_screen(&out, size(), "^\\", Duration::from_secs(10)).await;

        // EOF ends `cat`; `-isig` leaves VEOF working.
        typing.write_all(b"\x04exit 5\n").expect("type");
        let (code, client) = tokio::time::timeout(Duration::from_secs(20), client_loop)
            .await
            .expect("the client never finished")
            .expect("client task");
        assert_eq!(code.expect("the client loop failed"), 5);
        assert_eq!(host_loop.await.expect("host task").expect("host loop"), 5);

        let screen = text(client.screen());
        assert!(screen.lines().any(|l| l.trim_end() == "^\\"), "{screen}");
        assert!(!screen.contains("^\\^\\"), "two literals arrived: {screen}");
        assert!(client.shown.is_none(), "the double press left the popup up");
    }
```

- [ ] **Step 2: Run them**

Run: `cargo test --workspace --jobs 4 an_outage_raises_the_popup -- --test-threads 4` and `cargo test --workspace --jobs 4 a_quick_double_ctrl_backslash -- --test-threads 4`.
Expected: PASS. (They exercise code that already exists; the injections below are what prove they can fail.)

- [ ] **Step 3: Injection**

1. In `Ui::live_key`, delete `r.to_host.push(PREFIX);` from the double-press branch. `a_quick_double_ctrl_backslash_reaches_the_shell_as_one_literal` must FAIL (no `^\` ever reaches the screen). Restore.
2. In `Ui::live_key`, make the Closed branch push `PREFIX` to the host as well as opening (`r.to_host.push(b)` unconditionally). The double-press test must FAIL with `^\^\`. Restore.
3. In `Ui::tick`, change `Mode::Auto if phase == Phase::Live =>` to set `Mode::Closed` instead of `Lingering`. `an_outage_raises_the_popup_and_its_end_is_reported` must FAIL (no `LIVE again`). Restore.
4. In `ClientSession::popup_at`, return `None` unconditionally. The outage test must FAIL (no `● SILENT`). Restore.

- [ ] **Step 4: Gate and commit**

```bash
git add src/session.rs
git commit -m "$(cat <<'EOF'
test(client): the status popup end to end through the real loop

A blackholed link raises the popup on the replayed screen, its return is
reported as LIVE again, and nothing reaches the terminal outside the
renderer; a quick double Ctrl-\ arrives at the shell as one literal.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

### Task 9: `CHANGES.md` and `README.md`

**Files:**
- Modify: `CHANGES.md`
- Modify: `README.md`

**Interfaces:** none (documentation). Read both files' relevant sections before editing, and check every claim below against the code as merged — in particular the key bindings (`src/ui.rs`), the log path and cap (`src/activity.rs`), and the linger (`ui::LINGER`).

- [ ] **Step 1: `CHANGES.md`**

Under `## Unreleased` → `### New`, append:

```markdown
- **A status popup shows what the connection is doing.** `Ctrl-\` opens it
  while the link is healthy; pressed twice within half a second it sends one
  literal `Ctrl-\` to the remote program instead. It also opens by itself two
  seconds into an outage, and when the link comes back it says how — `● LIVE
  again via IPv4 punched · outage 4.2 s` — for three seconds before closing,
  unless you pressed a key in it. It shows the round-trip time now and its
  minimum, average and maximum over the last minute, loss, throughput, an RTT
  sparkline with gaps where the link was down, which link of the session this
  is, the standby and what it is doing, what the rebuild loop is trying and
  why its last attempt failed, and the last things oxutrm did. While the link
  is healthy `Esc` or `Ctrl-\` closes it, `q` quits, and any other key closes
  it and goes to the remote program, so typing never disappears into it. While
  the link is down everything you type is held, as before — `q`, `Esc` and all
  — and the popup's commands keep their `Ctrl-\` prefix: `Ctrl-\ q` quits, and
  once the host answers again `Ctrl-\ s` sends what you typed and `Ctrl-\ d`
  drops it. `c config` and `s sessions` are shown dimmed; they come later.

- **What oxutrm does to keep a session alive is logged.** Outages and their
  end, standby searches, finds and losses, probes and failovers, rebuild
  attempts and why they failed, held input sent or dropped: each is appended
  to `$XDG_STATE_HOME/oxutrm/client.log`, or `~/.local/state/oxutrm/client.log`,
  one line each, with the time in UTC, the ssh target and the start of the
  session id. Repeats are folded into one line. The file is rotated to
  `client.log.1` before it passes 1 MiB, so the two never hold more than
  2 MiB, and two clients sharing it cannot interleave inside a line. If it
  cannot be written, the popup says so once and the session carries on.
```

Under `### Changed`, append:

```markdown
- **Nothing is written over the session any more.** The `standby: …`, `no
  standby path`, `switched to standby …` and `path migrated …` lines are gone:
  they were wiped by the repaint that followed them and survived only in the
  scrollback. Their content is in the popup and the log. The one line still
  printed is the connect banner, before the terminal goes into raw mode.

- **The box that appeared during an outage is the popup now.** What it said —
  how long the host has been silent, what was typed blind, the rebuild attempt
  and its countdown, the question about held input — is a section of the
  popup, and the keys that answered it are the same.
```

- [ ] **Step 2: `README.md`**

1. Replace the paragraph beginning "Finding, losing or switching to a standby is announced once…" and the three indented example lines after it with:

```markdown
`Ctrl-\` opens a status popup on a healthy link, and an outage opens it by
itself: the round-trip time and its range over the last minute, loss,
throughput, which link of the session this is, the standby and what it is
doing, what the rebuild is trying, and the last things oxutrm did. Two quick
presses send one literal `Ctrl-\` to the remote program. Everything it shows
is also appended to `~/.local/state/oxutrm/client.log` (or under
`$XDG_STATE_HOME`), capped at 2 MiB with one rotation.
```

2. In the next paragraph, `"no standby path" means the session has no fallback` → `A standby row that says "none" means the session has no fallback`.
3. "the attempt fails cleanly and the box on screen says why" → "the attempt fails cleanly and the status popup says why".

- [ ] **Step 3: Check, gate and commit**

Run `grep -n 'no standby path\|announced once\|box on screen' README.md CHANGES.md` — the only hits left should be the older `CHANGES.md` entries that describe what earlier versions did (those stay; they are history). Run the gate; expected exit 0.

```bash
git add CHANGES.md README.md
git commit -m "$(cat <<'EOF'
docs: the status popup and the activity log

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
EOF
)"
```

---

## Spec coverage

| Spec | Task |
|---|---|
| §1.1 Ctrl-\ opens / double press | 4 (`ui.rs`), 6 (wired), 8 (e2e) |
| §1.1 layout: centred box, status, log, key bar | 1 |
| §1.1 linger ~3 s unless a key | 4, 6 |
| §1.1 quality facts | 3, 5 |
| §1.1 log in memory and file, 2 MiB | 2, 7 |
| §1.2 greyed `c config`, `s sessions` | 5 (deviation 9) |
| §2 units and constraints (no timer, deny, C1, escaping) | 1–5, Global Constraints |
| §3.1 states | 4 |
| §3.2 keys | 4 (deviations 1–3) |
| §4 quality ring, segments, standby block, identity | 3, 5, 6, 7 |
| §5.1 recording and event table | 2, 6 (`link`), 7 (the rest) |
| §5.2 retired screen writes | 7 |
| §5.3 the file | 2 (deviations 4–5), 7 (`connect`) |
| §6 rendering, repaint on change, resize, notice.rs removed | 1, 5, 6 |
| §7 testing | each task; 8 for session level |
| §8 changelog | 9 |

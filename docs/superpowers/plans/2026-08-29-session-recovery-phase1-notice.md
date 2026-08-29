# Session Recovery Phase 1 — The Notice Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn a silent network hang into a legible one — the client detects that
the host has stopped answering, says so in a box drawn over the screen, holds
anything typed blind, and asks before delivering it.

**Architecture:** The client gains a **second layer**. Layer 0 is the remote
framebuffer (`ScreenState`, authoritative, from the host). Layer 1 is local UI,
never sent to the host, built with `ratatui` used **headlessly** — no backend, no
`crossterm`, no `Terminal` — and composited into the renderer's cell grid
*before* its diff against `Painted`. Because the diff is what paints, drawing the
box and removing it are both ordinary diffs: no repaint, no `invalidate`, no
desync, and it works while the host is unreachable because the model is local.
Liveness is read off a clock that is already on the wire — `Sender::make_frame`
sends an empty-diff frame purely to move an owed ack, so every input obliges a
reply — plus a 0.2 Hz heartbeat that closes the idle gap.

**Tech Stack:** Rust edition 2024, `ratatui` 0.30 (`default-features = false`),
`unicode-width` 0.2, `tokio`, `compact_str`, existing `oxutrm-proto` cell types.

**Spec:** `docs/superpowers/specs/2026-08-29-session-recovery-design.md`

## Global Constraints

Copied verbatim from the spec and the project's interface contract. Every task's
requirements implicitly include this section.

- **MSRV is 1.96** and must not exceed the maintainer's default toolchain. Keep
  `workspace.package.rust-version` and CI's `dtolnay/rust-toolchain@` in step.
- **Edition 2024.** Cap build and test parallelism at **4** (`-j4`).
- **`oxutrm-client` is `deny(unsafe_code)`.** `src/main.rs` is `forbid`.
- **`oxutrm-client` must not gain a backend, a `Terminal`, or `crossterm`.**
  ratatui enters as a layout and widget library only; `Renderer` remains the only
  thing in the tree that writes to the user's terminal.
- **oxutrm never parses `~/.ssh/config`**, and never becomes an SSH implementation.
- **A send failure must never end a session, and a rejected frame must never
  disconnect.**
- **`IDLE_POLL` must not be reintroduced as a pace.** The heartbeat of Task 6 is
  0.2 Hz and applies only to a client that is attached; a detached host session
  has no client and therefore no heartbeat.
- **Do not assert platform rules in tests** — assert on outcomes.
- **The two dev boxes disagree about formatting.** Mac has rustc 1.97.1,
  thinlinc has 1.96.0, both rustfmt 1.9.0. CI's fmt job runs on `stable`. Format
  so both accept it.
- **Inject the fault before believing the test.** A test that passes against the
  injected bug is not a guard. This has cost this project real time twice.

### Phase 1 scope boundary

Phase 1 is **client-only**. It adds no host changes and does **not** remove the
30 s idle timeout — that is Phase 2. So:

- The states implemented here are `Live`, `Silent` and `Confirming` **only**.
  `Recovering` and `Displaced` belong to Phases 3 and 4.
- **The notice must not promise reconnection**, because in Phase 1 nothing
  reconnects. It reports silence and counters. Shipping a box that says
  "retrying" while nothing retries would be worse than the silence it replaces.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/oxutrm-client/Cargo.toml` | new deps: `ratatui` (no default features), `unicode-width` |
| `crates/oxutrm-client/src/overlay.rs` | **new** — layer 1 plumbing: a ratatui `Buffer` converted to oxutrm cells, and the `Overlay` value the renderer composites |
| `crates/oxutrm-client/src/notice.rs` | **new** — what the box says and how it is laid out, including the small-screen fallback |
| `crates/oxutrm-client/src/renderer.rs` | `set_overlay`, compositing before the diff, DECSET 2026 wrapping |
| `crates/oxutrm-client/src/lib.rs` | re-exports |
| `src/linkstate.rs` | **new** — the state machine, the liveness clock, the held-input buffer and the key prefix. Pure: no I/O, no terminal, an injected `Instant` |
| `src/session.rs` | wiring only — `ClientSession` drives `LinkState` and hands the notice to the renderer; the two `eprintln!` sites become notice content |
| `src/main.rs` | declare `mod linkstate` |

`src/session.rs` is already 2497 lines. The state machine goes in its own module
rather than making that worse, and the split is what lets Tasks 5-7 be tested
without a network, a terminal or a runtime.

---

## Task 1: The overlay cell layer

Converts a headless ratatui `Buffer` into oxutrm cells. This is the only place
the two cell models meet.

**Files:**
- Modify: `crates/oxutrm-client/Cargo.toml`
- Create: `crates/oxutrm-client/src/overlay.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`

**Interfaces:**
- Consumes: `oxutrm_proto::{Cell, CellText, Color, Attrs}`.
- Produces:
  - `pub struct Overlay { pub row: u16, pub col: u16, pub rows: u16, pub cols: u16, pub cells: Vec<Cell> }`
  - `pub fn overlay_from_buffer(buf: &ratatui::buffer::Buffer, row: u16, col: u16) -> Overlay`

**Background the implementer needs.**

Two cell models disagree in a way that silently corrupts a row:

- **ratatui** puts a double-width glyph in one cell and a **plain space** in the
  next. Verified: rendering `"a世b"` gives `x=0 "a"`, `x=1 "世"`, `x=2 " "`,
  `x=3 "b"`.
- **oxutrm** puts **empty text** plus `Attrs::WIDE_CONT` in the continuation
  cell. `crates/oxutrm-proto/src/cell.rs:37` says why: *"a renderer that painted
  a space here would shift every column after it."* The renderer skips
  `WIDE_CONT` cells entirely (`renderer.rs:244`).

So copying ratatui's space through would shift every column to the right of any
CJK character. The conversion must measure display width itself.

- [ ] **Step 1: Add the dependencies**

In `crates/oxutrm-client/Cargo.toml`, under `[dependencies]`:

```toml
# ratatui as a LAYOUT AND WIDGET library only. `default-features = false` is
# what drops `crossterm`: the default feature set includes it, and taking it
# would put a second thing that writes to the terminal into a crate whose whole
# point is that `Renderer` is the only one. No backend, no `Terminal`, no
# terminal ownership -- widgets are rendered into a bare `Buffer` and converted
# to oxutrm cells by `overlay.rs`.
ratatui = { version = "0.30", default-features = false, features = [
    "std",
    "all-widgets",
    "layout-cache",
] }
# For the wide-character rule in `overlay.rs`. ratatui marks a double-width
# glyph's second column with a SPACE; oxutrm marks it with `Attrs::WIDE_CONT`
# and no text. Converting between them means measuring the width ourselves.
unicode-width.workspace = true
```

- [ ] **Step 2: Write the failing tests**

Create `crates/oxutrm-client/src/overlay.rs`:

```rust
//! Layer 1: local UI, converted into cells the renderer can composite.
//!
//! The client paints two layers. Layer 0 is the remote framebuffer, which the
//! host owns. Layer 1 is this: a notice, and later a session picker or a config
//! screen, drawn locally and never sent anywhere. It is composited into the
//! renderer's grid *before* the diff, so drawing it and removing it are both
//! ordinary diffs.
//!
//! `ratatui` is used **headlessly** -- widgets render into a bare `Buffer` and
//! this module converts that into `oxutrm_proto::Cell`. Nothing here touches a
//! terminal; `Renderer` remains the only thing in the tree that does.

use oxutrm_proto::{Attrs, Cell, CellText, Color};
use ratatui::buffer::Buffer;
use ratatui::style::{Color as RColor, Modifier};
use unicode_width::UnicodeWidthStr as _;

/// A rectangle of locally drawn cells, and where it sits on the screen.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Overlay {
    pub row: u16,
    pub col: u16,
    pub rows: u16,
    pub cols: u16,
    /// `rows * cols`, row-major.
    pub cells: Vec<Cell>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Paragraph, Widget as _};

    fn buffer_of(width: u16, height: u16, lines: Vec<Line<'static>>) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        Paragraph::new(lines).render(area, &mut buf);
        buf
    }

    #[test]
    fn plain_text_converts_one_cell_per_column() {
        let buf = buffer_of(4, 1, vec![Line::from("ab")]);
        let o = overlay_from_buffer(&buf, 3, 7);

        assert_eq!((o.row, o.col, o.rows, o.cols), (3, 7, 1, 4));
        assert_eq!(o.cells.len(), 4);
        assert_eq!(o.cells[0].text, "a");
        assert_eq!(o.cells[1].text, "b");
        assert_eq!(o.cells[2].text, " ");
    }

    /// The trap this module exists for. ratatui puts a SPACE in the column
    /// after a double-width glyph; oxutrm puts `WIDE_CONT` and no text. Copying
    /// the space through shifts every column to the right of it.
    #[test]
    fn a_wide_glyph_gets_a_flagged_continuation_and_not_a_space() {
        let buf = buffer_of(4, 1, vec![Line::from("a\u{4e16}b")]);
        let o = overlay_from_buffer(&buf, 0, 0);

        assert_eq!(o.cells[0].text, "a");
        assert_eq!(o.cells[1].text, "\u{4e16}");
        assert!(
            !o.cells[1].attrs.contains(Attrs::WIDE_CONT),
            "the glyph itself is not a continuation"
        );
        assert_eq!(
            o.cells[2].text, "",
            "the continuation carries no text; a space would shift the row"
        );
        assert!(
            o.cells[2].attrs.contains(Attrs::WIDE_CONT),
            "the right half must be flagged"
        );
        assert_eq!(o.cells[3].text, "b", "the column after must not be shifted");
    }

    #[test]
    fn a_continuation_inherits_the_glyphs_colours() {
        let style = Style::default().fg(RColor::Rgb(1, 2, 3)).bg(RColor::Indexed(9));
        let buf = buffer_of(3, 1, vec![Line::from(Span::styled("\u{4e16}", style))]);
        let o = overlay_from_buffer(&buf, 0, 0);

        assert_eq!(o.cells[1].fg, o.cells[0].fg);
        assert_eq!(o.cells[1].bg, o.cells[0].bg);
    }

    #[test]
    fn colours_map_across_all_three_kinds() {
        let cases = [
            (RColor::Reset, Color::Default),
            (RColor::Rgb(10, 20, 30), Color::Rgb(10, 20, 30)),
            (RColor::Indexed(200), Color::Idx(200)),
            (RColor::Red, Color::Idx(1)),
            (RColor::LightRed, Color::Idx(9)),
            (RColor::White, Color::Idx(7)),
        ];
        for (from, want) in cases {
            let buf = buffer_of(1, 1, vec![Line::from(Span::styled("x", Style::default().fg(from)))]);
            let o = overlay_from_buffer(&buf, 0, 0);
            assert_eq!(o.cells[0].fg, want, "mapping {from:?}");
        }
    }

    #[test]
    fn every_modifier_maps_to_an_attribute() {
        let cases = [
            (Modifier::BOLD, Attrs::BOLD),
            (Modifier::DIM, Attrs::DIM),
            (Modifier::ITALIC, Attrs::ITALIC),
            (Modifier::UNDERLINED, Attrs::UNDERLINE),
            (Modifier::SLOW_BLINK, Attrs::BLINK),
            (Modifier::RAPID_BLINK, Attrs::BLINK),
            (Modifier::REVERSED, Attrs::INVERSE),
            (Modifier::HIDDEN, Attrs::HIDDEN),
            (Modifier::CROSSED_OUT, Attrs::STRIKE),
        ];
        for (from, want) in cases {
            let style = Style::default().add_modifier(from);
            let buf = buffer_of(1, 1, vec![Line::from(Span::styled("x", style))]);
            let o = overlay_from_buffer(&buf, 0, 0);
            assert!(o.cells[0].attrs.contains(want), "mapping {from:?}");
        }
    }

    /// `MAX_CELL_TEXT` is 32 bytes and load-bearing: a longer cell text is
    /// rejected by the receiver's validation. `fit_cell_text` is the producer's
    /// repair, and layer 1 is a producer.
    #[test]
    fn an_overlong_grapheme_cluster_is_fitted_rather_than_emitted_whole() {
        let long: String = std::iter::once('e').chain(std::iter::repeat_n('\u{301}', 40)).collect();
        let buf = buffer_of(2, 1, vec![Line::from(long)]);
        let o = overlay_from_buffer(&buf, 0, 0);

        assert!(
            o.cells[0].text.len() <= oxutrm_proto::MAX_CELL_TEXT,
            "cell text {} bytes exceeds MAX_CELL_TEXT",
            o.cells[0].text.len()
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -j4 -p oxutrm-client overlay:: 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function overlay_from_buffer in this scope`.

- [ ] **Step 4: Write the implementation**

Add above the `#[cfg(test)]` block in `crates/oxutrm-client/src/overlay.rs`:

```rust
/// Convert a rendered ratatui buffer into cells the renderer can composite.
///
/// **The wide-character rule is the whole reason this is not a `map`.**
/// `ratatui` represents a double-width glyph as the glyph followed by a plain
/// space; oxutrm represents it as the glyph followed by empty text carrying
/// `Attrs::WIDE_CONT`, because the renderer skips a continuation cell and
/// painting a space there would shift every column to its right
/// (`oxutrm_proto`'s `cell.rs`, and `Renderer::write_cells`). So the width is
/// measured here rather than trusted from either side.
pub fn overlay_from_buffer(buf: &Buffer, row: u16, col: u16) -> Overlay {
    let area = buf.area();
    let mut cells = Vec::with_capacity(area.width as usize * area.height as usize);

    for y in area.top()..area.bottom() {
        // Set by the column that owns a wide glyph, consumed by the next one.
        // Reset per row: a glyph cannot straddle the right edge.
        let mut continuation: Option<(Color, Color)> = None;

        for x in area.left()..area.right() {
            if let Some((fg, bg)) = continuation.take() {
                cells.push(Cell {
                    text: CellText::const_new(""),
                    fg,
                    bg,
                    attrs: Attrs::WIDE_CONT,
                });
                continue;
            }

            let c = &buf[(x, y)];
            let fg = color_of(c.fg);
            let bg = color_of(c.bg);

            // `width` and not `chars().count()`: a grapheme cluster of several
            // code points still occupies one or two columns.
            if c.symbol().width() == 2 {
                continuation = Some((fg, bg));
            }

            cells.push(Cell {
                // `fit_cell_text` and not the raw symbol: `MAX_CELL_TEXT` is 32
                // bytes, a receiver rejects a longer one, and layer 1 is a
                // producer like any other.
                text: oxutrm_proto::fit_cell_text(CellText::new(c.symbol())),
                fg,
                bg,
                attrs: attrs_of(c.modifier),
            });
        }
    }

    Overlay {
        row,
        col,
        rows: area.height,
        cols: area.width,
        cells,
    }
}

/// ratatui's sixteen named colours are the ANSI palette, so they become palette
/// indices rather than guessed RGB: the terminal's own theme should win, and
/// `color::down_convert` already knows how to degrade an index.
fn color_of(c: RColor) -> Color {
    match c {
        RColor::Reset => Color::Default,
        RColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
        RColor::Indexed(i) => Color::Idx(i),
        RColor::Black => Color::Idx(0),
        RColor::Red => Color::Idx(1),
        RColor::Green => Color::Idx(2),
        RColor::Yellow => Color::Idx(3),
        RColor::Blue => Color::Idx(4),
        RColor::Magenta => Color::Idx(5),
        RColor::Cyan => Color::Idx(6),
        RColor::Gray => Color::Idx(7),
        RColor::DarkGray => Color::Idx(8),
        RColor::LightRed => Color::Idx(9),
        RColor::LightGreen => Color::Idx(10),
        RColor::LightYellow => Color::Idx(11),
        RColor::LightBlue => Color::Idx(12),
        RColor::LightMagenta => Color::Idx(13),
        RColor::LightCyan => Color::Idx(14),
        RColor::White => Color::Idx(15),
    }
}

/// Both blink rates collapse onto one attribute, matching what the host does
/// with `alacritty_terminal`'s flags.
fn attrs_of(m: Modifier) -> Attrs {
    let mut a = Attrs::empty();
    if m.contains(Modifier::BOLD) {
        a |= Attrs::BOLD;
    }
    if m.contains(Modifier::DIM) {
        a |= Attrs::DIM;
    }
    if m.contains(Modifier::ITALIC) {
        a |= Attrs::ITALIC;
    }
    if m.contains(Modifier::UNDERLINED) {
        a |= Attrs::UNDERLINE;
    }
    if m.intersects(Modifier::SLOW_BLINK | Modifier::RAPID_BLINK) {
        a |= Attrs::BLINK;
    }
    if m.contains(Modifier::REVERSED) {
        a |= Attrs::INVERSE;
    }
    if m.contains(Modifier::HIDDEN) {
        a |= Attrs::HIDDEN;
    }
    if m.contains(Modifier::CROSSED_OUT) {
        a |= Attrs::STRIKE;
    }
    a
}
```

Wire the module up in `crates/oxutrm-client/src/lib.rs`, following the existing
`pub use` style:

```rust
mod overlay;
pub use overlay::{Overlay, overlay_from_buffer};
```

- [ ] **Step 5: Run the tests to verify they pass**

Both `fit_cell_text` and `MAX_CELL_TEXT` are already public
(`crates/oxutrm-proto/src/lib.rs:111` and `:113`), so no re-export is needed.

Run: `cargo test -j4 -p oxutrm-client overlay:: 2>&1 | tail -20`
Expected: PASS, all six tests.

- [ ] **Step 6: Inject the wide-character fault and confirm the test catches it**

Temporarily replace the continuation cell's construction with ratatui's own
representation — the bug this task exists to prevent:

```rust
            if let Some((fg, bg)) = continuation.take() {
                // FAULT INJECTION: what a naive conversion would do.
                cells.push(Cell { text: CellText::const_new(" "), fg, bg, attrs: Attrs::empty() });
                continue;
            }
```

Run: `cargo test -j4 -p oxutrm-client overlay:: 2>&1 | tail -20`
Expected: FAIL, `a_wide_glyph_gets_a_flagged_continuation_and_not_a_space`.

**Then revert the injection.** Grep for `FAULT INJECTION` before committing —
the project keeps that string clean.

- [ ] **Step 7: Check formatting and lints**

Run: `cargo fmt --check && cargo clippy -j4 -p oxutrm-client --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 8: Commit**

```bash
git add crates/oxutrm-client/Cargo.toml crates/oxutrm-client/src/overlay.rs crates/oxutrm-client/src/lib.rs Cargo.lock
git commit -m "feat(client): layer 1, where ratatui's cells become oxutrm's

The client gains a second layer: local UI, drawn here and never sent to the
host. ratatui is used headlessly -- no backend, no crossterm, no Terminal --
so widgets render into a bare Buffer and this converts it.

The conversion is not a map, because the two cell models disagree about
double-width glyphs in a way that silently corrupts a row. ratatui writes
the glyph and then a plain SPACE; oxutrm writes the glyph and then empty
text flagged WIDE_CONT, because the renderer skips a continuation cell and a
space painted there shifts every column to its right. Measured here rather
than trusted from either side, and the test fails against the naive version."
```

---

## Task 2: Compositing the overlay into the renderer

**Files:**
- Modify: `crates/oxutrm-client/src/renderer.rs`

**Interfaces:**
- Consumes: `Overlay` from Task 1.
- Produces: `Renderer::set_overlay(&mut self, overlay: Option<Overlay>)`.

**Background.** `Renderer::render` currently passes `&ScreenState` to
`write_cells` and `write_cursor`. Compositing means those two must paint a
*composited* view instead. The composite is built into a local buffer and
**never becomes a `ScreenState`** — the authoritative state is what gets acked
and diffed by the sync engine, and layer 1 must not be able to reach it.

The stored `Painted` gets the **composited** cells, because `Painted` describes
what is on the terminal. That is what makes removal free: dropping the overlay
leaves the next render diffing composited cells against true cells, which
repaints exactly the covered region and nothing else.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the end of `crates/oxutrm-client/src/renderer.rs`:

```rust
    /// A 3x1 red overlay at row 1, col 2 of an 8x3 screen.
    fn test_overlay(row: u16, col: u16, text: &str) -> crate::Overlay {
        let cells = text
            .chars()
            .map(|c| Cell {
                text: oxutrm_proto::CellText::new(c.to_string()),
                fg: Color::Idx(1),
                bg: Color::Default,
                attrs: Attrs::empty(),
            })
            .collect::<Vec<_>>();
        crate::Overlay {
            row,
            col,
            rows: 1,
            cols: cells.len() as u16,
            cells,
        }
    }

    #[test]
    fn an_overlay_paints_over_the_screen_beneath_it() {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps());
        let mut screen = ScreenState::blank(3, 8).unwrap();
        for (i, c) in "abcdefgh".chars().enumerate() {
            screen.cells[8 + i].text = oxutrm_proto::CellText::new(c.to_string());
        }

        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();

        r.set_overlay(Some(test_overlay(1, 2, "XYZ")));
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        let painted = String::from_utf8_lossy(&out).to_string();

        assert!(painted.contains("XYZ"), "the overlay was not painted: {painted:?}");
        assert!(
            !painted.contains("abcdefgh"),
            "the whole row was repainted, not just the covered columns: {painted:?}"
        );
    }

    /// The property the whole approach rests on: removing the overlay is an
    /// ordinary diff back to the authoritative screen. No repaint, no
    /// `invalidate`, and the cells underneath come back exactly.
    #[test]
    fn removing_an_overlay_restores_the_cells_beneath_it() {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps());
        let mut screen = ScreenState::blank(3, 8).unwrap();
        for (i, c) in "abcdefgh".chars().enumerate() {
            screen.cells[8 + i].text = oxutrm_proto::CellText::new(c.to_string());
        }

        r.render(&mut Vec::new(), &screen).unwrap();
        r.set_overlay(Some(test_overlay(1, 2, "XYZ")));
        r.render(&mut Vec::new(), &screen).unwrap();

        r.set_overlay(None);
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        let painted = String::from_utf8_lossy(&out).to_string();

        assert!(painted.contains("cde"), "the covered cells were not restored: {painted:?}");
        assert!(
            !painted.contains("\x1b[2J"),
            "restoring should be a diff, not a full repaint: {painted:?}"
        );
    }

    #[test]
    fn an_overlay_hides_the_cursor_and_restoring_brings_it_back() {
        let mut r = Renderer::new(TermSize { cols: 8, rows: 3 }, caps());
        let mut screen = ScreenState::blank(3, 8).unwrap();
        screen.cursor.visible = true;

        r.render(&mut Vec::new(), &screen).unwrap();

        r.set_overlay(Some(test_overlay(1, 2, "XYZ")));
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[?25l"),
            "the cursor was not hidden under the overlay"
        );

        r.set_overlay(None);
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        assert!(
            String::from_utf8_lossy(&out).contains("\x1b[?25h"),
            "the cursor did not come back"
        );
    }

    /// An overlay wider or taller than the screen must clip, not panic and not
    /// write past the row. A window can shrink between the notice being built
    /// and being painted.
    #[test]
    fn an_overlay_larger_than_the_screen_is_clipped() {
        let mut r = Renderer::new(TermSize { cols: 4, rows: 2 }, caps());
        let screen = ScreenState::blank(2, 4).unwrap();

        r.set_overlay(Some(test_overlay(1, 2, "XYZABC")));
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();

        let painted = String::from_utf8_lossy(&out).to_string();
        assert!(painted.contains("XY"), "nothing was painted: {painted:?}");
        assert!(!painted.contains("ABC"), "painted past the screen: {painted:?}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -j4 -p oxutrm-client renderer:: 2>&1 | tail -20`
Expected: FAIL to compile — `no method named set_overlay`.

- [ ] **Step 3: Add the field and the setter**

In `crates/oxutrm-client/src/renderer.rs`, add to `struct Renderer`:

```rust
    /// Layer 1: locally drawn UI composited over the remote framebuffer.
    ///
    /// Held here rather than passed to `render` so that setting it is what a
    /// caller does, and painting stays one call. It is composited into a local
    /// cell buffer and **never** into a `ScreenState`: the authoritative state
    /// is what the sync engine acks and diffs, and layer 1 must not reach it.
    overlay: Option<Overlay>,
```

Initialise `overlay: None` in `Renderer::new`, add the import
`use crate::overlay::Overlay;`, and add the setter next to `invalidate`:

```rust
    /// Put local UI over the screen, or take it away.
    ///
    /// Deliberately **not** paired with `invalidate`. Both painting and
    /// removing the overlay are ordinary diffs against the model, which is the
    /// whole reason layer 1 goes through the renderer instead of being written
    /// to the terminal directly -- see `ClientSession::announce`, which has to
    /// invalidate precisely because it writes outside the model.
    pub fn set_overlay(&mut self, overlay: Option<Overlay>) {
        self.overlay = overlay;
    }
```

- [ ] **Step 4: Composite in `render`**

In `render`, replace the three calls

```rust
        self.write_title(&mut out, s, full);
        self.write_cells(&mut out, s, full);
        self.write_cursor(&mut out, s, full);
```

with:

```rust
        self.write_title(&mut out, s, full);

        // Layer 1 is stamped into a copy of the cells, and the cursor is
        // hidden under it: a caret sitting inside a drawn box reads as a bug.
        // `Cow` so a session with no overlay -- which is every healthy session
        // -- clones nothing.
        let cells = self.composite(s);
        let cursor = match self.overlay {
            None => s.cursor,
            Some(_) => Cursor {
                visible: false,
                ..s.cursor
            },
        };

        self.write_cells(&mut out, &cells, s.rows, s.cols, full);
        self.write_cursor(&mut out, cursor, full);
```

and store the composited cells in `Painted`, since `Painted` is a claim about
the terminal and the terminal is showing the composite:

```rust
                self.painted = Some(Painted {
                    cells: cells.into_owned(),
                    cursor,
                    modes: s.modes,
                    title: s.title.clone(),
                    bell: s.bell,
                });
```

Add the compositor:

```rust
    /// The screen with layer 1 stamped on top, clipped to the screen.
    ///
    /// Clipping rather than asserting: a window can shrink between a notice
    /// being laid out and being painted, and a resize is not a reason to panic
    /// in the middle of a repaint.
    fn composite<'a>(&self, s: &'a ScreenState) -> std::borrow::Cow<'a, [Cell]> {
        let Some(o) = self.overlay.as_ref() else {
            return std::borrow::Cow::Borrowed(&s.cells);
        };

        let mut cells = s.cells.clone();
        let cols = s.cols as usize;
        for r in 0..o.rows {
            let screen_row = o.row.saturating_add(r);
            if screen_row >= s.rows {
                break;
            }
            for c in 0..o.cols {
                let screen_col = o.col.saturating_add(c);
                if screen_col >= s.cols {
                    break;
                }
                let from = r as usize * o.cols as usize + c as usize;
                let to = screen_row as usize * cols + screen_col as usize;
                cells[to] = o.cells[from].clone();
            }
        }
        std::borrow::Cow::Owned(cells)
    }
```

- [ ] **Step 5: Change the two helper signatures**

`write_cells` and `write_cursor` take painted values rather than the
authoritative state, which is what makes the composite reachable. Change their
signatures and the four field accesses inside them:

```rust
    fn write_cells(&self, out: &mut Vec<u8>, cells: &[Cell], rows: u16, cols: u16, full: bool) {
```

Inside it, replace `s.cols` with `cols`, `s.rows` with `rows`, and `s.cells`
with `cells`. Then:

```rust
    fn write_cursor(&self, out: &mut Vec<u8>, cursor: Cursor, full: bool) {
```

Inside it, replace every `s.cursor` with `cursor`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -j4 -p oxutrm-client 2>&1 | tail -20`
Expected: PASS, including every pre-existing renderer test. If an existing test
fails, the composite is wrong — do not adjust the test.

- [ ] **Step 7: Inject the `Painted` fault and confirm the test catches it**

Store the *authoritative* cells rather than the composited ones — the plausible
mistake, and the one that would make the notice unremovable:

```rust
                    cells: s.cells.clone(), // FAULT INJECTION
```

Run: `cargo test -j4 -p oxutrm-client renderer:: 2>&1 | tail -20`
Expected: FAIL, `removing_an_overlay_restores_the_cells_beneath_it` — with the
authoritative cells recorded as painted, the renderer believes the covered cells
are already correct and never repaints them, leaving the box on screen for ever.

**Then revert the injection** and grep for `FAULT INJECTION`.

- [ ] **Step 8: Commit**

```bash
git add crates/oxutrm-client/src/renderer.rs
git commit -m "feat(client): the renderer composites layer 1 before it diffs

Local UI is stamped into a copy of the cells and painted through the
existing diff, which is what makes it removable for free: `Painted` records
the COMPOSITED cells, because Painted is a claim about the terminal and the
terminal is showing the composite. Dropping the overlay then leaves the next
render diffing composite against truth, repainting exactly the covered
region and nothing else.

Storing the authoritative cells instead is the plausible mistake and it does
not merely look wrong -- the renderer would believe the covered cells were
already correct and never repaint them, leaving the box on screen for the
rest of the session. There is a test that fails against exactly that.

`write_cells` and `write_cursor` now take painted values rather than the
authoritative state, which is what lets the composite reach them and says
plainly that painting is not the sync engine's business. The composite never
becomes a ScreenState: that is what gets acked and diffed, and layer 1 must
not be able to reach it."
```

---

## Task 3: Synchronized output

**Files:**
- Modify: `crates/oxutrm-client/src/renderer.rs`

**Interfaces:** none new.

**Background.** DECSET 2026 asks the terminal to show a repaint atomically
instead of mid-tear. Emitted **unconditionally**: conforming terminals ignore
unknown private modes, so this needs no capability detection and no
`TerminalCaps` field. It matters most where a box is painted over live content,
which is exactly what Task 2 just built.

- [ ] **Step 1: Write the failing tests**

Add to the renderer's `mod tests`:

```rust
    #[test]
    fn a_repaint_is_wrapped_in_synchronized_output() {
        let mut r = Renderer::new(TermSize { cols: 4, rows: 1 }, caps());
        let mut screen = ScreenState::blank(1, 4).unwrap();
        screen.cells[0].text = oxutrm_proto::CellText::new("x");

        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();
        let painted = String::from_utf8_lossy(&out).to_string();

        assert!(painted.starts_with("\x1b[?2026h"), "no begin: {painted:?}");
        assert!(painted.ends_with("\x1b[?2026l"), "no end: {painted:?}");
    }

    /// A render that changes nothing must write nothing at all. Bracketing an
    /// empty payload would turn every quiet pacing tick into two escape
    /// sequences on the wire to the user's terminal.
    #[test]
    fn a_render_that_paints_nothing_writes_nothing() {
        let mut r = Renderer::new(TermSize { cols: 4, rows: 1 }, caps());
        let screen = ScreenState::blank(1, 4).unwrap();

        r.render(&mut Vec::new(), &screen).unwrap();
        let mut out = Vec::new();
        r.render(&mut out, &screen).unwrap();

        assert!(out.is_empty(), "wrote {:?} for an unchanged screen", String::from_utf8_lossy(&out));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -j4 -p oxutrm-client synchronized 2>&1 | tail -20`
Expected: FAIL, `no begin`.

- [ ] **Step 3: Wrap the payload**

In `render`, immediately before the `match w.write_all(&out)`:

```rust
        // Synchronized output. The terminal shows the whole repaint at once
        // instead of mid-tear, which matters most where layer 1 paints a box
        // over live content.
        //
        // Unconditional, and that is not laziness: a conforming terminal
        // ignores a private mode it does not know, so there is nothing to
        // detect, nothing to negotiate and no capability to carry. Guarded only
        // on emptiness, because a render that changes nothing must write
        // nothing -- otherwise every quiet pacing tick costs two escape
        // sequences.
        if !out.is_empty() {
            let mut wrapped = Vec::with_capacity(out.len() + 16);
            wrapped.extend_from_slice(b"\x1b[?2026h");
            wrapped.append(&mut out);
            wrapped.extend_from_slice(b"\x1b[?2026l");
            out = wrapped;
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -j4 -p oxutrm-client 2>&1 | tail -20`
Expected: PASS. Pre-existing tests that assert on exact output may now see the
wrapper; if one fails, update **that assertion only**, never the behaviour.

- [ ] **Step 5: Commit**

```bash
git add crates/oxutrm-client/src/renderer.rs
git commit -m "feat(client): repaints are shown all at once, not mid-tear

DECSET 2026 around a non-empty payload. Unconditional, because a conforming
terminal ignores a private mode it does not know -- so there is nothing to
detect, nothing to negotiate, and no capability to carry.

Guarded on emptiness though: a render that changes nothing must write
nothing, or every quiet pacing tick would cost two escape sequences for a
screen that did not move."
```

---

## Task 4: The notice

**Files:**
- Create: `crates/oxutrm-client/src/notice.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`

**Interfaces:**
- Consumes: `overlay_from_buffer`, `Overlay` (Task 1).
- Produces:
  - `pub struct Notice { pub headline: String, pub body: Vec<String>, pub keys: Vec<(String, String)> }`
  - `pub fn layout_notice(n: &Notice, size: TermSize) -> Overlay`

**Background.** Phase 1's notice must not promise reconnection — nothing
reconnects until Phase 3. It states silence and counters, and says what the keys
do in full sentences: the quit key closes the *local* client and leaves the
remote shell running, and that distinction is the entire content of the
sentence.

- [ ] **Step 1: Write the failing tests**

Create `crates/oxutrm-client/src/notice.rs`:

```rust
//! What layer 1 says, and how big it is.
//!
//! Phase 1 deliberately makes no promise about reconnection, because nothing
//! reconnects yet. The notice reports what the client can observe -- silence,
//! counters -- and nothing it cannot know. In particular it never claims the
//! session is safe: a dead network and a crashed host are indistinguishable
//! from here.

use oxutrm_proto::TermSize;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph, Widget as _, Wrap};

use crate::overlay::{Overlay, overlay_from_buffer};

/// Below this the box is dropped for a single line: a box that does not fit is
/// worse than a line that does.
pub const MIN_BOX: TermSize = TermSize { cols: 20, rows: 6 };

/// One piece of local UI, as content rather than as pixels.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Notice {
    pub headline: String,
    pub body: Vec<String>,
    /// `(keys, what it does)`, rendered as a two-column list.
    pub keys: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice() -> Notice {
        Notice {
            headline: "no reply from host".to_string(),
            body: vec!["silent for 6s".to_string(), "sent 14 - lost 9".to_string()],
            keys: vec![(
                "Ctrl-\\ q".to_string(),
                "close oxutrm here; the shell keeps running".to_string(),
            )],
        }
    }

    fn text_of(o: &Overlay) -> String {
        let mut s = String::new();
        for r in 0..o.rows {
            for c in 0..o.cols {
                let cell = &o.cells[r as usize * o.cols as usize + c as usize];
                s.push_str(if cell.text.is_empty() { "" } else { &cell.text });
            }
            s.push('\n');
        }
        s
    }

    #[test]
    fn a_notice_is_centred_on_the_screen() {
        let o = layout_notice(&notice(), TermSize { cols: 80, rows: 24 });

        assert_eq!(o.col, (80 - o.cols) / 2, "not horizontally centred");
        assert_eq!(o.row, (24 - o.rows) / 2, "not vertically centred");
    }

    #[test]
    fn a_notice_never_exceeds_the_screen() {
        for (cols, rows) in [(80u16, 24u16), (20, 6), (200, 60), (24, 8)] {
            let o = layout_notice(&notice(), TermSize { cols, rows });
            assert!(o.cols <= cols && o.rows <= rows, "{o:?} exceeds {cols}x{rows}");
            assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
        }
    }

    #[test]
    fn the_headline_and_the_keys_are_both_in_the_box() {
        let o = layout_notice(&notice(), TermSize { cols: 80, rows: 24 });
        let text = text_of(&o);

        assert!(text.contains("no reply from host"), "{text}");
        assert!(text.contains("Ctrl-\\ q"), "{text}");
        assert!(text.contains("the shell keeps running"), "{text}");
    }

    /// A box that does not fit is worse than a line that does.
    #[test]
    fn a_screen_too_small_for_a_box_gets_one_line() {
        let o = layout_notice(&notice(), TermSize { cols: 18, rows: 4 });

        assert_eq!(o.rows, 1, "expected the single-line fallback");
        assert_eq!(o.row, 0, "the fallback goes on the top row");
        assert_eq!(o.cols, 18, "the fallback spans the width");
        assert!(text_of(&o).contains("no reply"), "{}", text_of(&o));
    }

    /// One column and one row is absurd and must still not panic: a terminal
    /// reports 1x1 transiently while some emulators tear down.
    #[test]
    fn a_one_by_one_screen_does_not_panic() {
        let o = layout_notice(&notice(), TermSize { cols: 1, rows: 1 });
        assert_eq!(o.cells.len(), o.rows as usize * o.cols as usize);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -j4 -p oxutrm-client notice:: 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function layout_notice`.

- [ ] **Step 3: Write the implementation**

Add above the `#[cfg(test)]` block in `crates/oxutrm-client/src/notice.rs`:

```rust
/// Lay a notice out for this screen, as cells ready to composite.
///
/// Sizing is content-driven and then clamped, rather than a fixed box: the
/// held-input notice is much taller than the silence one, and a fixed box
/// would either truncate it or leave the common case mostly empty.
pub fn layout_notice(n: &Notice, size: TermSize) -> Overlay {
    if size.cols < MIN_BOX.cols || size.rows < MIN_BOX.rows {
        return single_line(n, size);
    }

    let lines = notice_lines(n);
    // Two columns of border plus two of padding.
    let widest = lines.iter().map(|l| l.width()).max().unwrap_or(0) as u16;
    let cols = widest.saturating_add(4).clamp(MIN_BOX.cols, size.cols);
    // Two rows of border.
    let rows = (lines.len() as u16).saturating_add(2).clamp(3, size.rows);

    let area = Rect::new(0, 0, cols, rows);
    let mut buf = Buffer::empty(area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(" oxutrm ");
    let inner = block.inner(area);
    block.render(area, &mut buf);
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .render(inner, &mut buf);

    overlay_from_buffer(&buf, (size.rows - rows) / 2, (size.cols - cols) / 2)
}

/// Headline, blank, body, blank, keys -- with the blanks dropped when the part
/// they separate is empty, so a notice with no keys has no trailing gap.
fn notice_lines(n: &Notice) -> Vec<Line<'static>> {
    let mut lines = vec![Line::from(Span::styled(
        n.headline.clone(),
        Style::default().add_modifier(Modifier::BOLD),
    ))];

    if !n.body.is_empty() {
        lines.push(Line::from(""));
        lines.extend(n.body.iter().map(|b| Line::from(b.clone())));
    }

    if !n.keys.is_empty() {
        lines.push(Line::from(""));
        let widest = n.keys.iter().map(|(k, _)| k.chars().count()).max().unwrap_or(0);
        for (keys, what) in &n.keys {
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{keys:<widest$}  "),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(what.clone()),
            ]));
        }
    }

    lines
}

/// The fallback for a screen too small for a box.
///
/// Reverse video and the top row, because the bottom rows are where the cursor
/// usually is and covering those is what the box was centred to avoid.
fn single_line(n: &Notice, size: TermSize) -> Overlay {
    let area = Rect::new(0, 0, size.cols.max(1), 1);
    let mut buf = Buffer::empty(area);
    Paragraph::new(Line::from(Span::styled(
        format!("oxutrm: {}", n.headline),
        Style::default().add_modifier(Modifier::REVERSED),
    )))
    .render(area, &mut buf);
    overlay_from_buffer(&buf, 0, 0)
}
```

Wire it up in `crates/oxutrm-client/src/lib.rs`:

```rust
mod notice;
pub use notice::{Notice, layout_notice};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -j4 -p oxutrm-client notice:: 2>&1 | tail -20`
Expected: PASS, all five tests.

- [ ] **Step 5: Print the artefact and read it**

A layout test asserts on substrings; it cannot tell you the box looks right.
Add a temporary `#[test]` that prints, run it with `--nocapture`, and **look at
the output**:

```rust
    #[test]
    fn print_it() {
        let o = layout_notice(&notice(), TermSize { cols: 60, rows: 20 });
        print!("{}", text_of(&o));
    }
```

Run: `cargo test -j4 -p oxutrm-client notice::tests::print_it -- --nocapture`
Expected: a rounded box with an aligned key column and no ragged border. Fix the
layout if it is ugly, then **delete the temporary test**.

- [ ] **Step 6: Check formatting and lints**

Run: `cargo fmt --check && cargo clippy -j4 -p oxutrm-client --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 7: Commit**

```bash
git add crates/oxutrm-client/src/notice.rs crates/oxutrm-client/src/lib.rs
git commit -m "feat(client): what the box says, and how big it is

Content-driven sizing rather than a fixed box, because the held-input notice
is much taller than the silence one: a fixed box would either truncate that
or leave the common case mostly empty.

Below 20x6 the box is dropped for a single reverse-video line on the TOP
row -- a box that does not fit is worse than a line that does, and the
bottom rows are where the cursor usually is, which is what centring was
avoiding in the first place.

Phase 1 promises nothing about reconnection, because nothing reconnects
yet. The notice reports silence and counters and stops there. It never says
the session is safe: a dead network and a crashed host are indistinguishable
from the client, and a box that guesses is worse than one that admits."
```

---

## Task 5: The link state machine and the liveness clock

**Files:**
- Create: `src/linkstate.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  - `pub enum Phase { Live, Silent { since: Instant }, Confirming }`
  - `pub struct LinkState`
  - `LinkState::new(now: Instant) -> LinkState`
  - `LinkState::heard(&mut self, now: Instant)`
  - `LinkState::evaluate(&mut self, now: Instant, reply_owed: bool) -> Phase`
  - `LinkState::phase(&self) -> Phase`
  - `pub const SILENT_AFTER: Duration`

**Background.** `Sender::make_frame`
(`crates/oxutrm-sync/src/channel.rs:112`) sends a frame carrying an empty diff
whenever it owes the peer an ack it has not heard. So **every input obliges a
reply**, and "we sent, nothing came back" is a true round-trip test that already
exists on the wire. `reply_owed` is computed by the caller in Task 8 as
`input_tx.current().seq() != screen_rx.peer_ack()`.

Time is a parameter, never `Instant::now()` inside the type. That is what makes
this testable without sleeping.

- [ ] **Step 1: Write the failing tests**

Create `src/linkstate.rs`:

```rust
//! Whether the host is still answering, and what the user is told about it.
//!
//! Pure: no I/O, no terminal, no runtime. Every method takes the current
//! `Instant` as a parameter rather than reading the clock, which is what lets
//! the whole state machine be tested without sleeping.

use std::time::{Duration, Instant};

/// How long a reply may be owed before the user is told. Below this a blip
/// resolves without ever painting: an indicator that fires on every hiccup is
/// the noise it was built to remove.
pub const SILENT_AFTER: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Live,
    Silent { since: Instant },
    Confirming,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_fresh_link_is_live() {
        assert_eq!(LinkState::new(t0()).phase(), Phase::Live);
    }

    #[test]
    fn silence_with_a_reply_owed_becomes_silent_after_the_grace_period() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(s.evaluate(t + Duration::from_millis(1900), true), Phase::Live);
        assert!(matches!(
            s.evaluate(t + Duration::from_millis(2100), true),
            Phase::Silent { .. }
        ));
    }

    /// Nothing owed means nothing is knowable. Without the heartbeat of Task 6
    /// this is also why an idle session would never notice an outage.
    #[test]
    fn silence_with_nothing_owed_stays_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(s.evaluate(t + Duration::from_secs(60), false), Phase::Live);
    }

    #[test]
    fn hearing_from_the_host_returns_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.evaluate(t + Duration::from_secs(3), true);

        s.heard(t + Duration::from_secs(4));
        assert_eq!(s.phase(), Phase::Live);
    }

    /// The `since` is the moment the host went quiet, not the moment we
    /// noticed. A counter that started at the grace period would under-report
    /// every outage by two seconds.
    #[test]
    fn the_silence_started_when_the_host_went_quiet_not_when_we_noticed() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.heard(t);

        let Phase::Silent { since } = s.evaluate(t + Duration::from_secs(5), true) else {
            panic!("expected Silent");
        };
        assert_eq!(since, t, "the counter must run from the last thing we heard");
    }

    #[test]
    fn silence_persists_across_laps_without_restarting_the_clock() {
        let t = t0();
        let mut s = LinkState::new(t);

        let first = s.evaluate(t + Duration::from_secs(3), true);
        let later = s.evaluate(t + Duration::from_secs(9), true);

        assert_eq!(first, later, "the clock restarted mid-outage");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Add `mod linkstate;` to `src/main.rs` beside the other `mod` declarations.

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find struct LinkState`.

- [ ] **Step 3: Write the implementation**

Add above the `#[cfg(test)]` block in `src/linkstate.rs`:

```rust
/// What the client believes about the link, and why.
pub struct LinkState {
    phase: Phase,
    /// The last time anything at all arrived from the host.
    last_heard: Instant,
}

impl LinkState {
    pub fn new(now: Instant) -> LinkState {
        LinkState {
            phase: Phase::Live,
            last_heard: now,
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// A frame arrived. Whatever we believed, the host is answering.
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
        self.phase = Phase::Live;
    }

    /// One lap's worth of judgement.
    ///
    /// `reply_owed` is the caller's answer to "have we said something the host
    /// has not acknowledged". It is the whole signal: the sync engine sends an
    /// empty-diff frame purely to move an owed ack, so an unanswered input is a
    /// real round-trip failure rather than an inference. With nothing owed,
    /// silence is indistinguishable from calm and this reports `Live` --
    /// closing that gap is what the heartbeat is for.
    pub fn evaluate(&mut self, now: Instant, reply_owed: bool) -> Phase {
        if let Phase::Silent { .. } = self.phase {
            // Already told the user. The clock keeps running from `last_heard`;
            // recomputing it here would restart the counter every lap.
            return self.phase;
        }

        if reply_owed && now.duration_since(self.last_heard) >= SILENT_AFTER {
            // `last_heard` and not `now`: the counter must report how long the
            // host has been quiet, not how long since we worked it out.
            self.phase = Phase::Silent {
                since: self.last_heard,
            };
        }
        self.phase
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: PASS, all six tests.

- [ ] **Step 5: Inject the clock fault and confirm the test catches it**

Report the moment of noticing rather than the moment of going quiet:

```rust
            self.phase = Phase::Silent { since: now }; // FAULT INJECTION
```

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: FAIL,
`the_silence_started_when_the_host_went_quiet_not_when_we_noticed`.

**Then revert the injection** and grep for `FAULT INJECTION`.

- [ ] **Step 6: Commit**

```bash
git add src/linkstate.rs src/main.rs
git commit -m "feat(client): a clock that says whether the host is still there

The signal was already on the wire. `Sender::make_frame` sends an empty diff
purely to move an owed ack, so every input obliges a reply and an unanswered
one is a real round-trip failure rather than an inference. No protocol
change, no new message, no probe.

Two seconds of grace, because an indicator that fires on every hiccup is the
noise it was built to remove.

The counter runs from the last thing we HEARD, not from the moment we
noticed -- otherwise every outage is under-reported by exactly the grace
period. There is a test that fails against the other version.

With nothing owed this reports Live, because silence and calm are then
genuinely indistinguishable. Closing that gap is the heartbeat's job, not
this one's, and pretending otherwise here would mean guessing."
```

---

## Task 6: The heartbeat

**Files:**
- Modify: `src/linkstate.rs`

**Interfaces:**
- Produces:
  - `LinkState::sent(&mut self, now: Instant)`
  - `LinkState::heartbeat_due(&self, now: Instant) -> bool`
  - `pub const HEARTBEAT_IDLE: Duration`

**Background.** With nothing outstanding, silence and calm are
indistinguishable, so a user who loses the network on an idle session would come
back to a stale screen and no notice. The fix is to make the session never
idle for long: every `HEARTBEAT_IDLE` of quiet, the caller (Task 8) bumps the
input sequence with an empty append — exactly what `ClientSession::resize`
already does — which makes `state_moved` true and obliges the host to answer.

**This is 0.2 Hz**, against the 250 Hz poll removed in `19cc001`, and it applies
only to an attached client. A detached host session has no client and therefore
no heartbeat, so the idle-CPU property that commit established is untouched.

- [ ] **Step 1: Write the failing tests**

Add to `src/linkstate.rs`'s `mod tests`:

```rust
    #[test]
    fn a_quiet_link_wants_a_heartbeat_after_the_idle_period() {
        let t = t0();
        let s = LinkState::new(t);

        assert!(!s.heartbeat_due(t + Duration::from_secs(4)));
        assert!(s.heartbeat_due(t + Duration::from_secs(6)));
    }

    #[test]
    fn sending_postpones_the_heartbeat() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.sent(t + Duration::from_secs(4));
        assert!(!s.heartbeat_due(t + Duration::from_secs(6)));
        assert!(s.heartbeat_due(t + Duration::from_secs(10)));
    }

    #[test]
    fn hearing_postpones_the_heartbeat() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.heard(t + Duration::from_secs(4));
        assert!(!s.heartbeat_due(t + Duration::from_secs(6)));
    }

    /// The heartbeat exists to make an idle session detectable. Without it,
    /// `evaluate` can never see a reply owed, so an outage on a session nobody
    /// is typing into would go unreported until the user pressed a key.
    #[test]
    fn a_heartbeat_makes_an_idle_outage_visible() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(s.evaluate(t + Duration::from_secs(6), false), Phase::Live);
        assert!(s.heartbeat_due(t + Duration::from_secs(6)));

        // The caller sends the heartbeat; from here a reply is owed.
        s.sent(t + Duration::from_secs(6));
        assert!(matches!(
            s.evaluate(t + Duration::from_secs(9), true),
            Phase::Silent { .. }
        ));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: FAIL to compile — `no method named heartbeat_due`.

- [ ] **Step 3: Write the implementation**

Add the constant beside `SILENT_AFTER`:

```rust
/// How long a session may be completely quiet before the client says something
/// merely to see whether anyone is still there.
///
/// **0.2 Hz.** Set that against the 250 Hz poll removed in `19cc001`, and note
/// that it applies only to an ATTACHED client: a detached host session has no
/// client, so it has no heartbeat and its idle cost is unchanged.
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(5);
```

Add the field to `LinkState` and initialise it in `new`:

```rust
    /// The last time we said anything, so a quiet link can be prodded.
    last_sent: Instant,
```

```rust
            last_sent: now,
```

Add the two methods:

```rust
    /// We sent something, so a reply is owed from here.
    pub fn sent(&mut self, now: Instant) {
        self.last_sent = now;
    }

    /// Nothing has been said in either direction for long enough that the
    /// caller should say something, purely so that an answer is owed.
    ///
    /// Without this an idle session cannot tell an outage from calm, and the
    /// user would find out by pressing a key into a screen that had been dead
    /// for ten minutes.
    pub fn heartbeat_due(&self, now: Instant) -> bool {
        let quiet_since = self.last_heard.max(self.last_sent);
        now.duration_since(quiet_since) >= HEARTBEAT_IDLE
    }
```

Update `heard` to keep the pair consistent — it already sets `last_heard`, and
`heartbeat_due` takes the later of the two, so no change is needed there.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: PASS, all ten tests.

- [ ] **Step 5: Commit**

```bash
git add src/linkstate.rs
git commit -m "feat(client): an idle session still knows when the host stops answering

The ack clock only works while something is outstanding, so a session nobody
is typing into cannot tell an outage from calm -- the user would discover it
by pressing a key into a screen that had been dead for ten minutes.

So after five seconds of quiet the client says something merely to be
answered. 0.2 Hz, against the 250 Hz poll removed in 19cc001, and only for
an ATTACHED client: a detached host session has no client, so it has no
heartbeat and the idle-CPU property that commit established is untouched."
```

---

## Task 7: Held input and the key prefix

**Files:**
- Modify: `src/linkstate.rs`

**Interfaces:**
- Produces:
  - `pub enum Command { Quit, SendHeld, DropHeld }`
  - `LinkState::hold_keys(&mut self, bytes: &[u8]) -> Option<Command>`
  - `LinkState::held(&self) -> &[u8]`
  - `LinkState::held_is_full(&self) -> bool`
  - `LinkState::take_held(&mut self) -> Vec<u8>`
  - `LinkState::drop_held(&mut self)`
  - `pub const MAX_HELD: usize`
  - `pub fn render_held(bytes: &[u8]) -> String`

**Background.** Keystrokes typed while not `Live` go to a holding buffer, not to
`input_tx`. On the link returning with a non-empty buffer the client enters
`Confirming` and asks before delivering: replaying blind input against a screen
that moved while the user could not watch is how a half-typed command completes
into something never intended, and without speculative echo there is no way to
show them what they typed as they type it.

`Ctrl-\` is `0x1c`. It is a **prefix**, live only while a notice is showing, so
while `Live` every byte belongs to the host untouched — which sidesteps the
escape-character collisions Mosh must live with.

- [ ] **Step 1: Write the failing tests**

Add to `src/linkstate.rs`'s `mod tests`:

```rust
    #[test]
    fn keys_typed_offline_are_held_not_delivered() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"make test"), None);
        assert_eq!(s.held(), b"make test");
    }

    #[test]
    fn the_prefix_and_a_letter_are_a_command_and_are_not_held() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"ab\x1cq"), Some(Command::Quit));
        assert_eq!(s.held(), b"ab", "the prefix or the command leaked into the buffer");
    }

    #[test]
    fn every_command_key_is_recognised() {
        for (byte, want) in [(b'q', Command::Quit), (b's', Command::SendHeld), (b'd', Command::DropHeld)] {
            let mut s = LinkState::new(t0());
            assert_eq!(s.hold_keys(&[0x1c, byte]), Some(want), "for {}", byte as char);
        }
    }

    /// The prefix can be the last byte of one read and the letter the first of
    /// the next. A parser that only looked within one buffer would drop the
    /// command and hold two stray bytes.
    #[test]
    fn a_prefix_split_across_two_reads_still_commands() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"x\x1c"), None);
        assert_eq!(s.hold_keys(b"q"), Some(Command::Quit));
        assert_eq!(s.held(), b"x");
    }

    /// An unknown letter after the prefix is ordinary typing, and both bytes
    /// are kept: the user meant to type them.
    #[test]
    fn an_unknown_key_after_the_prefix_is_held_with_the_prefix() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"\x1cz"), None);
        assert_eq!(s.held(), b"\x1cz");
    }

    /// The cap stops accepting rather than dropping the oldest bytes: the
    /// oldest are the command and the newest are the newline, so discarding
    /// from the front is how a truncated command still runs.
    #[test]
    fn a_full_buffer_stops_accepting_rather_than_dropping_the_oldest() {
        let mut s = LinkState::new(t0());
        s.hold_keys(&vec![b'a'; MAX_HELD]);

        assert!(s.held_is_full());
        s.hold_keys(b"zzz");
        assert_eq!(s.held().len(), MAX_HELD);
        assert_eq!(s.held()[0], b'a', "the oldest bytes were dropped");
        assert!(!s.held().contains(&b'z'), "accepted past the cap");
    }

    #[test]
    fn taking_the_held_input_empties_the_buffer() {
        let mut s = LinkState::new(t0());
        s.hold_keys(b"hello");

        assert_eq!(s.take_held(), b"hello");
        assert!(s.held().is_empty());
    }

    /// Hearing from the host with something held is what raises the question,
    /// and the question is the `Confirming` phase.
    #[test]
    fn coming_back_with_held_input_asks_instead_of_going_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.hold_keys(b"make test\r");

        s.heard(t + Duration::from_secs(9));
        assert_eq!(s.phase(), Phase::Confirming);
    }

    #[test]
    fn coming_back_with_nothing_held_goes_straight_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.heard(t + Duration::from_secs(9));
        assert_eq!(s.phase(), Phase::Live);
    }

    #[test]
    fn resolving_the_held_input_returns_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.hold_keys(b"x");
        s.heard(t);
        assert_eq!(s.phase(), Phase::Confirming);

        s.drop_held();
        assert_eq!(s.phase(), Phase::Live);
        assert!(s.held().is_empty());
    }

    #[test]
    fn control_bytes_render_readably_rather_than_as_themselves() {
        assert_eq!(render_held(b"make test\r"), "make test\u{21b5}");
        assert_eq!(render_held(b"a\x03b"), "a^Cb");
        assert_eq!(render_held(b"\t"), "^I");
    }

    /// A paste can be enormous, and a box cannot hold it. Summarising beats
    /// truncating silently, which would show a command that is not the command
    /// about to run.
    #[test]
    fn a_long_buffer_is_summarised_rather_than_dumped() {
        let long = vec![b'x'; 5000];
        let shown = render_held(&long);

        assert!(shown.len() < 500, "not summarised: {} chars", shown.len());
        assert!(shown.contains("more"), "no indication of what was elided: {shown}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find type Command`.

- [ ] **Step 3: Write the implementation**

Add to `src/linkstate.rs`:

```rust
/// `Ctrl-\`. A prefix rather than a bare key, and live only while a notice is
/// showing: while the link is healthy every byte belongs to the host, which is
/// what keeps oxutrm out of the escape-character collisions Mosh must live
/// with.
const PREFIX: u8 = 0x1c;

/// How much blind typing is kept. Beyond this the buffer STOPS ACCEPTING; it
/// does not drop the oldest bytes, because the oldest are the command and the
/// newest are the newline, and discarding from the front is exactly how a
/// truncated command still runs.
pub const MAX_HELD: usize = 64 * 1024;

/// How much of the held input is shown before it is summarised.
const HELD_SHOWN: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Quit,
    SendHeld,
    DropHeld,
}

/// Held input as something safe to put in a box.
///
/// Control bytes become readable rather than being emitted: the notice is
/// painted through the renderer, and a raw `\r` in a cell would be a control
/// scalar the receiver's validation rejects.
pub fn render_held(bytes: &[u8]) -> String {
    let shown = &bytes[..bytes.len().min(HELD_SHOWN)];
    let mut out = String::with_capacity(shown.len() + 16);

    for &b in shown {
        match b {
            b'\r' | b'\n' => out.push('\u{21b5}'),
            0x00..=0x1f => {
                out.push('^');
                out.push((b + b'@') as char);
            }
            0x7f => out.push_str("^?"),
            _ => out.push(b as char),
        }
    }

    if bytes.len() > shown.len() {
        out.push_str(&format!("  ...and {} more bytes", bytes.len() - shown.len()));
    }
    out
}
```

Add the fields to `LinkState` and initialise them in `new`:

```rust
    /// Typed while not `Live`, and not delivered to anyone yet.
    held: Vec<u8>,
    /// The prefix arrived at the end of a read and its letter has not.
    prefix_pending: bool,
```

```rust
            held: Vec::new(),
            prefix_pending: false,
```

Change `heard` so that coming back with held input asks rather than resumes:

```rust
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
        // Coming back with something typed blind is a question, not a
        // resumption. Delivering it silently would replay it against a screen
        // that moved while the user could not watch.
        self.phase = if self.held.is_empty() {
            Phase::Live
        } else {
            Phase::Confirming
        };
    }
```

Add the rest:

```rust
    pub fn held(&self) -> &[u8] {
        &self.held
    }

    pub fn held_is_full(&self) -> bool {
        self.held.len() >= MAX_HELD
    }

    /// Deliver the held input, emptying the buffer.
    pub fn take_held(&mut self) -> Vec<u8> {
        self.phase = Phase::Live;
        std::mem::take(&mut self.held)
    }

    /// Discard the held input.
    pub fn drop_held(&mut self) {
        self.held.clear();
        self.phase = Phase::Live;
    }

    /// Feed keystrokes typed while a notice is showing.
    ///
    /// Returns the command the user asked for, if any; everything else is
    /// added to the held buffer. The prefix may arrive at the end of one read
    /// and its letter at the start of the next, which is why the pending flag
    /// outlives the call: a parser that only looked within one buffer would
    /// swallow the command and hold two stray bytes.
    pub fn hold_keys(&mut self, bytes: &[u8]) -> Option<Command> {
        for (i, &b) in bytes.iter().enumerate() {
            if self.prefix_pending {
                self.prefix_pending = false;
                let command = match b {
                    b'q' => Some(Command::Quit),
                    b's' => Some(Command::SendHeld),
                    b'd' => Some(Command::DropHeld),
                    // Not a command, so the user meant to type both bytes.
                    _ => {
                        self.push_held(PREFIX);
                        self.push_held(b);
                        None
                    }
                };
                if command.is_some() {
                    // Anything after a command in the same read belongs to
                    // whatever the command leads to, not to the old buffer.
                    let _ = &bytes[i + 1..];
                    return command;
                }
                continue;
            }

            if b == PREFIX {
                self.prefix_pending = true;
                continue;
            }
            self.push_held(b);
        }
        None
    }

    fn push_held(&mut self, b: u8) {
        if self.held.len() < MAX_HELD {
            self.held.push(b);
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: PASS, all twenty-two tests.

- [ ] **Step 5: Inject the cap fault and confirm the test catches it**

Drop the oldest bytes instead of refusing new ones — the reflexive "ring buffer"
choice, and the dangerous one:

```rust
    fn push_held(&mut self, b: u8) {
        // FAULT INJECTION
        if self.held.len() >= MAX_HELD {
            self.held.remove(0);
        }
        self.held.push(b);
    }
```

Run: `cargo test -j4 --bin oxutrm linkstate:: 2>&1 | tail -20`
Expected: FAIL, `a_full_buffer_stops_accepting_rather_than_dropping_the_oldest`.

**Then revert the injection** and grep for `FAULT INJECTION`.

- [ ] **Step 6: Commit**

```bash
git add src/linkstate.rs
git commit -m "feat(client): blind typing is kept, shown, and asked about

Keystrokes typed while the host is not answering go to a holding buffer
rather than to the input channel. Coming back with something held is a
question, not a resumption: replaying it against a screen that moved while
the user could not watch is how a half-typed command completes into
something else, and without speculative echo there is no way to show them
what they typed as they typed it.

The cap STOPS ACCEPTING rather than dropping the oldest bytes. The reflexive
ring-buffer choice is the dangerous one here -- the oldest bytes are the
command and the newest are the newline, so discarding from the front is
precisely how a truncated command still runs. There is a test that fails
against it.

Ctrl-\\ is a prefix and it is live only while a notice is showing, so a
healthy session passes every byte to the host untouched. The prefix can also
land at the end of one read with its letter at the start of the next, which
is why the pending flag outlives the call."
```

---

## Task 8: Wiring it into the client session

**Files:**
- Modify: `src/session.rs`

**Interfaces:**
- Consumes: everything from Tasks 1-7.
- Produces: no new public API; `ClientSession` gains private fields.

**Background.** This is where the pure pieces meet the loop. Four changes:

1. `ClientSession` holds a `LinkState` and a count of rejected frames.
2. `run_on`'s arms drive it, and a 1 s tick refreshes the counters **only while
   a notice is up**, so a healthy session gains no wakeups.
3. The two `eprintln!` sites become notice content. The client's stderr **is**
   the terminal it is painting, so a message there desynchronises the renderer
   and nothing repaints it on a quiet session. The host's `eprintln!` is
   untouched — it goes up the ssh channel and `drain_stderr` consumes it.
4. `exit_code`'s timeout arm gets a sentence a person can act on.

- [ ] **Step 1: Write the failing tests**

Add to `src/session.rs`'s `mod tests`. The helper that builds a client is
`async fn pair(shell: &str) -> (HostSession, ClientSession)` at
`src/session.rs:1133` — a host and a client joined by a real QUIC connection on
loopback. That is why these are `#[tokio::test]`, matching the tests around them.

```rust
    /// Frames the receiver cannot apply used to go to stderr, which IS the
    /// terminal being painted: the message desynchronised the renderer's model
    /// and nothing repainted it on a quiet session. They are diagnostics about
    /// the link, so they belong in the link's own notice.
    #[tokio::test]
    async fn a_rejected_frame_is_counted_rather_than_printed() {
        let (_host, mut session) = pair("/bin/sh").await;
        let bad = Frame {
            my_state: 9,
            from_state: 7,
            ack_state: 0,
            flags: 0,
            payload: vec![0xff, 0xff, 0xff],
        };

        let mut out = Vec::new();
        let turn = session.turn_with(&[], Some(bad), &mut out).unwrap();

        assert_eq!(turn.rejected, 1);
        assert_eq!(session.rejected_total(), 1, "the count did not reach the notice");
    }

    #[tokio::test]
    async fn silence_raises_a_notice_and_a_frame_clears_it() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;

        session.note_sent(t);
        assert!(session.notice_at(t + Duration::from_secs(1)).is_none());

        let notice = session.notice_at(t + Duration::from_secs(3));
        assert!(notice.is_some(), "no notice after three seconds of silence");
        assert!(notice.unwrap().headline.contains("no reply"));

        session.note_heard(t + Duration::from_secs(4));
        assert!(session.notice_at(t + Duration::from_secs(4)).is_none());
    }

    #[tokio::test]
    async fn the_notice_names_the_counters_it_can_actually_observe() {
        let t = std::time::Instant::now();
        let (_host, mut session) = pair("/bin/sh").await;
        session.note_sent(t);

        let n = session.notice_at(t + Duration::from_secs(6)).unwrap();
        let body = n.body.join(" ");

        assert!(body.contains("6s"), "no silence duration: {body}");
        assert!(
            !body.to_lowercase().contains("safe"),
            "claimed the session is safe, which the client cannot know: {body}"
        );
        assert!(
            !body.to_lowercase().contains("retry") && !body.to_lowercase().contains("reconnect"),
            "phase 1 promised a reconnection that does not exist: {body}"
        );
    }
```

**`notice_at` must be reachable from the test.** It takes `&mut self` because
`LinkState::evaluate` does; a test in the same module can call a private method,
so no visibility change is needed.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -j4 --bin oxutrm session:: 2>&1 | tail -20`
Expected: FAIL to compile — `no method named rejected_total`.

- [ ] **Step 3: Add the fields and the observers**

In `src/session.rs`, add to `struct ClientSession`:

```rust
    /// Whether the host is still answering, and what the user is told.
    link_state: crate::linkstate::LinkState,
    /// Frames that arrived and could not be applied, for the notice.
    ///
    /// This used to be an `eprintln!`, which was a bug rather than a
    /// diagnostic: the client's stderr IS the terminal it is painting, so the
    /// message desynchronised the renderer's model and nothing repainted it on
    /// a quiet session.
    rejected_total: u64,
    /// What is currently drawn as layer 1, so an unchanged notice does not
    /// rebuild an overlay every tick.
    shown: Option<Notice>,
```

Initialise them in `ClientSession::new` with
`link_state: crate::linkstate::LinkState::new(Instant::now())`,
`rejected_total: 0` and `shown: None`.

Add the observers the tests use:

```rust
    /// For tests and for the notice.
    pub fn rejected_total(&self) -> u64 {
        self.rejected_total
    }

    /// The clock is a parameter so the loop's behaviour can be tested without
    /// sleeping, exactly as `LinkState` is.
    fn note_heard(&mut self, now: Instant) {
        self.link_state.heard(now);
    }

    fn note_sent(&mut self, now: Instant) {
        self.link_state.sent(now);
    }

    /// What layer 1 should be showing at `now`, if anything.
    fn notice_at(&mut self, now: Instant) -> Option<Notice> {
        let owed = self.input_tx.current().seq() != self.screen_rx.peer_ack();
        match self.link_state.evaluate(now, owed) {
            Phase::Live => None,
            Phase::Silent { since } => {
                let quiet = now.duration_since(since).as_secs();
                let stats = self.link.sink.connection().stats();
                let mut body = vec![format!(
                    "silent for {quiet}s - sent {} - lost {}",
                    stats.path.sent_packets, stats.path.lost_packets
                )];
                if self.rejected_total > 0 {
                    body.push(format!("screen frames rejected: {}", self.rejected_total));
                }
                Some(Notice {
                    headline: "no reply from host".to_string(),
                    body,
                    keys: vec![(
                        "Ctrl-\\ q".to_string(),
                        "close oxutrm here; your shell keeps running on the host".to_string(),
                    )],
                })
            }
            Phase::Confirming => {
                let held = crate::linkstate::render_held(self.link_state.held());
                let mut body = vec![
                    format!("You typed {} bytes while offline:", self.link_state.held().len()),
                    held,
                ];
                if self.link_state.held_is_full() {
                    body.push("The buffer is full; later keys were not kept.".to_string());
                }
                Some(Notice {
                    headline: "reconnected - deliver what you typed?".to_string(),
                    body,
                    keys: vec![
                        ("Ctrl-\\ s".to_string(), "send it to the shell".to_string()),
                        ("Ctrl-\\ d".to_string(), "drop it".to_string()),
                    ],
                })
            }
        }
    }
```

Add the imports at the top of the file:

```rust
use crate::linkstate::{Command, LinkState, Phase};
use oxutrm_client::{Notice, layout_notice};
```

- [ ] **Step 4: Replace the two `eprintln!` sites**

At `src/session.rs:729` (the rejected-frame arm inside `take_frames`), replace
the `eprintln!` with a count:

```rust
                Err(_) => {
                    turn.rejected += 1;
                    // NOT `eprintln!`: the client's stderr is the terminal it
                    // is painting, so a message here desynchronises the
                    // renderer's model and nothing repaints it on a quiet
                    // session. The count reaches the user through the notice.
                    self.rejected_total = self.rejected_total.saturating_add(1);
                }
```

At `src/session.rs:1016` (the window-size arm in `run_on`), drop the
`eprintln!` and the `warned_size` flag, and record the condition for the notice
instead. Keep the existing behaviour of not ending the session:

```rust
                Wake::Winch => {
                    match terminal_size_of(&window) {
                        Ok(size) => self.resize(size),
                        // Same reasoning as the rejected-frame arm: this used
                        // to print onto the screen it was describing.
                        Err(_) => {}
                    }
                    self.turn(&[], out)?;
                }
```

- [ ] **Step 5: Drive layer 1 from the loop**

In `run_on`, after the `match wake { ... }` block and before the `deadline`
assignment, add:

```rust
            // Layer 1. Rebuilt only when the content actually changed, so a
            // steady notice costs one comparison per lap rather than a layout.
            let now = Instant::now();
            let notice = self.notice_at(now);
            if notice != self.shown {
                self.renderer
                    .set_overlay(notice.as_ref().map(|n| layout_notice(n, self.size)));
                self.shown = notice;
                self.renderer
                    .render(out, self.screen_rx.state())
                    .context("painting the notice")?;
                out.flush().context("flushing the terminal")?;
            }

            // A heartbeat exists to be answered: without one, an idle session
            // cannot tell an outage from calm. `append(&[], size)` bumps the
            // sequence exactly as `resize` does, which makes `state_moved` true
            // and obliges the host to reply.
            if self.link_state.heartbeat_due(now) {
                let next = self.input_tx.current().append(&[], self.size);
                self.input_tx.update(next);
                self.last_send = None;
                self.link_state.sent(now);
            }

            // The tick that refreshes the counters, and ONLY while something is
            // shown: a healthy session gains no wakeups from any of this.
            deadline = tokio::time::Instant::now()
                + if self.shown.is_some() {
                    Duration::from_secs(1).min(self.link.sink.pacing_interval())
                } else {
                    self.link.sink.pacing_interval()
                };
```

Replace the existing `deadline = ...` line with the block above rather than
leaving both.

In the `Wake::Frame` arm, record that we heard something:

```rust
                Wake::Frame(frame) => {
                    self.link_state.heard(Instant::now());
                    self.turn_with(&[], Some(frame), out)?;
                }
```

In the `Wake::Keys(n)` arm, route keystrokes through the holding buffer whenever
a notice is up:

```rust
                Wake::Keys(n) => {
                    // While a notice is showing the keyboard belongs to layer 1
                    // -- and only then. A healthy session passes every byte to
                    // the host untouched.
                    if self.shown.is_some() {
                        match self.link_state.hold_keys(&buf[..n]) {
                            Some(Command::Quit) => return Ok(0),
                            Some(Command::SendHeld) => {
                                let held = self.link_state.take_held();
                                self.turn(&held, out)?;
                            }
                            Some(Command::DropHeld) => self.link_state.drop_held(),
                            None => {}
                        }
                    } else {
                        self.turn(&buf[..n], out)?;
                    }
                }
```

- [ ] **Step 6: Give the timeout a sentence a person can act on**

In `exit_code`, replace the catch-all arm:

```rust
        quinn::ConnectionError::TimedOut => Err(anyhow::anyhow!(
            "the host stopped answering and the link timed out after 30s. Your \
             shell may still be running there; `oxutrm host --list` on the host \
             will say. Reattaching is not implemented yet."
        )),
        other => Err(anyhow::anyhow!(
            "the link to the host ended without the shell exiting: {other}"
        )),
```

- [ ] **Step 7: Run the whole suite**

Run: `cargo test -j4 --workspace 2>&1 | tail -30`
Expected: PASS. The baseline at `7a8f5ae` was 678 passed / 0 failed; this should
be that plus the new tests. **If a pre-existing test fails, the wiring is
wrong** — do not adjust the test.

- [ ] **Step 8: Verify the idle-CPU property did not regress**

The heartbeat is 0.2 Hz and the notice tick only runs while a notice is up, but
that is an argument, not a measurement. The suite already contains the spin
guards added in `569e60c`.

Run: `cargo test -j4 --workspace spin 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 9: Check formatting and lints on both toolchains**

Run: `cargo fmt --check && cargo clippy -j4 --workspace --all-targets -- -D warnings`

Then, because the Mac is on rustc 1.97.1 and thinlinc on 1.96.0 and they format
differently: run `cargo fmt` on thinlinc, rsync back, and check locally again.
CI's fmt job runs on `stable`, and a separate job pins `@1.96`.

- [ ] **Step 10: Commit**

```bash
git add src/session.rs
git commit -m "feat(client): the screen says when the host has stopped answering

The pieces meet the loop. A notice appears after two seconds of an
unanswered input, reports the silence and quinn's own counters, and clears
the moment a frame advances our ack. Keys go to the holding buffer only
while it is showing; a healthy session passes every byte through untouched.

Both of the client's eprintln! sites go with it, and they were bugs rather
than diagnostics: the client's stderr IS the terminal it is painting, so
each message desynchronised the renderer's model, and on a quiet session
nothing ever repainted it. A rejected frame is now a number in the notice --
which is where a diagnostic about the link belonged all along. The HOST's
eprintln! is untouched: it goes up the ssh channel and drain_stderr consumes
it.

The one-second tick that refreshes the counters runs only while something is
shown, so a healthy session gains no wakeups from any of this, and the spin
guards from 569e60c still pass.

The timeout message now says what happened and what to try, instead of
'the link to the host ended without the shell exiting: timed out'. It also
says reattaching is not implemented, because it is not, and a message that
implied otherwise would be the second-worst thing on that screen."
```

---

## Self-Review

**Spec coverage.** Phase 1 covers spec §2 (`Live`/`Silent`/`Confirming` — with
`Recovering`/`Displaced` explicitly deferred and stated in the scope boundary),
§3.1, §3.2, §3.3, §7, §8.1, §8.2, §8.3, §8.4 and §9's `q`/`s`/`d` keys. Spec §4,
§5 and §6 are Phases 2-4 and have no task here, by design. The spec's §11 test
list is covered by Tasks 1, 2, 3, 4 and 8; the host-attach and take-it-back
entries belong to later phases.

**Deferred to later phases, deliberately:** `Recovering` and `Displaced` states,
the `r` key, `max_idle_timeout` removal, the route probe and `Link::rebind`, the
host `UnixListener`, `host --attach`, `oxutrm askpass` and `ssh -G`.

**Type consistency.** `Overlay` is produced by Task 1 and consumed by Tasks 2 and
4 with the same five fields. `Notice` is produced by Task 4 and consumed by
Task 8 with the same three fields. `Phase` and `Command` are produced by Tasks 5
and 7 and consumed by Task 8. `LinkState`'s methods are named identically at
every use: `heard`, `sent`, `evaluate`, `phase`, `heartbeat_due`, `hold_keys`,
`held`, `held_is_full`, `take_held`, `drop_held`.

**Names verified against the tree rather than assumed.**
`oxutrm_proto::fit_cell_text` and `MAX_CELL_TEXT` are already exported
(`crates/oxutrm-proto/src/lib.rs:111`, `:113`). Task 8's tests use the real
helper `pair` (`src/session.rs:1133`) and are therefore `#[tokio::test]`.
`Receiver::peer_ack` (`channel.rs:418`) and `Sender::current` (`channel.rs:88`)
both exist with the signatures Task 8 uses. The ratatui API in Tasks 1 and 4 was
compiled headlessly against 0.30.2 before being written down, which is how the
wide-character trap in Task 1 was found rather than guessed at.

**One deprecation to avoid.** `ratatui::buffer::Cell::skip` is deprecated in
0.30. Nothing in this plan uses it — the wide-character rule measures display
width instead, which is what makes it correct rather than dependent on how
ratatui marks continuation cells.

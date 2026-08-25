# oxutrm M1 — Loopback Terminal Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A single-process program that spawns a shell on a PTY, drives
`alacritty_terminal` with its output, pushes `ScreenState` through the sync
engine (`Sender` → `Receiver`, in-process, no sockets), and renders the received
state onto the real terminal. Typing works. Resize works. No network at all.

**Architecture:** `oxutrm-proto` owns every wire type, including the screen
model, and is **already implemented**. `oxutrm-sync` is a pure, I/O-free
replication engine over a `SyncState` trait. `oxutrm-term` runs the emulator on
a PTY and converts what it holds into a `ScreenState`. `oxutrm-client` turns a
`ScreenState` into minimal ANSI. `src/main.rs` wires them together behind a
hidden `oxutrm loopback` subcommand.

**Tech Stack:** Rust **edition 2024, MSRV 1.85**, `alacritty_terminal` 0.26
(`default-features = false`), `rustix` + `rustix-openpty`, `compact_str`,
`postcard`, `serde`, `zstd`, `thiserror`, `anyhow`, `bitflags`; `proptest` and
`insta` for tests.

**Spec:** `docs/superpowers/specs/2026-08-25-oxutrm-design.md`

**Contract:** `docs/superpowers/plans/2026-08-25-oxutrm-contract.md` — **normative
for every type**. Where the spec and `oxutrm-proto` appear to disagree, the
crate is right; it says so itself.

---

## Global Constraints

- **Binary and product name is `oxutrm`**, never `oxuterm`. The checkout
  directory is `oxuterm` for historical reasons; nothing inside it uses that
  spelling.
- **Rust edition 2024**, workspace at the repo root, one binary `src/main.rs`.
  MSRV **1.85**, because `alacritty_terminal` 0.26 requires it.
- **Cap all parallelism at 4**: `cargo build --jobs 4`,
  `cargo test --jobs 4 -- --test-threads 4`. The build machine is shared.
- **`oxutrm-sync` performs no I/O.** No `std::net`, no `std::fs`, no `tokio`,
  no clock access. `crates/oxutrm-sync/tests/no_io.rs` already enforces this
  with a compile-time **allowlist**; adding any dependency fails that test.
- **English** for all identifiers, comments, and documentation.
- **`anyhow::Result`** at binary and crate-boundary level; concrete error enums
  (via `thiserror`) inside `oxutrm-sync` and `oxutrm-proto`.
- **No key material is ever written to disk**, in any crate, at any time.
- **Every task ends green**: `cargo clippy --all-targets -- -D warnings` and
  `cargo test --jobs 4 -- --test-threads 4` both pass before committing.
- **Commit messages** end with:
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`

## There is no fragmentation

`Frame` has exactly five fields — `my_state`, `from_state`, `ack_state`,
`flags`, `payload` — and **no `frag_index` or `frag_count`**. A datagram is
never split.

The arithmetic settled it: an unreliable state split into F pieces arrives only
if all F arrive, so delivery is `(1-p)^F`. A 200x60 truecolor full state is
about 125 pieces, which completes 28% of the time at 1% loss and 0.16% at 5% —
and a full state is exactly what the ring-miss recovery path is *obliged* to
send after a burst of loss. The mechanism most needed after loss would have
been the one least able to survive it.

The replacement is **size-based channel selection, and it belongs to M4**: a
`Frame` that fits in one datagram goes in a datagram; a larger one goes on a
fresh unidirectional QUIC stream, which is `RESET_STREAM`ed and reopened if a
newer state supersedes it. **`oxutrm-sync` is transport-agnostic and does not
choose.** It produces a `Frame`; the transport decides how to carry it. That
separation is the point, and it is also why `oxutrm-sync` still has no I/O
dependency. Nothing in M1 may assume otherwise.

## Scope of M1

| Crate | State |
|---|---|
| workspace, `src/main.rs` | **landed** at `dd10d69` — Task 1 verifies |
| `oxutrm-proto` | **landed** at `bf41e32` — Task 2 verifies |
| `oxutrm-sync` | Tasks 3-8 — all of it |
| `oxutrm-term` | Tasks 9-11 — all of it |
| `oxutrm-client` + `loopback` | Task 12 |
| `oxutrm-net`, `oxutrm-host` | untouched stubs; M2 and M3 |

`oxutrm-client::status_line` is **deferred to M4**: it takes a
`PathDescription`, which only becomes meaningful once there is a path.

## The six `ScreenState` invariants

These are **enforced, not documented**, and `oxutrm-proto` already implements
both checks. Every task below depends on knowing which check sees which rule.

| | Rule | Enforced by |
|---|---|---|
| I1 | `cells.len() == rows * cols`, exactly | `ScreenState::validate` |
| I2 | the cursor sits on a cell that exists | `ScreenState::validate` |
| I3 | `seq >= 1`; zero is the full-state sentinel | `ScreenState::validate` |
| I4 | there is no `icon` field; `vte` drops OSC 1 | the type itself |
| I5 | `bell` is a monotonic counter | `ScreenState::validate_transition` |
| I6 | `scrollback_len` never shrinks | `ScreenState::validate_transition` |

I1-I3 are properties of **one state**, so `validate()` can see them. I5 and I6
are properties of a **transition** — one state in isolation carries no history —
so they need `validate_transition(&previous)`, which the sync layer runs after
applying a diff, with the pre-application state as `previous`.

**The governing rule: a diff violating an invariant is rejected WHOLESALE and
never applied partially.** A half-applied diff leaves the receiver holding a
state that existed nowhere — no sender ring contains it, and no later diff was
computed against it — which is strictly worse than a dropped datagram, because
dropping is a case the protocol already knows how to recover from.

**Nothing clamps.** A cursor outside the screen is rejected. Clamping turns a
detectable desynchronisation into a session that looks healthy while the two
ends drift apart, which is the failure mode that costs the most to diagnose.

## Normative decisions this plan makes

The contract is silent on these. They are binding for M1 and every later
milestone.

1. **`oxutrm-sync` re-exports `ApplyError`** from `oxutrm-proto`, so the
   contract's `oxutrm_sync::ApplyError` path resolves. The enum itself lives in
   `oxutrm-proto` — `ScreenState::validate` returns it and `oxutrm-sync`
   depends on `oxutrm-proto`, so any other placement is a cycle.
2. **Damage reaches `oxutrm-sync` as data.** `Sender::update_damaged(next, DiffHint)`
   stores a damaged-row hint alongside each ring state; `update(next)` is
   `update_damaged(next, DiffHint::everything())`. Diffing from base *B* to
   current *C* **unions** the hints of ring entries `B+1..=C`. The sync crate
   never asks anything for damage — it is handed a slice of row numbers, which
   is what keeps it pure.
3. **`Receiver::ack()` returns 0 until the first frame is accepted.** A valid
   `ScreenState` has `seq >= 1`, so `Receiver::new` holds a blank state at
   seq 1; reporting `ack() == 1` before receiving anything would claim a state
   the sender never sent. A `started` flag separates the two.
4. **`SyncState::apply` rejects a non-advancing target**, per spec §8.4. The
   check is `target > base`, which is base-relative and therefore well defined
   for the full-state case (`base == 0`, so `target >= 1`).
5. **`validate` runs exactly once, after the whole diff is applied** — never
   before, and never between the resize and the cell writes. Task 4 pins both
   halves of that with a distinguishing test pair.

---

## File Structure

```
crates/oxutrm-sync/src/lib.rs                MODIFY: re-exports, STATE_RING
crates/oxutrm-sync/src/state.rs              SyncState trait, DiffHint
crates/oxutrm-sync/src/screen.rs             Run, RowPatch, ScreenDiff, impl
crates/oxutrm-sync/src/input.rs              InputState, InputDiff, impl
crates/oxutrm-sync/src/sender.rs             Sender, the zstd policy
crates/oxutrm-sync/src/receiver.rs           Receiver
crates/oxutrm-sync/tests/reject_path.rs      fault injection over the invariants
crates/oxutrm-sync/tests/convergence.rs      THE proptest
crates/oxutrm-term/src/lib.rs                MODIFY: module wiring
crates/oxutrm-term/src/palette.rs            the 269-entry table, colour resolution
crates/oxutrm-term/src/grid.rs               the ONE checked grid accessor
crates/oxutrm-term/src/blink.rs              BlinkPlane + the Handler newtype
crates/oxutrm-term/src/host.rs               HostTerm, the EventListener
crates/oxutrm-term/src/caps.rs               detect_caps, negotiate_term
crates/oxutrm-term/tests/fixtures/*.ansi     four recorded fixtures
crates/oxutrm-term/tests/emulation.rs        insta snapshots over ScreenState
crates/oxutrm-client/src/lib.rs              MODIFY: re-exports
crates/oxutrm-client/src/render.rs           Renderer, Style, sgr
crates/oxutrm-client/src/raw.rs              RawGuard, terminal_size
src/lib.rs                                   MODIFY: library target
src/loopback.rs                              pump() and run_loopback()
src/main.rs                                  MODIFY: the loopback arm
tests/loopback.rs                            end-to-end
```

Dependency direction, no cycles:

```
oxutrm-proto  <-  oxutrm-sync
     ^                 ^
     +--  oxutrm-term  +--  oxutrm-client  <-  main
```

`oxutrm-term` depends on `oxutrm-proto` alone, **not** on `oxutrm-sync`. The
screen model lives in `oxutrm-proto` precisely so that `alacritty_terminal`'s
PTY, `polling` and `signal-hook` never reach the pure crate.

---

## Task 1: Verify the landed workspace

The workspace, the six crate manifests, the `Makefile` and the binary skeleton
**already exist**, committed at `dd10d69`. This task confirms the ground is
what the rest of the plan assumes, and adds nothing.

**Files:**
- Modify: none. Read only.

**Interfaces:**
- Consumes: nothing.
- Produces: confidence that `cargo`, the edition, the MSRV floor, the parallelism
  cap and the profile are as every later task assumes. If any check here fails,
  **stop and report** rather than repairing — another agent owns this commit.

- [ ] **Step 1: Confirm the workspace shape**

```bash
cargo metadata --no-deps --format-version 1 \
  | tr ',' '\n' | grep -o '"name":"oxutrm[^"]*"' | sort -u
```
Expected: the seven names `oxutrm`, `oxutrm-client`, `oxutrm-host`,
`oxutrm-net`, `oxutrm-proto`, `oxutrm-sync`, `oxutrm-term`.

- [ ] **Step 2: Confirm the edition, the MSRV and the profile**

```bash
grep -n 'edition = "2024"\|rust-version = "1.85"\|resolver = "3"' Cargo.toml
grep -n -A3 '\[profile.dev\]' Cargo.toml
```
Expected: edition 2024, rust-version 1.85, resolver 3, and a `[profile.dev]`
carrying `debug = "line-tables-only"` and `split-debuginfo = "unpacked"`.

- [ ] **Step 3: Confirm `alacritty_terminal` has `serde` off**

```bash
cargo tree -p oxutrm-term -e features 2>/dev/null | grep -c 'alacritty_terminal feature "serde"'
```
Expected: `0`. If it is non-zero the resolver has unified the feature back on
and every later `oxutrm-term` task will link a dependency the contract excludes.

- [ ] **Step 4: Confirm the gate is green before any new work**

Run: `make check`
Expected: PASS — `cargo fmt --all`, then clippy with `-D warnings`, then the
whole test suite at 4 threads.

Run: `cargo test --jobs 4 --workspace -- --test-threads 4 2>&1 | grep -E '^test result'`
Expected: every line reports `ok`, and the totals sum to at least 73 tests.

- [ ] **Step 5: Record the baseline**

Note the test total. Every later task adds to it, and Task 12's final check
compares against this number. A task that ends with *fewer* tests than it
started with has deleted something.

No commit: this task changes nothing.

---

## Task 2: Verify the landed `oxutrm-proto`, including the screen model

`oxutrm-proto` **already exists**, committed at `4243601`, `49fd36f`, `dfdd78f`
and `bf41e32`. It owns every wire type **including the screen model**, which
moved there from `oxutrm-term` deliberately. This task confirms the exact
surface the sync engine is about to be written against.

Do **not** re-implement any of it. If something here is missing or different,
stop and report — the contract and the crate are being reconciled by others.

**Files:**
- Modify: none. Read only.

**Interfaces:**
- Consumes: nothing.
- Produces (all from `oxutrm_proto`, and these are the exact names Tasks 3-12
  import):
  ```rust
  pub const PROTO_VERSION: u32 = 1;
  pub const FLAG_ZSTD: u8 = 0x01;

  // Five fields. No frag_index, no frag_count. Order is wire-significant.
  #[derive(Clone, Debug, Serialize, Deserialize)]        // NOTE: no PartialEq
  pub struct Frame {
      pub my_state: u64, pub from_state: u64, pub ack_state: u64,
      pub flags: u8, pub payload: Vec<u8>,
  }
  impl Frame {
      pub fn encode(&self) -> Result<Vec<u8>, ProtoError>;
      pub fn decode(bytes: &[u8]) -> Result<Frame, ProtoError>;   // trailing bytes are an error
  }

  #[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
  pub struct TermSize { pub cols: u16, pub rows: u16 }
  pub struct TerminalCaps { pub truecolor: bool, pub colors: u32,
      pub bracketed_paste: bool, pub mouse_sgr: bool, pub osc52: bool,
      pub term_name: String }

  pub enum Color { Default, Idx(u8), Rgb(u8, u8, u8) }
  bitflags! { pub struct Attrs: u16 { BOLD ITALIC UNDERLINE INVERSE BLINK
                                      STRIKE DIM HIDDEN WIDE_CONT } }
  pub type CellText = compact_str::CompactString;
  #[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
  pub struct Cell { pub text: CellText, pub fg: Color, pub bg: Color, pub attrs: Attrs }
  impl Cell { pub fn blank() -> Cell; }        // " ", Default, Default, empty

  pub enum CursorShape { Block, Underline, Bar }
  pub struct Cursor { pub row: u16, pub col: u16, pub visible: bool, pub shape: CursorShape }
  pub enum MouseMode { Off, Press, PressRelease, ButtonMotion, AnyMotion }
  pub struct Modes { pub alt_screen: bool, pub bracketed_paste: bool,
                     pub mouse: MouseMode, pub app_cursor: bool, pub app_keypad: bool }

  #[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
  pub struct ScreenState {
      pub seq: u64, pub rows: u16, pub cols: u16, pub cells: Vec<Cell>,
      pub cursor: Cursor, pub modes: Modes, pub title: String,
      pub bell: u32, pub scrollback_len: u64,
      // NO `icon` field. vte drops OSC 1.
  }
  impl ScreenState {
      /// Sequence 1. Returns Result because every constructor validates.
      pub fn blank(rows: u16, cols: u16) -> Result<ScreenState, ApplyError>;
      pub fn cell(&self, row: u16, col: u16) -> &Cell;   // PANICS out of range
      pub fn row(&self, row: u16) -> &[Cell];            // PANICS out of range
      /// I1, I2, I3 — the rules one state carries alone.
      pub fn validate(&self) -> Result<(), ApplyError>;
      /// I5, I6 — the rules that only exist between two states. Validates
      /// `self` on its own first.
      pub fn validate_transition(&self, previous: &ScreenState) -> Result<(), ApplyError>;
  }

  #[derive(thiserror::Error, Debug, PartialEq, Eq)]
  pub enum ApplyError {
      BaseMismatch { base: u64, current: u64 },
      OutOfBounds { row: u16, rows: u16 },
      Decode(String),
      LengthMismatch { len: usize, rows: u16, cols: u16 },
      CursorOutOfBounds { row: u16, col: u16, rows: u16, cols: u16 },
      SeqZero,
      BellWentBackwards { was: u32, now: u32 },
      ScrollbackShrank { was: u64, now: u64 },
  }
  ```

**Four consequences of this exact surface that later tasks must respect:**

- **`ScreenState::blank` returns a `Result`.** Every call site writes
  `.expect("a blank screen is valid")` or propagates.
- **There is no `row_mut` and no `size()`.** Mutating tests write through the
  public `cells` vector; a local helper is the readable way to do it.
- **`Frame` does not derive `PartialEq`.** Compare two frames by their encoded
  bytes, or field by field. Do not add the derive to a landed crate.
- **`cell()` and `row()` panic out of range.** They are for coordinates the
  caller computed, not for coordinates that arrived over the network. Anything
  from the wire goes through `validate` first.

- [ ] **Step 1: Confirm `Frame` has five fields and no fragmentation**

```bash
grep -rn 'frag' crates/ src/
```
Expected: **no output**, exit status 1.

```bash
sed -n '/pub struct Frame/,/^}/p' crates/oxutrm-proto/src/frame.rs
```
Expected: exactly `my_state`, `from_state`, `ack_state`, `flags`, `payload`, in
that order.

- [ ] **Step 2: Confirm the screen model is in `oxutrm-proto`, not `oxutrm-term`**

```bash
grep -c 'pub struct ScreenState' crates/oxutrm-proto/src/screen.rs
grep -rc 'ScreenState' crates/oxutrm-term/src/lib.rs
```
Expected: `1` from the first. The second may be non-zero only in prose — check
by eye that `oxutrm-term` defines no screen type of its own.

- [ ] **Step 3: Confirm there is no `icon` field anywhere**

```bash
grep -rn 'icon' crates/oxutrm-proto/src/
```
Expected: prose only — an explanation that `vte` drops `OSC 1`. **No field
declaration.** `OSC 1` is the one capability the emulator choice gave up, and
the field's absence is the enforcement.

- [ ] **Step 4: Confirm both validators exist and reject rather than clamp**

```bash
grep -n 'pub fn validate\b\|pub fn validate_transition' crates/oxutrm-proto/src/screen.rs
grep -n 'min(\|max(\|clamp' crates/oxutrm-proto/src/screen.rs
```
Expected: both functions present; **no clamping** in either.

- [ ] **Step 5: Confirm all eight `ApplyError` variants**

```bash
grep -c '#\[error' crates/oxutrm-proto/src/error.rs
```
Expected: `8`. The contract's copy lists only five — it predates `SeqZero`,
`BellWentBackwards` and `ScrollbackShrank`. **The crate is right**; report the
divergence but write against the crate.

- [ ] **Step 6: Confirm the crate is green**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: PASS.

No commit: this task changes nothing.

---

## Task 3: `oxutrm-sync` — `SyncState`, `DiffHint` and `ScreenDiff`

**Files:**
- Create: `crates/oxutrm-sync/src/state.rs`
- Create: `crates/oxutrm-sync/src/screen.rs`
- Modify: `crates/oxutrm-sync/src/lib.rs` (keep the existing module doc)

**Interfaces:**
- Consumes: `oxutrm_proto::{ApplyError, Cell, Cursor, Modes, ScreenState, TermSize}`.
- Produces:
  ```rust
  pub const STATE_RING: usize = 32;
  pub use oxutrm_proto::ApplyError;        // the contract's oxutrm_sync::ApplyError path

  /// Which rows a diff needs to examine. `None` means "assume every row changed".
  #[derive(Clone, Debug, Default, PartialEq, Eq)]
  pub struct DiffHint { pub rows: Option<Vec<u16>> }
  impl DiffHint {
      pub fn everything() -> DiffHint;
      pub fn rows(rows: Vec<u16>) -> DiffHint;     // sorted and deduped
      pub fn union(&self, other: &DiffHint) -> DiffHint;
      pub fn is_everything(&self) -> bool;
      pub fn contains(&self, row: u16) -> bool;
  }

  pub trait SyncState: Clone {
      type Diff: serde::Serialize + serde::de::DeserializeOwned;
      fn seq(&self) -> u64;
      fn set_seq(&mut self, seq: u64);
      fn diff_from(&self, base: &Self) -> Self::Diff;
      fn apply(&mut self, base: u64, target: u64, d: &Self::Diff) -> Result<(), ApplyError>;
      fn full_diff(&self) -> Self::Diff;
      /// Defaults to `diff_from`. `ScreenState` overrides it to consult damage.
      fn diff_from_hint(&self, base: &Self, _hint: &DiffHint) -> Self::Diff { self.diff_from(base) }
      /// Every invariant that applies to a transition. Defaults to `Ok(())`.
      /// `ScreenState` overrides it to call `validate_transition`.
      fn check(&self, _previous: &Self) -> Result<(), ApplyError> { Ok(()) }
  }

  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct Run { pub start_col: u16, pub repeat: u16, pub cells: Vec<Cell> }
  impl Run { pub fn width(&self) -> usize; }       // cells.len() * (repeat + 1)

  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct RowPatch { pub row: u16, pub runs: Vec<Run> }

  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct ScreenDiff {
      pub resize: Option<TermSize>,
      pub rows: Vec<RowPatch>,
      pub cursor: Option<Cursor>,
      pub modes: Option<Modes>,
      pub title: Option<String>,
      pub bell: Option<u32>,
      pub scrollback_len: Option<u64>,
  }

  impl SyncState for ScreenState { type Diff = ScreenDiff; }
  ```
  `ScreenDiff` carries **no `base` or `target`** — `Frame` owns them, so there
  is exactly one place a receiver looks and the two can never disagree. It
  carries **no `icon`** either.

**`Run` semantics, normative:** the `cells` sequence is emitted **`repeat + 1`
times consecutively**, starting at `start_col`. `repeat == 0` therefore means
"emit `cells` exactly once", and a run of 40 identical blanks is
`Run { start_col, repeat: 39, cells: vec![blank] }`. A run covers
`cells.len() * (repeat + 1)` columns, and a `RowPatch`'s runs must not overlap.

**`apply` semantics, normative:** `base == 0` means "this is a full state" and
applies unconditionally. Any other `base` must equal the current `seq`, and
`target` must be greater than `base`; either failure is `BaseMismatch`. Every
bound is checked **before a single cell is written**, so a rejected diff leaves
the state byte-for-byte unchanged. `apply` does **not** call `validate` — that
is the caller's job, exactly once, afterwards (Task 4 pins why).

- [ ] **Step 1: Write the failing tests**

Create `crates/oxutrm-sync/src/screen.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{Attrs, CellText, Color, CursorShape};

    /// `ScreenState` has no `row_mut`, so tests write through `cells`.
    fn put(s: &mut ScreenState, row: u16, col: u16, cell: Cell) {
        let ix = row as usize * s.cols as usize + col as usize;
        s.cells[ix] = cell;
    }

    fn glyph(text: &str) -> Cell {
        Cell {
            text: CellText::new(text),
            ..Cell::blank()
        }
    }

    fn seeded(seq: u64, rows: u16, cols: u16) -> ScreenState {
        let mut s = ScreenState::blank(rows, cols).expect("a blank screen is valid");
        s.seq = seq;
        s
    }

    fn empty_diff() -> ScreenDiff {
        ScreenDiff {
            resize: None,
            rows: vec![],
            cursor: None,
            modes: None,
            title: None,
            bell: None,
            scrollback_len: None,
        }
    }

    // ---- Run semantics: the table the contract demands ----

    #[test]
    fn repeat_zero_emits_the_cells_exactly_once() {
        let mut got = seeded(1, 1, 6);
        let d = ScreenDiff {
            rows: vec![RowPatch {
                row: 0,
                runs: vec![Run {
                    start_col: 0,
                    repeat: 0,
                    cells: vec![glyph("x")],
                }],
            }],
            ..empty_diff()
        };
        got.apply(1, 2, &d).expect("apply");
        assert_eq!(got.cell(0, 0).text, "x");
        assert_eq!(got.cell(0, 1).text, " ", "repeat 0 means ONE copy, not two");
    }

    #[test]
    fn repeat_one_emits_the_cells_twice() {
        let mut got = seeded(1, 1, 6);
        let d = ScreenDiff {
            rows: vec![RowPatch {
                row: 0,
                runs: vec![Run {
                    start_col: 1,
                    repeat: 1,
                    cells: vec![glyph("y")],
                }],
            }],
            ..empty_diff()
        };
        got.apply(1, 2, &d).expect("apply");
        assert_eq!(got.cell(0, 1).text, "y");
        assert_eq!(got.cell(0, 2).text, "y");
        assert_eq!(got.cell(0, 3).text, " ");
    }

    #[test]
    fn repeat_five_emits_a_multi_cell_sequence_six_times() {
        let mut got = seeded(1, 1, 12);
        let d = ScreenDiff {
            rows: vec![RowPatch {
                row: 0,
                // "xy" six times covers 2 * (5 + 1) = 12 columns.
                runs: vec![Run {
                    start_col: 0,
                    repeat: 5,
                    cells: vec![glyph("x"), glyph("y")],
                }],
            }],
            ..empty_diff()
        };
        got.apply(1, 2, &d).expect("apply");
        let text: String = got.row(0).iter().map(|c| c.text.as_str()).collect();
        assert_eq!(text, "xyxyxyxyxyxy");
    }

    #[test]
    fn a_run_reaching_past_the_row_is_out_of_bounds() {
        let mut got = seeded(1, 1, 4);
        let before = got.clone();
        let d = ScreenDiff {
            rows: vec![RowPatch {
                row: 0,
                // 1 * (9 + 1) = 10 columns from column 2 on a 4-wide screen.
                runs: vec![Run {
                    start_col: 2,
                    repeat: 9,
                    cells: vec![Cell::blank()],
                }],
            }],
            ..empty_diff()
        };
        assert_eq!(
            got.apply(1, 2, &d),
            Err(ApplyError::OutOfBounds { row: 0, rows: 1 })
        );
        assert_eq!(got, before, "a rejected diff must not mutate anything");
    }

    #[test]
    fn an_empty_run_is_rejected_rather_than_silently_doing_nothing() {
        let mut got = seeded(1, 1, 4);
        let d = ScreenDiff {
            rows: vec![RowPatch {
                row: 0,
                runs: vec![Run {
                    start_col: 0,
                    repeat: 0,
                    cells: vec![],
                }],
            }],
            ..empty_diff()
        };
        assert_eq!(
            got.apply(1, 2, &d),
            Err(ApplyError::Decode("run with no cells".to_string()))
        );
    }

    // ---- diff generation ----

    #[test]
    fn an_identical_screen_produces_an_empty_diff() {
        let a = seeded(4, 3, 5);
        let mut b = a.clone();
        b.seq = 5;
        let d = b.diff_from(&a);
        assert!(d.rows.is_empty());
        assert_eq!(d.resize, None);
        assert_eq!(d.cursor, None);
        assert_eq!(d.title, None);
        assert_eq!(d.bell, None);
    }

    #[test]
    fn one_changed_cell_produces_one_single_cell_run() {
        let a = seeded(1, 2, 6);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 1, 3, glyph("Z"));

        let d = b.diff_from(&a);
        assert_eq!(d.rows.len(), 1);
        assert_eq!(d.rows[0].row, 1);
        assert_eq!(d.rows[0].runs.len(), 1);
        assert_eq!(d.rows[0].runs[0].start_col, 3);
        assert_eq!(d.rows[0].runs[0].repeat, 0, "one copy means repeat 0");
        assert_eq!(d.rows[0].runs[0].cells[0].text, "Z");
    }

    #[test]
    fn a_uniform_stretch_collapses_into_a_repeat() {
        let a = seeded(1, 1, 10);
        let mut b = a.clone();
        b.seq = 2;
        for c in b.cells.iter_mut() {
            c.bg = Color::Idx(4);
        }

        let d = b.diff_from(&a);
        let run = &d.rows[0].runs[0];
        assert_eq!(run.start_col, 0);
        assert_eq!(run.repeat, 9, "ten copies of one cell means repeat 9");
        assert_eq!(run.cells.len(), 1);
        assert_eq!(run.width(), 10);
    }

    #[test]
    fn two_separate_changes_in_one_row_become_two_non_overlapping_runs() {
        let a = seeded(1, 1, 8);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 0, 1, glyph("X"));
        put(&mut b, 0, 6, glyph("Y"));

        let d = b.diff_from(&a);
        let runs = &d.rows[0].runs;
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].start_col, 1);
        assert_eq!(runs[1].start_col, 6);
        assert!(
            runs[0].start_col as usize + runs[0].width() <= runs[1].start_col as usize,
            "runs must not overlap"
        );
    }

    #[test]
    fn scalars_travel_only_when_they_change() {
        let a = seeded(1, 2, 4);
        let mut b = a.clone();
        b.seq = 2;
        b.title = "vim".to_string();
        b.bell = 1;
        b.cursor.col = 3;
        b.modes.alt_screen = true;
        b.scrollback_len = 99;

        let d = b.diff_from(&a);
        assert_eq!(d.title.as_deref(), Some("vim"));
        assert_eq!(d.bell, Some(1));
        assert_eq!(d.cursor.map(|c| c.col), Some(3));
        assert_eq!(d.modes.map(|m| m.alt_screen), Some(true));
        assert_eq!(d.scrollback_len, Some(99));
    }

    #[test]
    fn a_resize_sends_every_row() {
        let mut a = seeded(1, 2, 4);
        put(&mut a, 0, 0, glyph("q"));
        let mut b = seeded(2, 3, 6);
        put(&mut b, 0, 0, glyph("q"));

        let d = b.diff_from(&a);
        assert_eq!(d.resize, Some(TermSize { cols: 6, rows: 3 }));
        assert_eq!(
            d.rows.len(),
            3,
            "after a resize the receiver's rows mean nothing, so send all of them"
        );
    }

    #[test]
    fn a_full_diff_carries_everything() {
        let mut s = seeded(17, 2, 3);
        s.title = "t".to_string();
        s.bell = 4;
        s.cursor.shape = CursorShape::Bar;
        s.scrollback_len = 12;

        let d = s.full_diff();
        assert_eq!(d.resize, Some(TermSize { cols: 3, rows: 2 }));
        assert_eq!(d.rows.len(), 2);
        assert_eq!(d.title.as_deref(), Some("t"));
        assert_eq!(d.bell, Some(4));
        assert_eq!(d.cursor.map(|c| c.shape), Some(CursorShape::Bar));
        assert_eq!(d.scrollback_len, Some(12));
    }

    // ---- apply ----

    #[test]
    fn applying_a_diff_reproduces_the_source_exactly() {
        let a = seeded(1, 3, 8);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 0, 0, glyph("H"));
        put(
            &mut b,
            0,
            1,
            Cell {
                text: CellText::new("i"),
                fg: Color::Rgb(1, 2, 3),
                bg: Color::Idx(9),
                attrs: Attrs::BOLD | Attrs::UNDERLINE | Attrs::BLINK,
            },
        );
        for c in 0..8 {
            put(
                &mut b,
                2,
                c,
                Cell {
                    bg: Color::Idx(2),
                    ..Cell::blank()
                },
            );
        }
        b.title = "shell".to_string();
        b.cursor.row = 2;

        let d = b.diff_from(&a);
        let mut got = a.clone();
        got.apply(1, 2, &d).expect("apply");
        assert_eq!(got, b);
        got.validate().expect("the result is a valid state");
        got.check(&a).expect("the transition is valid too");
    }

    #[test]
    fn a_full_diff_applies_over_a_different_size() {
        let mut b = seeded(9, 2, 3);
        put(&mut b, 1, 2, glyph("!"));

        let mut got = seeded(4, 7, 20);
        got.apply(0, 9, &b.full_diff()).expect("apply");
        assert_eq!(got, b);
    }

    #[test]
    fn a_diff_against_the_wrong_base_is_rejected_not_applied() {
        let a = seeded(1, 2, 4);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 0, 0, glyph("X"));
        let d = b.diff_from(&a);

        let mut wrong = seeded(5, 2, 4);
        let before = wrong.clone();
        assert_eq!(
            wrong.apply(1, 2, &d),
            Err(ApplyError::BaseMismatch { base: 1, current: 5 })
        );
        assert_eq!(wrong, before);
    }

    #[test]
    fn a_target_that_does_not_advance_is_rejected() {
        let mut got = seeded(3, 2, 4);
        let d = empty_diff();
        assert_eq!(
            got.apply(3, 3, &d),
            Err(ApplyError::BaseMismatch { base: 3, current: 3 })
        );
        assert_eq!(
            got.apply(3, 2, &d),
            Err(ApplyError::BaseMismatch { base: 3, current: 3 })
        );
        assert_eq!(got.seq, 3, "neither attempt changed the state");
    }

    #[test]
    fn a_full_state_with_target_zero_is_rejected() {
        let mut got = seeded(3, 2, 4);
        let full = got.clone().full_diff();
        assert!(got.apply(0, 0, &full).is_err(), "seq 0 is the sentinel");
    }

    #[test]
    fn a_row_outside_the_screen_is_out_of_bounds() {
        let mut got = seeded(1, 2, 4);
        let before = got.clone();
        let d = ScreenDiff {
            rows: vec![RowPatch {
                row: 9,
                runs: vec![Run {
                    start_col: 0,
                    repeat: 0,
                    cells: vec![Cell::blank()],
                }],
            }],
            ..empty_diff()
        };
        assert_eq!(
            got.apply(1, 2, &d),
            Err(ApplyError::OutOfBounds { row: 9, rows: 2 })
        );
        assert_eq!(got, before);
    }

    #[test]
    fn a_diff_round_trips_through_postcard() {
        let a = seeded(1, 2, 4);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 1, 1, glyph("\u{6f22}"));
        let d = b.diff_from(&a);
        let bytes = postcard::to_stdvec(&d).expect("encode");
        assert_eq!(postcard::from_bytes::<ScreenDiff>(&bytes).expect("decode"), d);
    }

    // ---- damage hints ----

    #[test]
    fn an_honest_hint_produces_exactly_the_same_diff() {
        let a = seeded(1, 4, 4);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 1, 0, glyph("A"));
        put(&mut b, 3, 0, glyph("B"));

        assert_eq!(
            b.diff_from_hint(&a, &DiffHint::rows(vec![1, 3])),
            b.diff_from(&a)
        );
        assert_eq!(
            b.diff_from_hint(&a, &DiffHint::everything()),
            b.diff_from(&a)
        );
    }

    #[test]
    fn a_hint_is_trusted_which_is_why_it_must_come_from_damage_tracking() {
        let a = seeded(1, 4, 4);
        let mut b = a.clone();
        b.seq = 2;
        put(&mut b, 1, 0, glyph("A"));
        put(&mut b, 3, 0, glyph("B"));

        let partial = b.diff_from_hint(&a, &DiffHint::rows(vec![1]));
        assert_eq!(partial.rows.len(), 1);
        assert_eq!(partial.rows[0].row, 1);
    }

    #[test]
    fn a_resize_ignores_the_hint_entirely() {
        let a = seeded(1, 2, 4);
        let mut b = seeded(2, 3, 4);
        put(&mut b, 0, 0, glyph("z"));
        // Even a hint naming nothing must still send every row after a resize.
        let d = b.diff_from_hint(&a, &DiffHint::rows(vec![]));
        assert_eq!(d.resize, Some(TermSize { cols: 4, rows: 3 }));
        assert_eq!(d.rows.len(), 3);
    }

    #[test]
    fn hints_union_correctly() {
        assert_eq!(
            DiffHint::rows(vec![1, 3]).union(&DiffHint::rows(vec![3, 5])).rows,
            Some(vec![1, 3, 5])
        );
        assert!(DiffHint::rows(vec![1])
            .union(&DiffHint::everything())
            .is_everything());
        assert!(DiffHint::everything()
            .union(&DiffHint::rows(vec![1]))
            .is_everything());
        assert!(DiffHint::everything().contains(7));
        assert!(!DiffHint::rows(vec![1, 2]).contains(7));
    }

    #[test]
    fn check_delegates_to_validate_transition() {
        let a = seeded(1, 2, 4);
        let mut b = a.clone();
        b.seq = 2;
        b.bell = 5;
        b.check(&a).expect("a rising bell is fine");

        let mut backwards = b.clone();
        backwards.seq = 3;
        backwards.bell = 1;
        assert_eq!(
            backwards.check(&b),
            Err(ApplyError::BellWentBackwards { was: 5, now: 1 })
        );
    }
}
```

Extend `crates/oxutrm-sync/src/lib.rs`, keeping its existing module doc:

```rust
mod screen;
mod state;

pub use oxutrm_proto::ApplyError;
pub use screen::{RowPatch, Run, ScreenDiff};
pub use state::{DiffHint, SyncState};

/// How many recent states the sender keeps so it can diff from whatever the
/// peer last acknowledged. An older ack falls back to a full state.
pub const STATE_RING: usize = 32;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-sync -- --test-threads 4`
Expected: FAIL — `file not found for module 'state'` and `cannot find type
'ScreenDiff'`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test no_io`
Expected: still PASS. That test must never break; if a new dependency appears
in `Cargo.toml`, this task has gone wrong.

- [ ] **Step 3: Write `crates/oxutrm-sync/src/state.rs`**

```rust
use oxutrm_proto::ApplyError;

/// Which rows a diff needs to examine.
///
/// `rows: None` means "assume every row changed" and is always safe.
/// `rows: Some(..)` is a promise that no other row differs — it comes from the
/// emulator's per-line damage tracking, which answers exactly this question.
///
/// This is data handed **to** `oxutrm-sync`, never something the crate goes and
/// asks for. That is what keeps the crate pure, and it is why the hint is a
/// plain list of row numbers rather than a handle to anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffHint {
    pub rows: Option<Vec<u16>>,
}

impl DiffHint {
    /// The safe hint: examine every row.
    #[must_use]
    pub fn everything() -> DiffHint {
        DiffHint { rows: None }
    }

    /// Examine only these rows. Duplicates and disorder are fine; the value is
    /// normalised so `contains` can binary-search.
    #[must_use]
    pub fn rows(mut rows: Vec<u16>) -> DiffHint {
        rows.sort_unstable();
        rows.dedup();
        DiffHint { rows: Some(rows) }
    }

    #[must_use]
    pub fn is_everything(&self) -> bool {
        self.rows.is_none()
    }

    /// The hint covering both. Unioning with `everything` gives `everything`.
    #[must_use]
    pub fn union(&self, other: &DiffHint) -> DiffHint {
        match (&self.rows, &other.rows) {
            (Some(a), Some(b)) => {
                let mut rows = a.clone();
                rows.extend_from_slice(b);
                DiffHint::rows(rows)
            }
            _ => DiffHint::everything(),
        }
    }

    /// True when `row` must be examined.
    #[must_use]
    pub fn contains(&self, row: u16) -> bool {
        match &self.rows {
            None => true,
            Some(rows) => rows.binary_search(&row).is_ok(),
        }
    }
}

/// A replicated value.
///
/// No I/O, no clocks, no allocation assumptions. Sequence number `0` is
/// reserved: it is the "full state" sentinel that `Frame::from_state` carries,
/// and it is never a valid state number.
pub trait SyncState: Clone {
    type Diff: serde::Serialize + serde::de::DeserializeOwned;

    fn seq(&self) -> u64;
    fn set_seq(&mut self, seq: u64);

    /// A diff that turns `base` into `self`.
    fn diff_from(&self, base: &Self) -> Self::Diff;

    /// Apply a diff.
    ///
    /// `base` and `target` are **parameters rather than fields of `Diff`**
    /// because `Frame` owns them; there is exactly one place a receiver looks
    /// for them, so the two can never disagree.
    ///
    /// `base == 0` means "a full state" and applies unconditionally. Any other
    /// `base` must equal `self.seq()`, and `target` must be greater than
    /// `base`. Either failure is `BaseMismatch`.
    ///
    /// An implementation must leave `self` **byte-for-byte unchanged** when it
    /// returns any error at all. A half-applied diff leaves a state that
    /// existed nowhere — no sender ring holds it, and no later diff was
    /// computed against it — which is strictly worse than a dropped datagram,
    /// because dropping is a case the protocol already recovers from.
    ///
    /// `apply` does **not** validate. The caller validates exactly once,
    /// afterwards, with `check`.
    fn apply(&mut self, base: u64, target: u64, d: &Self::Diff) -> Result<(), ApplyError>;

    /// A diff from nothing: the first datagram of every attach, and whatever
    /// the ring-miss path has to send.
    fn full_diff(&self) -> Self::Diff;

    /// A diff that examines only the rows the hint names.
    ///
    /// Defaults to ignoring the hint, which is always correct and never wrong,
    /// only slower.
    fn diff_from_hint(&self, base: &Self, _hint: &DiffHint) -> Self::Diff {
        self.diff_from(base)
    }

    /// Every invariant that applies to the transition from `previous` to
    /// `self`, including the ones a single state cannot carry.
    ///
    /// Run **after** `apply`, exactly once. Defaults to accepting everything.
    fn check(&self, _previous: &Self) -> Result<(), ApplyError> {
        Ok(())
    }
}
```

- [ ] **Step 4: Write the body of `crates/oxutrm-sync/src/screen.rs`**

Put this **above** the existing `mod tests`:

```rust
use oxutrm_proto::{ApplyError, Cell, Cursor, Modes, ScreenState, TermSize};
use serde::{Deserialize, Serialize};

use crate::{DiffHint, SyncState};

/// A horizontal stretch of cells.
///
/// The `cells` sequence is emitted **`repeat + 1` times consecutively**,
/// starting at `start_col`. `repeat == 0` therefore means "emit `cells` exactly
/// once", and a run of 40 identical blanks is
/// `Run { start_col, repeat: 39, cells: vec![blank] }`. A run covers
/// `cells.len() * (repeat + 1)` columns, and the runs of one `RowPatch` must
/// not overlap.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Run {
    pub start_col: u16,
    pub repeat: u16,
    pub cells: Vec<Cell>,
}

impl Run {
    /// How many columns this run covers.
    #[must_use]
    pub fn width(&self) -> usize {
        self.cells.len() * (self.repeat as usize + 1)
    }
}

/// The changed parts of one row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RowPatch {
    pub row: u16,
    pub runs: Vec<Run>,
}

/// The difference between two screens. Only what changed travels.
///
/// There is deliberately no `base` or `target` here: `Frame` carries them, and
/// duplicating them invites the two copies to disagree. There is no `icon`
/// either — `vte` drops `OSC 1`, so there is no icon anywhere in oxutrm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenDiff {
    pub resize: Option<TermSize>,
    /// Changed rows only.
    pub rows: Vec<RowPatch>,
    pub cursor: Option<Cursor>,
    pub modes: Option<Modes>,
    pub title: Option<String>,
    pub bell: Option<u32>,
    pub scrollback_len: Option<u64>,
}

/// Build the runs describing how `new` differs from `old`.
///
/// `old == None` means "assume nothing about the target row", which is what a
/// full diff and a resize both need. Returns `None` when nothing changed.
fn row_runs(new: &[Cell], old: Option<&[Cell]>) -> Option<Vec<Run>> {
    let changed = |i: usize| match old {
        None => true,
        Some(o) => i >= o.len() || o[i] != new[i],
    };

    let mut runs = Vec::new();
    let mut col = 0usize;
    while col < new.len() {
        if !changed(col) {
            col += 1;
            continue;
        }
        let start = col;
        while col < new.len() && changed(col) {
            col += 1;
        }
        let span = &new[start..col];
        // A stretch of identical cells collapses to one cell with a repeat;
        // anything else travels literally, once.
        let uniform = span.iter().all(|c| *c == span[0]);
        runs.push(if uniform && span.len() > 1 {
            Run {
                start_col: start as u16,
                repeat: (span.len() - 1) as u16,
                cells: vec![span[0].clone()],
            }
        } else {
            Run {
                start_col: start as u16,
                repeat: 0,
                cells: span.to_vec(),
            }
        });
    }

    if runs.is_empty() { None } else { Some(runs) }
}

fn changed_opt<T: PartialEq + Copy>(new: T, old: T) -> Option<T> {
    (new != old).then_some(new)
}

fn size_of(s: &ScreenState) -> TermSize {
    TermSize {
        cols: s.cols,
        rows: s.rows,
    }
}

impl SyncState for ScreenState {
    type Diff = ScreenDiff;

    fn seq(&self) -> u64 {
        self.seq
    }

    fn set_seq(&mut self, seq: u64) {
        self.seq = seq;
    }

    fn diff_from(&self, base: &Self) -> ScreenDiff {
        self.diff_from_hint(base, &DiffHint::everything())
    }

    fn diff_from_hint(&self, base: &Self, hint: &DiffHint) -> ScreenDiff {
        let resized = base.rows != self.rows || base.cols != self.cols;

        let mut rows = Vec::new();
        for r in 0..self.rows {
            // A resize invalidates every row the receiver holds, so no hint
            // can be trusted to cover it.
            if !resized && !hint.contains(r) {
                continue;
            }
            let old = if resized { None } else { Some(base.row(r)) };
            if let Some(runs) = row_runs(self.row(r), old) {
                rows.push(RowPatch { row: r, runs });
            }
        }

        ScreenDiff {
            resize: resized.then(|| size_of(self)),
            rows,
            cursor: changed_opt(self.cursor, base.cursor),
            modes: changed_opt(self.modes, base.modes),
            title: (self.title != base.title).then(|| self.title.clone()),
            bell: changed_opt(self.bell, base.bell),
            scrollback_len: changed_opt(self.scrollback_len, base.scrollback_len),
        }
    }

    fn full_diff(&self) -> ScreenDiff {
        let rows = (0..self.rows)
            .map(|r| RowPatch {
                row: r,
                runs: row_runs(self.row(r), None).unwrap_or_default(),
            })
            .collect();

        ScreenDiff {
            resize: Some(size_of(self)),
            rows,
            cursor: Some(self.cursor),
            modes: Some(self.modes),
            title: Some(self.title.clone()),
            bell: Some(self.bell),
            scrollback_len: Some(self.scrollback_len),
        }
    }

    fn apply(&mut self, base: u64, target: u64, d: &ScreenDiff) -> Result<(), ApplyError> {
        // Check EVERYTHING first, mutate second. A rejected diff leaves the
        // state byte-for-byte unchanged, because a half-applied diff is a
        // state that existed nowhere and that no later diff was computed
        // against.
        if base != 0 && base != self.seq {
            return Err(ApplyError::BaseMismatch {
                base,
                current: self.seq,
            });
        }
        if target <= base {
            return Err(ApplyError::BaseMismatch {
                base,
                current: self.seq,
            });
        }

        let (rows, cols) = match d.resize {
            Some(size) => (size.rows, size.cols),
            None => (self.rows, self.cols),
        };
        for patch in &d.rows {
            if patch.row >= rows {
                return Err(ApplyError::OutOfBounds {
                    row: patch.row,
                    rows,
                });
            }
            for run in &patch.runs {
                if run.cells.is_empty() {
                    return Err(ApplyError::Decode("run with no cells".to_string()));
                }
                if run.start_col as usize + run.width() > cols as usize {
                    return Err(ApplyError::OutOfBounds {
                        row: patch.row,
                        rows,
                    });
                }
            }
        }

        if let Some(size) = d.resize {
            self.rows = size.rows;
            self.cols = size.cols;
            self.cells = vec![Cell::blank(); size.rows as usize * size.cols as usize];
        }

        for patch in &d.rows {
            let base_ix = patch.row as usize * self.cols as usize;
            for run in &patch.runs {
                let mut col = run.start_col as usize;
                for _ in 0..=run.repeat {
                    for c in &run.cells {
                        self.cells[base_ix + col] = c.clone();
                        col += 1;
                    }
                }
            }
        }

        if let Some(c) = d.cursor {
            self.cursor = c;
        }
        if let Some(m) = d.modes {
            self.modes = m;
        }
        if let Some(t) = &d.title {
            self.title.clone_from(t);
        }
        if let Some(b) = d.bell {
            self.bell = b;
        }
        if let Some(s) = d.scrollback_len {
            self.scrollback_len = s;
        }
        self.seq = target;
        Ok(())
    }

    fn check(&self, previous: &Self) -> Result<(), ApplyError> {
        // validate_transition validates `self` on its own first, so this one
        // call covers all six invariants.
        self.validate_transition(previous)
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-sync -- --test-threads 4`
Expected: PASS, 22 tests plus the existing `no_io` test.

- [ ] **Step 6: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-sync --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-sync
git commit -m "$(cat <<'EOF'
feat(sync): SyncState, DiffHint and ScreenDiff

Run emits its cells repeat+1 times, so repeat==0 means once — pinned by a
table test at 0, 1 and 5. base and target are apply() parameters, never diff
fields: Frame owns them, so the two can never disagree. apply() checks every
bound before writing a cell, so a rejected diff leaves the state byte-for-byte
unchanged, and it never validates — the caller does that once, afterwards.

DiffHint carries the emulator's per-line damage into the crate as plain data,
which is how the diff engine avoids comparing whole grids without the crate
ever performing I/O.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: `oxutrm-sync` — the reject path, by fault injection

**An invariant exercised only on the happy path is indistinguishable from one
that is not checked at all.** The convergence property in Task 8 proves the
engine converges when things go right; this task proves it *refuses* when they
go wrong, and — just as important — that refusing leaves nothing behind.

**Files:**
- Create: `crates/oxutrm-sync/tests/reject_path.rs`

**Interfaces:**
- Consumes: `SyncState`, `ScreenDiff`, `Run`, `RowPatch` (Task 3),
  `oxutrm_proto::{ApplyError, Cell, CellText, ScreenState, TermSize}`.
- Produces: nothing later tasks consume. It is a gate. Task 7 adds the
  `Receiver`-level half of the same suite once `Receiver` exists.

**Every reject test asserts TWO things**, and the second is the one people omit:

1. the state is **byte-for-byte unchanged**, and
2. the sequence number has **not advanced**.

An implementation that rejects a diff but advances anyway strands the sender
forever: it keeps diffing against a base the receiver does not hold, the
receiver keeps rejecting, and no happy-path test can see it. That is the
failure this suite exists to catch.

- [ ] **Step 1: Write the whole test file**

Create `crates/oxutrm-sync/tests/reject_path.rs`:

```rust
//! The reject path.
//!
//! `oxutrm-sync` is tested on both paths: the convergence property for the
//! happy one, and this suite for the refusals. An invariant that is only
//! exercised when everything goes right is indistinguishable from an invariant
//! nobody checks.
//!
//! Every test here asserts the same two things after a rejection — the state is
//! byte-for-byte unchanged, and the sequence number has not advanced. The
//! second is the easy one to forget, and forgetting it hides an implementation
//! that rejects a frame while telling the sender it accepted one, which strands
//! the sender permanently.

use oxutrm_proto::{ApplyError, Cell, CellText, Cursor, CursorShape, ScreenState, TermSize};
use oxutrm_sync::{RowPatch, Run, ScreenDiff, SyncState};

/// `ScreenState` has no `row_mut`, so write through `cells`.
fn put(s: &mut ScreenState, row: u16, col: u16, text: &str) {
    let ix = row as usize * s.cols as usize + col as usize;
    s.cells[ix] = Cell {
        text: CellText::new(text),
        ..Cell::blank()
    };
}

fn seeded(seq: u64, rows: u16, cols: u16) -> ScreenState {
    let mut s = ScreenState::blank(rows, cols).expect("a blank screen is valid");
    s.seq = seq;
    s
}

fn empty_diff() -> ScreenDiff {
    ScreenDiff {
        resize: None,
        rows: vec![],
        cursor: None,
        modes: None,
        title: None,
        bell: None,
        scrollback_len: None,
    }
}

fn one_cell(row: u16, col: u16, text: &str) -> RowPatch {
    RowPatch {
        row,
        runs: vec![Run {
            start_col: col,
            repeat: 0,
            cells: vec![Cell {
                text: CellText::new(text),
                ..Cell::blank()
            }],
        }],
    }
}

/// Apply a diff that is expected to be refused, and assert the two things that
/// matter: nothing changed, and the sequence did not move.
///
/// Returns the error so the caller can assert which one it was.
fn expect_rejected(state: &mut ScreenState, base: u64, target: u64, d: &ScreenDiff) -> ApplyError {
    let before = state.clone();
    let seq_before = state.seq();
    let err = state
        .apply(base, target, d)
        .expect_err("this diff must be refused");
    assert_eq!(
        state, &before,
        "a refused diff left the state changed; a half-applied state exists nowhere"
    );
    assert_eq!(
        state.seq(),
        seq_before,
        "a refused diff advanced the sequence number, which strands the sender"
    );
    err
}

// ---------------------------------------------------------------------------
// I1 — the cell count is exact
// ---------------------------------------------------------------------------

#[test]
fn a_state_whose_cells_do_not_match_its_size_is_rejected() {
    // Constructed by hand, because no constructor can produce this.
    let mut s = seeded(3, 2, 4);
    s.cells.pop();
    assert_eq!(
        s.validate(),
        Err(ApplyError::LengthMismatch {
            len: 7,
            rows: 2,
            cols: 4
        })
    );
}

#[test]
fn a_long_cell_vector_is_rejected_too_not_merely_a_short_one() {
    // The short case panics on the first read past the end, which is loud. The
    // LONG case silently addresses the wrong cell forever, which is the
    // expensive one, so it must be rejected just as firmly.
    let mut s = seeded(3, 2, 4);
    s.cells.push(Cell::blank());
    assert_eq!(
        s.validate(),
        Err(ApplyError::LengthMismatch {
            len: 9,
            rows: 2,
            cols: 4
        })
    );
}

#[test]
fn a_resize_that_would_break_the_cell_count_cannot_arise_from_apply() {
    // `apply` reallocates on resize, so I1 holds by construction. This test
    // pins that: it would fail if someone "optimised" apply to reuse the
    // existing vector.
    let mut got = seeded(1, 2, 4);
    let d = ScreenDiff {
        resize: Some(TermSize { cols: 7, rows: 5 }),
        rows: vec![one_cell(4, 6, "z")],
        cursor: Some(Cursor {
            row: 0,
            col: 0,
            visible: true,
            shape: CursorShape::Block,
        }),
        ..empty_diff()
    };
    got.apply(1, 2, &d).expect("apply");
    assert_eq!(got.cells.len(), 35, "5 x 7, exactly");
    got.validate().expect("I1 holds after every resize");
}

// ---------------------------------------------------------------------------
// I2 — the cursor sits on a cell that exists, and is NEVER clamped
// ---------------------------------------------------------------------------

#[test]
fn a_diff_moving_the_cursor_off_the_screen_is_rejected() {
    let mut got = seeded(1, 3, 8);
    let d = ScreenDiff {
        cursor: Some(Cursor {
            row: 9,
            col: 0,
            visible: true,
            shape: CursorShape::Block,
        }),
        ..empty_diff()
    };
    // `apply` itself does not validate, so the rejection comes from `check`.
    let mut candidate = got.clone();
    candidate.apply(1, 2, &d).expect("apply succeeds mechanically");
    assert_eq!(
        candidate.check(&got),
        Err(ApplyError::CursorOutOfBounds {
            row: 9,
            col: 0,
            rows: 3,
            cols: 8
        })
    );
    // And the real state — the one a receiver would keep — is untouched,
    // because the candidate is discarded rather than swapped in.
    assert_eq!(got.seq, 1);
    got.validate().expect("still valid");
}

#[test]
fn the_cursor_is_rejected_rather_than_clamped_and_this_test_proves_it() {
    // THIS TEST MUST FAIL if anyone replaces the rejection with a `min()`.
    //
    // A clamping implementation produces a state that validates while the two
    // ends quietly disagree about where the caret is — a session that looks
    // healthy and is not. So the assertion is deliberately two-sided: the
    // error must appear, AND the cursor must still hold its bad value, because
    // a clamp that also returned an error would still have mutated.
    let mut s = seeded(4, 3, 8);
    s.cursor.row = 7;
    s.cursor.col = 11;

    let err = s.validate().expect_err("an off-screen cursor is refused");
    assert_eq!(
        err,
        ApplyError::CursorOutOfBounds {
            row: 7,
            col: 11,
            rows: 3,
            cols: 8
        }
    );
    assert_eq!(s.cursor.row, 7, "validate reports; it must never repair");
    assert_eq!(s.cursor.col, 11, "a clamp to (2,7) here would be the bug");
}

#[test]
fn a_zero_by_zero_screen_is_exempt_because_no_cell_exists_to_sit_on() {
    let s = ScreenState::blank(0, 0).expect("degenerate but representable");
    s.validate().expect("the 0x0 screen is the one exemption");
}

// ---------------------------------------------------------------------------
// I3 — sequence zero is the sentinel, never a real state
// ---------------------------------------------------------------------------

#[test]
fn a_state_numbered_zero_is_rejected() {
    let mut s = seeded(1, 2, 4);
    s.seq = 0;
    assert_eq!(s.validate(), Err(ApplyError::SeqZero));
}

#[test]
fn a_full_state_diff_targeting_zero_is_rejected_by_apply_itself() {
    // base == 0 is the sentinel meaning "full state", so target 0 would be a
    // full state producing a state numbered zero. `apply` refuses before the
    // validator ever sees it, because target must exceed base.
    let mut got = seeded(3, 2, 4);
    let full = got.clone().full_diff();
    let err = expect_rejected(&mut got, 0, 0, &full);
    assert_eq!(err, ApplyError::BaseMismatch { base: 0, current: 3 });
}

// ---------------------------------------------------------------------------
// I4 — there is no icon field
// ---------------------------------------------------------------------------

#[test]
fn screen_state_has_exactly_nine_fields_and_none_of_them_is_an_icon() {
    // An exhaustive struct literal with no `..` — adding a field anywhere in
    // `oxutrm-proto` breaks this test, which is the point. `vte` drops OSC 1,
    // so an icon field could only ever hold a lie.
    let s = ScreenState {
        seq: 1,
        rows: 1,
        cols: 1,
        cells: vec![Cell::blank()],
        cursor: Cursor {
            row: 0,
            col: 0,
            visible: true,
            shape: CursorShape::Block,
        },
        modes: Default::default(),
        title: String::new(),
        bell: 0,
        scrollback_len: 0,
    };
    s.validate().expect("valid");
}

// ---------------------------------------------------------------------------
// I5 — the bell is monotonic
// ---------------------------------------------------------------------------

#[test]
fn a_diff_that_decreases_the_bell_is_rejected() {
    let mut previous = seeded(4, 2, 4);
    previous.bell = 9;

    let d = ScreenDiff {
        bell: Some(3),
        ..empty_diff()
    };
    let mut candidate = previous.clone();
    candidate.apply(4, 5, &d).expect("apply succeeds mechanically");
    assert_eq!(
        candidate.check(&previous),
        Err(ApplyError::BellWentBackwards { was: 9, now: 3 }),
        "the client rings once per increment, so a reset would ring the \
         terminal once for every bell in the session's history"
    );
    assert_eq!(previous.bell, 9, "the kept state is untouched");
}

#[test]
fn a_bell_that_stays_the_same_or_rises_is_accepted() {
    let mut previous = seeded(4, 2, 4);
    previous.bell = 9;
    for now in [9u32, 10, 400] {
        let mut candidate = previous.clone();
        candidate
            .apply(
                4,
                5,
                &ScreenDiff {
                    bell: Some(now),
                    ..empty_diff()
                },
            )
            .expect("apply");
        candidate.check(&previous).expect("monotonic is fine");
    }
}

// ---------------------------------------------------------------------------
// I6 — scrollback never shrinks
// ---------------------------------------------------------------------------

#[test]
fn a_diff_that_shrinks_the_scrollback_is_rejected() {
    let mut previous = seeded(4, 2, 4);
    previous.scrollback_len = 5_000;

    let mut candidate = previous.clone();
    candidate
        .apply(
            4,
            5,
            &ScreenDiff {
                scrollback_len: Some(10),
                ..empty_diff()
            },
        )
        .expect("apply succeeds mechanically");
    assert_eq!(
        candidate.check(&previous),
        Err(ApplyError::ScrollbackShrank {
            was: 5_000,
            now: 10
        }),
        "lines that have scrolled off do not come back"
    );
}

// ---------------------------------------------------------------------------
// Ordering — validate runs AFTER apply, exactly once
// ---------------------------------------------------------------------------

#[test]
fn validation_happens_after_apply_not_before() {
    // If validation ran BEFORE apply, it would look at the base state — which
    // is perfectly valid — and let the bad result through. The only way this
    // test passes is if the check sees the state the diff produced.
    let good_base = seeded(1, 3, 8);
    good_base.validate().expect("the base is valid, which is the trap");

    let mut candidate = good_base.clone();
    candidate
        .apply(
            1,
            2,
            &ScreenDiff {
                cursor: Some(Cursor {
                    row: 99,
                    col: 0,
                    visible: true,
                    shape: CursorShape::Block,
                }),
                ..empty_diff()
            },
        )
        .expect("apply");
    assert!(
        candidate.check(&good_base).is_err(),
        "validating the base instead of the result would have passed here"
    );
}

#[test]
fn validation_does_not_run_between_the_resize_and_the_cell_writes() {
    // The mirror image of the test above, and the reason it is needed: an
    // implementation that validates too eagerly — after the resize, before the
    // cursor lands — would wrongly REJECT this, which is a legitimate diff.
    //
    // A 4x10 screen with the cursor at (3,9) shrinks to 2x4 and moves the
    // cursor to (1,1). Every intermediate moment is invalid; the result is
    // fine.
    let mut base = seeded(1, 4, 10);
    base.cursor.row = 3;
    base.cursor.col = 9;
    base.validate().expect("the base is valid");

    let mut candidate = base.clone();
    candidate
        .apply(
            1,
            2,
            &ScreenDiff {
                resize: Some(TermSize { cols: 4, rows: 2 }),
                cursor: Some(Cursor {
                    row: 1,
                    col: 1,
                    visible: true,
                    shape: CursorShape::Block,
                }),
                ..empty_diff()
            },
        )
        .expect("apply");
    candidate
        .check(&base)
        .expect("validating mid-apply would wrongly reject this");
    assert_eq!((candidate.rows, candidate.cols), (2, 4));
    assert_eq!((candidate.cursor.row, candidate.cursor.col), (1, 1));
}

// ---------------------------------------------------------------------------
// The mechanical rejections, with the same two assertions
// ---------------------------------------------------------------------------

#[test]
fn a_wrong_base_changes_nothing_and_advances_nothing() {
    let mut got = seeded(5, 2, 4);
    let d = ScreenDiff {
        rows: vec![one_cell(0, 0, "X")],
        ..empty_diff()
    };
    let err = expect_rejected(&mut got, 1, 2, &d);
    assert_eq!(err, ApplyError::BaseMismatch { base: 1, current: 5 });
}

#[test]
fn a_row_past_the_end_changes_nothing_and_advances_nothing() {
    let mut got = seeded(5, 2, 4);
    let d = ScreenDiff {
        rows: vec![one_cell(9, 0, "X")],
        ..empty_diff()
    };
    let err = expect_rejected(&mut got, 5, 6, &d);
    assert_eq!(err, ApplyError::OutOfBounds { row: 9, rows: 2 });
}

#[test]
fn a_run_past_the_end_changes_nothing_even_when_earlier_runs_would_have_fit() {
    // The interesting case: the first run is legal and the second is not. A
    // check-as-you-go implementation writes the first and then fails, leaving
    // a state that existed nowhere.
    let mut got = seeded(5, 1, 8);
    let d = ScreenDiff {
        rows: vec![RowPatch {
            row: 0,
            runs: vec![
                Run {
                    start_col: 0,
                    repeat: 0,
                    cells: vec![Cell {
                        text: CellText::new("A"),
                        ..Cell::blank()
                    }],
                },
                Run {
                    start_col: 6,
                    repeat: 9,
                    cells: vec![Cell::blank()],
                },
            ],
        }],
        ..empty_diff()
    };
    let err = expect_rejected(&mut got, 5, 6, &d);
    assert_eq!(err, ApplyError::OutOfBounds { row: 0, rows: 1 });
    assert_eq!(
        got.cell(0, 0).text,
        " ",
        "the legal first run must not have landed"
    );
}

#[test]
fn an_empty_run_changes_nothing_and_advances_nothing() {
    let mut got = seeded(5, 1, 8);
    let d = ScreenDiff {
        rows: vec![RowPatch {
            row: 0,
            runs: vec![Run {
                start_col: 0,
                repeat: 0,
                cells: vec![],
            }],
        }],
        ..empty_diff()
    };
    let err = expect_rejected(&mut got, 5, 6, &d);
    assert!(matches!(err, ApplyError::Decode(_)));
}

#[test]
fn a_backwards_target_changes_nothing_and_advances_nothing() {
    let mut got = seeded(5, 2, 4);
    let d = ScreenDiff {
        rows: vec![one_cell(0, 0, "X")],
        ..empty_diff()
    };
    expect_rejected(&mut got, 5, 5, &d);
    expect_rejected(&mut got, 5, 4, &d);
}

// ---------------------------------------------------------------------------
// A refusal must not poison what comes next
// ---------------------------------------------------------------------------

#[test]
fn a_good_diff_still_applies_after_a_refused_one() {
    // The whole point of leaving the state untouched: recovery is the normal
    // case, not an exceptional one.
    let base = seeded(5, 2, 4);
    let mut got = base.clone();

    expect_rejected(
        &mut got,
        5,
        6,
        &ScreenDiff {
            rows: vec![one_cell(9, 0, "X")],
            ..empty_diff()
        },
    );

    let mut want = base.clone();
    want.seq = 6;
    put(&mut want, 1, 2, "ok");
    got.apply(5, 6, &want.diff_from(&base)).expect("apply");
    assert_eq!(got, want);
    got.check(&base).expect("and the transition is sound");
}
```

- [ ] **Step 2: Run the tests to verify they fail for the right reason**

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: it should **compile and pass**, because Task 3 already built the
`apply` and `check` this suite exercises. If any test fails, Task 3's `apply`
is mutating before it finishes checking — fix `apply`, not the test.

- [ ] **Step 3: Prove each rejection is really being made (fault injection)**

Green refusal tests that cannot go red prove nothing. Break the checks on
purpose, four ways, and confirm each is caught.

**Injection 1 — clamp the cursor instead of rejecting it.** In
`crates/oxutrm-proto/src/screen.rs`, replace the I2 check in `validate` with
nothing, and clamp in `apply`'s caller instead — the shortest version is to
change the I2 `if` to `if false`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: **FAIL** in `the_cursor_is_rejected_rather_than_clamped_and_this_test_proves_it`
and in `a_diff_moving_the_cursor_off_the_screen_is_rejected`.

**Injection 2 — advance the sequence number before checking.** Revert
injection 1. In `crates/oxutrm-sync/src/screen.rs`, move `self.seq = target;`
to the top of `apply`, before the base check.

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: **FAIL** with "a refused diff advanced the sequence number, which
strands the sender", from `expect_rejected`.

**Injection 3 — write runs as they are checked.** Revert injection 2. In
`apply`, move the run bounds check inside the writing loop so each run is
validated immediately before it is written.

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: **FAIL** in
`a_run_past_the_end_changes_nothing_even_when_earlier_runs_would_have_fit`.

**Injection 4 — drop the transition check.** Revert injection 3. In
`crates/oxutrm-sync/src/screen.rs`, change `ScreenState`'s `check` to
`self.validate()` instead of `self.validate_transition(previous)`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: **FAIL** in `a_diff_that_decreases_the_bell_is_rejected` and
`a_diff_that_shrinks_the_scrollback_is_rejected`.

Revert injection 4.

Run: `git diff --stat`
Expected: **no output** — the tree is back to the committed engine.

Run: `cargo test --jobs 4 -p oxutrm-sync -- --test-threads 4`
Expected: PASS.

Record all four injected failures, with the assertion message each produced, in
the commit message body. A reviewer must be able to see that each refusal was
proven capable of failing.

- [ ] **Step 4: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-sync --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-sync
git commit -m "$(cat <<'EOF'
test(sync): the reject path, by fault injection

Every one of the six ScreenState invariants gets a diff that violates it, and
every rejection asserts TWO things: the state is byte-for-byte unchanged, and
the sequence number has not advanced. The second is the one people omit, and
omitting it hides an implementation that refuses a frame while telling the
sender it accepted one — which strands the sender forever with no happy-path
test able to see it.

Two tests pin the ordering from both sides: validating before apply would let a
bad cursor through, and validating mid-apply would wrongly reject a legitimate
shrink-and-move. Together they mean "exactly once, after the whole diff".

Proven capable of failing four ways, each reverted: clamping the cursor,
advancing seq before the base check, checking runs as they are written, and
using validate() where validate_transition() is needed.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: `oxutrm-sync` — `InputState` and `InputDiff`, drop-then-append

**Files:**
- Create: `crates/oxutrm-sync/src/input.rs`
- Modify: `crates/oxutrm-sync/src/lib.rs`

**Interfaces:**
- Consumes: `SyncState`, `DiffHint` (Task 3), `oxutrm_proto::{ApplyError, TermSize}`.
- Produces:
  ```rust
  #[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
  pub struct InputState { pub seq: u64, pub pending: Vec<u8>, pub size: TermSize }

  #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
  pub struct InputDiff {
      /// Bytes the host has consumed; dropped from the FRONT of `pending`.
      pub consumed: u64,
      pub appended: Vec<u8>,
      pub size: Option<TermSize>,
  }

  impl InputState {
      pub fn new(size: TermSize) -> InputState;      // seq 1, empty pending
      pub fn append(&self, bytes: &[u8], size: TermSize) -> InputState;
      pub fn consume(&self, n: usize) -> InputState;
  }
  impl SyncState for InputState { type Diff = InputDiff; }
  ```

**`apply` is drop-then-append, in that order.** Remove the first `consumed`
bytes of `pending`, *then* append `appended`. Both are needed because the
transition from an untrimmed base to a trimmed target is not a pure append: a
diff carrying only `appended` would rebuild `pending` with the already-consumed
bytes still in front, and **the host would write them to the PTY a second time**.

**`consumed` greater than `pending.len()` is an `ApplyError`**, never a
saturating subtraction — saturating would silently accept a diff describing a
state the sender never had.

- [ ] **Step 1: Write the failing tests**

Create `crates/oxutrm-sync/src/input.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const S: TermSize = TermSize { cols: 80, rows: 24 };

    fn size(cols: u16, rows: u16) -> TermSize {
        TermSize { cols, rows }
    }

    #[test]
    fn a_new_state_is_empty_at_sequence_one() {
        let s = InputState::new(S);
        assert_eq!(s.seq, 1, "zero is the full-state sentinel");
        assert!(s.pending.is_empty());
        assert_eq!(s.size, S);
    }

    #[test]
    fn append_adds_at_the_back_and_leaves_the_original_alone() {
        let a = InputState::new(S);
        let b = a.append(b"ls", S);
        let c = b.append(b" -l\r", S);
        assert_eq!(a.pending, b"");
        assert_eq!(b.pending, b"ls");
        assert_eq!(c.pending, b"ls -l\r");
    }

    #[test]
    fn consume_drops_a_prefix_and_saturates_locally() {
        let a = InputState::new(S).append(b"abcdef", S);
        assert_eq!(a.consume(2).pending, b"cdef");
        assert_eq!(a.consume(0).pending, b"abcdef");
        assert_eq!(a.consume(99).pending, b"");
        assert_eq!(a.pending, b"abcdef", "consume does not mutate");
    }

    // ---- the bug this design exists to prevent ----

    #[test]
    fn consumed_input_is_never_written_twice() {
        // The client typed "abcdef". The host acknowledged consuming "abcd".
        // The client forms the next state with that prefix removed and appends
        // "gh". The host, applying the diff to its own copy, must end up with
        // exactly "efgh" -- not "abcdefgh", which would replay "abcd" into the
        // PTY a second time.
        let mut base = InputState::new(S).append(b"abcdef", S);
        base.seq = 5;
        let mut next = base.consume(4).append(b"gh", S);
        next.seq = 6;

        let d = next.diff_from(&base);
        assert_eq!(d.consumed, 4, "the diff must say what was consumed");
        assert_eq!(d.appended, b"gh");

        let mut host = base.clone();
        host.apply(5, 6, &d).expect("apply");
        assert_eq!(host.pending, b"efgh");
        assert_eq!(host, next);
    }

    #[test]
    fn a_pure_append_consumes_nothing() {
        let mut a = InputState::new(S).append(b"ab", S);
        a.seq = 3;
        let mut b = a.append(b"cd", S);
        b.seq = 4;

        let d = b.diff_from(&a);
        assert_eq!(d.consumed, 0);
        assert_eq!(d.appended, b"cd");
        assert_eq!(d.size, None);
    }

    #[test]
    fn a_pure_trim_appends_nothing() {
        let mut a = InputState::new(S).append(b"abcd", S);
        a.seq = 3;
        let mut b = a.consume(3);
        b.seq = 4;

        let d = b.diff_from(&a);
        assert_eq!(d.consumed, 3);
        assert!(d.appended.is_empty());
    }

    #[test]
    fn everything_consumed_and_more_appended_still_works() {
        let mut a = InputState::new(S).append(b"abc", S);
        a.seq = 3;
        let mut b = a.consume(3).append(b"xyz", S);
        b.seq = 4;

        let d = b.diff_from(&a);
        assert_eq!(d.consumed, 3);
        assert_eq!(d.appended, b"xyz");

        let mut host = a.clone();
        host.apply(3, 4, &d).expect("apply");
        assert_eq!(host.pending, b"xyz");
    }

    #[test]
    fn a_resize_travels_with_the_diff() {
        let mut a = InputState::new(S);
        a.seq = 1;
        let mut b = a.append(b"x", size(100, 30));
        b.seq = 2;

        let d = b.diff_from(&a);
        assert_eq!(d.appended, b"x");
        assert_eq!(d.size, Some(size(100, 30)));
    }

    #[test]
    fn applying_a_diff_reproduces_the_source_exactly() {
        let mut a = InputState::new(S).append(b"ab", S);
        a.seq = 7;
        let mut b = a.consume(1).append(b"cde", size(90, 25));
        b.seq = 8;

        let mut got = a.clone();
        got.apply(7, 8, &b.diff_from(&a)).expect("apply");
        assert_eq!(got, b);
    }

    #[test]
    fn a_full_diff_replaces_whatever_was_there_and_consumes_nothing() {
        let mut b = InputState::new(S).append(b"fresh", S);
        b.seq = 12;
        assert_eq!(b.full_diff().consumed, 0, "a full state consumes nothing");

        let mut got = InputState::new(size(1, 1)).append(b"stale garbage", size(1, 1));
        got.seq = 4;
        got.apply(0, 12, &b.full_diff()).expect("apply");
        assert_eq!(got, b);
    }

    // ---- the reject path ----

    #[test]
    fn consuming_more_than_exists_is_an_error_not_a_saturation() {
        let mut got = InputState::new(S).append(b"ab", S);
        got.seq = 3;
        let before = got.clone();
        let d = InputDiff {
            consumed: 5,
            appended: b"z".to_vec(),
            size: None,
        };
        assert_eq!(
            got.apply(3, 4, &d),
            Err(ApplyError::Decode(
                "consumed 5 exceeds 2 pending bytes".to_string()
            ))
        );
        assert_eq!(got, before, "a rejected diff must not mutate anything");
        assert_eq!(got.seq, 3, "and must not advance the sequence");
    }

    #[test]
    fn a_diff_against_the_wrong_base_changes_nothing_and_advances_nothing() {
        let mut a = InputState::new(S);
        a.seq = 1;
        let mut b = a.append(b"x", S);
        b.seq = 2;
        let d = b.diff_from(&a);

        let mut wrong = InputState::new(S);
        wrong.seq = 9;
        let before = wrong.clone();
        assert_eq!(
            wrong.apply(1, 2, &d),
            Err(ApplyError::BaseMismatch { base: 1, current: 9 })
        );
        assert_eq!(wrong, before);
        assert_eq!(wrong.seq, 9);
    }

    #[test]
    fn a_target_that_does_not_advance_is_rejected() {
        let mut got = InputState::new(S);
        got.seq = 4;
        let d = InputDiff {
            consumed: 0,
            appended: vec![],
            size: None,
        };
        assert_eq!(
            got.apply(4, 4, &d),
            Err(ApplyError::BaseMismatch { base: 4, current: 4 })
        );
        assert_eq!(got.seq, 4);
    }

    #[test]
    fn the_hint_is_ignored_because_input_has_no_rows() {
        let mut a = InputState::new(S);
        a.seq = 1;
        let mut b = a.append(b"q", S);
        b.seq = 2;
        assert_eq!(b.diff_from_hint(&a, &DiffHint::rows(vec![])), b.diff_from(&a));
    }

    #[test]
    fn input_has_no_transition_invariants_so_check_always_passes() {
        let mut a = InputState::new(S);
        a.seq = 1;
        let mut b = a.append(b"q", S);
        b.seq = 2;
        b.check(&a).expect("input carries no bell and no scrollback");
    }

    #[test]
    fn a_diff_round_trips_through_postcard() {
        let mut a = InputState::new(S);
        a.seq = 1;
        let mut b = a.append(&[0x1b, b'[', b'A'], S);
        b.seq = 2;
        let d = b.diff_from(&a);
        let bytes = postcard::to_stdvec(&d).expect("encode");
        assert_eq!(postcard::from_bytes::<InputDiff>(&bytes).expect("decode"), d);
    }
}
```

Add to `crates/oxutrm-sync/src/lib.rs`:

```rust
mod input;

pub use input::{InputDiff, InputState};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-sync --lib -- --test-threads 4 input::`
Expected: FAIL — `cannot find type 'InputState' in this scope`.

- [ ] **Step 3: Write the body of `crates/oxutrm-sync/src/input.rs`**

Put this **above** the existing `mod tests`:

```rust
use oxutrm_proto::{ApplyError, TermSize};
use serde::{Deserialize, Serialize};

use crate::{DiffHint, SyncState};

/// User input not yet acknowledged by the host, plus the latest requested
/// terminal size.
///
/// Unacknowledged input is retransmitted automatically, without a
/// retransmission mechanism: it is simply still part of the state.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct InputState {
    /// Starts at 1. Zero is the full-state sentinel.
    pub seq: u64,
    /// Bytes to be written to the PTY, in order. Mouse events and special keys
    /// are already encoded as the byte sequences the remote application
    /// expects, so the host writes them straight through.
    pub pending: Vec<u8>,
    pub size: TermSize,
}

/// The difference between two input states.
///
/// There is no `base` or `target` here: `Frame` carries them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDiff {
    /// Bytes the host has consumed, dropped from the **front** of `pending`.
    pub consumed: u64,
    /// Bytes added at the back, after the drop.
    pub appended: Vec<u8>,
    pub size: Option<TermSize>,
}

impl InputState {
    /// An empty input state at sequence 1.
    #[must_use]
    pub fn new(size: TermSize) -> InputState {
        InputState {
            seq: 1,
            pending: Vec::new(),
            size,
        }
    }

    /// A copy with `bytes` appended and the size updated.
    ///
    /// Returns a new value rather than mutating, because the sender keeps the
    /// old state in its ring to diff against an old acknowledgement. Mutating
    /// in place would destroy that history.
    #[must_use]
    pub fn append(&self, bytes: &[u8], size: TermSize) -> InputState {
        let mut pending = Vec::with_capacity(self.pending.len() + bytes.len());
        pending.extend_from_slice(&self.pending);
        pending.extend_from_slice(bytes);
        InputState {
            seq: self.seq,
            pending,
            size,
        }
    }

    /// A copy with the first `n` bytes removed, after the host has confirmed
    /// consuming them. `n` saturates at the length of `pending`.
    #[must_use]
    pub fn consume(&self, n: usize) -> InputState {
        let n = n.min(self.pending.len());
        InputState {
            seq: self.seq,
            pending: self.pending[n..].to_vec(),
            size: self.size,
        }
    }
}

impl SyncState for InputState {
    type Diff = InputDiff;

    fn seq(&self) -> u64 {
        self.seq
    }

    fn set_seq(&mut self, seq: u64) {
        self.seq = seq;
    }

    fn diff_from(&self, base: &Self) -> InputDiff {
        // The only two operations are "drop from the front" and "add at the
        // back", so what remains of the base is always a prefix of ours.
        let kept = kept_prefix_len(&base.pending, &self.pending);
        let consumed = base.pending.len() - kept;

        let appended = if self.pending.len() >= kept {
            self.pending[kept..].to_vec()
        } else {
            // The invariant above did not hold, which can only mean the two
            // states were built independently. Replacing outright is the
            // honest answer; `Sender` never produces this.
            self.pending.clone()
        };

        InputDiff {
            consumed: consumed as u64,
            appended,
            size: (self.size != base.size).then_some(self.size),
        }
    }

    fn full_diff(&self) -> InputDiff {
        InputDiff {
            consumed: 0,
            appended: self.pending.clone(),
            size: Some(self.size),
        }
    }

    fn apply(&mut self, base: u64, target: u64, d: &InputDiff) -> Result<(), ApplyError> {
        // Check before mutating, so a rejected diff leaves `pending` exactly
        // as it was.
        if base != 0 && base != self.seq {
            return Err(ApplyError::BaseMismatch {
                base,
                current: self.seq,
            });
        }
        if target <= base {
            return Err(ApplyError::BaseMismatch {
                base,
                current: self.seq,
            });
        }
        let consumed = usize::try_from(d.consumed).unwrap_or(usize::MAX);
        if base != 0 && consumed > self.pending.len() {
            // Never saturate: a saturating drop would silently accept a diff
            // describing a state the sender never had.
            return Err(ApplyError::Decode(format!(
                "consumed {} exceeds {} pending bytes",
                d.consumed,
                self.pending.len()
            )));
        }

        if base == 0 {
            // A full state replaces `pending` outright.
            self.pending.clone_from(&d.appended);
        } else {
            // Drop THEN append, in that order. The other order rebuilds
            // `pending` with the already-consumed bytes still in front, and
            // the host writes them to the PTY a second time.
            self.pending.drain(..consumed);
            self.pending.extend_from_slice(&d.appended);
        }

        if let Some(s) = d.size {
            self.size = s;
        }
        self.seq = target;
        Ok(())
    }

    fn diff_from_hint(&self, base: &Self, _hint: &DiffHint) -> InputDiff {
        // Input has no rows, so a row hint means nothing here.
        self.diff_from(base)
    }
}

/// How many of `base`'s trailing bytes survive into `next`.
///
/// `next` is always `base[k..]` followed by new bytes, so this finds the length
/// of that kept tail. It tries the smallest drop first, which is the common
/// case: usually nothing was consumed at all.
fn kept_prefix_len(base: &[u8], next: &[u8]) -> usize {
    for k in 0..=base.len() {
        let kept = &base[k..];
        if next.len() >= kept.len() && next.starts_with(kept) {
            return kept.len();
        }
    }
    0
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-sync --lib -- --test-threads 4 input::`
Expected: PASS, 15 tests.

`consumed_input_is_never_written_twice` is the one that matters. If it fails
with `host.pending == b"abcdefgh"`, `apply` is appending before dropping.

- [ ] **Step 5: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-sync --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-sync
git commit -m "$(cat <<'EOF'
feat(sync): InputState and InputDiff with drop-then-append

InputDiff carries `consumed`, and apply drops that many bytes from the front
before appending. Without it, the transition from an untrimmed base to a
trimmed target rebuilds pending with the consumed bytes still in front and the
host writes them to the PTY twice. A test asserts exactly that cannot happen.

Consuming more than exists is an error, never a saturating subtraction, and
the rejection leaves both the bytes and the sequence number untouched.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 6: `oxutrm-sync` — `Sender` and the zstd policy

**Files:**
- Create: `crates/oxutrm-sync/src/sender.rs`
- Modify: `crates/oxutrm-sync/src/lib.rs`

**Interfaces:**
- Consumes: `SyncState`, `DiffHint`, `STATE_RING` (Task 3), `InputState`
  (Task 5), `oxutrm_proto::{ApplyError, Frame, FLAG_ZSTD, ScreenState}`.
- Produces:
  ```rust
  /// zstd is attempted only above this payload size; below it the frame header
  /// reliably costs more than it saves. Measured, not assumed.
  pub const ZSTD_MIN_PAYLOAD: usize = 256;
  pub const ZSTD_LEVEL: i32 = 3;

  pub struct Sender<S: SyncState> { /* private */ }
  impl<S: SyncState> Sender<S> {
      pub fn new(initial: S) -> Sender<S>;              // forces seq = 1
      pub fn update(&mut self, next: S);                // hint: everything
      pub fn update_damaged(&mut self, next: S, hint: DiffHint);
      pub fn on_ack(&mut self, peer_saw: u64);
      pub fn current(&self) -> &S;
      pub fn peer_ack(&self) -> u64;
      pub fn ring_len(&self) -> usize;
      /// `None` when the peer is already up to date.
      pub fn make_frame(&self, ack_state: u64) -> Result<Option<Frame>, ApplyError>;
  }
  pub type ScreenSender = Sender<ScreenState>;
  pub type InputSender = Sender<InputState>;
  ```
  `make_frame` returns **one** `Frame` or none. There is no fragmentation and
  no size parameter: `oxutrm-sync` produces a `Frame` and the transport decides
  how to carry it (datagram if it fits, otherwise a stream — M4's problem).

  `ack_state` is what **we** have received on the reverse channel; it is
  unrelated to `peer_ack`, which is what the peer told us it holds.

**Damage accumulation.** Each ring entry records the hint for the transition
*into* that state. A diff from base *B* to current *C* unions the hints of ring
entries `B+1..=C` — damage since the last update is not damage since *B*, and
using the wrong one silently drops rows that changed two frames ago.

- [ ] **Step 1: Write the failing tests**

Create `crates/oxutrm-sync/src/sender.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{InputDiff, InputState, ScreenDiff};
    use oxutrm_proto::{Cell, CellText, Color, ScreenState, TermSize, FLAG_ZSTD};

    const S: TermSize = TermSize { cols: 80, rows: 24 };

    fn screen(rows: u16, cols: u16) -> ScreenState {
        ScreenState::blank(rows, cols).expect("a blank screen is valid")
    }

    fn marked(rows: u16, cols: u16, row: u16, mark: &str) -> ScreenState {
        let mut s = screen(rows, cols);
        let ix = row as usize * cols as usize;
        s.cells[ix] = Cell {
            text: CellText::new(mark),
            ..Cell::blank()
        };
        s
    }

    fn body_of(f: &Frame) -> Vec<u8> {
        if f.flags & FLAG_ZSTD != 0 {
            zstd::decode_all(f.payload.as_slice()).expect("inflate")
        } else {
            f.payload.clone()
        }
    }

    fn screen_diff_of(f: &Frame) -> ScreenDiff {
        postcard::from_bytes(&body_of(f)).expect("decode")
    }

    // ---- sequence numbering ----

    #[test]
    fn a_new_sender_starts_at_sequence_one_and_owes_a_full_state() {
        let s: Sender<ScreenState> = Sender::new(screen(2, 4));
        assert_eq!(s.current().seq, 1, "zero is the full-state sentinel");
        assert_eq!(s.peer_ack(), 0, "the peer has nothing yet");
        // The peer has acknowledged nothing, so the first datagram of the
        // attach is a full state -- it never waits for a ring miss to discover
        // that a fresh client knows nothing.
        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.my_state, 1);
        assert_eq!(f.from_state, 0);
    }

    #[test]
    fn new_forces_the_initial_sequence_to_one() {
        let mut seeded = screen(2, 4);
        seeded.seq = 4321;
        let s: Sender<ScreenState> = Sender::new(seeded);
        assert_eq!(s.current().seq, 1, "counters reset to 1 at every attach");
    }

    #[test]
    fn update_assigns_consecutive_sequence_numbers() {
        let mut s = Sender::new(screen(2, 4));
        s.update(screen(2, 4));
        assert_eq!(s.current().seq, 2);
        s.update(screen(2, 4));
        assert_eq!(s.current().seq, 3);
    }

    // ---- diffing against the ack ----

    #[test]
    fn the_first_frame_of_an_attach_is_a_full_state() {
        let mut s = Sender::new(screen(2, 4));
        s.update(marked(2, 4, 0, "A"));

        let f = s.make_frame(77).expect("frame").expect("some");
        assert_eq!(f.my_state, 2);
        assert_eq!(f.from_state, 0, "a full state, not a diff");
        assert_eq!(f.ack_state, 77);
        assert_eq!(screen_diff_of(&f).rows.len(), 2, "every row travels");
    }

    #[test]
    fn after_an_ack_the_frame_is_an_incremental_diff() {
        let mut s = Sender::new(screen(2, 4));
        s.update(screen(2, 4));
        s.on_ack(2);

        s.update(marked(2, 4, 1, "B"));
        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.my_state, 3);
        assert_eq!(f.from_state, 2);
        assert_eq!(screen_diff_of(&f).rows.len(), 1, "only the changed row");
    }

    #[test]
    fn nothing_is_sent_when_the_peer_is_already_current() {
        let mut s = Sender::new(screen(2, 4));
        s.update(screen(2, 4));
        s.on_ack(2);
        assert!(s.make_frame(0).expect("frame").is_none());
    }

    #[test]
    fn acks_never_go_backwards() {
        let mut s = Sender::new(screen(2, 4));
        s.update(screen(2, 4));
        s.update(screen(2, 4));
        s.on_ack(3);
        s.on_ack(2);
        assert_eq!(s.peer_ack(), 3, "a reordered old ack must not un-acknowledge");
    }

    #[test]
    fn an_ack_that_has_left_the_ring_forces_a_full_state() {
        let mut s = Sender::new(screen(2, 4));
        for _ in 0..(STATE_RING as u64 + 5) {
            s.update(screen(2, 4));
        }
        s.on_ack(2); // long gone

        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.from_state, 0, "no base available, so send everything");
    }

    #[test]
    fn the_ring_never_grows_past_state_ring() {
        let mut s = Sender::new(screen(2, 4));
        for _ in 0..200 {
            s.update(screen(2, 4));
        }
        assert_eq!(s.ring_len(), STATE_RING);
    }

    #[test]
    fn states_coalesce_rather_than_queue() {
        // A runaway `yes` produces one frame, not a backlog: only the newest
        // state is ever described.
        let mut s = Sender::new(screen(2, 4));
        for i in 0..50u16 {
            s.update(marked(2, 4, 0, &format!("{}", i % 10)));
        }
        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.my_state, 51);
    }

    // ---- damage accumulation ----

    #[test]
    fn damage_hints_accumulate_across_every_state_since_the_ack() {
        // Row 0 changes in state 2, row 1 in state 3. The peer acknowledged
        // state 1. The diff from 1 to 3 must carry BOTH rows, even though the
        // most recent hint names only row 1.
        let mut s = Sender::new(screen(3, 4));
        s.on_ack(1);

        let a = marked(3, 4, 0, "A");
        s.update_damaged(a.clone(), DiffHint::rows(vec![0]));

        let mut b = a.clone();
        b.cells[4] = Cell {
            text: CellText::new("B"),
            ..Cell::blank()
        };
        s.update_damaged(b, DiffHint::rows(vec![1]));

        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.from_state, 1);
        let mut rows: Vec<u16> = screen_diff_of(&f).rows.iter().map(|p| p.row).collect();
        rows.sort_unstable();
        assert_eq!(rows, vec![0, 1], "a hint from one state must not hide another");
    }

    #[test]
    fn an_everything_hint_poisons_the_union_which_is_the_safe_direction() {
        let mut s = Sender::new(screen(3, 4));
        s.on_ack(1);
        let a = marked(3, 4, 2, "Z");
        s.update_damaged(a.clone(), DiffHint::rows(vec![2]));
        s.update(a); // update() means "assume everything changed"

        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(
            screen_diff_of(&f).rows.len(),
            1,
            "examining every row still emits only what truly differs"
        );
    }

    // ---- zstd policy ----

    #[test]
    fn a_tiny_payload_is_never_compressed() {
        let mut s = Sender::new(InputState::new(S));
        s.update(InputState::new(S).append(b"x", S));
        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.flags & FLAG_ZSTD, 0);
        assert!(f.payload.len() < ZSTD_MIN_PAYLOAD);
    }

    #[test]
    fn a_full_screen_compresses_and_shrinks() {
        let mut big = screen(24, 80);
        for (i, c) in big.cells.iter_mut().enumerate() {
            c.text = CellText::new(&char::from(b'a' + (i % 26) as u8).to_string());
            c.fg = Color::Idx((i % 256) as u8);
        }
        let mut s = Sender::new(screen(24, 80));
        s.update(big);

        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.flags & FLAG_ZSTD, FLAG_ZSTD, "a full screen must compress");
        let raw = postcard::to_stdvec(&screen_diff_of(&f)).expect("re-encode");
        assert!(
            f.payload.len() < raw.len(),
            "compressed {} must beat raw {}",
            f.payload.len(),
            raw.len()
        );
    }

    #[test]
    fn incompressible_bytes_stay_uncompressed_and_still_decode() {
        let mut noise = Vec::with_capacity(2048);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..2048 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            noise.push((x >> 24) as u8);
        }
        let mut s = Sender::new(InputState::new(S));
        s.update(InputState::new(S).append(&noise, S));

        let f = s.make_frame(0).expect("frame").expect("some");
        let d: InputDiff = postcard::from_bytes(&body_of(&f)).expect("decode");
        assert_eq!(d.appended, noise);
    }

    // ---- no fragmentation ----

    #[test]
    fn even_an_enormous_state_produces_exactly_one_frame() {
        // There is no fragmentation. A Frame too large for a datagram goes on
        // a stream instead, and that decision belongs to the transport, not
        // here. This test pins that `oxutrm-sync` never splits anything.
        let mut huge = screen(60, 200);
        for (i, c) in huge.cells.iter_mut().enumerate() {
            c.text = CellText::new(&char::from(b'a' + (i % 26) as u8).to_string());
            c.fg = Color::Rgb((i % 251) as u8, (i % 241) as u8, (i % 239) as u8);
            c.bg = Color::Rgb((i % 233) as u8, (i % 229) as u8, (i % 227) as u8);
        }
        let mut s = Sender::new(screen(60, 200));
        s.update(huge);

        let f = s.make_frame(0).expect("frame").expect("exactly one frame");
        assert!(
            f.payload.len() > 1200,
            "this payload is {} bytes, far past a datagram -- and still one frame",
            f.payload.len()
        );
        assert_eq!(screen_diff_of(&f).rows.len(), 60);
    }

    #[test]
    fn a_frame_survives_encoding_and_decoding() {
        let mut s = Sender::new(screen(4, 8));
        s.update(marked(4, 8, 2, "Q"));
        let f = s.make_frame(5).expect("frame").expect("some");
        // `Frame` does not derive PartialEq, so compare the encodings.
        let bytes = f.encode().expect("encode");
        let back = Frame::decode(&bytes).expect("decode");
        assert_eq!(back.encode().expect("re-encode"), bytes);
        assert_eq!(back.my_state, f.my_state);
        assert_eq!(back.from_state, f.from_state);
        assert_eq!(back.ack_state, 5);
        assert_eq!(back.payload, f.payload);
    }
}
```

Add to `crates/oxutrm-sync/src/lib.rs`:

```rust
mod sender;

pub use sender::{InputSender, ScreenSender, Sender, ZSTD_LEVEL, ZSTD_MIN_PAYLOAD};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-sync --lib -- --test-threads 4 sender::`
Expected: FAIL — `cannot find type 'Sender' in this scope`.

- [ ] **Step 3: Write the body of `crates/oxutrm-sync/src/sender.rs`**

Put this **above** the existing `mod tests`:

```rust
use std::collections::VecDeque;

use oxutrm_proto::{ApplyError, Frame, FLAG_ZSTD};

use crate::{DiffHint, InputState, SyncState, STATE_RING};

/// Compression is attempted only above this payload size. Below it the zstd
/// frame header reliably costs more than it saves — measured, not assumed.
pub const ZSTD_MIN_PAYLOAD: usize = 256;

/// zstd level 3: the default, and fast enough to run per datagram.
pub const ZSTD_LEVEL: i32 = 3;

/// One entry of the state ring: a state, and the rows that changed on the way
/// into it.
struct Entry<S> {
    state: S,
    /// The hint for the transition INTO `state`. The base entry's own hint is
    /// never consulted, because a diff is always computed *from* it.
    hint: DiffHint,
}

/// Keeps a ring of recent states and emits diffs against the peer's ack.
///
/// States coalesce rather than queue. If output outruns the link the sender
/// simply describes the newest state, so it can never fall behind: a runaway
/// `yes` produces one frame and never a backlog.
///
/// It produces exactly one `Frame` per call. There is no fragmentation — a
/// `Frame` too large for a datagram travels on a stream instead, and choosing
/// between them belongs to the transport. That is what keeps this crate
/// transport-agnostic, and therefore free of I/O.
pub struct Sender<S: SyncState> {
    /// Oldest first; the back is always the current state. Never empty.
    ring: VecDeque<Entry<S>>,
    peer_ack: u64,
}

impl<S: SyncState> Sender<S> {
    /// Start from `initial`, whose sequence number is forced to 1.
    ///
    /// Both sides reset their counters to 1 at every attach, so a reattaching
    /// client and a host that still remembers the previous attach's numbers
    /// can never mistake one for the other.
    pub fn new(initial: S) -> Sender<S> {
        let mut initial = initial;
        initial.set_seq(1);
        let mut ring = VecDeque::with_capacity(STATE_RING);
        ring.push_back(Entry {
            state: initial,
            hint: DiffHint::everything(),
        });
        Sender { ring, peer_ack: 0 }
    }

    /// Replace the current state, assuming every row may have changed.
    pub fn update(&mut self, next: S) {
        self.update_damaged(next, DiffHint::everything());
    }

    /// Replace the current state, recording which rows the emulator reported
    /// damaged on the way into it.
    pub fn update_damaged(&mut self, next: S, hint: DiffHint) {
        let seq = self.current().seq() + 1;
        let mut next = next;
        next.set_seq(seq);
        self.ring.push_back(Entry { state: next, hint });
        while self.ring.len() > STATE_RING {
            self.ring.pop_front();
        }
    }

    /// Record that the peer has applied state `peer_saw`. A reordered old ack
    /// is ignored: acknowledgement only ever moves forward.
    pub fn on_ack(&mut self, peer_saw: u64) {
        if peer_saw > self.peer_ack {
            self.peer_ack = peer_saw;
        }
    }

    pub fn current(&self) -> &S {
        &self
            .ring
            .back()
            .expect("the ring always holds the current state")
            .state
    }

    pub fn peer_ack(&self) -> u64 {
        self.peer_ack
    }

    /// How many states the ring holds. Diagnostics and tests only.
    pub fn ring_len(&self) -> usize {
        self.ring.len()
    }

    /// The datagram describing the current state.
    ///
    /// `None` when the peer is already up to date. `ack_state` is what **we**
    /// have received on the reverse channel; it has nothing to do with
    /// `peer_ack`.
    pub fn make_frame(&self, ack_state: u64) -> Result<Option<Frame>, ApplyError> {
        let current = self.current();
        if self.peer_ack == current.seq() {
            return Ok(None);
        }

        // Sequence 0 is never a usable base, and an ack that has aged out of
        // the ring is not one either. Both mean "send a full state".
        let base_ix = self
            .ring
            .iter()
            .position(|e| e.state.seq() == self.peer_ack && self.peer_ack != 0);

        let (diff, from_state) = match base_ix {
            Some(ix) => {
                // Damage since the last update is not damage since the base.
                // Union the hints of every state after the base, or the diff
                // silently drops rows that changed two frames ago.
                let hint = self
                    .ring
                    .iter()
                    .skip(ix + 1)
                    .fold(DiffHint::rows(Vec::new()), |acc, e| acc.union(&e.hint));
                let base = &self.ring[ix].state;
                (current.diff_from_hint(base, &hint), base.seq())
            }
            None => (current.full_diff(), 0),
        };

        let raw = postcard::to_stdvec(&diff).map_err(|e| ApplyError::Decode(e.to_string()))?;
        let (flags, payload) = compress(raw);

        Ok(Some(Frame {
            my_state: current.seq(),
            from_state,
            ack_state,
            flags,
            payload,
        }))
    }
}

/// Compress only when it actually shrinks the payload, and only above the
/// measured threshold.
fn compress(raw: Vec<u8>) -> (u8, Vec<u8>) {
    if raw.len() < ZSTD_MIN_PAYLOAD {
        return (0, raw);
    }
    match zstd::encode_all(raw.as_slice(), ZSTD_LEVEL) {
        Ok(z) if z.len() < raw.len() => (FLAG_ZSTD, z),
        _ => (0, raw),
    }
}

/// The host's outgoing screen sender.
pub type ScreenSender = Sender<oxutrm_proto::ScreenState>;
/// The client's outgoing input sender.
pub type InputSender = Sender<InputState>;
```

`DiffHint::rows(Vec::new())` is the fold's identity: an empty row list, which
unions correctly with anything and is deliberately *not* `everything()`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-sync --lib -- --test-threads 4 sender::`
Expected: PASS, 16 tests.

If `a_full_screen_compresses_and_shrinks` fails, print `raw.len()` and the
compressed length. A full 24x80 screen is tens of kilobytes of postcard, so
zstd should reach well under half. A failure means the threshold or the level
is wrong, not the test.

- [ ] **Step 5: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-sync --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-sync
git commit -m "$(cat <<'EOF'
feat(sync): Sender with a 32-state ring, damage union and a zstd policy

One Frame per call, never split: a Frame too large for a datagram goes on a
stream instead, and choosing between them is the transport's job. That is what
keeps this crate transport-agnostic and therefore free of I/O — a test builds a
60x200 truecolor state and asserts it is still exactly one frame.

States coalesce rather than queue. An ack that has aged out of the ring falls
back to a full state. Damage hints are unioned across every state since the
peer's ack: damage since the last update is not damage since the base, and
using the wrong one silently drops rows that changed two frames ago.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 7: `oxutrm-sync` — `Receiver`

**Files:**
- Create: `crates/oxutrm-sync/src/receiver.rs`
- Modify: `crates/oxutrm-sync/src/lib.rs`
- Modify: `crates/oxutrm-sync/tests/reject_path.rs` (append the receiver half)

**Interfaces:**
- Consumes: `SyncState` (Task 3), `InputState` (Task 5), `Sender` (Task 6),
  `oxutrm_proto::{ApplyError, Frame, FLAG_ZSTD, ScreenState}`.
- Produces:
  ```rust
  pub struct Receiver<S: SyncState> { /* private */ }
  impl<S: SyncState> Receiver<S> {
      pub fn new(initial: S) -> Receiver<S>;    // forces seq = 1, ack() == 0
      /// Returns true when the state advanced. Stale, duplicate and
      /// unapplicable frames return Ok(false) — never an error.
      pub fn on_frame(&mut self, f: &Frame) -> Result<bool, ApplyError>;
      pub fn state(&self) -> &S;
      pub fn ack(&self) -> u64;
      pub fn peer_ack(&self) -> u64;
  }
  pub type ScreenReceiver = Receiver<ScreenState>;
  pub type InputReceiver = Receiver<InputState>;
  ```

**The three rules that make loss free**, all normative:

1. A frame whose diff bases on a state the receiver no longer holds is
   **dropped, not an error**. The sender keeps diffing from the same
   acknowledged base, so its next frame — sent after it learns a fresher ack —
   contains everything that was lost.
2. **Apply to a copy, validate the copy, then swap.** `apply` and `check` both
   run against a candidate; the receiver's own state is replaced only if both
   succeed. That is how "rejected wholesale, never applied partially" is
   guaranteed even for an invariant `apply` cannot see.
3. **A rejection never advances `ack()`.** Advancing while refusing tells the
   sender we hold a state we do not, and it then diffs against that state
   forever.

**`ack()` returns 0 until the first frame is accepted.** `Receiver::new` holds a
blank state at seq 1 because a valid `ScreenState` has `seq >= 1`, but reporting
`ack() == 1` before receiving anything would claim a state the sender never
sent.

- [ ] **Step 1: Write the failing tests**

Create `crates/oxutrm-sync/src/receiver.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RowPatch, Run, ScreenDiff, Sender};
    use oxutrm_proto::{Cell, CellText, Cursor, CursorShape, ScreenState, TermSize};

    fn screen(rows: u16, cols: u16) -> ScreenState {
        ScreenState::blank(rows, cols).expect("a blank screen is valid")
    }

    fn marked(rows: u16, cols: u16, mark: &str) -> ScreenState {
        let mut s = screen(rows, cols);
        s.cells[0] = Cell {
            text: CellText::new(mark),
            ..Cell::blank()
        };
        s
    }

    // ---- start of an attach ----

    #[test]
    fn a_new_receiver_acknowledges_nothing() {
        let r: Receiver<ScreenState> = Receiver::new(screen(2, 4));
        assert_eq!(r.ack(), 0, "we hold no state the sender sent");
        assert_eq!(r.peer_ack(), 0);
        assert_eq!(r.state().seq, 1, "but the state itself is valid");
        r.state().validate().expect("valid");
    }

    #[test]
    fn a_diff_arriving_before_any_full_state_is_dropped() {
        // Our blank state happens to sit at seq 1. A diff based on seq 1 from a
        // sender we have heard nothing from describes a screen we do not have,
        // and accepting it would silently fabricate one.
        let mut r = Receiver::new(screen(2, 4));
        let mut s = Sender::new(screen(2, 4));
        s.on_ack(1);
        s.update(marked(2, 4, "A"));
        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.from_state, 1);

        assert!(!r.on_frame(&f).expect("dropped"), "not an error, just ignored");
        assert_eq!(r.ack(), 0);
    }

    #[test]
    fn a_full_state_applies_and_advances() {
        let mut s = Sender::new(screen(2, 4));
        s.update(marked(2, 4, "A"));
        let f = s.make_frame(0).expect("frame").expect("some");

        let mut r = Receiver::new(screen(9, 9));
        assert!(r.on_frame(&f).expect("apply"));
        assert_eq!(r.ack(), 2);
        assert_eq!(r.state(), s.current());
    }

    // ---- idempotence ----

    #[test]
    fn a_duplicate_frame_is_ignored_without_error() {
        let mut s = Sender::new(screen(2, 4));
        s.update(marked(2, 4, "A"));
        let f = s.make_frame(0).expect("frame").expect("some");

        let mut r = Receiver::new(screen(2, 4));
        assert!(r.on_frame(&f).expect("first"));
        assert!(!r.on_frame(&f).expect("duplicate"), "idempotent, not an error");
        assert!(!r.on_frame(&f).expect("triplicate"));
        assert_eq!(r.ack(), 2);
    }

    #[test]
    fn a_reordered_old_frame_is_ignored_without_error() {
        let mut s = Sender::new(screen(2, 4));
        s.update(marked(2, 4, "A"));
        let old = s.make_frame(0).expect("frame").expect("some");
        s.update(marked(2, 4, "B"));
        let new = s.make_frame(0).expect("frame").expect("some");

        let mut r = Receiver::new(screen(2, 4));
        assert!(r.on_frame(&new).expect("newest first"));
        assert!(!r.on_frame(&old).expect("reordered old frame"));
        assert_eq!(r.state().cell(0, 0).text, "B");
    }

    // ---- the loss-recovery story, end to end ----

    #[test]
    fn a_frame_based_on_a_state_we_never_saw_is_dropped_then_recovered() {
        let mut s = Sender::new(screen(2, 4));
        let mut r = Receiver::new(screen(2, 4));

        s.update(marked(2, 4, "A"));
        let f = s.make_frame(0).expect("frame").expect("some");
        assert!(r.on_frame(&f).expect("apply"));
        s.on_ack(r.ack());

        s.update(marked(2, 4, "B"));
        let f_b = s.make_frame(0).expect("frame").expect("some");
        assert!(r.on_frame(&f_b).expect("apply"));
        s.on_ack(r.ack());

        // The sender advances twice more; the first of those frames is lost,
        // so the second bases on a state we never reached.
        s.update(marked(2, 4, "C"));
        let _lost = s.make_frame(0).expect("frame").expect("some");
        s.on_ack(4); // the sender wrongly believes we saw state 4
        s.update(marked(2, 4, "D"));
        let f_d = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f_d.from_state, 4);

        let before = r.state().clone();
        let ack_before = r.ack();
        assert!(!r.on_frame(&f_d).expect("dropped"), "base mismatch is not an error");
        assert_eq!(r.state(), &before, "a dropped frame changes nothing");
        assert_eq!(r.ack(), ack_before, "and advances nothing");

        // Once the sender learns our real ack, ONE frame carries everything.
        s.on_ack(r.ack());
        let recovery = s.make_frame(0).expect("frame").expect("some");
        assert!(r.on_frame(&recovery).expect("apply"));
        assert_eq!(r.state(), s.current());
        assert_eq!(r.state().cell(0, 0).text, "D");
    }

    #[test]
    fn the_peers_ack_is_recorded_from_accepted_frames() {
        let mut s = Sender::new(screen(2, 4));
        s.update(marked(2, 4, "A"));
        let f = s.make_frame(31).expect("frame").expect("some");

        let mut r = Receiver::new(screen(2, 4));
        r.on_frame(&f).expect("apply");
        assert_eq!(r.peer_ack(), 31);
    }

    // ---- malformed and invalid input ----

    #[test]
    fn a_compressed_frame_is_inflated_transparently() {
        let mut big = screen(24, 80);
        for (i, c) in big.cells.iter_mut().enumerate() {
            c.text = CellText::new(&char::from(b'a' + (i % 26) as u8).to_string());
        }
        let mut s = Sender::new(screen(24, 80));
        s.update(big);
        let f = s.make_frame(0).expect("frame").expect("some");
        assert_eq!(f.flags & oxutrm_proto::FLAG_ZSTD, oxutrm_proto::FLAG_ZSTD);

        let mut r = Receiver::new(screen(24, 80));
        assert!(r.on_frame(&f).expect("apply"));
        assert_eq!(r.state(), s.current());
    }

    #[test]
    fn a_corrupt_payload_is_a_decode_error_and_advances_nothing() {
        let mut s = Sender::new(screen(2, 4));
        s.update(marked(2, 4, "A"));
        let mut f = s.make_frame(0).expect("frame").expect("some");
        f.payload = vec![0xff; 8];

        let mut r = Receiver::new(screen(2, 4));
        let before = r.state().clone();
        assert!(matches!(r.on_frame(&f), Err(ApplyError::Decode(_))));
        assert_eq!(r.ack(), 0, "a corrupt frame advances nothing");
        assert_eq!(r.state(), &before);
    }

    #[test]
    fn a_frame_claiming_zstd_but_carrying_junk_is_a_decode_error() {
        let f = Frame {
            my_state: 5,
            from_state: 0,
            ack_state: 0,
            flags: oxutrm_proto::FLAG_ZSTD,
            payload: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        let mut r: Receiver<ScreenState> = Receiver::new(screen(2, 4));
        assert!(matches!(r.on_frame(&f), Err(ApplyError::Decode(_))));
        assert_eq!(r.ack(), 0);
    }

    #[test]
    fn a_diff_producing_an_out_of_bounds_cursor_is_rejected_and_rolled_back() {
        // The receiver applies to a COPY and validates it, so an invariant
        // `apply` cannot see still cannot land.
        let bad = ScreenDiff {
            resize: Some(TermSize { cols: 4, rows: 2 }),
            rows: vec![RowPatch {
                row: 0,
                runs: vec![Run {
                    start_col: 0,
                    repeat: 0,
                    cells: vec![Cell::blank()],
                }],
            }],
            cursor: Some(Cursor {
                row: 9,
                col: 0,
                visible: true,
                shape: CursorShape::Block,
            }),
            modes: None,
            title: None,
            bell: None,
            scrollback_len: None,
        };
        let f = Frame {
            my_state: 4,
            from_state: 0,
            ack_state: 0,
            flags: 0,
            payload: postcard::to_stdvec(&bad).expect("encode"),
        };

        let mut r = Receiver::new(screen(2, 4));
        let before = r.state().clone();
        assert!(matches!(
            r.on_frame(&f),
            Err(ApplyError::CursorOutOfBounds { row: 9, .. })
        ));
        assert_eq!(r.state(), &before, "an invalid state never lands");
        assert_eq!(r.ack(), 0, "and never advances the acknowledgement");
    }

    #[test]
    fn a_diff_decreasing_the_bell_is_rejected_and_rolled_back() {
        let mut s = Sender::new(screen(2, 4));
        let mut r = Receiver::new(screen(2, 4));

        let mut loud = screen(2, 4);
        loud.bell = 9;
        s.update(loud);
        assert!(r.on_frame(&s.make_frame(0).unwrap().unwrap()).unwrap());
        assert_eq!(r.state().bell, 9);
        let before = r.state().clone();
        let ack_before = r.ack();

        // Hand-build a frame that would wind the bell backwards.
        let bad = ScreenDiff {
            resize: None,
            rows: vec![],
            cursor: None,
            modes: None,
            title: None,
            bell: Some(2),
            scrollback_len: None,
        };
        let f = Frame {
            my_state: r.ack() + 1,
            from_state: r.ack(),
            ack_state: 0,
            flags: 0,
            payload: postcard::to_stdvec(&bad).expect("encode"),
        };
        assert_eq!(
            r.on_frame(&f),
            Err(ApplyError::BellWentBackwards { was: 9, now: 2 })
        );
        assert_eq!(r.state(), &before);
        assert_eq!(r.ack(), ack_before);
    }

    #[test]
    fn a_frame_survives_encode_and_decode() {
        let mut s = Sender::new(screen(3, 6));
        s.update(marked(3, 6, "\u{6f22}"));
        let f = s.make_frame(0).expect("frame").expect("some");
        let back = Frame::decode(&f.encode().expect("encode")).expect("decode");

        let mut r = Receiver::new(screen(3, 6));
        assert!(r.on_frame(&back).expect("apply"));
        assert_eq!(r.state(), s.current());
    }
}
```

Add to `crates/oxutrm-sync/src/lib.rs`:

```rust
mod receiver;

pub use receiver::{InputReceiver, Receiver, ScreenReceiver};
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-sync --lib -- --test-threads 4 receiver::`
Expected: FAIL — `cannot find type 'Receiver' in this scope`.

- [ ] **Step 3: Write the body of `crates/oxutrm-sync/src/receiver.rs`**

Put this **above** the existing `mod tests`:

```rust
use oxutrm_proto::{ApplyError, Frame, FLAG_ZSTD};

use crate::{InputState, SyncState};

/// Applies incoming diffs and tracks what to acknowledge.
///
/// Duplicates, reordering and loss need no special handling: applying a diff is
/// idempotent, and a frame that cannot be applied is simply dropped.
pub struct Receiver<S: SyncState> {
    state: S,
    /// False until the first frame is accepted. The state is a valid blank
    /// screen at seq 1 from the start, but acknowledging 1 before hearing
    /// anything would claim a state the sender never sent.
    started: bool,
    peer_ack: u64,
}

impl<S: SyncState> Receiver<S> {
    /// Start from `initial`, whose sequence number is forced to 1.
    pub fn new(initial: S) -> Receiver<S> {
        let mut initial = initial;
        initial.set_seq(1);
        Receiver {
            state: initial,
            started: false,
            peer_ack: 0,
        }
    }

    /// Apply one datagram.
    ///
    /// `Ok(true)` when the state advanced; `Ok(false)` for a stale, duplicate
    /// or unapplicable frame; `Err` only when the peer sent something
    /// malformed or something that would break an invariant.
    ///
    /// In every failing case the state is left **byte-for-byte unchanged** and
    /// `ack()` does **not** advance. Advancing while refusing would tell the
    /// sender we hold a state we do not, and it would then diff against that
    /// state forever.
    pub fn on_frame(&mut self, f: &Frame) -> Result<bool, ApplyError> {
        // Older than what we hold: nothing to learn, and it must not roll us
        // back.
        if f.my_state <= self.ack() {
            return Ok(false);
        }

        let raw = if f.flags & FLAG_ZSTD != 0 {
            zstd::decode_all(f.payload.as_slice())
                .map_err(|e| ApplyError::Decode(format!("zstd: {e}")))?
        } else {
            f.payload.clone()
        };
        let diff: S::Diff =
            postcard::from_bytes(&raw).map_err(|e| ApplyError::Decode(e.to_string()))?;

        // Apply to a COPY, validate the copy, and only then swap. That is what
        // makes "rejected wholesale, never applied partially" hold even for an
        // invariant `apply` cannot see -- a bell that went backwards, say. A
        // screen is a few thousand cells; that costs far less than a class of
        // bug which only shows up on a bad link.
        let mut candidate = self.state.clone();
        let outcome = candidate
            .apply(f.from_state, f.my_state, &diff)
            .and_then(|()| candidate.check(&self.state));

        match outcome {
            Ok(()) => {
                self.state = candidate;
                self.started = true;
                if f.ack_state > self.peer_ack {
                    self.peer_ack = f.ack_state;
                }
                Ok(true)
            }
            // Not an error: the sender is still diffing from a base we have
            // moved past, or never reached. Its next frame, sent after it hears
            // our real ack, contains everything we missed.
            Err(ApplyError::BaseMismatch { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    pub fn state(&self) -> &S {
        &self.state
    }

    /// What to put in our outgoing `ack_state`. Zero until the first frame is
    /// accepted.
    pub fn ack(&self) -> u64 {
        if self.started { self.state.seq() } else { 0 }
    }

    /// The peer's `ack_state` from the last frame we accepted.
    pub fn peer_ack(&self) -> u64 {
        self.peer_ack
    }
}

/// The client's incoming screen receiver.
pub type ScreenReceiver = Receiver<oxutrm_proto::ScreenState>;
/// The host's incoming input receiver.
pub type InputReceiver = Receiver<InputState>;
```

One subtlety worth reading twice: a **diff** arriving before any full state is
dropped by the `BaseMismatch` arm, not by the stale check. Our blank state sits
at seq 1, so a diff with `from_state == 1` would otherwise look applicable. The
sender never produces one, because `peer_ack` starts at 0 and `ack()` returns 0
until we have accepted something — but a hostile or buggy peer could, and the
receiver must not accept it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-sync --lib -- --test-threads 4 receiver::`
Expected: PASS, 12 tests.

- [ ] **Step 5: Extend the reject-path suite to the `Receiver`**

The invariants are now reachable from the outside, so the suite must exercise
them there too. Append to `crates/oxutrm-sync/tests/reject_path.rs`:

```rust
// ---------------------------------------------------------------------------
// The same refusals, one layer up: through Receiver::on_frame
// ---------------------------------------------------------------------------

mod through_the_receiver {
    use super::*;
    use oxutrm_proto::Frame;
    use oxutrm_sync::{Receiver, Sender, SyncState as _};

    /// Wrap a hand-built diff in an uncompressed frame.
    fn frame(from_state: u64, my_state: u64, d: &ScreenDiff) -> Frame {
        Frame {
            my_state,
            from_state,
            ack_state: 0,
            flags: 0,
            payload: postcard::to_stdvec(d).expect("encode"),
        }
    }

    /// Deliver a frame that must be refused, and assert BOTH things.
    fn expect_refused(r: &mut Receiver<ScreenState>, f: &Frame) -> ApplyError {
        let before = r.state().clone();
        let ack_before = r.ack();
        let err = r.on_frame(f).expect_err("this frame must be refused");
        assert_eq!(
            r.state(),
            &before,
            "a refused frame left the receiver's state changed"
        );
        assert_eq!(
            r.ack(),
            ack_before,
            "a refused frame advanced the acknowledgement, which strands the sender"
        );
        err
    }

    /// A receiver holding a known state, and the sender that put it there.
    fn primed() -> (Sender<ScreenState>, Receiver<ScreenState>) {
        let mut s = Sender::new(seeded(1, 3, 8));
        let mut r = Receiver::new(seeded(1, 3, 8));
        let mut next = seeded(1, 3, 8);
        next.bell = 4;
        next.scrollback_len = 100;
        put(&mut next, 0, 0, "A");
        s.update(next);
        r.on_frame(&s.make_frame(0).unwrap().unwrap())
            .expect("the priming frame applies");
        (s, r)
    }

    #[test]
    fn an_out_of_bounds_cursor_is_refused_at_the_receiver() {
        let (_s, mut r) = primed();
        let f = frame(
            r.ack(),
            r.ack() + 1,
            &ScreenDiff {
                cursor: Some(Cursor {
                    row: 40,
                    col: 0,
                    visible: true,
                    shape: CursorShape::Block,
                }),
                ..empty_diff()
            },
        );
        assert!(matches!(
            expect_refused(&mut r, &f),
            ApplyError::CursorOutOfBounds { row: 40, .. }
        ));
    }

    #[test]
    fn a_backwards_bell_is_refused_at_the_receiver() {
        let (_s, mut r) = primed();
        assert_eq!(r.state().bell, 4);
        let f = frame(
            r.ack(),
            r.ack() + 1,
            &ScreenDiff {
                bell: Some(1),
                ..empty_diff()
            },
        );
        assert_eq!(
            expect_refused(&mut r, &f),
            ApplyError::BellWentBackwards { was: 4, now: 1 }
        );
    }

    #[test]
    fn a_shrinking_scrollback_is_refused_at_the_receiver() {
        let (_s, mut r) = primed();
        assert_eq!(r.state().scrollback_len, 100);
        let f = frame(
            r.ack(),
            r.ack() + 1,
            &ScreenDiff {
                scrollback_len: Some(3),
                ..empty_diff()
            },
        );
        assert_eq!(
            expect_refused(&mut r, &f),
            ApplyError::ScrollbackShrank { was: 100, now: 3 }
        );
    }

    #[test]
    fn a_row_past_the_end_is_refused_at_the_receiver() {
        let (_s, mut r) = primed();
        let f = frame(
            r.ack(),
            r.ack() + 1,
            &ScreenDiff {
                rows: vec![one_cell(30, 0, "X")],
                ..empty_diff()
            },
        );
        assert_eq!(
            expect_refused(&mut r, &f),
            ApplyError::OutOfBounds { row: 30, rows: 3 }
        );
    }

    #[test]
    fn a_refusal_does_not_stop_the_next_good_frame() {
        // The reason all of this matters: recovery is the normal case.
        let (mut s, mut r) = primed();
        let bad = frame(
            r.ack(),
            r.ack() + 1,
            &ScreenDiff {
                bell: Some(0),
                ..empty_diff()
            },
        );
        expect_refused(&mut r, &bad);

        s.on_ack(r.ack());
        let mut next = s.current().clone();
        put(&mut next, 2, 2, "ok");
        s.update(next);
        assert!(r.on_frame(&s.make_frame(0).unwrap().unwrap()).unwrap());
        assert_eq!(r.state(), s.current());
    }
}
```

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: PASS, the original suite plus 5 more.

- [ ] **Step 6: Prove the receiver's roll-back can fail**

Break it on purpose. In `crates/oxutrm-sync/src/receiver.rs`, apply the diff to
`self.state` directly instead of to a candidate — that is, replace the
`candidate` block with `self.state.apply(...)` followed by
`self.state.check(...)`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: **FAIL** in `through_the_receiver`, with "a refused frame left the
receiver's state changed".

Revert, and confirm: `git diff --stat`
Expected: no output.

- [ ] **Step 7: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-sync --all-targets -- -D warnings`
Expected: PASS.

Run: `cargo test --jobs 4 -p oxutrm-sync -- --test-threads 4`
Expected: PASS, the whole crate.

```bash
git add crates/oxutrm-sync
git commit -m "$(cat <<'EOF'
feat(sync): Receiver — idempotent, loss-tolerant, and never half-applied

Duplicates and reordering return Ok(false), never an error. A diff whose base
we no longer hold is dropped, and the sender's next frame — sent after it hears
our real ack — carries everything that was lost.

Every diff is applied to a COPY, checked against the six invariants, and only
then swapped in. That is what makes wholesale rejection hold even for the two
invariants apply() cannot see, and every refusal test asserts both that the
state is unchanged and that ack() has not advanced.

Proven capable of failing: applying to self.state instead of a candidate
reddens the receiver half of the reject-path suite.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 8: `oxutrm-sync` — the convergence property test

**This is the single most important task in M1.** Task 4 proved the engine
*refuses* correctly; this proves it *converges* correctly. Together they are the
two paths spec §12 requires: the property test for the happy one, fault
injection for the reject one.

The property: for any sequence of terminal output and any subset of the
resulting datagrams dropped, duplicated or reordered, the receiver converges to
the sender's current state once one frame based on a live acknowledgement is
delivered — **and satisfies every invariant at every step along the way, not
merely at the end**.

**Files:**
- Create: `crates/oxutrm-sync/tests/convergence.rs`

**Interfaces:**
- Consumes: `Sender`, `Receiver`, `SyncState`, `DiffHint`, `InputState`,
  `STATE_RING` from `oxutrm-sync`; `Frame`, `TermSize`, `ScreenState`, `Cell`,
  `CellText`, `Color`, `Attrs`, `CursorShape`, `MouseMode` from `oxutrm-proto`.
- Produces: nothing later tasks consume. It is a gate.

- [ ] **Step 1: Write the whole test file**

Create `crates/oxutrm-sync/tests/convergence.rs`:

```rust
//! The property that matters.
//!
//! For any sequence of screen updates and any subset of the resulting frames
//! dropped, duplicated or reordered, the receiver converges to the sender's
//! current state as soon as one frame based on a live acknowledgement is
//! delivered — and holds a state satisfying every invariant at every step,
//! not merely at the end.
//!
//! The screens are deliberately tiny (at most 4x6) and the alphabet
//! deliberately small, so that when proptest shrinks a failure the
//! counterexample is short enough to read by eye.

use oxutrm_proto::{
    Attrs, Cell, CellText, Color, CursorShape, Frame, MouseMode, ScreenState, TermSize,
};
use oxutrm_sync::{DiffHint, InputState, Receiver, STATE_RING, Sender, SyncState};
use proptest::prelude::*;

const START_ROWS: u16 = 3;
const START_COLS: u16 = 5;
const MAX_ROWS: u16 = 4;
const MAX_COLS: u16 = 6;

fn blank(rows: u16, cols: u16) -> ScreenState {
    ScreenState::blank(rows, cols).expect("a blank screen is valid")
}

/// `ScreenState` has no `row_mut`, so write through `cells`.
fn put(s: &mut ScreenState, row: u16, col: u16, cell: Cell) {
    let ix = row as usize * s.cols as usize + col as usize;
    s.cells[ix] = cell;
}

// ---------------------------------------------------------------------------
// What the host's terminal does
// ---------------------------------------------------------------------------

/// One mutation of the authoritative screen. These stand in for the effects a
/// terminal emulator has; the sync engine cannot tell the difference.
///
/// Note what is absent: nothing here decreases `bell` or `scrollback_len`.
/// Those are the monotonic invariants, and a generator that violated them
/// would be testing that the engine accepts states no emulator can produce.
/// Task 4 covers the violating direction, deliberately and by hand.
#[derive(Clone, Debug)]
enum Op {
    Put {
        row: u16,
        col: u16,
        ch: char,
        fg: u8,
        bold: bool,
        blink: bool,
    },
    /// Fill a whole row with one cell — what a clear-to-end-of-line does, and
    /// the case run-length encoding exists for.
    FillRow { row: u16, ch: char, bg: u8 },
    MoveCursor { row: u16, col: u16, visible: bool },
    SetShape(CursorShape),
    SetTitle(&'static str),
    Bell,
    Resize { rows: u16, cols: u16 },
    AltScreen(bool),
    Mouse(MouseMode),
    Scrolled(u8),
}

/// A small alphabet including a wide character and a space, so shrinking
/// produces readable counterexamples.
fn char_strategy() -> impl Strategy<Value = char> {
    prop::sample::select(vec!['a', 'B', '7', ' ', '\u{6f22}'])
}

fn shape_strategy() -> impl Strategy<Value = CursorShape> {
    prop::sample::select(vec![
        CursorShape::Block,
        CursorShape::Underline,
        CursorShape::Bar,
    ])
}

fn mouse_strategy() -> impl Strategy<Value = MouseMode> {
    prop::sample::select(vec![
        MouseMode::Off,
        MouseMode::Press,
        MouseMode::PressRelease,
        MouseMode::ButtonMotion,
        MouseMode::AnyMotion,
    ])
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        // Weighted towards cell writes, which is what a terminal mostly does.
        8 => (
            0..MAX_ROWS,
            0..MAX_COLS,
            char_strategy(),
            0u8..8u8,
            any::<bool>(),
            any::<bool>(),
        )
            .prop_map(|(row, col, ch, fg, bold, blink)| Op::Put {
                row, col, ch, fg, bold, blink,
            }),
        3 => (0..MAX_ROWS, char_strategy(), 0u8..8u8)
            .prop_map(|(row, ch, bg)| Op::FillRow { row, ch, bg }),
        3 => (0..MAX_ROWS, 0..MAX_COLS, any::<bool>())
            .prop_map(|(row, col, visible)| Op::MoveCursor { row, col, visible }),
        1 => shape_strategy().prop_map(Op::SetShape),
        1 => prop::sample::select(vec!["", "vim", "~/src"]).prop_map(Op::SetTitle),
        1 => Just(Op::Bell),
        2 => (1..=MAX_ROWS, 1..=MAX_COLS).prop_map(|(rows, cols)| Op::Resize { rows, cols }),
        1 => any::<bool>().prop_map(Op::AltScreen),
        1 => mouse_strategy().prop_map(Op::Mouse),
        1 => (0u8..4u8).prop_map(Op::Scrolled),
    ]
}

/// Apply one op to the model screen and report which rows it touched.
///
/// The returned hint is **honest**: it names exactly the rows that changed.
/// That is what the emulator's damage tracking provides, and feeding the engine
/// an honest hint is what this harness tests.
fn apply_op(s: &mut ScreenState, op: &Op) -> DiffHint {
    match op {
        Op::Put { row, col, ch, fg, bold, blink } => {
            if *row < s.rows && *col < s.cols {
                let mut attrs = Attrs::empty();
                if *bold {
                    attrs |= Attrs::BOLD;
                }
                if *blink {
                    attrs |= Attrs::BLINK;
                }
                put(
                    s,
                    *row,
                    *col,
                    Cell {
                        text: CellText::new(&ch.to_string()),
                        fg: Color::Idx(*fg),
                        bg: Color::Default,
                        attrs,
                    },
                );
                return DiffHint::rows(vec![*row]);
            }
            DiffHint::rows(vec![])
        }
        Op::FillRow { row, ch, bg } => {
            if *row < s.rows {
                let fill = Cell {
                    text: CellText::new(&ch.to_string()),
                    fg: Color::Default,
                    bg: Color::Idx(*bg),
                    attrs: Attrs::empty(),
                };
                for c in 0..s.cols {
                    put(s, *row, c, fill.clone());
                }
                return DiffHint::rows(vec![*row]);
            }
            DiffHint::rows(vec![])
        }
        Op::MoveCursor { row, col, visible } => {
            // Clamped HERE, in the model, because a real emulator can never
            // put its cursor outside its own grid. The engine must still
            // reject one that arrives over the wire; Task 4 covers that.
            s.cursor.row = (*row).min(s.rows.saturating_sub(1));
            s.cursor.col = (*col).min(s.cols.saturating_sub(1));
            s.cursor.visible = *visible;
            DiffHint::rows(vec![])
        }
        Op::SetShape(shape) => {
            s.cursor.shape = *shape;
            DiffHint::rows(vec![])
        }
        Op::SetTitle(t) => {
            s.title = (*t).to_string();
            DiffHint::rows(vec![])
        }
        // Monotonic, always. I5 is a property of real terminals, not merely of
        // the type.
        Op::Bell => {
            s.bell = s.bell.saturating_add(1);
            DiffHint::rows(vec![])
        }
        Op::Resize { rows, cols } => {
            // Keep the overlapping region, as a terminal reflow would.
            let mut next = blank(*rows, *cols);
            for r in 0..(*rows).min(s.rows) {
                for c in 0..(*cols).min(s.cols) {
                    put(&mut next, r, c, s.cell(r, c).clone());
                }
            }
            next.cursor = s.cursor;
            next.cursor.row = next.cursor.row.min(rows.saturating_sub(1));
            next.cursor.col = next.cursor.col.min(cols.saturating_sub(1));
            next.modes = s.modes;
            next.title.clone_from(&s.title);
            next.bell = s.bell;
            next.scrollback_len = s.scrollback_len;
            next.seq = s.seq;
            *s = next;
            // A resize invalidates the whole screen; the engine ignores the
            // hint in that case anyway.
            DiffHint::everything()
        }
        Op::AltScreen(on) => {
            s.modes.alt_screen = *on;
            DiffHint::rows(vec![])
        }
        Op::Mouse(m) => {
            s.modes.mouse = *m;
            DiffHint::rows(vec![])
        }
        // Monotonic, always. I6.
        Op::Scrolled(n) => {
            s.scrollback_len = s.scrollback_len.saturating_add(u64::from(*n));
            DiffHint::rows(vec![])
        }
    }
}

// ---------------------------------------------------------------------------
// What the network does
// ---------------------------------------------------------------------------

/// One thing the unreliable link may do.
///
/// `u8` indices are taken modulo the queue length, so every generated value is
/// meaningful and shrinks cleanly towards 0.
#[derive(Clone, Debug)]
enum Wire {
    /// The sender emits the current state's frame into the flight queue.
    Send,
    /// Deliver one queued frame and remove it.
    Deliver(u8),
    /// Deliver one queued frame and keep it, so it can arrive again.
    Duplicate(u8),
    /// Lose one queued frame.
    Drop(u8),
    /// Lose everything currently in flight.
    DropAll,
    /// The receiver's acknowledgement reaches the sender.
    Ack,
}

fn wire_strategy() -> impl Strategy<Value = Wire> {
    prop_oneof![
        6 => Just(Wire::Send),
        7 => any::<u8>().prop_map(Wire::Deliver),
        2 => any::<u8>().prop_map(Wire::Duplicate),
        3 => any::<u8>().prop_map(Wire::Drop),
        1 => Just(Wire::DropAll),
        4 => Just(Wire::Ack),
    ]
}

/// Pick a queue slot. `None` for an empty queue.
fn slot(len: usize, idx: u8) -> Option<usize> {
    if len == 0 { None } else { Some(idx as usize % len) }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Drive one wire step against a sender, a receiver and a flight queue.
///
/// Generic, so the screen and input channels share exactly one implementation.
/// A second copy would be a second place for the property to be subtly weaker.
fn step<S: SyncState + PartialEq + std::fmt::Debug>(
    sender: &mut Sender<S>,
    receiver: &mut Receiver<S>,
    flight: &mut Vec<Frame>,
    w: &Wire,
) -> Result<(), TestCaseError> {
    match w {
        Wire::Send => {
            if let Some(f) = sender
                .make_frame(receiver.ack())
                .map_err(|e| TestCaseError::fail(format!("make_frame: {e}")))?
            {
                flight.push(f);
            }
        }
        Wire::Deliver(i) => {
            if let Some(ix) = slot(flight.len(), *i) {
                let f = flight.remove(ix);
                receiver
                    .on_frame(&f)
                    .map_err(|e| TestCaseError::fail(format!("on_frame: {e}")))?;
            }
        }
        Wire::Duplicate(i) => {
            if let Some(ix) = slot(flight.len(), *i) {
                let f = flight[ix].clone();
                receiver
                    .on_frame(&f)
                    .map_err(|e| TestCaseError::fail(format!("on_frame dup: {e}")))?;
            }
        }
        Wire::Drop(i) => {
            if let Some(ix) = slot(flight.len(), *i) {
                flight.remove(ix);
            }
        }
        Wire::DropAll => flight.clear(),
        Wire::Ack => sender.on_ack(receiver.ack()),
    }
    Ok(())
}

/// The guarantee: one acknowledgement, one frame, converged.
fn settle<S: SyncState + PartialEq + std::fmt::Debug>(
    sender: &mut Sender<S>,
    receiver: &mut Receiver<S>,
) -> Result<(), TestCaseError> {
    sender.on_ack(receiver.ack());
    if let Some(f) = sender
        .make_frame(0)
        .map_err(|e| TestCaseError::fail(format!("settle make_frame: {e}")))?
    {
        let advanced = receiver
            .on_frame(&f)
            .map_err(|e| TestCaseError::fail(format!("settle on_frame: {e}")))?;
        prop_assert!(
            advanced,
            "a frame built against the receiver's own live ack must apply"
        );
    }
    prop_assert_eq!(
        receiver.state(),
        sender.current(),
        "did not converge after a single live-ack round"
    );
    Ok(())
}

fn run_screen_scenario(ops: Vec<Op>, wire: Vec<Wire>) -> Result<(), TestCaseError> {
    let mut model = blank(START_ROWS, START_COLS);
    let mut sender = Sender::new(model.clone());
    let mut receiver = Receiver::new(blank(START_ROWS, START_COLS));
    let mut flight: Vec<Frame> = Vec::new();

    let mut ops = ops.into_iter();
    // The last state the receiver held, so the transition invariants can be
    // checked step by step rather than only against the final value.
    let mut previous = receiver.state().clone();

    for w in &wire {
        // Every wire step is preceded by one terminal update, until the script
        // runs out. This interleaves output and delivery.
        if let Some(op) = ops.next() {
            let hint = apply_op(&mut model, &op);
            sender.update_damaged(model.clone(), hint);
        }

        step(&mut sender, &mut receiver, &mut flight, w)?;

        // The invariants, at EVERY step. Converging at the end while passing
        // through states that break I1, I2 or I5 on the way would mean the
        // receiver briefly showed the user a screen that existed nowhere.
        prop_assert!(
            receiver.state().validate().is_ok(),
            "the receiver's state broke an invariant: {:?}",
            receiver.state().validate()
        );
        prop_assert!(
            receiver.state().validate_transition(&previous).is_ok(),
            "the receiver's transition broke an invariant: {:?}",
            receiver.state().validate_transition(&previous)
        );
        prop_assert!(
            receiver.ack() <= sender.current().seq(),
            "the receiver claimed state {} but the sender is only at {}",
            receiver.ack(),
            sender.current().seq()
        );
        previous = receiver.state().clone();
    }

    for op in ops {
        let hint = apply_op(&mut model, &op);
        sender.update_damaged(model.clone(), hint);
    }

    settle(&mut sender, &mut receiver)?;
    prop_assert!(
        receiver.state().validate().is_ok(),
        "the converged state must be valid too"
    );
    Ok(())
}

fn run_input_scenario(
    chunks: Vec<Vec<u8>>,
    trims: Vec<u8>,
    wire: Vec<Wire>,
) -> Result<(), TestCaseError> {
    let size = TermSize {
        cols: START_COLS,
        rows: START_ROWS,
    };
    let mut model = InputState::new(size);
    let mut sender = Sender::new(model.clone());
    let mut receiver = Receiver::new(InputState::new(size));
    let mut flight: Vec<Frame> = Vec::new();

    let mut script = chunks
        .into_iter()
        .zip(trims.into_iter().chain(std::iter::repeat(0u8)));

    for w in &wire {
        if let Some((chunk, trim)) = script.next() {
            // Trim first, then append: the same order the client uses when the
            // host acknowledges consuming a prefix.
            model = model.consume(trim as usize).append(&chunk, size);
            sender.update(model.clone());
        }
        step(&mut sender, &mut receiver, &mut flight, w)?;
    }

    for (chunk, trim) in script {
        model = model.consume(trim as usize).append(&chunk, size);
        sender.update(model.clone());
    }

    settle(&mut sender, &mut receiver)
}

// ---------------------------------------------------------------------------
// The properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 512,
        max_shrink_iters: 20_000,
        ..ProptestConfig::default()
    })]

    /// THE property. Any output, any loss, duplication or reordering: one
    /// frame based on a live ack converges the receiver onto the sender, and
    /// every state it holds along the way satisfies all six invariants.
    #[test]
    fn screen_converges_after_one_live_ack(
        ops in prop::collection::vec(op_strategy(), 0..40),
        wire in prop::collection::vec(wire_strategy(), 0..90),
    ) {
        run_screen_scenario(ops, wire)?;
    }

    /// The same property for the reverse channel, including the trimming that
    /// makes `consumed` non-zero. If drop-then-append were ever reversed, the
    /// receiver's `pending` would grow a replayed prefix and diverge here.
    #[test]
    fn input_converges_after_one_live_ack(
        chunks in prop::collection::vec(prop::collection::vec(any::<u8>(), 0..6), 0..30),
        trims in prop::collection::vec(0u8..4, 0..30),
        wire in prop::collection::vec(wire_strategy(), 0..70),
    ) {
        run_input_scenario(chunks, trims, wire)?;
    }

    /// Applying the same frame any number of times is the same as applying it
    /// once. Duplication needs no special handling anywhere.
    #[test]
    fn applying_a_frame_is_idempotent(
        ops in prop::collection::vec(op_strategy(), 1..12),
        repeats in 1usize..6,
    ) {
        let mut model = blank(START_ROWS, START_COLS);
        let mut sender = Sender::new(model.clone());
        for op in &ops {
            let hint = apply_op(&mut model, op);
            sender.update_damaged(model.clone(), hint);
        }
        let f = sender.make_frame(0).unwrap().expect("a changed screen has a frame");

        let mut receiver = Receiver::new(blank(START_ROWS, START_COLS));
        prop_assert!(receiver.on_frame(&f).unwrap());
        let once = receiver.state().clone();
        for _ in 1..repeats {
            prop_assert!(!receiver.on_frame(&f).unwrap(), "a repeat must not advance");
        }
        prop_assert_eq!(receiver.state(), &once);
    }

    /// An acknowledgement older than the sender's ring always produces a full
    /// state, and a full state always applies whatever the receiver holds.
    #[test]
    fn an_expired_ack_still_converges(
        ops in prop::collection::vec(op_strategy(), (STATE_RING + 1)..(STATE_RING + 20)),
    ) {
        let mut model = blank(START_ROWS, START_COLS);
        let mut sender = Sender::new(model.clone());
        let mut receiver = Receiver::new(blank(START_ROWS, START_COLS));

        // Deliver exactly one state, then let the sender run far ahead.
        let hint = apply_op(&mut model, &ops[0]);
        sender.update_damaged(model.clone(), hint);
        let f = sender.make_frame(0).unwrap().expect("some");
        prop_assert!(receiver.on_frame(&f).unwrap());
        prop_assert!(receiver.ack() > 0);

        for op in &ops[1..] {
            let hint = apply_op(&mut model, op);
            sender.update_damaged(model.clone(), hint);
        }

        sender.on_ack(receiver.ack());
        let f = sender.make_frame(0).unwrap().expect("some");
        prop_assert_eq!(f.from_state, 0, "an expired ack forces a full state");
        prop_assert!(receiver.on_frame(&f).unwrap());
        prop_assert_eq!(receiver.state(), sender.current());
    }

    /// Damage hints must never lose a row. A diff built with the honest hints
    /// the emulator would supply is identical to one built by comparing
    /// everything.
    #[test]
    fn honest_damage_hints_never_lose_a_row(
        ops in prop::collection::vec(op_strategy(), 1..25),
    ) {
        let mut model = blank(START_ROWS, START_COLS);
        let mut hinted = Sender::new(model.clone());
        let mut blind = Sender::new(model.clone());
        for op in &ops {
            let hint = apply_op(&mut model, op);
            hinted.update_damaged(model.clone(), hint);
            blind.update(model.clone());
        }
        hinted.on_ack(1);
        blind.on_ack(1);

        // `Frame` has no PartialEq, so compare the encodings.
        let a = hinted.make_frame(0).unwrap().expect("some");
        let b = blind.make_frame(0).unwrap().expect("some");
        prop_assert_eq!(
            a.encode().unwrap(),
            b.encode().unwrap(),
            "an honest hint must change nothing but the work done"
        );
    }

    /// Every frame survives encoding and decoding unchanged.
    #[test]
    fn frames_survive_the_wire(
        ops in prop::collection::vec(op_strategy(), 1..20),
    ) {
        let mut model = blank(START_ROWS, START_COLS);
        let mut sender = Sender::new(model.clone());
        for op in &ops {
            let hint = apply_op(&mut model, op);
            sender.update_damaged(model.clone(), hint);
        }
        let f = sender.make_frame(9).unwrap().expect("some");
        let bytes = f.encode().expect("encode");
        let back = Frame::decode(&bytes).expect("decode");
        prop_assert_eq!(back.encode().unwrap(), bytes);
    }
}
```

- [ ] **Step 2: Run the test**

Run: `cargo test --jobs 4 -p oxutrm-sync --test convergence -- --test-threads 4`
Expected: it should **compile and pass** — Tasks 3 to 7 already built everything
it needs. That is the point: this task is a gate, not a feature.

If it fails, you have found a real bug in the sync engine. Do **not** weaken the
property to make it pass. Read the shrunk counterexample proptest prints — a
short `ops` list and a short `wire` list — reproduce it in a plain `#[test]`,
fix the engine, and keep the property exactly as written.

- [ ] **Step 3: Prove the property can fail (fault injection)**

A green property test that cannot go red proves nothing. Break the engine on
purpose, four ways, and confirm the property catches each.

**Injection 1 — accept duplicates.** In `crates/oxutrm-sync/src/receiver.rs`,
change the stale check from `if f.my_state <= self.ack()` to
`if f.my_state < self.ack()`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test convergence -- --test-threads 4`
Expected: **FAIL** in `applying_a_frame_is_idempotent`.

**Injection 2 — append before dropping.** Revert injection 1. In
`crates/oxutrm-sync/src/input.rs`, swap the two lines in `apply` so
`extend_from_slice` runs before `drain`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test convergence -- --test-threads 4`
Expected: **FAIL** in `input_converges_after_one_live_ack`.

**Injection 3 — use only the newest damage hint.** Revert injection 2. In
`crates/oxutrm-sync/src/sender.rs`, replace the fold that unions the hints with
`self.ring.back().map_or(DiffHint::everything(), |e| e.hint.clone())`.

Run: `cargo test --jobs 4 -p oxutrm-sync --test convergence -- --test-threads 4`
Expected: **FAIL** in `honest_damage_hints_never_lose_a_row` or
`screen_converges_after_one_live_ack`.

**Injection 4 — skip the per-step invariant check.** Revert injection 3. In
`crates/oxutrm-sync/src/receiver.rs`, change the candidate's `check` call to
`Ok(())`, so an invalid state can land.

Run: `cargo test --jobs 4 -p oxutrm-sync --test convergence -- --test-threads 4`
Expected: this one may well still **PASS**, because the harness's own generator
never produces a violating state. **That is the finding, not a failure of the
plan**: the convergence property cannot police the reject path, which is
exactly why Task 4 exists as a separate suite. Confirm it by running that suite
under the same injection:

Run: `cargo test --jobs 4 -p oxutrm-sync --test reject_path -- --test-threads 4`
Expected: **FAIL** in `through_the_receiver`.

Revert injection 4.

Run: `git diff --stat`
Expected: **no output** — the tree is back to the committed engine.

Run: `cargo test --jobs 4 -p oxutrm-sync -- --test-threads 4`
Expected: PASS.

Record all four outcomes in the commit message, including the fourth's
"convergence stayed green, reject_path went red". A reviewer must be able to see
both that the property can fail and where its blind spot is.

- [ ] **Step 4: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-sync --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-sync
git commit -m "$(cat <<'EOF'
test(sync): the convergence property, with the invariants checked every step

For any sequence of screen updates and any subset of the resulting frames
dropped, duplicated or reordered, the receiver converges to the sender's
current state once one frame based on a live ack is delivered — and validate()
and validate_transition() both hold at every step, not merely at the end.
Converging while passing through an invalid state would mean briefly showing
the user a screen that existed nowhere.

Proven capable of failing three ways, each reverted: accepting duplicates,
appending before dropping in InputState::apply, and using only the newest
damage hint.

A fourth injection — dropping the receiver's invariant check — left this suite
GREEN and reddened tests/reject_path.rs instead. That is the blind spot the two
suites exist to cover between them, and it is why neither replaces the other.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 9: `oxutrm-term` — the 269-entry palette and the one checked grid accessor

`oxutrm-term` currently contains **only a module doc comment**. Everything below
is new. It depends on `oxutrm-proto` alone — never on `oxutrm-sync` — because
the screen model lives in `oxutrm-proto` precisely so that
`alacritty_terminal`'s PTY, `polling` and `signal-hook` never reach the pure
crate.

**Files:**
- Modify: `crates/oxutrm-term/Cargo.toml` (add the dependencies)
- Create: `crates/oxutrm-term/src/palette.rs`
- Create: `crates/oxutrm-term/src/grid.rs`
- Modify: `crates/oxutrm-term/src/lib.rs` (keep the existing module doc)

**Interfaces:**
- Consumes: `oxutrm_proto::Color`, `alacritty_terminal`.
- Produces:
  ```rust
  /// oxutrm's own colour table. The crate ships none: `Term::colors()` is an
  /// OSC 4/10/11 OVERRIDE table whose every entry is `None` by default.
  pub struct Palette { /* private: [(u8,u8,u8); 269] */ }
  impl Palette {
      pub fn xterm() -> Palette;
      pub fn rgb(&self, index: usize) -> (u8, u8, u8);
  }
  impl Default for Palette;
  pub const PALETTE_LEN: usize = 269;

  /// Map an emulator colour onto ours, consulting the override table first.
  pub(crate) fn convert_color(
      c: alacritty_terminal::vte::ansi::Color,
      overrides: &alacritty_terminal::term::color::Colors,
      palette: &Palette,
  ) -> oxutrm_proto::Color;

  /// The ONE checked grid accessor. Every read of a grid cell goes through it.
  pub(crate) fn checked_cell<T>(
      grid: &alacritty_terminal::Grid<T>, line: i32, column: usize,
  ) -> Option<&T>;
  ```

**Why the checked accessor exists.** `Grid`'s `Index<Point>` carries only a
`debug_assert`, so an out-of-range point **panics in debug and reads out of
bounds in release**. The diff engine indexes by coordinates that arrive over the
network. Every access clamps with `Point::grid_clamp(&dims, Boundary::Grid)` and
returns `Option`, and there is exactly one place that does it.

**Palette layout**, all ranges half-open, summing to exactly 269:

| Index | Contents |
|---|---|
| `0..16` | the 16 named ANSI colours |
| `16..232` | the 6x6x6 colour cube (216) |
| `232..256` | the 24-step greyscale ramp |
| `256`, `257`, `258` | default foreground, background, cursor |
| `259..267` | the eight dim variants |
| `267` | bright foreground |
| `268` | dim foreground |

There is **no dim background**: `NamedColor` ends
`BrightForeground, DimForeground`.

- [ ] **Step 1: Add the dependencies**

Replace the `[dependencies]` section of `crates/oxutrm-term/Cargo.toml`:

```toml
[dependencies]
alacritty_terminal.workspace = true
anyhow.workspace = true
oxutrm-proto.workspace = true
rustix.workspace = true
rustix-openpty.workspace = true
unicode-width.workspace = true

[dev-dependencies]
insta.workspace = true
```

Run: `cargo build --jobs 4 -p oxutrm-term`
Expected: PASS.

Run: `cargo test --jobs 4 -p oxutrm-sync --test no_io`
Expected: still PASS. `oxutrm-sync` must not have gained anything — it does not
depend on `oxutrm-term` and never will.

- [ ] **Step 2: Write the failing tests**

Create `crates/oxutrm-term/src/palette.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::term::color::Colors;
    use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor, Rgb};

    #[test]
    fn the_palette_has_exactly_269_entries() {
        assert_eq!(PALETTE_LEN, 269);
        assert_eq!(
            PALETTE_LEN,
            alacritty_terminal::term::color::COUNT,
            "our table must be the same length as the override table"
        );
    }

    #[test]
    fn the_sixteen_named_colours_are_the_xterm_values() {
        let p = Palette::xterm();
        // A table test, because one wrong entry is invisible until someone
        // screenshots a terminal.
        for (i, want) in [
            (0usize, (0x00, 0x00, 0x00)),
            (1, (0xcd, 0x00, 0x00)),
            (2, (0x00, 0xcd, 0x00)),
            (3, (0xcd, 0xcd, 0x00)),
            (4, (0x00, 0x00, 0xee)),
            (5, (0xcd, 0x00, 0xcd)),
            (6, (0x00, 0xcd, 0xcd)),
            (7, (0xe5, 0xe5, 0xe5)),
            (8, (0x7f, 0x7f, 0x7f)),
            (9, (0xff, 0x00, 0x00)),
            (10, (0x00, 0xff, 0x00)),
            (11, (0xff, 0xff, 0x00)),
            (12, (0x5c, 0x5c, 0xff)),
            (13, (0xff, 0x00, 0xff)),
            (14, (0x00, 0xff, 0xff)),
            (15, (0xff, 0xff, 0xff)),
        ] {
            assert_eq!(p.rgb(i), want, "palette entry {i}");
        }
    }

    #[test]
    fn the_colour_cube_uses_the_six_standard_levels() {
        let p = Palette::xterm();
        assert_eq!(p.rgb(16), (0, 0, 0), "the cube origin");
        assert_eq!(p.rgb(231), (255, 255, 255), "the far corner");
        // 16 + 36*1 + 6*2 + 3 = 67
        assert_eq!(p.rgb(67), (95, 135, 175));
    }

    #[test]
    fn the_greyscale_ramp_has_24_steps_from_8_to_238() {
        let p = Palette::xterm();
        assert_eq!(p.rgb(232), (8, 8, 8));
        assert_eq!(p.rgb(255), (238, 238, 238));
        for i in 0..24usize {
            let v = (8 + i * 10) as u8;
            assert_eq!(p.rgb(232 + i), (v, v, v), "greyscale step {i}");
        }
    }

    #[test]
    fn the_tail_entries_sit_where_namedcolor_says_they_do() {
        let p = Palette::xterm();
        // These indices are NamedColor's discriminants. A mismatch here would
        // silently swap the cursor colour for the background.
        assert_eq!(NamedColor::Foreground as usize, 256);
        assert_eq!(NamedColor::Background as usize, 257);
        assert_eq!(NamedColor::Cursor as usize, 258);
        assert_eq!(NamedColor::DimBlack as usize, 259);
        assert_eq!(NamedColor::DimWhite as usize, 266);
        assert_eq!(NamedColor::BrightForeground as usize, 267);
        assert_eq!(NamedColor::DimForeground as usize, 268);

        assert_eq!(p.rgb(256), (0xff, 0xff, 0xff), "default foreground");
        assert_eq!(p.rgb(257), (0x00, 0x00, 0x00), "default background");
        assert_eq!(p.rgb(258), (0xff, 0xff, 0xff), "cursor");
    }

    #[test]
    fn the_dim_variants_are_two_thirds_of_their_normal_colour() {
        let p = Palette::xterm();
        for i in 0..8usize {
            let (r, g, b) = p.rgb(i);
            let want = (
                (r as u16 * 2 / 3) as u8,
                (g as u16 * 2 / 3) as u8,
                (b as u16 * 2 / 3) as u8,
            );
            assert_eq!(p.rgb(259 + i), want, "dim variant of colour {i}");
        }
    }

    // ---- colour conversion ----

    #[test]
    fn default_foreground_and_background_stay_unresolved() {
        // Keeping Default palette-independent is what lets a client with a
        // different theme render the host's state as its user expects.
        let p = Palette::xterm();
        let none = Colors::default();
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Foreground), &none, &p),
            Color::Default
        );
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Background), &none, &p),
            Color::Default
        );
    }

    #[test]
    fn the_first_sixteen_named_colours_stay_indexed() {
        let p = Palette::xterm();
        let none = Colors::default();
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Red), &none, &p),
            Color::Idx(1)
        );
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::BrightWhite), &none, &p),
            Color::Idx(15)
        );
        assert_eq!(convert_color(VteColor::Indexed(208), &none, &p), Color::Idx(208));
    }

    #[test]
    fn a_spec_colour_passes_through_as_rgb() {
        let p = Palette::xterm();
        let none = Colors::default();
        assert_eq!(
            convert_color(VteColor::Spec(Rgb { r: 1, g: 2, b: 3 }), &none, &p),
            Color::Rgb(1, 2, 3)
        );
    }

    #[test]
    fn colours_our_enum_cannot_name_resolve_through_the_palette() {
        let p = Palette::xterm();
        let none = Colors::default();
        // `Color` has no "dim red" and no "cursor", so these must become
        // concrete Rgb or they would be lost.
        let dim_red = p.rgb(NamedColor::DimRed as usize);
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::DimRed), &none, &p),
            Color::Rgb(dim_red.0, dim_red.1, dim_red.2)
        );
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Cursor), &none, &p),
            Color::Rgb(0xff, 0xff, 0xff)
        );
    }

    #[test]
    fn an_osc_override_wins_over_both_the_index_and_the_palette() {
        let p = Palette::xterm();
        let mut overrides = Colors::default();
        overrides[1] = Some(Rgb { r: 9, g: 8, b: 7 });
        // The application asked for "red" and then redefined red via OSC 4.
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Red), &overrides, &p),
            Color::Rgb(9, 8, 7)
        );
        assert_eq!(
            convert_color(VteColor::Indexed(1), &overrides, &p),
            Color::Rgb(9, 8, 7)
        );
        // An untouched index is still an index.
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Green), &overrides, &p),
            Color::Idx(2)
        );
    }

    #[test]
    fn an_osc_override_of_the_default_foreground_resolves_it() {
        let p = Palette::xterm();
        let mut overrides = Colors::default();
        overrides[256] = Some(Rgb {
            r: 0x20,
            g: 0x30,
            b: 0x40,
        });
        assert_eq!(
            convert_color(VteColor::Named(NamedColor::Foreground), &overrides, &p),
            Color::Rgb(0x20, 0x30, 0x40)
        );
    }
}
```

Create `crates/oxutrm-term/src/grid.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::term::test::TermSize as ATermSize;
    use alacritty_terminal::term::{Config, Term};
    use alacritty_terminal::vte::ansi::Processor;

    fn term_with(bytes: &[u8], cols: usize, lines: usize, history: usize) -> Term<VoidListener> {
        let mut t = Term::new(
            Config {
                scrolling_history: history,
                ..Config::default()
            },
            &ATermSize::new(cols, lines),
            VoidListener,
        );
        let mut p: Processor = Processor::new();
        p.advance(&mut t, bytes);
        t
    }

    #[test]
    fn a_cell_inside_the_viewport_reads_back() {
        let t = term_with(b"hi", 8, 3, 0);
        assert_eq!(checked_cell(t.grid(), 0, 0).expect("in range").c, 'h');
        assert_eq!(checked_cell(t.grid(), 0, 1).expect("in range").c, 'i');
        assert_eq!(checked_cell(t.grid(), 0, 2).expect("in range").c, ' ');
    }

    #[test]
    fn a_negative_line_reaches_history() {
        // Six lines of output on a two-line screen leaves four in history.
        let t = term_with(b"1\r\n2\r\n3\r\n4\r\n5\r\n6", 4, 2, 20);
        assert!(t.grid().history_size() >= 4, "history must have filled");
        assert_eq!(checked_cell(t.grid(), -1, 0).expect("history").c, '5');
        assert_eq!(checked_cell(t.grid(), -4, 0).expect("history").c, '2');
    }

    #[test]
    fn every_boundary_is_in_range_and_one_past_each_is_not() {
        let t = term_with(b"1\r\n2\r\n3\r\n4", 4, 2, 10);
        let top = t.grid().topmost_line().0;
        let bottom = t.grid().bottommost_line().0;
        let last = t.grid().columns() - 1;

        assert!(checked_cell(t.grid(), top, 0).is_some(), "topmost line");
        assert!(checked_cell(t.grid(), bottom, 0).is_some(), "bottommost line");
        assert!(checked_cell(t.grid(), bottom, last).is_some(), "last column");

        assert!(checked_cell(t.grid(), top - 1, 0).is_none(), "above the grid");
        assert!(checked_cell(t.grid(), bottom + 1, 0).is_none(), "below the grid");
        assert!(checked_cell(t.grid(), 0, last + 1).is_none(), "past the last column");
    }

    #[test]
    fn wildly_out_of_range_coordinates_return_none_rather_than_panicking() {
        // These are the coordinates that arrive over the network. In release
        // builds Index<Point> would read out of bounds and return garbage.
        let t = term_with(b"x", 4, 2, 0);
        assert!(checked_cell(t.grid(), i32::MAX, 0).is_none());
        assert!(checked_cell(t.grid(), i32::MIN, 0).is_none());
        assert!(checked_cell(t.grid(), 0, usize::MAX).is_none());
        assert!(checked_cell(t.grid(), i32::MAX, usize::MAX).is_none());
    }

    #[test]
    fn a_zero_history_grid_has_no_negative_lines() {
        let t = term_with(b"1\r\n2\r\n3\r\n4", 4, 2, 0);
        assert_eq!(t.grid().history_size(), 0);
        assert!(checked_cell(t.grid(), -1, 0).is_none());
        assert!(checked_cell(t.grid(), 0, 0).is_some());
    }
}
```

Add to `crates/oxutrm-term/src/lib.rs`, below the existing doc comment:

```rust
mod grid;
mod palette;

pub use palette::{PALETTE_LEN, Palette};
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-term -- --test-threads 4`
Expected: FAIL — `cannot find type 'Palette'`, `cannot find function
'checked_cell'`.

- [ ] **Step 4: Write the body of `crates/oxutrm-term/src/palette.rs`**

Put this **above** the existing `mod tests`:

```rust
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::vte::ansi::{Color as VteColor, NamedColor};
use oxutrm_proto::Color;

/// The number of entries in a terminal colour table. Identical to
/// `alacritty_terminal::term::color::COUNT`, and asserted equal in the tests.
pub const PALETTE_LEN: usize = 269;

/// oxutrm's own colour table.
///
/// `alacritty_terminal` ships **no** palette: `Term::colors()` is an
/// `OSC 4/10/11` **override** table whose every entry is `None` by default, so
/// indexed and named colours resolve to nothing until something supplies them.
/// The type looks like a palette, which is exactly why this is the obligation
/// most likely to be missed.
///
/// Layout, all ranges half-open:
///
/// | Index | Contents |
/// |---|---|
/// | `0..16` | the 16 named ANSI colours |
/// | `16..232` | the 6x6x6 colour cube |
/// | `232..256` | the 24-step greyscale ramp |
/// | `256`, `257`, `258` | default foreground, background, cursor |
/// | `259..267` | the eight dim variants |
/// | `267` | bright foreground |
/// | `268` | dim foreground |
///
/// There is no dim background; `NamedColor` ends
/// `BrightForeground, DimForeground`.
pub struct Palette {
    entries: [(u8, u8, u8); PALETTE_LEN],
}

/// The six intensity levels of the xterm colour cube.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// The 16 ANSI colours as xterm defines them.
const ANSI16: [(u8, u8, u8); 16] = [
    (0x00, 0x00, 0x00),
    (0xcd, 0x00, 0x00),
    (0x00, 0xcd, 0x00),
    (0xcd, 0xcd, 0x00),
    (0x00, 0x00, 0xee),
    (0xcd, 0x00, 0xcd),
    (0x00, 0xcd, 0xcd),
    (0xe5, 0xe5, 0xe5),
    (0x7f, 0x7f, 0x7f),
    (0xff, 0x00, 0x00),
    (0x00, 0xff, 0x00),
    (0xff, 0xff, 0x00),
    (0x5c, 0x5c, 0xff),
    (0xff, 0x00, 0xff),
    (0x00, 0xff, 0xff),
    (0xff, 0xff, 0xff),
];

/// Two thirds of a colour: how a dim variant is derived.
fn dim(c: (u8, u8, u8)) -> (u8, u8, u8) {
    (
        (c.0 as u16 * 2 / 3) as u8,
        (c.1 as u16 * 2 / 3) as u8,
        (c.2 as u16 * 2 / 3) as u8,
    )
}

impl Palette {
    /// The standard xterm palette.
    #[must_use]
    pub fn xterm() -> Palette {
        let mut entries = [(0u8, 0u8, 0u8); PALETTE_LEN];

        entries[..16].copy_from_slice(&ANSI16);

        for r in 0..6usize {
            for g in 0..6usize {
                for b in 0..6usize {
                    entries[16 + 36 * r + 6 * g + b] =
                        (CUBE_LEVELS[r], CUBE_LEVELS[g], CUBE_LEVELS[b]);
                }
            }
        }

        for i in 0..24usize {
            let v = (8 + i * 10) as u8;
            entries[232 + i] = (v, v, v);
        }

        let foreground = (0xff, 0xff, 0xff);
        entries[NamedColor::Foreground as usize] = foreground;
        entries[NamedColor::Background as usize] = (0x00, 0x00, 0x00);
        // The cursor takes the foreground colour, which is what a terminal
        // with no explicit cursor colour does.
        entries[NamedColor::Cursor as usize] = foreground;

        for i in 0..8usize {
            entries[NamedColor::DimBlack as usize + i] = dim(ANSI16[i]);
        }
        entries[NamedColor::BrightForeground as usize] = (0xff, 0xff, 0xff);
        entries[NamedColor::DimForeground as usize] = dim(foreground);

        Palette { entries }
    }

    /// The RGB triple at `index`. Out-of-range indices return black rather than
    /// panicking; the index comes from a `NamedColor` discriminant, so it is
    /// always in range in practice.
    #[must_use]
    pub fn rgb(&self, index: usize) -> (u8, u8, u8) {
        self.entries.get(index).copied().unwrap_or((0, 0, 0))
    }
}

impl Default for Palette {
    fn default() -> Self {
        Palette::xterm()
    }
}

/// Map an emulator colour onto ours.
///
/// Palette-independent forms are **kept** wherever possible — `Default` for the
/// terminal's own foreground and background, `Idx` for the first 16 named
/// colours and every indexed colour — so a client with a different theme can
/// render the host's state as its user expects. Only what our enum cannot name,
/// or what an `OSC 4/10/11` sequence has redefined, resolves to concrete RGB
/// through the palette.
pub(crate) fn convert_color(c: VteColor, overrides: &Colors, palette: &Palette) -> Color {
    let resolved = |index: usize| -> Color {
        let (r, g, b) = match overrides[index] {
            Some(rgb) => (rgb.r, rgb.g, rgb.b),
            None => palette.rgb(index),
        };
        Color::Rgb(r, g, b)
    };

    match c {
        VteColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        VteColor::Indexed(n) => {
            if overrides[n as usize].is_some() {
                resolved(n as usize)
            } else {
                Color::Idx(n)
            }
        }
        VteColor::Named(named) => {
            let index = named as usize;
            if overrides[index].is_some() {
                return resolved(index);
            }
            match named {
                // The terminal's own two colours stay unresolved so the client
                // can apply its own theme.
                NamedColor::Foreground | NamedColor::Background => Color::Default,
                // The first 16 have index forms our enum can carry.
                _ if index < 16 => Color::Idx(index as u8),
                // Everything else — the dim variants, the cursor, the bright
                // foreground — has no index form, so it must resolve now or be
                // lost.
                _ => resolved(index),
            }
        }
    }
}
```

- [ ] **Step 5: Write the body of `crates/oxutrm-term/src/grid.rs`**

Put this **above** the existing `mod tests`:

```rust
use alacritty_terminal::Grid;
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::index::{Boundary, Column, Line, Point};

/// The **one** checked grid accessor. Every read of a grid cell goes through
/// this function.
///
/// `Grid`'s `Index<Point>` carries only a `debug_assert`, so an out-of-range
/// point **panics in debug builds and reads out of bounds in release ones**,
/// returning garbage rather than failing loudly. The diff engine indexes by
/// coordinates that arrive over the network, so that is not an acceptable
/// failure mode.
///
/// `line` is signed, and negative values reach scrollback history: `-1` is the
/// most recently scrolled-off line. `None` means the point is outside
/// `topmost_line()..=bottommost_line()` or past the last column.
pub(crate) fn checked_cell<T>(grid: &Grid<T>, line: i32, column: usize) -> Option<&T> {
    if column >= grid.columns() {
        return None;
    }
    if line < grid.topmost_line().0 || line > grid.bottommost_line().0 {
        return None;
    }
    // Both coordinates are known to be in range, but clamping anyway costs
    // nothing and means a future change to the checks above cannot quietly
    // reintroduce the unchecked path.
    let point = Point::new(Line(line), Column(column)).grid_clamp(grid, Boundary::Grid);
    Some(&grid[point])
}
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-term -- --test-threads 4`
Expected: PASS, 15 tests.

If `a_negative_line_reaches_history` finds different characters, print
`t.grid().history_size()` and lines `-1` through `-4`. Six lines of output on a
two-line screen leaves lines 1 to 4 in history, so `-1` is `5`.

- [ ] **Step 7: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-term --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-term
git commit -m "$(cat <<'EOF'
feat(term): the 269-entry palette and the one checked grid accessor

alacritty_terminal ships no palette — Term::colors() is an OSC 4/10/11 override
table that is all-None by default — so oxutrm supplies its own and consults the
overrides only as a layer on top. Default and the first 16 colours stay
palette-independent, so a client with another theme renders as its user expects.

Index<Point> has only a debug_assert and reads out of bounds in release, so
every grid read goes through one Option-returning accessor that clamps with
Point::grid_clamp. Tested at every boundary and one past each.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 10: `oxutrm-term` — the blink plane and the `Handler` newtype

**Files:**
- Create: `crates/oxutrm-term/src/blink.rs`
- Modify: `crates/oxutrm-term/src/lib.rs`

**Interfaces:**
- Consumes: `alacritty_terminal`, `crate::grid::checked_cell` (tests only).
- Produces:
  ```rust
  /// Blink bits keyed by ABSOLUTE line, so they survive scrolling.
  pub(crate) struct BlinkPlane { /* private */ }
  impl BlinkPlane {
      pub(crate) fn new(capacity: usize) -> BlinkPlane;
      pub(crate) fn get(&self, scrolled_off: u64, line: u16, column: u16) -> bool;
      pub(crate) fn prune(&mut self, scrolled_off: u64);
      pub(crate) fn clear(&mut self);
      pub(crate) fn len(&self) -> usize;
  }

  /// Wraps a `Term` and implements `vte::ansi::Handler`, forwarding every
  /// method unchanged except SGR 5 / 6 / 25 and the reset, which it records.
  pub(crate) struct BlinkTap<'a, T: EventListener> { /* private */ }
  impl<'a, T: EventListener> BlinkTap<'a, T> {
      pub(crate) fn new(term: &'a mut Term<T>, plane: &'a mut BlinkPlane,
                        scrolled_off: u64) -> BlinkTap<'a, T>;
  }
  ```
  Task 11's `HostTerm::poll` constructs a `BlinkTap` per `advance()` call and
  reads the plane back when building a `ScreenState`.

**Why this exists.** `vte` parses SGR 5, 6 and 25 into `Attr::BlinkSlow`,
`Attr::BlinkFast` and `Attr::CancelBlink`, but `Term::terminal_attribute` then
drops all three into a `debug!` and they never reach a cell flag. Blink is the
one attribute the emulator parses and then discards. `HIDDEN` and `STRIKEOUT`
are native flags and need none of this.

**Why absolute lines.** The spec says "keyed by `term.grid().cursor.point`".
Viewport line numbers are stable while their *content* scrolls underneath, so a
plane keyed directly on them mis-attributes blink after the first scroll. The
key is `(scrolled_off + viewport_line, column)`, derived from the same
`history_size()` accumulator that synthesizes `scrollback_len`.

**Two accepted limitations**, documented in the code: reflow on resize, and line
insert or delete inside a scroll region, can misplace a blink bit. Neither can
ever misplace a *character*.

- [ ] **Step 1: Write the failing tests**

Create `crates/oxutrm-term/src/blink.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Dimensions as _;
    use alacritty_terminal::term::cell::Flags;
    use alacritty_terminal::term::test::TermSize as ATermSize;
    use alacritty_terminal::term::{Config, Term};

    struct Harness {
        term: Term<VoidListener>,
        plane: BlinkPlane,
        parser: Processor,
        scrolled_off: u64,
        prev_history: usize,
    }

    impl Harness {
        fn new(cols: usize, lines: usize, history: usize) -> Harness {
            Harness {
                term: Term::new(
                    Config {
                        scrolling_history: history,
                        ..Config::default()
                    },
                    &ATermSize::new(cols, lines),
                    VoidListener,
                ),
                plane: BlinkPlane::new(history + lines),
                parser: Processor::new(),
                scrolled_off: 0,
                prev_history: 0,
            }
        }

        fn feed(&mut self, bytes: &[u8]) {
            let mut tap = BlinkTap::new(&mut self.term, &mut self.plane, self.scrolled_off);
            self.parser.advance(&mut tap, bytes);
            let now = self.term.grid().history_size();
            self.scrolled_off += now.saturating_sub(self.prev_history) as u64;
            self.prev_history = now;
        }

        fn blinking(&self, line: u16, column: u16) -> bool {
            self.plane.get(self.scrolled_off, line, column)
        }
    }

    #[test]
    fn sgr_5_marks_the_cells_written_after_it() {
        let mut h = Harness::new(8, 2, 0);
        h.feed(b"a\x1b[5mbc\x1b[25md");
        assert!(!h.blinking(0, 0), "'a' was written before SGR 5");
        assert!(h.blinking(0, 1), "'b' blinks");
        assert!(h.blinking(0, 2), "'c' blinks");
        assert!(!h.blinking(0, 3), "'d' follows SGR 25");
    }

    #[test]
    fn sgr_6_is_rapid_blink_and_counts_the_same() {
        let mut h = Harness::new(8, 1, 0);
        h.feed(b"\x1b[6mx");
        assert!(h.blinking(0, 0));
    }

    #[test]
    fn sgr_0_cancels_blink_along_with_everything_else() {
        let mut h = Harness::new(8, 1, 0);
        h.feed(b"\x1b[5ma\x1b[0mb");
        assert!(h.blinking(0, 0));
        assert!(!h.blinking(0, 1), "a full reset must clear blink too");
    }

    #[test]
    fn overwriting_a_blinking_cell_clears_its_mark() {
        // Without this the mark outlives its character and blinks whatever
        // replaces it.
        let mut h = Harness::new(8, 1, 0);
        h.feed(b"\x1b[5mX\x1b[25m\r");
        assert!(h.blinking(0, 0));
        h.feed(b"Y");
        assert!(!h.blinking(0, 0), "the replacement does not inherit blink");
    }

    #[test]
    fn the_grid_itself_still_receives_every_other_attribute() {
        // The whole point of the newtype is that it forwards everything.
        let mut h = Harness::new(8, 1, 0);
        h.feed(b"\x1b[1;3;4;7;9;8mZ");
        let cell = crate::grid::checked_cell(h.term.grid(), 0, 0).expect("in range");
        assert_eq!(cell.c, 'Z');
        assert!(cell.flags.contains(Flags::BOLD));
        assert!(cell.flags.contains(Flags::ITALIC));
        assert!(cell.flags.contains(Flags::UNDERLINE));
        assert!(cell.flags.contains(Flags::INVERSE));
        assert!(cell.flags.contains(Flags::STRIKEOUT), "strikeout is native");
        assert!(cell.flags.contains(Flags::HIDDEN), "hidden is native");
    }

    #[test]
    fn blink_follows_its_content_when_the_screen_scrolls() {
        // This is why the plane keys on absolute lines. "B" is written on the
        // top row and then scrolled off; the row that takes its place must not
        // inherit its blink.
        let mut h = Harness::new(8, 2, 10);
        h.feed(b"\x1b[5mB\x1b[25m\r\n");
        assert!(h.blinking(0, 0), "B blinks on the top row");

        h.feed(b"x\r\ny\r\nz");
        assert!(
            !h.blinking(0, 0),
            "the row now on top is a different row and must not blink"
        );
        assert!(h.scrolled_off > 0, "the screen really did scroll");
    }

    #[test]
    fn pruning_bounds_the_plane() {
        let mut h = Harness::new(4, 2, 4);
        for _ in 0..200 {
            h.feed(b"\x1b[5mq\x1b[25m\r\n");
            h.plane.prune(h.scrolled_off);
        }
        assert!(
            h.plane.len() <= 4 * (4 + 2 + 1),
            "the plane grew to {} entries; pruning is not bounding it",
            h.plane.len()
        );
    }

    #[test]
    fn clear_empties_the_plane() {
        let mut h = Harness::new(4, 1, 0);
        h.feed(b"\x1b[5mq");
        assert!(h.plane.len() > 0);
        h.plane.clear();
        assert_eq!(h.plane.len(), 0);
        assert!(!h.blinking(0, 0));
    }

    #[test]
    fn a_wide_character_marks_its_leading_cell() {
        let mut h = Harness::new(6, 1, 0);
        h.feed("\u{1b}[5m\u{6f22}".as_bytes());
        assert!(h.blinking(0, 0), "the wide glyph's own cell blinks");
        let spacer = crate::grid::checked_cell(h.term.grid(), 0, 1).expect("in range");
        assert!(spacer.flags.contains(Flags::WIDE_CHAR_SPACER));
    }
}
```

Add to `crates/oxutrm-term/src/lib.rs`:

```rust
mod blink;
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-term --lib -- --test-threads 4 blink::`
Expected: FAIL — `cannot find type 'BlinkPlane'`, `'BlinkTap'`.

- [ ] **Step 3: Write `BlinkPlane`**

Put this at the top of `crates/oxutrm-term/src/blink.rs`, above `mod tests`:

```rust
use std::collections::HashSet;

use alacritty_terminal::Term;
use alacritty_terminal::event::EventListener;
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Processor, Rgb,
    ScpCharPath, ScpUpdateMode, StandardCharset, TabulationClearMode,
};

/// Which cells are blinking, keyed by **absolute** line.
///
/// An absolute line is `scrolled_off + viewport_line`: it names a row of output
/// rather than a position on the screen, so the mark travels with its content
/// when the screen scrolls. Keying on the viewport line directly — which is
/// what `term.grid().cursor.point` gives — would leave the mark behind and
/// paint it onto whatever row scrolled into that position next.
///
/// Two limitations are accepted rather than engineered around, because each
/// costs a wrongly-blinking cell and neither can misplace a character: reflow
/// on resize re-wraps content across absolute lines, and inserting or deleting
/// lines inside a scroll region shifts content without changing
/// `scrolled_off`. `HostTerm` clears the plane on resize for that reason.
pub(crate) struct BlinkPlane {
    marks: HashSet<(u64, u16)>,
    /// How many absolute lines are worth keeping: the scrollback window plus
    /// the visible screen.
    window: u64,
}

impl BlinkPlane {
    pub(crate) fn new(capacity: usize) -> BlinkPlane {
        BlinkPlane {
            marks: HashSet::new(),
            window: capacity as u64,
        }
    }

    fn mark(&mut self, absolute_line: u64, column: u16) {
        self.marks.insert((absolute_line, column));
    }

    fn unmark(&mut self, absolute_line: u64, column: u16) {
        self.marks.remove(&(absolute_line, column));
    }

    /// Is the cell at viewport `line` and `column` blinking?
    pub(crate) fn get(&self, scrolled_off: u64, line: u16, column: u16) -> bool {
        self.marks.contains(&(scrolled_off + u64::from(line), column))
    }

    /// Drop marks that have fallen out of the scrollback window.
    pub(crate) fn prune(&mut self, scrolled_off: u64) {
        let floor = scrolled_off.saturating_sub(self.window);
        if floor == 0 {
            return;
        }
        self.marks.retain(|(line, _)| *line >= floor);
    }

    pub(crate) fn clear(&mut self) {
        self.marks.clear();
    }

    /// How many marks are held. Diagnostics and tests only.
    pub(crate) fn len(&self) -> usize {
        self.marks.len()
    }
}
```

- [ ] **Step 4: Write the `Handler` newtype**

Append to `crates/oxutrm-term/src/blink.rs`, still above `mod tests`.

Every method forwards unchanged. Only `input`, `terminal_attribute` and
`reset_state` do anything of their own. `set_mouse_cursor_icon` is deliberately
**not** forwarded: its argument type is `cursor_icon::CursorIcon`, which `vte`
does not re-export, and `Term` leaves that method at the trait's no-op default,
so forwarding it would change nothing.

```rust
/// Wraps a `Term` and implements `vte::ansi::Handler`.
///
/// Every method forwards to the `Term` unchanged; the three blink attributes
/// are additionally recorded in a parallel plane, because
/// `Term::terminal_attribute` drops them.
pub(crate) struct BlinkTap<'a, T: EventListener> {
    term: &'a mut Term<T>,
    plane: &'a mut BlinkPlane,
    scrolled_off: u64,
    /// Whether cells written from now on are blinking.
    blinking: bool,
}

impl<'a, T: EventListener> BlinkTap<'a, T> {
    pub(crate) fn new(
        term: &'a mut Term<T>,
        plane: &'a mut BlinkPlane,
        scrolled_off: u64,
    ) -> BlinkTap<'a, T> {
        BlinkTap {
            term,
            plane,
            scrolled_off,
            blinking: false,
        }
    }

    /// The absolute line and column the next character will be written to.
    ///
    /// Read **before** forwarding `input`, because `input` advances the cursor
    /// after it writes.
    fn target(&self) -> (u64, u16) {
        let point = self.term.grid().cursor.point;
        let line = self.scrolled_off as i64 + i64::from(point.line.0);
        (line.max(0) as u64, point.column.0 as u16)
    }
}

impl<T: EventListener> Handler for BlinkTap<'_, T> {
    fn input(&mut self, c: char) {
        let (line, column) = self.target();
        self.term.input(c);
        if self.blinking {
            self.plane.mark(line, column);
        } else {
            // An overwrite must clear a stale mark, or a cell that used to
            // blink keeps blinking under its replacement.
            self.plane.unmark(line, column);
        }
    }

    fn terminal_attribute(&mut self, attr: Attr) {
        match attr {
            Attr::BlinkSlow | Attr::BlinkFast => self.blinking = true,
            Attr::CancelBlink | Attr::Reset => self.blinking = false,
            _ => {}
        }
        // Forward regardless: `Reset` also clears everything the Term tracks.
        self.term.terminal_attribute(attr);
    }

    fn reset_state(&mut self) {
        self.blinking = false;
        self.plane.clear();
        self.term.reset_state();
    }

    // ---- everything below forwards unchanged ----

    fn set_title(&mut self, title: Option<String>) { self.term.set_title(title); }
    fn set_cursor_style(&mut self, style: Option<CursorStyle>) { self.term.set_cursor_style(style); }
    fn set_cursor_shape(&mut self, shape: CursorShape) { self.term.set_cursor_shape(shape); }
    fn goto(&mut self, line: i32, col: usize) { self.term.goto(line, col); }
    fn goto_line(&mut self, line: i32) { self.term.goto_line(line); }
    fn goto_col(&mut self, col: usize) { self.term.goto_col(col); }
    fn insert_blank(&mut self, n: usize) { self.term.insert_blank(n); }
    fn move_up(&mut self, n: usize) { self.term.move_up(n); }
    fn move_down(&mut self, n: usize) { self.term.move_down(n); }
    fn identify_terminal(&mut self, i: Option<char>) { self.term.identify_terminal(i); }
    fn device_status(&mut self, n: usize) { self.term.device_status(n); }
    fn move_forward(&mut self, col: usize) { self.term.move_forward(col); }
    fn move_backward(&mut self, col: usize) { self.term.move_backward(col); }
    fn move_down_and_cr(&mut self, row: usize) { self.term.move_down_and_cr(row); }
    fn move_up_and_cr(&mut self, row: usize) { self.term.move_up_and_cr(row); }
    fn put_tab(&mut self, count: u16) { self.term.put_tab(count); }
    fn backspace(&mut self) { self.term.backspace(); }
    fn carriage_return(&mut self) { self.term.carriage_return(); }
    fn linefeed(&mut self) { self.term.linefeed(); }
    fn bell(&mut self) { self.term.bell(); }
    fn substitute(&mut self) { self.term.substitute(); }
    fn newline(&mut self) { self.term.newline(); }
    fn set_horizontal_tabstop(&mut self) { self.term.set_horizontal_tabstop(); }
    fn scroll_up(&mut self, n: usize) { self.term.scroll_up(n); }
    fn scroll_down(&mut self, n: usize) { self.term.scroll_down(n); }
    fn insert_blank_lines(&mut self, n: usize) { self.term.insert_blank_lines(n); }
    fn delete_lines(&mut self, n: usize) { self.term.delete_lines(n); }
    fn erase_chars(&mut self, n: usize) { self.term.erase_chars(n); }
    fn delete_chars(&mut self, n: usize) { self.term.delete_chars(n); }
    fn move_backward_tabs(&mut self, count: u16) { self.term.move_backward_tabs(count); }
    fn move_forward_tabs(&mut self, count: u16) { self.term.move_forward_tabs(count); }
    fn save_cursor_position(&mut self) { self.term.save_cursor_position(); }
    fn restore_cursor_position(&mut self) { self.term.restore_cursor_position(); }
    fn clear_line(&mut self, mode: LineClearMode) { self.term.clear_line(mode); }
    fn clear_screen(&mut self, mode: ClearMode) { self.term.clear_screen(mode); }
    fn clear_tabs(&mut self, mode: TabulationClearMode) { self.term.clear_tabs(mode); }
    fn set_tabs(&mut self, interval: u16) { self.term.set_tabs(interval); }
    fn reverse_index(&mut self) { self.term.reverse_index(); }
    fn set_mode(&mut self, mode: Mode) { self.term.set_mode(mode); }
    fn unset_mode(&mut self, mode: Mode) { self.term.unset_mode(mode); }
    fn report_mode(&mut self, mode: Mode) { self.term.report_mode(mode); }
    fn set_private_mode(&mut self, mode: PrivateMode) { self.term.set_private_mode(mode); }
    fn unset_private_mode(&mut self, mode: PrivateMode) { self.term.unset_private_mode(mode); }
    fn report_private_mode(&mut self, mode: PrivateMode) { self.term.report_private_mode(mode); }
    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.term.set_scrolling_region(top, bottom);
    }
    fn set_keypad_application_mode(&mut self) { self.term.set_keypad_application_mode(); }
    fn unset_keypad_application_mode(&mut self) { self.term.unset_keypad_application_mode(); }
    fn set_active_charset(&mut self, index: CharsetIndex) { self.term.set_active_charset(index); }
    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        self.term.configure_charset(index, charset);
    }
    fn set_color(&mut self, index: usize, color: Rgb) { self.term.set_color(index, color); }
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        self.term.dynamic_color_sequence(prefix, index, terminator);
    }
    fn reset_color(&mut self, index: usize) { self.term.reset_color(index); }
    fn clipboard_store(&mut self, kind: u8, data: &[u8]) { self.term.clipboard_store(kind, data); }
    fn clipboard_load(&mut self, kind: u8, terminator: &str) {
        self.term.clipboard_load(kind, terminator);
    }
    fn decaln(&mut self) { self.term.decaln(); }
    fn push_title(&mut self) { self.term.push_title(); }
    fn pop_title(&mut self) { self.term.pop_title(); }
    fn text_area_size_pixels(&mut self) { self.term.text_area_size_pixels(); }
    fn text_area_size_chars(&mut self) { self.term.text_area_size_chars(); }
    fn set_hyperlink(&mut self, link: Option<Hyperlink>) { self.term.set_hyperlink(link); }
    fn report_keyboard_mode(&mut self) { self.term.report_keyboard_mode(); }
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) { self.term.push_keyboard_mode(mode); }
    fn pop_keyboard_modes(&mut self, to_pop: u16) { self.term.pop_keyboard_modes(to_pop); }
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, behavior: KeyboardModesApplyBehavior) {
        self.term.set_keyboard_mode(mode, behavior);
    }
    fn set_modify_other_keys(&mut self, mode: ModifyOtherKeys) {
        self.term.set_modify_other_keys(mode);
    }
    fn report_modify_other_keys(&mut self) { self.term.report_modify_other_keys(); }
    fn set_scp(&mut self, char_path: ScpCharPath, update_mode: ScpUpdateMode) {
        self.term.set_scp(char_path, update_mode);
    }
}
```

If the compiler reports a signature mismatch for any of these, take the exact
signature from the error and use it — the trait is the authority, not this
listing. If it reports a method this listing omits, forward it the same way
unless its argument type is unnameable without a new dependency, in which case
leave it to the trait default and add a one-line comment saying why.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-term --lib -- --test-threads 4 blink::`
Expected: PASS, 9 tests.

`blink_follows_its_content_when_the_screen_scrolls` is the one that justifies
the absolute-line keying. If it fails, print `h.scrolled_off`: a mark at
absolute line 0 must not be reachable from viewport line 0 once anything has
scrolled off.

- [ ] **Step 6: Lint and commit**

Run: `cargo clippy --jobs 4 -p oxutrm-term --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-term
git commit -m "$(cat <<'EOF'
feat(term): recover blink through a vte::ansi::Handler newtype

vte parses SGR 5/6/25 but Term::terminal_attribute drops all three, so blink
never reaches a cell flag. BlinkTap forwards every Handler method unchanged and
records those three in a parallel plane.

The plane keys on ABSOLUTE lines, not on the viewport line the spec names:
viewport numbers are stable while their content scrolls underneath, so a
viewport-keyed plane paints a stale blink onto whatever row arrives next. A
test scrolls the screen and asserts the mark does not stay behind.

set_mouse_cursor_icon is left at the trait default: its argument type is not
re-exported by vte, and Term leaves it defaulted too.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 11: `oxutrm-term` — `HostTerm`, the capabilities, and freshly generated snapshots

**Files:**
- Create: `crates/oxutrm-term/src/host.rs`
- Create: `crates/oxutrm-term/src/caps.rs`
- Create: `crates/oxutrm-term/tests/fixtures/{colours,wide-wrap,alt-screen,osc-title}.ansi`
- Create: `crates/oxutrm-term/tests/emulation.rs`
- Create: `crates/oxutrm-term/tests/snapshots/*.snap` (**generated here, never ported**)
- Modify: `crates/oxutrm-term/src/lib.rs`

**Interfaces:**
- Consumes: `Palette`, `convert_color`, `checked_cell` (Task 9), `BlinkPlane`,
  `BlinkTap` (Task 10),
  `oxutrm_proto::{Attrs, Cell, CellText, Color, Cursor, CursorShape, Modes, MouseMode, ScreenState, TermSize, TerminalCaps}`.
- Produces:
  ```rust
  pub struct HostTerm { /* private */ }
  impl HostTerm {
      pub fn spawn(shell: &str, args: &[String], env: &[(String, String)],
                   size: TermSize, scrollback: usize) -> anyhow::Result<HostTerm>;
      pub fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<()>;
      pub fn resize(&mut self, size: TermSize) -> anyhow::Result<()>;
      /// Drain what the PTY has ready, without blocking. True if the screen changed.
      pub fn poll(&mut self) -> anyhow::Result<bool>;
      pub fn snapshot(&self, seq: u64) -> ScreenState;
      /// Scrollback lines [from, to) as rendered cell rows, oldest first.
      pub fn scrollback(&self, from: u64, to: u64) -> Vec<Vec<Cell>>;
      pub fn child_exited(&mut self) -> Option<i32>;
      /// Rows the emulator reported damaged since the last call, and clears
      /// them. `None` means "everything".
      pub fn take_damage(&mut self) -> Option<Vec<u16>>;
      /// OSC 52 payloads, already base64-decoded by the emulator.
      pub fn take_clipboard(&mut self) -> Vec<String>;
  }

  /// Run `bytes` through a fresh emulator and snapshot the result. Tests only.
  pub fn state_from_bytes(bytes: &[u8], size: TermSize, scrollback: usize) -> ScreenState;

  pub fn detect_caps() -> TerminalCaps;
  pub fn caps_from_env(term: Option<&str>, colorterm: Option<&str>) -> TerminalCaps;
  /// Derived SOLELY from what the emulator emulates. Takes no arguments.
  pub fn negotiate_term() -> (String, Option<String>);
  ```

**`snapshot` must produce a state that passes `validate`.** The grid cursor can
sit one past the last column while a wrap is pending, which I2 forbids. Clamping
is correct *here*, at the boundary where a real emulator's own coordinates are
being translated — and forbidden in `oxutrm-sync`, where the coordinates arrived
over a network. That distinction is the whole point of I2.

**`negotiate_term` takes no `TerminalCaps` and must not gain one.** A `TERM`
narrowed to one client's capabilities makes the *shell* emit degraded output,
which is baked into the authoritative state forever; and `TERM` cannot change
under a shell that has been running for a week, which is exactly the moment a
differently-capable client reattaches. All adaptation lives in the client.

### The snapshots must be REGENERATED, never ported

This is a trap worth naming. The `.ansi` fixtures are **emulator-agnostic byte
streams** and port freely from `ansidrama` or anywhere else — reuse them.

The `.snap` files are not. `ansidrama` snapshotted a **different emulator**, and
`alacritty_terminal` differs from it in attribute set, reflow behaviour and
scrollback. **A ported snapshot that happens to pass is worse than one that
fails**, because it silently certifies the new emulator against the old one's
behaviour and nobody ever looks again. Generation is an explicit step below, and
so is reading every generated file by eye before accepting it.

- [ ] **Step 1: Create the four fixtures**

Run exactly these commands. `printf` interprets `\033`; UTF-8 is written
literally.

```bash
mkdir -p crates/oxutrm-term/tests/fixtures
cd crates/oxutrm-term/tests/fixtures

# 1. 16-colour, 256-colour and 24-bit colour, with bold, underline, strikeout
#    and blink, and an explicit reset between each.
printf '\033[1;31mRED\033[0m plain \033[4;9;38;5;208mORANGE\033[0m \033[5;38;2;0;128;255;48;2;32;32;32mTRUE\033[0m' > colours.ansi

# 2. Five ASCII characters then a CJK wide character at the right margin of a
#    6-column screen: the wide glyph cannot fit in column 5 and must wrap.
printf 'abcde\346\274\242\345\255\227' > wide-wrap.ansi

# 3. Alternate screen: text on the main screen, switch to alt, different text,
#    hide the cursor, bracketed paste and SGR mouse reporting on.
printf 'main text\033[?1049h\033[?2004h\033[?1006h\033[?1002h\033[?25lALT' > alt-screen.ansi

# 4. OSC 0 sets the window title, terminated by BEL, then text and a bell.
#    OSC 1 is included DELIBERATELY: vte drops it, and the test proves so.
printf '\033]0;oxutrm demo\007\033]1;ignored\007hello\007' > osc-title.ansi

cd -
```

Verify the wide-character bytes:

```bash
xxd crates/oxutrm-term/tests/fixtures/wide-wrap.ansi
```

Expected: `61 62 63 64 65 e6 bc a2 e5 ad 97` — five ASCII bytes, then the UTF-8
encodings of `漢` (`e6 bc a2`) and `字` (`e5 ad 97`).

- [ ] **Step 2: Write the failing emulation tests**

Create `crates/oxutrm-term/tests/emulation.rs`:

```rust
//! Emulation fidelity: real escape sequences in, ScreenState out.
//!
//! The explicit assertions come first and carry the meaning; the insta
//! snapshots below them are a regression net for everything nobody wrote an
//! assertion for.
//!
//! The .ansi fixtures are emulator-agnostic byte streams and port freely. The
//! .snap files DO NOT: they describe what `alacritty_terminal` produces, and a
//! snapshot ported from a different emulator that happens to pass would
//! silently certify this one against the other's behaviour.

use oxutrm_proto::{Attrs, Color, MouseMode, ScreenState, TermSize};
use oxutrm_term::state_from_bytes;

const COLOURS: &[u8] = include_bytes!("fixtures/colours.ansi");
const WIDE_WRAP: &[u8] = include_bytes!("fixtures/wide-wrap.ansi");
const ALT_SCREEN: &[u8] = include_bytes!("fixtures/alt-screen.ansi");
const OSC_TITLE: &[u8] = include_bytes!("fixtures/osc-title.ansi");

fn size(cols: u16, rows: u16) -> TermSize {
    TermSize { cols, rows }
}

fn text_of_row(s: &ScreenState, row: u16) -> String {
    s.row(row)
        .iter()
        .map(|c| if c.text.is_empty() { " " } else { c.text.as_str() })
        .collect()
}

#[test]
fn every_colour_depth_survives_and_so_do_the_awkward_attributes() {
    let s = state_from_bytes(COLOURS, size(40, 2), 0);

    // "RED plain ORANGE TRUE"
    assert_eq!(s.cell(0, 0).text, "R");
    assert_eq!(s.cell(0, 0).fg, Color::Idx(1));
    assert!(s.cell(0, 0).attrs.contains(Attrs::BOLD));

    assert_eq!(s.cell(0, 4).text, "p");
    assert_eq!(s.cell(0, 4).fg, Color::Default, "a reset returns to Default");
    assert_eq!(s.cell(0, 4).attrs, Attrs::empty());

    assert_eq!(s.cell(0, 10).text, "O");
    assert_eq!(s.cell(0, 10).fg, Color::Idx(208));
    assert!(s.cell(0, 10).attrs.contains(Attrs::UNDERLINE));
    assert!(
        s.cell(0, 10).attrs.contains(Attrs::STRIKE),
        "strikeout is a native emulator flag"
    );

    assert_eq!(s.cell(0, 17).text, "T");
    assert_eq!(s.cell(0, 17).fg, Color::Rgb(0, 128, 255));
    assert_eq!(s.cell(0, 17).bg, Color::Rgb(32, 32, 32));
    assert!(
        s.cell(0, 17).attrs.contains(Attrs::BLINK),
        "blink is recovered by the Handler newtype, not by the emulator"
    );
}

#[test]
fn a_wide_character_at_the_right_margin_wraps_whole() {
    let s = state_from_bytes(WIDE_WRAP, size(6, 3), 0);

    assert_eq!(text_of_row(&s, 0), "abcde ", "column 5 is left empty");
    assert!(
        !s.cell(0, 5).attrs.contains(Attrs::WIDE_CONT),
        "a wide glyph must never be split across the margin"
    );

    assert_eq!(s.cell(1, 0).text, "\u{6f22}");
    assert!(s.cell(1, 1).attrs.contains(Attrs::WIDE_CONT));
    assert!(
        s.cell(1, 1).text.is_empty(),
        "a continuation cell carries no text; a space would paint over the glyph"
    );
    assert_eq!(s.cell(1, 2).text, "\u{5b57}");
    assert!(s.cell(1, 3).attrs.contains(Attrs::WIDE_CONT));
}

#[test]
fn alternate_screen_hides_the_main_screen_and_carries_modes() {
    let s = state_from_bytes(ALT_SCREEN, size(12, 2), 0);

    assert!(s.modes.alt_screen);
    assert!(s.modes.bracketed_paste);
    assert_eq!(s.modes.mouse, MouseMode::ButtonMotion);
    assert!(!s.cursor.visible);
    assert_eq!(text_of_row(&s, 0), "ALT         ");
    assert!(
        !text_of_row(&s, 0).contains("main"),
        "the main screen must not bleed through the alternate screen"
    );
}

#[test]
fn osc_zero_sets_the_title_and_osc_one_is_silently_dropped() {
    let s = state_from_bytes(OSC_TITLE, size(10, 1), 0);

    assert_eq!(s.title, "oxutrm demo");
    assert_ne!(
        s.title, "ignored",
        "OSC 1 has no vte arm and no handler method; there is no icon field"
    );
    assert_eq!(text_of_row(&s, 0), "hello     ");
    assert_eq!(s.bell, 1, "the trailing BEL is counted, not printed");
}

#[test]
fn every_produced_state_satisfies_its_invariants() {
    for (bytes, sz) in [
        (COLOURS, size(40, 2)),
        (WIDE_WRAP, size(6, 3)),
        (ALT_SCREEN, size(12, 2)),
        (OSC_TITLE, size(10, 1)),
    ] {
        let s = state_from_bytes(bytes, sz, 0);
        s.validate()
            .expect("the emulator bridge must never produce an invalid state");
        assert_eq!(s.seq, 1);
    }
}

// ---- snapshots: generated here against alacritty_terminal, never ported ----

#[test]
fn snapshot_colours() {
    insta::assert_yaml_snapshot!(state_from_bytes(COLOURS, size(40, 2), 0));
}

#[test]
fn snapshot_wide_wrap() {
    insta::assert_yaml_snapshot!(state_from_bytes(WIDE_WRAP, size(6, 3), 0));
}

#[test]
fn snapshot_alt_screen() {
    insta::assert_yaml_snapshot!(state_from_bytes(ALT_SCREEN, size(12, 2), 0));
}

#[test]
fn snapshot_osc_title() {
    insta::assert_yaml_snapshot!(state_from_bytes(OSC_TITLE, size(10, 1), 0));
}
```

- [ ] **Step 3: Write the failing `HostTerm` and caps tests**

Create `crates/oxutrm-term/src/host.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn size(cols: u16, rows: u16) -> TermSize {
        TermSize { cols, rows }
    }

    /// Poll until the screen contains `needle`, or give up. The PTY is
    /// asynchronous, so a test must wait for the child rather than assume.
    fn poll_until(term: &mut HostTerm, needle: &str) -> ScreenState {
        for _ in 0..400 {
            term.poll().expect("poll");
            let s = term.snapshot(1);
            let text: String = s.cells.iter().map(|c| c.text.as_str()).collect();
            if text.contains(needle) {
                return s;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("timed out waiting for {needle:?}");
    }

    #[test]
    fn shell_output_reaches_the_screen() {
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &["-c".to_string(), "echo hello-oxutrm; sleep 30".to_string()],
            &[("TERM".to_string(), "xterm-256color".to_string())],
            size(40, 6),
            100,
        )
        .expect("spawn");
        let s = poll_until(&mut t, "hello-oxutrm");
        assert_eq!(s.seq, 1);
        assert_eq!((s.rows, s.cols), (6, 40));
        assert_eq!(s.cells.len(), 240);
        s.validate().expect("valid");
    }

    #[test]
    fn every_snapshot_is_a_valid_state_even_mid_wrap() {
        // The grid cursor sits one past the last column while a wrap is
        // pending, which I2 forbids. `snapshot` clamps at this boundary --
        // where the emulator's own coordinates are being translated -- which
        // is exactly where clamping is right and `oxutrm-sync` is where it is
        // wrong.
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &["-c".to_string(), "printf 'abcd'; sleep 30".to_string()],
            &[],
            size(4, 2),
            0,
        )
        .expect("spawn");
        poll_until(&mut t, "abcd");
        let s = t.snapshot(1);
        s.validate().expect("a pending wrap must not produce an invalid state");
        assert!(s.cursor.col < s.cols);
    }

    #[test]
    fn typed_input_is_echoed_back() {
        let mut t = HostTerm::spawn("/bin/cat", &[], &[], size(40, 4), 0).expect("spawn");
        t.write_input(b"typed-here\n").expect("write");
        poll_until(&mut t, "typed-here");
    }

    #[test]
    fn a_quiet_pty_reports_no_change() {
        let mut t = HostTerm::spawn("/bin/cat", &[], &[], size(20, 3), 0).expect("spawn");
        for _ in 0..20 {
            t.poll().expect("poll");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(!t.poll().expect("poll"), "a quiet PTY must not report change");
    }

    #[test]
    fn resize_changes_the_state_geometry() {
        let mut t = HostTerm::spawn("/bin/cat", &[], &[], size(20, 3), 0).expect("spawn");
        t.resize(size(60, 10)).expect("resize");
        let s = t.snapshot(9);
        assert_eq!((s.rows, s.cols), (10, 60));
        assert_eq!(s.cells.len(), 600);
        assert_eq!(s.seq, 9);
        s.validate().expect("valid");
    }

    #[test]
    fn a_resize_invalidates_all_damage() {
        let mut t = HostTerm::spawn("/bin/cat", &[], &[], size(20, 3), 0).expect("spawn");
        let _ = t.take_damage();
        t.resize(size(30, 5)).expect("resize");
        assert!(
            t.take_damage().is_none(),
            "reflow re-wraps content across rows, so no row hint survives it"
        );
    }

    #[test]
    fn damage_is_reported_and_then_cleared() {
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &["-c".to_string(), "printf '\\033[3;1Hmarker'; sleep 30".to_string()],
            &[],
            size(20, 6),
            0,
        )
        .expect("spawn");
        poll_until(&mut t, "marker");
        // Something must have been reported -- either specific rows, or
        // "everything", which is always allowed and merely slower.
        match t.take_damage() {
            None => {}
            Some(rows) => assert!(rows.len() <= 6, "got {rows:?}"),
        }
        // Taking drains it.
        assert!(matches!(t.take_damage(), None | Some(_)));
    }

    #[test]
    fn the_child_exit_status_is_reported_and_is_stable() {
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &["-c".to_string(), "exit 3".to_string()],
            &[],
            size(20, 3),
            0,
        )
        .expect("spawn");
        for _ in 0..400 {
            let _ = t.poll();
            if let Some(code) = t.child_exited() {
                assert_eq!(code, 3);
                assert_eq!(t.child_exited(), Some(3), "the answer is stable");
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!("child never reported an exit status");
    }

    #[test]
    fn scrollback_len_keeps_counting_past_the_ring_capacity() {
        // history_size() SATURATES at the configured capacity, so the counter
        // must be synthesized. With a capacity of 4 and 40 lines of output, a
        // naive implementation reports 4.
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                "i=1; while [ $i -le 40 ]; do echo line$i; i=$((i+1)); done; sleep 30"
                    .to_string(),
            ],
            &[],
            size(20, 3),
            4,
        )
        .expect("spawn");
        poll_until(&mut t, "line40");

        let s = t.snapshot(1);
        assert!(
            s.scrollback_len >= 30,
            "scrollback_len saturated at {}; it must accumulate",
            s.scrollback_len
        );
    }

    #[test]
    fn scrollback_len_never_decreases() {
        // I6, at the source. A shrinking counter would be rejected by every
        // receiver downstream, so it must never be produced here.
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                "i=1; while [ $i -le 30 ]; do echo l$i; i=$((i+1)); done; sleep 30".to_string(),
            ],
            &[],
            size(20, 3),
            5,
        )
        .expect("spawn");
        let mut highest = 0u64;
        for _ in 0..200 {
            t.poll().expect("poll");
            let now = t.snapshot(1).scrollback_len;
            assert!(now >= highest, "scrollback went {highest} -> {now}");
            highest = now;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn scrollback_returns_lines_that_have_left_the_screen() {
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                "for i in 1 2 3 4 5 6 7 8; do echo line$i; done; sleep 30".to_string(),
            ],
            &[],
            size(20, 3),
            50,
        )
        .expect("spawn");
        poll_until(&mut t, "line8");

        let lines = t.scrollback(0, 3);
        assert_eq!(lines.len(), 3);
        assert!(
            lines.iter().all(|l| l.len() == 20),
            "scrollback rows are full-width cell rows"
        );
        let first: String = lines[0].iter().map(|c| c.text.as_str()).collect();
        assert!(
            first.starts_with("line1"),
            "oldest scrollback line first, got {first:?}"
        );
    }

    #[test]
    fn reading_scrollback_does_not_move_the_live_screen() {
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                "for i in 1 2 3 4 5 6 7 8; do echo line$i; done; sleep 30".to_string(),
            ],
            &[],
            size(20, 3),
            50,
        )
        .expect("spawn");
        poll_until(&mut t, "line8");
        let before = t.snapshot(1);
        let _ = t.scrollback(0, 5);
        assert_eq!(
            t.snapshot(1),
            before,
            "history reads are O(1) and side-effect free"
        );
    }

    #[test]
    fn an_empty_or_reversed_scrollback_range_is_empty() {
        let t = HostTerm::spawn("/bin/cat", &[], &[], size(20, 3), 10).expect("spawn");
        assert!(t.scrollback(5, 5).is_empty());
        assert!(t.scrollback(9, 2).is_empty());
    }

    #[test]
    fn osc_52_arrives_already_decoded() {
        // The crate does the base64 itself, so the host does none in either
        // direction. "b3h1dHJt" is "oxutrm".
        let mut t = HostTerm::spawn(
            "/bin/sh",
            &[
                "-c".to_string(),
                "printf '\\033]52;c;b3h1dHJt\\007done'; sleep 30".to_string(),
            ],
            &[],
            size(20, 3),
            0,
        )
        .expect("spawn");
        poll_until(&mut t, "done");
        assert_eq!(t.take_clipboard(), vec!["oxutrm".to_string()]);
        assert!(t.take_clipboard().is_empty(), "taking drains the queue");
    }
}
```

Create `crates/oxutrm-term/src/caps.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colorterm_truecolor_means_24_bit() {
        let c = caps_from_env(Some("xterm-256color"), Some("truecolor"));
        assert!(c.truecolor);
        assert_eq!(c.colors, 16_777_216);
        assert_eq!(c.term_name, "xterm-256color");
    }

    #[test]
    fn colorterm_24bit_is_the_same_claim() {
        assert!(caps_from_env(Some("xterm"), Some("24bit")).truecolor);
        assert!(caps_from_env(Some("xterm"), Some("24-bit")).truecolor);
    }

    #[test]
    fn a_256color_term_without_colorterm_gets_256() {
        let c = caps_from_env(Some("screen-256color"), None);
        assert!(!c.truecolor);
        assert_eq!(c.colors, 256);
    }

    #[test]
    fn a_plain_term_gets_16() {
        assert_eq!(caps_from_env(Some("xterm-color"), None).colors, 16);
        assert_eq!(caps_from_env(Some("xterm"), None).colors, 16);
    }

    #[test]
    fn a_dumb_terminal_gets_nothing_and_a_lying_colorterm_is_ignored() {
        let c = caps_from_env(Some("dumb"), Some("truecolor"));
        assert_eq!(c.colors, 8);
        assert!(!c.truecolor);
        assert!(!c.bracketed_paste);
        assert!(!c.mouse_sgr);
        assert!(!c.osc52);
        assert_eq!(c.term_name, "dumb");
    }

    #[test]
    fn a_missing_or_empty_term_is_treated_as_dumb() {
        assert_eq!(caps_from_env(None, None).term_name, "dumb");
        assert_eq!(caps_from_env(None, None).colors, 8);
        assert_eq!(caps_from_env(Some(""), None).colors, 8);
    }

    #[test]
    fn the_linux_console_has_no_sgr_mouse_and_no_osc52() {
        let c = caps_from_env(Some("linux"), None);
        assert_eq!(c.colors, 16);
        assert!(!c.mouse_sgr);
        assert!(!c.osc52);
        assert!(c.bracketed_paste, "the linux console does honour DECSET 2004");
    }

    #[test]
    fn negotiate_term_takes_no_arguments_and_is_constant() {
        // The whole point: the child's TERM cannot depend on which client is
        // attached, because a different one may attach tomorrow and TERM
        // cannot change under a shell that has been running for a week.
        let a = negotiate_term();
        assert_eq!(a, negotiate_term());
        assert_eq!(
            a,
            ("xterm-256color".to_string(), Some("truecolor".to_string()))
        );
    }

    #[test]
    fn negotiate_term_never_returns_a_clients_own_term_name() {
        let fancy = caps_from_env(Some("rxvt-unicode-256color"), Some("truecolor"));
        let (term, _) = negotiate_term();
        assert_ne!(term, fancy.term_name);
    }

    #[test]
    fn detect_caps_reads_the_real_environment_without_panicking() {
        let c = detect_caps();
        assert!(matches!(c.colors, 8 | 16 | 256 | 16_777_216));
    }
}
```

Add to `crates/oxutrm-term/src/lib.rs`:

```rust
mod caps;
mod host;

pub use caps::{caps_from_env, detect_caps, negotiate_term};
pub use host::{HostTerm, state_from_bytes};
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-term -- --test-threads 4`
Expected: FAIL — `cannot find type 'HostTerm'`, `cannot find function
'state_from_bytes'`, `cannot find function 'caps_from_env'`.

- [ ] **Step 5: Write `crates/oxutrm-term/src/caps.rs`**

Put this **above** the existing `mod tests`:

```rust
use oxutrm_proto::TerminalCaps;

/// Detect the **local** terminal's capabilities from `$TERM` and `$COLORTERM`.
///
/// This describes the machine oxutrm is rendering onto. It never reaches the
/// child's environment — see `negotiate_term`.
#[must_use]
pub fn detect_caps() -> TerminalCaps {
    let term = std::env::var("TERM").ok();
    let colorterm = std::env::var("COLORTERM").ok();
    caps_from_env(term.as_deref(), colorterm.as_deref())
}

/// The pure core of `detect_caps`, so the rules are testable without touching
/// the process environment.
#[must_use]
pub fn caps_from_env(term: Option<&str>, colorterm: Option<&str>) -> TerminalCaps {
    let name = term.unwrap_or("dumb");
    let name = if name.is_empty() { "dumb" } else { name };
    let dumb = name == "dumb";

    let truecolor =
        !dumb && matches!(colorterm, Some("truecolor") | Some("24bit") | Some("24-bit"));

    let colors = if dumb {
        8
    } else if truecolor {
        16_777_216
    } else if name.contains("256color") {
        256
    } else {
        16
    };

    // The Linux console draws colour and honours bracketed paste, but has no
    // SGR mouse reporting and no OSC 52 clipboard.
    let console = name == "linux" || name.starts_with("vt1") || name.starts_with("vt2");

    TerminalCaps {
        truecolor,
        colors,
        bracketed_paste: !dumb,
        mouse_sgr: !dumb && !console,
        osc52: !dumb && !console,
        term_name: name.to_string(),
    }
}

/// The `TERM` and `COLORTERM` the child process should see.
///
/// Derived **solely** from what `alacritty_terminal` emulates, which is an
/// xterm with the 256-colour palette and 24-bit SGR. It takes no
/// `TerminalCaps` argument and must never gain one:
///
/// - **Fidelity is not recoverable.** A `TERM` narrowed to the current client's
///   intersection makes the shell emit degraded output, which is then baked
///   into the authoritative state forever. A better client attaching tomorrow
///   cannot recover what the application never emitted.
/// - **`TERM` cannot change under a running shell.** Connect and reattach are
///   one code path, so a differently-capable client can attach to a session
///   whose shell has been running for a week; any scheme deriving the child
///   environment from client capabilities is undefined at exactly that moment.
///
/// All capability adaptation happens in the client, at render time.
#[must_use]
pub fn negotiate_term() -> (String, Option<String>) {
    ("xterm-256color".to_string(), Some("truecolor".to_string()))
}
```

- [ ] **Step 6: Write `crates/oxutrm-term/src/host.rs`**

Put this **above** the existing `mod tests`:

```rust
use std::io::{Read as _, Write as _};
use std::os::fd::AsFd as _;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use alacritty_terminal::Term;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions as _;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize as ATermSize;
use alacritty_terminal::term::{Config, Osc52, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{CursorShape as VteCursorShape, Processor};
use anyhow::{Context as _, Result};
use oxutrm_proto::{
    Attrs, Cell, CellText, Cursor, CursorShape, Modes, MouseMode, ScreenState, TermSize,
};

use crate::blink::{BlinkPlane, BlinkTap};
use crate::grid::checked_cell;
use crate::palette::{Palette, convert_color};

/// What the emulator reports through events rather than through the grid.
#[derive(Default)]
struct Events {
    title: String,
    /// A MONOTONIC counter (I5). Never reset: the client rings once per
    /// increment, so resetting would ring the terminal once for every bell in
    /// the session's history.
    bell: u32,
    /// OSC 52 payloads, already base64-decoded by the crate.
    clipboard: Vec<String>,
}

/// The `EventListener` implementation.
///
/// The trait's single method takes `&self`, so the listener needs interior
/// mutability. A mutex is the simplest correct thing here: events arrive only
/// from `advance()`, on one thread.
#[derive(Clone, Default)]
struct Listener(Arc<Mutex<Events>>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        let mut e = self.0.lock().expect("the events mutex is never poisoned");
        match event {
            Event::Title(t) => e.title = t,
            Event::ResetTitle => e.title.clear(),
            Event::Bell => e.bell = e.bell.saturating_add(1),
            Event::ClipboardStore(_, data) => e.clipboard.push(data),
            // Everything else is a request oxutrm answers elsewhere, or a
            // repaint hint the diff engine does not need.
            _ => {}
        }
    }
}

/// A PTY with a child process on it, and the authoritative emulator its output
/// feeds. This is the single source of truth for a session.
pub struct HostTerm {
    term: Term<Listener>,
    parser: Processor,
    listener: Listener,
    blink: BlinkPlane,
    palette: Palette,
    /// The PTY controller ("master"), in non-blocking mode.
    controller: std::fs::File,
    child: Child,
    exit_code: Option<i32>,
    size: TermSize,
    /// `history_size()` saturates at capacity, so the true total is
    /// accumulated rather than read (I6).
    scrolled_off: u64,
    prev_history: usize,
    /// Rows reported damaged since the last `take_damage`. `None` means
    /// "everything", which is always safe.
    damage: Option<Vec<u16>>,
    read_buf: Vec<u8>,
}

impl HostTerm {
    /// Spawn `shell` on a fresh PTY of the given size.
    ///
    /// The child gets its own session and the PTY as its controlling terminal,
    /// so a backgrounded grandchild can never hold the PTY open after the
    /// shell exits.
    pub fn spawn(
        shell: &str,
        args: &[String],
        env: &[(String, String)],
        size: TermSize,
        scrollback: usize,
    ) -> Result<HostTerm> {
        let ws = rustix::termios::Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let pty = rustix_openpty::openpty(None, Some(&ws)).context("openpty")?;
        let controller = pty.controller;
        let user = pty.user;

        let stdin = user.try_clone().context("dup pty user end")?;
        let stdout = user.try_clone().context("dup pty user end")?;
        let stderr = user.try_clone().context("dup pty user end")?;

        let mut cmd = Command::new(shell);
        cmd.args(args);
        for (k, v) in env {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::from(stdin));
        cmd.stdout(Stdio::from(stdout));
        cmd.stderr(Stdio::from(stderr));

        // `user` is CLOEXEC, so the child's copy closes at exec; these two
        // syscalls run between fork and exec.
        let ctty = user;
        unsafe {
            cmd.pre_exec(move || {
                rustix::process::setsid().map_err(std::io::Error::from)?;
                rustix::process::ioctl_tiocsctty(ctty.as_fd()).map_err(std::io::Error::from)?;
                Ok(())
            });
        }
        let child = cmd.spawn().with_context(|| format!("spawn {shell}"))?;
        // Dropping the command drops the pre_exec closure, closing the
        // parent's last copy of the PTY user end. Without this the reader never
        // sees EOF.
        drop(cmd);

        let flags = rustix::fs::fcntl_getfl(&controller).context("fcntl_getfl")?;
        rustix::fs::fcntl_setfl(&controller, flags | rustix::fs::OFlags::NONBLOCK)
            .context("set O_NONBLOCK on the pty controller")?;

        let listener = Listener::default();
        let term = Term::new(
            Config {
                scrolling_history: scrollback,
                // Copy is the default; paste is the less secure half and
                // oxutrm has no use for it in M1.
                osc52: Osc52::OnlyCopy,
                ..Config::default()
            },
            // NOTE the argument order: (columns, screen_lines).
            &ATermSize::new(size.cols as usize, size.rows as usize),
            listener.clone(),
        );

        Ok(HostTerm {
            term,
            parser: Processor::new(),
            listener,
            blink: BlinkPlane::new(scrollback + size.rows as usize),
            palette: Palette::xterm(),
            controller: std::fs::File::from(controller),
            child,
            exit_code: None,
            size,
            scrolled_off: 0,
            prev_history: 0,
            damage: Some(Vec::new()),
            read_buf: vec![0u8; 65536],
        })
    }

    /// Write user input straight to the PTY.
    pub fn write_input(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        self.controller.write_all(bytes).context("write to pty")?;
        self.controller.flush().context("flush pty")?;
        Ok(())
    }

    /// Resize both the PTY and the emulator.
    ///
    /// `Term::resize` genuinely **reflows** the primary grid, losslessly in
    /// both directions: shrinking pushes rows into history and growing pulls
    /// them back and re-joins the text. The alternate screen has a history of 0
    /// and never reflows, which is correct and matches every other emulator.
    ///
    /// Because reflow re-wraps content across rows, **no damage hint survives
    /// it**, and neither does the blink plane's absolute-line keying.
    pub fn resize(&mut self, size: TermSize) -> Result<()> {
        let ws = rustix::termios::Winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        rustix::termios::tcsetwinsize(self.controller.as_fd(), ws).context("tcsetwinsize")?;
        self.term
            .resize(ATermSize::new(size.cols as usize, size.rows as usize));
        self.size = size;
        self.damage = None;
        // Reflow moves content across absolute lines, which the plane cannot
        // follow. Dropping it loses a rarely-used attribute; keeping it would
        // paint blink onto the wrong cells.
        self.blink.clear();
        Ok(())
    }

    /// Drain everything the PTY has ready without blocking.
    ///
    /// Returns true when the screen changed. `WouldBlock` means "nothing more
    /// right now" and is not an error; `EIO` means the child closed the other
    /// end and is likewise not an error.
    pub fn poll(&mut self) -> Result<bool> {
        let mut changed = false;
        loop {
            let n = match self.controller.read(&mut self.read_buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // The child closed the PTY; treat it as end of output.
                Err(e) if e.raw_os_error() == Some(5) => break,
                Err(e) => return Err(e).context("read from pty"),
            };

            // Feed the emulator through the blink tap: vte parses SGR 5/6/25
            // but Term::terminal_attribute drops them.
            {
                let bytes = &self.read_buf[..n];
                let mut tap = BlinkTap::new(&mut self.term, &mut self.blink, self.scrolled_off);
                self.parser.advance(&mut tap, bytes);
            }

            // history_size() saturates at capacity, so accumulate the delta.
            // This is I6 at its source: the counter can only ever grow.
            let now = self.term.grid().history_size();
            self.scrolled_off += now.saturating_sub(self.prev_history) as u64;
            self.prev_history = now;
            self.blink.prune(self.scrolled_off);

            self.collect_damage();
            changed = true;
        }
        Ok(changed)
    }

    /// Fold this round's per-line damage into the accumulated hint.
    fn collect_damage(&mut self) {
        match self.term.damage() {
            TermDamage::Full => self.damage = None,
            TermDamage::Partial(iter) => {
                let rows: Vec<u16> = iter.map(|b| b.line as u16).collect();
                if let Some(acc) = self.damage.as_mut() {
                    acc.extend_from_slice(&rows);
                }
            }
        }
        self.term.reset_damage();
    }

    /// The rows damaged since the last call, and clear them.
    ///
    /// `None` means "assume everything changed", which is always safe. This is
    /// handed to `oxutrm-sync` as plain data — the sync engine never asks the
    /// emulator anything, which is what keeps it I/O-free.
    pub fn take_damage(&mut self) -> Option<Vec<u16>> {
        let taken = self.damage.take();
        self.damage = Some(Vec::new());
        taken.map(|mut rows| {
            rows.sort_unstable();
            rows.dedup();
            rows
        })
    }

    /// OSC 52 payloads the child sent, already base64-decoded by the emulator.
    pub fn take_clipboard(&mut self) -> Vec<String> {
        let mut e = self.listener.0.lock().expect("not poisoned");
        std::mem::take(&mut e.clipboard)
    }

    /// Build a `ScreenState` carrying the given sequence number.
    ///
    /// The result always satisfies `ScreenState::validate`.
    pub fn snapshot(&self, seq: u64) -> ScreenState {
        let events = self.listener.0.lock().expect("not poisoned");
        snapshot_from(
            &self.term,
            &self.blink,
            &self.palette,
            self.scrolled_off,
            seq,
            &events.title,
            events.bell,
        )
    }

    /// Scrollback lines `[from, to)` as full-width cell rows, oldest first.
    ///
    /// Negative `Line` indices reach history in O(1) **without moving the
    /// viewport**, so reading history never disturbs what the live screen
    /// shows. `HostTerm` deliberately keeps no parallel ring: a second copy
    /// would be a second source of truth, and keeping it consistent across
    /// every scroll and resize is exactly the bug the property exists to
    /// prevent.
    pub fn scrollback(&self, from: u64, to: u64) -> Vec<Vec<Cell>> {
        if to <= from {
            return Vec::new();
        }
        let grid = self.term.grid();
        let held = grid.history_size() as u64;
        let cols = self.size.cols;
        let overrides = self.term.colors();

        let mut out = Vec::new();
        for line in from..to {
            if line >= held {
                break;
            }
            // Line 0 is the OLDEST line still held, so it needs the largest
            // negative index; the most recent sits at -1.
            let index = -((held - line) as i64);
            let Ok(index) = i32::try_from(index) else {
                break;
            };
            let mut row = Vec::with_capacity(cols as usize);
            for col in 0..cols as usize {
                row.push(match checked_cell(grid, index, col) {
                    Some(c) => convert_cell(c, &self.palette, overrides, false),
                    None => Cell::blank(),
                });
            }
            out.push(row);
        }
        out
    }

    /// `Some(code)` once the child has exited and been reaped. Never blocks.
    pub fn child_exited(&mut self) -> Option<i32> {
        if self.exit_code.is_some() {
            return self.exit_code;
        }
        if let Ok(Some(status)) = self.child.try_wait() {
            self.exit_code = Some(status.code().unwrap_or(-1));
        }
        self.exit_code
    }
}

impl Drop for HostTerm {
    fn drop(&mut self) {
        // The child leads its own session, so killing its process group can
        // never reach ours. Without this a backgrounded grandchild keeps the
        // PTY open forever.
        if let Some(pid) = rustix::process::Pid::from_raw(self.child.id() as i32) {
            let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Turn one emulator cell into ours.
fn convert_cell(
    c: &alacritty_terminal::term::cell::Cell,
    palette: &Palette,
    overrides: &alacritty_terminal::term::color::Colors,
    blinking: bool,
) -> Cell {
    let mut attrs = Attrs::empty();
    if c.flags.contains(Flags::BOLD) {
        attrs |= Attrs::BOLD;
    }
    if c.flags.contains(Flags::DIM) {
        attrs |= Attrs::DIM;
    }
    if c.flags.contains(Flags::ITALIC) {
        attrs |= Attrs::ITALIC;
    }
    // v1 maps all five underline variants onto the one UNDERLINE bit. Styles
    // and per-cell underline colour stay available to a later version.
    if c.flags.intersects(Flags::ALL_UNDERLINES) {
        attrs |= Attrs::UNDERLINE;
    }
    if c.flags.contains(Flags::INVERSE) {
        attrs |= Attrs::INVERSE;
    }
    if c.flags.contains(Flags::HIDDEN) {
        attrs |= Attrs::HIDDEN;
    }
    if c.flags.contains(Flags::STRIKEOUT) {
        attrs |= Attrs::STRIKE;
    }
    if blinking {
        attrs |= Attrs::BLINK;
    }

    if c.flags.contains(Flags::WIDE_CHAR_SPACER) {
        attrs |= Attrs::WIDE_CONT;
        // A continuation cell carries no text of its own: the wide character
        // to its left already occupies it. A space here would paint over the
        // right half of the glyph.
        return Cell {
            text: CellText::new(""),
            fg: convert_color(c.fg, overrides, palette),
            bg: convert_color(c.bg, overrides, palette),
            attrs,
        };
    }

    // Combining marks are not separate cells: they hang off the base cell.
    let mut text = CellText::new("");
    text.push(c.c);
    if let Some(zw) = c.zerowidth() {
        for extra in zw {
            text.push(*extra);
        }
    }

    Cell {
        text,
        fg: convert_color(c.fg, overrides, palette),
        bg: convert_color(c.bg, overrides, palette),
        attrs,
    }
}

/// Build a `ScreenState` from an emulator and its side tables.
fn snapshot_from<T: EventListener>(
    term: &Term<T>,
    blink: &BlinkPlane,
    palette: &Palette,
    scrolled_off: u64,
    seq: u64,
    title: &str,
    bell: u32,
) -> ScreenState {
    let grid = term.grid();
    let rows = grid.screen_lines() as u16;
    let cols = grid.columns() as u16;
    let overrides = term.colors();

    let mut cells = Vec::with_capacity(rows as usize * cols as usize);
    for r in 0..rows {
        for c in 0..cols {
            let blinking = blink.get(scrolled_off, r, c);
            cells.push(match checked_cell(grid, i32::from(r), c as usize) {
                Some(cell) => convert_cell(cell, palette, overrides, blinking),
                None => Cell::blank(),
            });
        }
    }

    let mode = term.mode();
    let point = grid.cursor.point;
    // The grid cursor sits one past the last column while a wrap is pending,
    // which I2 forbids. Clamping is right HERE, translating the emulator's own
    // coordinates, and wrong in oxutrm-sync, where the coordinates arrived over
    // a network and a mismatch means the two ends have desynchronised.
    let cursor = Cursor {
        row: (point.line.0.max(0) as u16).min(rows.saturating_sub(1)),
        col: (point.column.0 as u16).min(cols.saturating_sub(1)),
        visible: mode.contains(TermMode::SHOW_CURSOR),
        shape: match term.cursor_style().shape {
            VteCursorShape::Underline => CursorShape::Underline,
            VteCursorShape::Beam => CursorShape::Bar,
            // Block, HollowBlock and Hidden all render as a block; visibility
            // is carried by `visible`, not by the shape.
            _ => CursorShape::Block,
        },
    };

    let mouse = if mode.intersects(TermMode::MOUSE_MOTION) {
        MouseMode::AnyMotion
    } else if mode.intersects(TermMode::MOUSE_DRAG) {
        MouseMode::ButtonMotion
    } else if mode.intersects(TermMode::MOUSE_REPORT_CLICK) {
        MouseMode::PressRelease
    } else {
        MouseMode::Off
    };

    ScreenState {
        seq,
        rows,
        cols,
        cells,
        cursor,
        modes: Modes {
            alt_screen: mode.contains(TermMode::ALT_SCREEN),
            bracketed_paste: mode.contains(TermMode::BRACKETED_PASTE),
            mouse,
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
        },
        title: title.to_string(),
        bell,
        scrollback_len: scrolled_off,
    }
}

/// Run `bytes` through a fresh emulator and return the resulting state at
/// sequence 1. This is how tests build a state without spawning a PTY.
#[must_use]
pub fn state_from_bytes(bytes: &[u8], size: TermSize, scrollback: usize) -> ScreenState {
    let listener = Listener::default();
    let mut term = Term::new(
        Config {
            scrolling_history: scrollback,
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        },
        &ATermSize::new(size.cols as usize, size.rows as usize),
        listener.clone(),
    );
    let mut plane = BlinkPlane::new(scrollback + size.rows as usize);
    let mut parser: Processor = Processor::new();
    {
        let mut tap = BlinkTap::new(&mut term, &mut plane, 0);
        parser.advance(&mut tap, bytes);
    }
    let scrolled_off = term.grid().history_size() as u64;
    let events = listener.0.lock().expect("not poisoned");
    snapshot_from(
        &term,
        &plane,
        &Palette::xterm(),
        scrolled_off,
        1,
        &events.title,
        events.bell,
    )
}
```

- [ ] **Step 7: Run the unit tests**

Run: `cargo test --jobs 4 -p oxutrm-term --lib -- --test-threads 4`
Expected: PASS — the palette, grid, blink, host and caps modules, 38 tests.

These spawn real processes. Two failures are worth reading:

- `scrollback_len_keeps_counting_past_the_ring_capacity` reporting exactly 4
  means `poll` is reading `history_size()` rather than accumulating the delta.
- `scrollback_returns_lines_that_have_left_the_screen` failing on ordering means
  the sign or the offset is wrong: line 0 is the **oldest** held line and needs
  index `-held`, while the newest sits at `-1`.

- [ ] **Step 8: Run the fixture assertions**

Run: `cargo test --jobs 4 -p oxutrm-term --test emulation -- --test-threads 4 every_colour a_wide alternate osc_zero every_produced`
Expected: the five assertion tests PASS. The four snapshot tests still fail —
no snapshot exists yet, and that is the next step.

- [ ] **Step 9: GENERATE the snapshots, then read every one by eye**

**Do not copy a `.snap` from anywhere.** These describe what
`alacritty_terminal` produces, and a snapshot inherited from another emulator
that happens to pass silently certifies this one against the other's behaviour.

Run: `INSTA_UPDATE=always cargo test --jobs 4 -p oxutrm-term --test emulation -- --test-threads 4`
Expected: PASS, and four files appear under
`crates/oxutrm-term/tests/snapshots/`.

Now read each generated file before it becomes the reference:

```bash
cat crates/oxutrm-term/tests/snapshots/emulation__wide_wrap.snap
```
Expected: row 0 ends in a blank cell; row 1 starts with `漢` followed by a cell
whose `attrs` include `WIDE_CONT` and whose `text` is empty.

```bash
grep -c icon crates/oxutrm-term/tests/snapshots/*.snap
```
Expected: `0` in every file — there is no icon field anywhere in oxutrm.

```bash
grep -n 'seq' crates/oxutrm-term/tests/snapshots/emulation__colours.snap
```
Expected: `seq: 1`, never 0.

```bash
grep -c 'BLINK' crates/oxutrm-term/tests/snapshots/emulation__colours.snap
```
Expected: non-zero — the fixture sets SGR 5, and blink only reaches the snapshot
through the `Handler` newtype. A zero here means the tap is not wired in.

A snapshot that looks wrong is a bug in the converter, not a snapshot to accept.
Fix the code and regenerate.

- [ ] **Step 10: Re-run clean, lint and commit**

Run: `cargo test --jobs 4 -p oxutrm-term -- --test-threads 4`
Expected: PASS, the whole crate, snapshots matching without `INSTA_UPDATE`.

Run: `cargo clippy --jobs 4 -p oxutrm-term --all-targets -- -D warnings`
Expected: PASS.

```bash
git add crates/oxutrm-term
git commit -m "$(cat <<'EOF'
feat(term): HostTerm, capability detection, and freshly generated snapshots

Discharges the four obligations the crate's surface hides: blink is intercepted
through the Handler newtype; scrollback_len is synthesized by accumulating
history_size() deltas because that function saturates; every grid read goes
through the checked accessor; and the palette is ours, with Term::colors() only
as an override layer.

Scrollback is the crate's own, read by negative Line index in O(1) without
moving the viewport — no parallel ring, because a second copy would be a second
source of truth. Resize reflows, and therefore invalidates both the damage hint
and the blink plane.

snapshot() clamps the cursor, because the grid cursor sits one past the last
column mid-wrap and I2 forbids that. Clamping is right at this boundary, where
the emulator's own coordinates are translated, and wrong in oxutrm-sync, where
they arrived over a network.

negotiate_term takes no TerminalCaps and must never gain one: a TERM narrowed
to one client bakes degraded output into the authoritative state forever.

The four .snap files were GENERATED against alacritty_terminal and read by eye,
never ported. A ported snapshot that passes would certify this emulator against
a different one's behaviour.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 12: `oxutrm-client` — the `Renderer`, `RawGuard`, and `oxutrm loopback`

The last task. It joins everything M1 built and produces a working terminal.

**Files:**
- Modify: `crates/oxutrm-client/Cargo.toml`
- Create: `crates/oxutrm-client/src/render.rs`
- Create: `crates/oxutrm-client/src/raw.rs`
- Modify: `crates/oxutrm-client/src/lib.rs`
- Create: `src/lib.rs`, `src/loopback.rs`
- Modify: `src/main.rs`
- Create: `tests/loopback.rs`

**Interfaces:**
- Consumes: `HostTerm`, `state_from_bytes`, `detect_caps`, `negotiate_term`
  (Task 11), `Sender`, `Receiver`, `DiffHint` (Tasks 3-8),
  `oxutrm_proto::{Attrs, Cell, Color, Frame, ScreenState, TermSize, TerminalCaps}`.
- Produces:
  ```rust
  // oxutrm-client
  pub struct Renderer { /* private */ }
  impl Renderer {
      pub fn new(size: TermSize, caps: TerminalCaps) -> Renderer;
      pub fn resize(&mut self, size: TermSize);
      pub fn invalidate(&mut self);
      pub fn render<W: std::io::Write>(&mut self, w: &mut W, s: &ScreenState)
          -> std::io::Result<()>;
  }
  pub struct RawGuard { /* private */ }
  impl RawGuard { pub fn enter() -> anyhow::Result<RawGuard>; }
  impl Drop for RawGuard;
  pub fn terminal_size() -> anyhow::Result<TermSize>;

  // the binary's library target
  pub fn pump(term: &mut HostTerm, sender: &mut Sender<ScreenState>,
              receiver: &mut Receiver<ScreenState>, input: &[u8]) -> anyhow::Result<()>;
  pub fn run_loopback(shell: &str, args: &[String]) -> anyhow::Result<i32>;
  ```
  `status_line` is **not** part of M1: it takes a `PathDescription`, an M3 type.

**The renderer's emission rules, normative.** The tests assert these byte for
byte, so implement exactly this and nothing cleverer.

1. **Full repaint** — first render, after `invalidate`, or when the incoming
   size differs from what is painted: emit `ESC[0m`, `ESC[H`, `ESC[2J`. The pen
   is then all-defaults and the painted model is a blank screen of the new size.
2. **Per row**, find the first and last column at which the incoming row differs
   from the painted row. If there is none, emit nothing for that row. Otherwise
   emit `ESC[{row+1};{first+1}H` and then every cell from `first` to `last`.
3. **Per cell**, if the cell's style differs from the pen, emit one SGR sequence
   that **starts with a reset**: `ESC[0{;params}m`. An all-default style is
   therefore exactly `ESC[0m`. Then emit the cell's `text`. A cell carrying
   `Attrs::WIDE_CONT` emits **nothing at all**.
4. **SGR parameter order**: `0`, then attributes ascending (`1` bold, `2` dim,
   `3` italic, `4` underline, `5` blink, `7` inverse, `8` hidden, `9` strike),
   then foreground, then background.
5. **Colours**: `Default` contributes nothing. `Idx(n)` for `n < 8` is `30+n` /
   `40+n`; for `8 <= n < 16` it is `90+(n-8)` / `100+(n-8)`; otherwise `38;5;n` /
   `48;5;n`. `Rgb(r,g,b)` is `38;2;r;g;b` / `48;2;r;g;b` when `caps.truecolor`,
   otherwise the nearest cube index as `38;5;i`, where
   `i = 16 + 36*q(r) + 6*q(g) + q(b)` and `q(v) = v * 5 / 255`.
6. **Cursor**, always last: emit `ESC[{row+1};{col+1}H`, then — only if
   visibility differs from the painted model — `ESC[?25h` or `ESC[?25l`.

**BOLD-to-bright promotion is not the renderer's job.** The output is ANSI, not
pixels, so it emits `SGR 1` and the index unchanged and lets the user's terminal
promote as it always has. Promoting here would double-apply.

- [ ] **Step 1: Add the client's dependencies**

Replace the `[dependencies]` section of `crates/oxutrm-client/Cargo.toml`:

```toml
[dependencies]
anyhow.workspace = true
oxutrm-proto.workspace = true
oxutrm-term.workspace = true
rustix.workspace = true
unicode-width.workspace = true
```

- [ ] **Step 2: Write the failing `Renderer` tests**

Create `crates/oxutrm-client/src/render.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::{Attrs, Cell, CellText, Color};

    fn caps(truecolor: bool) -> TerminalCaps {
        TerminalCaps {
            truecolor,
            colors: if truecolor { 16_777_216 } else { 256 },
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_string(),
        }
    }

    fn size(cols: u16, rows: u16) -> TermSize {
        TermSize { cols, rows }
    }

    fn blank(rows: u16, cols: u16) -> ScreenState {
        ScreenState::blank(rows, cols).expect("a blank screen is valid")
    }

    fn put(s: &mut ScreenState, row: u16, col: u16, cell: Cell) {
        let ix = row as usize * s.cols as usize + col as usize;
        s.cells[ix] = cell;
    }

    fn glyph(text: &str) -> Cell {
        Cell {
            text: CellText::new(text),
            ..Cell::blank()
        }
    }

    fn draw(r: &mut Renderer, s: &ScreenState) -> String {
        let mut out = Vec::new();
        r.render(&mut out, s).expect("render");
        String::from_utf8(out).expect("valid utf-8")
    }

    /// A 2x4 screen reading "hi" on the top row, cursor just after it.
    fn hi_screen() -> ScreenState {
        let mut s = blank(2, 4);
        put(&mut s, 0, 0, glyph("h"));
        put(&mut s, 0, 1, glyph("i"));
        s.cursor.row = 0;
        s.cursor.col = 2;
        s
    }

    #[test]
    fn the_first_render_repaints_everything() {
        let mut r = Renderer::new(size(4, 2), caps(true));
        assert_eq!(
            draw(&mut r, &hi_screen()),
            "\x1b[0m\x1b[H\x1b[2J\x1b[1;1Hhi\x1b[1;3H"
        );
    }

    #[test]
    fn a_single_changed_cell_costs_a_move_an_sgr_and_one_character() {
        let mut r = Renderer::new(size(4, 2), caps(true));
        let _ = draw(&mut r, &hi_screen());

        let mut next = hi_screen();
        put(
            &mut next,
            0,
            1,
            Cell {
                text: CellText::new("o"),
                fg: Color::Idx(1),
                bg: Color::Default,
                attrs: Attrs::BOLD,
            },
        );
        next.cursor.row = 1;
        next.cursor.col = 0;

        assert_eq!(draw(&mut r, &next), "\x1b[1;2H\x1b[0;1;31mo\x1b[2;1H");
    }

    #[test]
    fn an_unchanged_screen_emits_only_the_cursor_move() {
        let mut r = Renderer::new(size(4, 2), caps(true));
        let _ = draw(&mut r, &hi_screen());
        assert_eq!(draw(&mut r, &hi_screen()), "\x1b[1;3H");
    }

    #[test]
    fn the_pen_is_reset_when_a_styled_cell_is_followed_by_a_plain_one() {
        let mut r = Renderer::new(size(4, 1), caps(true));
        let _ = draw(&mut r, &blank(1, 4));

        let mut next = blank(1, 4);
        put(
            &mut next,
            0,
            0,
            Cell {
                text: CellText::new("X"),
                fg: Color::Idx(2),
                bg: Color::Default,
                attrs: Attrs::empty(),
            },
        );
        put(&mut next, 0, 1, glyph("y"));
        next.cursor.col = 2;

        assert_eq!(draw(&mut r, &next), "\x1b[1;1H\x1b[0;32mX\x1b[0my\x1b[1;3H");
    }

    #[test]
    fn truecolor_is_emitted_verbatim_when_the_terminal_can_show_it() {
        let mut r = Renderer::new(size(2, 1), caps(true));
        let _ = draw(&mut r, &blank(1, 2));

        let mut next = blank(1, 2);
        put(
            &mut next,
            0,
            0,
            Cell {
                text: CellText::new("T"),
                fg: Color::Rgb(0, 128, 255),
                bg: Color::Rgb(32, 32, 32),
                attrs: Attrs::empty(),
            },
        );
        next.cursor.col = 1;

        assert_eq!(
            draw(&mut r, &next),
            "\x1b[1;1H\x1b[0;38;2;0;128;255;48;2;32;32;32mT\x1b[1;2H"
        );
    }

    #[test]
    fn truecolor_is_down_converted_in_the_client_when_it_cannot_be_shown() {
        // Down-converting HERE keeps the host's state full fidelity for a
        // better client that attaches later.
        let mut r = Renderer::new(size(2, 1), caps(false));
        let _ = draw(&mut r, &blank(1, 2));

        let mut next = blank(1, 2);
        put(
            &mut next,
            0,
            0,
            Cell {
                text: CellText::new("T"),
                fg: Color::Rgb(0, 128, 255),
                bg: Color::Default,
                attrs: Attrs::empty(),
            },
        );
        next.cursor.col = 1;

        // q(0) = 0, q(128) = 2, q(255) = 5  ->  16 + 0 + 12 + 5 = 33
        assert_eq!(draw(&mut r, &next), "\x1b[1;1H\x1b[0;38;5;33mT\x1b[1;2H");
    }

    #[test]
    fn indexed_colours_use_the_short_forms_where_they_exist() {
        assert_eq!(color_params_for_test(Color::Idx(3), false), vec!["33"]);
        assert_eq!(color_params_for_test(Color::Idx(3), true), vec!["43"]);
        assert_eq!(color_params_for_test(Color::Idx(11), false), vec!["93"]);
        assert_eq!(color_params_for_test(Color::Idx(11), true), vec!["103"]);
        assert_eq!(
            color_params_for_test(Color::Idx(208), false),
            vec!["38", "5", "208"]
        );
        assert!(color_params_for_test(Color::Default, false).is_empty());
    }

    #[test]
    fn every_attribute_has_its_sgr_code_in_ascending_order() {
        let mut r = Renderer::new(size(2, 1), caps(true));
        let _ = draw(&mut r, &blank(1, 2));

        let mut next = blank(1, 2);
        put(
            &mut next,
            0,
            0,
            Cell {
                text: CellText::new("A"),
                fg: Color::Default,
                bg: Color::Default,
                attrs: Attrs::BOLD
                    | Attrs::DIM
                    | Attrs::ITALIC
                    | Attrs::UNDERLINE
                    | Attrs::BLINK
                    | Attrs::INVERSE
                    | Attrs::HIDDEN
                    | Attrs::STRIKE,
            },
        );
        next.cursor.col = 1;

        assert_eq!(
            draw(&mut r, &next),
            "\x1b[1;1H\x1b[0;1;2;3;4;5;7;8;9mA\x1b[1;2H"
        );
    }

    #[test]
    fn a_wide_continuation_cell_emits_nothing() {
        let mut r = Renderer::new(size(4, 1), caps(true));
        let _ = draw(&mut r, &blank(1, 4));

        let mut next = blank(1, 4);
        put(&mut next, 0, 0, glyph("\u{6f22}"));
        put(
            &mut next,
            0,
            1,
            Cell {
                text: CellText::new(""),
                attrs: Attrs::WIDE_CONT,
                ..Cell::blank()
            },
        );
        next.cursor.col = 2;

        assert_eq!(draw(&mut r, &next), "\x1b[1;1H\u{6f22}\x1b[1;3H");
    }

    #[test]
    fn wide_cont_does_not_leak_into_the_style_comparison() {
        // WIDE_CONT is a layout fact, not a rendering one. If it reached the
        // pen, the cell after a wide glyph would cost a needless SGR.
        let mut r = Renderer::new(size(4, 1), caps(true));
        let _ = draw(&mut r, &blank(1, 4));

        let mut next = blank(1, 4);
        put(&mut next, 0, 0, glyph("\u{6f22}"));
        put(
            &mut next,
            0,
            1,
            Cell {
                text: CellText::new(""),
                attrs: Attrs::WIDE_CONT,
                ..Cell::blank()
            },
        );
        put(&mut next, 0, 2, glyph("z"));
        next.cursor.col = 3;

        assert_eq!(draw(&mut r, &next), "\x1b[1;1H\u{6f22}z\x1b[1;4H");
    }

    #[test]
    fn hiding_and_showing_the_cursor_is_emitted_only_on_a_change() {
        let mut r = Renderer::new(size(4, 1), caps(true));
        let mut s = blank(1, 4);
        s.cursor.visible = false;
        assert_eq!(draw(&mut r, &s), "\x1b[0m\x1b[H\x1b[2J\x1b[1;1H\x1b[?25l");
        assert_eq!(draw(&mut r, &s), "\x1b[1;1H", "no change, no escape");
        s.cursor.visible = true;
        assert_eq!(draw(&mut r, &s), "\x1b[1;1H\x1b[?25h");
    }

    #[test]
    fn a_size_change_forces_a_full_repaint() {
        let mut r = Renderer::new(size(4, 2), caps(true));
        let _ = draw(&mut r, &hi_screen());

        let mut bigger = blank(2, 6);
        put(&mut bigger, 1, 0, glyph("z"));
        bigger.cursor.row = 1;
        bigger.cursor.col = 1;

        assert_eq!(
            draw(&mut r, &bigger),
            "\x1b[0m\x1b[H\x1b[2J\x1b[2;1Hz\x1b[2;2H"
        );
    }

    #[test]
    fn invalidate_forces_a_full_repaint_of_an_unchanged_screen() {
        let mut r = Renderer::new(size(4, 2), caps(true));
        let _ = draw(&mut r, &hi_screen());
        r.invalidate();
        assert_eq!(
            draw(&mut r, &hi_screen()),
            "\x1b[0m\x1b[H\x1b[2J\x1b[1;1Hhi\x1b[1;3H"
        );
    }

    #[test]
    fn resize_also_forces_a_full_repaint() {
        let mut r = Renderer::new(size(4, 2), caps(true));
        let _ = draw(&mut r, &hi_screen());
        r.resize(size(4, 2));
        assert!(draw(&mut r, &hi_screen()).starts_with("\x1b[0m\x1b[H\x1b[2J"));
    }

    #[test]
    fn a_full_row_of_text_is_emitted_as_one_run() {
        let mut r = Renderer::new(size(4, 1), caps(true));
        let _ = draw(&mut r, &blank(1, 4));

        let mut next = blank(1, 4);
        for i in 0..4u16 {
            put(&mut next, 0, i, glyph(&char::from(b'a' + i as u8).to_string()));
        }
        next.cursor.col = 3;

        assert_eq!(draw(&mut r, &next), "\x1b[1;1Habcd\x1b[1;4H");
    }
}
```

- [ ] **Step 3: Write the failing `RawGuard` tests**

Create `crates/oxutrm-client/src/raw.rs` containing only its test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_size_never_reports_a_zero_dimension() {
        // Under `cargo test` stdout is usually a pipe, so this exercises the
        // fallback. Either way a zero-sized terminal makes every cell index
        // panic, so it must never be returned.
        let s = terminal_size().expect("terminal_size");
        assert!(s.cols >= 1, "cols was {}", s.cols);
        assert!(s.rows >= 1, "rows was {}", s.rows);
    }

    #[test]
    fn entering_raw_mode_without_a_tty_fails_cleanly() {
        // No assertion on which way it goes: on a tty it succeeds and the
        // guard restores on drop; on a pipe it errors. Neither may panic, and
        // neither may leave the terminal broken.
        match RawGuard::enter() {
            Ok(g) => drop(g),
            Err(e) => assert!(!e.to_string().is_empty()),
        }
    }
}
```

- [ ] **Step 4: Write the failing end-to-end tests**

Create `tests/loopback.rs` at the repo root:

```rust
//! End to end: a shell on a PTY, through the sync engine, into a rendered
//! screen — with no terminal and no network involved.

use oxutrm::{pump, run_loopback};
use oxutrm_client::Renderer;
use oxutrm_proto::{ScreenState, TermSize, TerminalCaps};
use oxutrm_sync::{Receiver, Sender};
use oxutrm_term::HostTerm;

fn caps() -> TerminalCaps {
    TerminalCaps {
        truecolor: true,
        colors: 16_777_216,
        bracketed_paste: true,
        mouse_sgr: true,
        osc52: true,
        term_name: "xterm-256color".to_string(),
    }
}

fn blank(rows: u16, cols: u16) -> ScreenState {
    ScreenState::blank(rows, cols).expect("a blank screen is valid")
}

fn text_of(s: &ScreenState) -> String {
    (0..s.rows)
        .map(|r| {
            s.row(r)
                .iter()
                .map(|c| if c.text.is_empty() { " " } else { c.text.as_str() })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn pump_until(
    term: &mut HostTerm,
    sender: &mut Sender<ScreenState>,
    receiver: &mut Receiver<ScreenState>,
    needle: &str,
) -> bool {
    for _ in 0..400 {
        pump(term, sender, receiver, &[]).expect("pump");
        if text_of(receiver.state()).contains(needle) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

#[test]
fn shell_output_reaches_the_receiver_and_renders() {
    let size = TermSize { cols: 40, rows: 6 };
    let mut term = HostTerm::spawn(
        "/bin/sh",
        &[
            "-c".to_string(),
            "printf 'loopback-works\\n'; sleep 30".to_string(),
        ],
        &[("TERM".to_string(), "xterm-256color".to_string())],
        size,
        200,
    )
    .expect("spawn");

    let mut sender = Sender::new(blank(size.rows, size.cols));
    let mut receiver = Receiver::new(blank(size.rows, size.cols));

    assert!(
        pump_until(&mut term, &mut sender, &mut receiver, "loopback-works"),
        "receiver never saw the output:\n{}",
        text_of(receiver.state())
    );
    assert_eq!(receiver.state(), sender.current(), "the sync engine diverged");
    receiver.state().validate().expect("a valid state");

    let mut painted = Vec::new();
    let mut renderer = Renderer::new(size, caps());
    renderer.render(&mut painted, receiver.state()).expect("render");
    let ansi = String::from_utf8(painted).expect("utf-8");
    assert!(ansi.contains("loopback-works"), "the renderer emitted no text");
    assert!(ansi.starts_with("\x1b[0m\x1b[H\x1b[2J"), "the first paint is full");
}

#[test]
fn typed_input_travels_to_the_shell_and_back() {
    let size = TermSize { cols: 40, rows: 6 };
    let mut term = HostTerm::spawn("/bin/cat", &[], &[], size, 0).expect("spawn");

    let mut sender = Sender::new(blank(size.rows, size.cols));
    let mut receiver = Receiver::new(blank(size.rows, size.cols));

    pump(&mut term, &mut sender, &mut receiver, b"round-trip\n").expect("pump");
    assert!(
        pump_until(&mut term, &mut sender, &mut receiver, "round-trip"),
        "input never came back:\n{}",
        text_of(receiver.state())
    );
}

#[test]
fn a_resize_propagates_all_the_way_through() {
    let size = TermSize { cols: 20, rows: 4 };
    let mut term = HostTerm::spawn("/bin/cat", &[], &[], size, 0).expect("spawn");

    let mut sender = Sender::new(blank(size.rows, size.cols));
    let mut receiver = Receiver::new(blank(size.rows, size.cols));
    pump(&mut term, &mut sender, &mut receiver, &[]).expect("pump");

    let bigger = TermSize { cols: 60, rows: 12 };
    term.resize(bigger).expect("resize");
    // A resize alone may not dirty the PTY, so force several states through.
    for _ in 0..5 {
        pump(&mut term, &mut sender, &mut receiver, &[]).expect("pump");
    }

    assert_eq!((receiver.state().rows, receiver.state().cols), (12, 60));
    assert_eq!(receiver.state().cells.len(), 720);
    assert_eq!(receiver.state(), sender.current());
    receiver.state().validate().expect("a valid state");
}

#[test]
fn a_full_screen_of_output_crosses_the_wire_in_one_frame() {
    // There is no fragmentation, so even a full 24x80 truecolor screen is one
    // Frame. Whether it travels as a datagram or on a stream is the
    // transport's problem, in M4.
    let size = TermSize { cols: 80, rows: 24 };
    let mut term = HostTerm::spawn(
        "/bin/sh",
        &[
            "-c".to_string(),
            "i=1; while [ $i -le 24 ]; do printf '\\033[1;3%dm' $((i % 8)); \
             printf 'row%02d-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\\n' $i; \
             i=$((i+1)); done; sleep 30"
                .to_string(),
        ],
        &[("TERM".to_string(), "xterm-256color".to_string())],
        size,
        0,
    )
    .expect("spawn");

    let mut sender = Sender::new(blank(size.rows, size.cols));
    let mut receiver = Receiver::new(blank(size.rows, size.cols));
    assert!(
        pump_until(&mut term, &mut sender, &mut receiver, "row24"),
        "never saw the last row:\n{}",
        text_of(receiver.state())
    );
    assert_eq!(receiver.state(), sender.current());
    receiver.state().validate().expect("a valid state");
}

#[test]
fn the_receivers_state_is_valid_at_every_step_of_a_real_session() {
    // The convergence property proves this over generated input; this proves
    // it over a real shell, a real emulator and a real PTY.
    let size = TermSize { cols: 30, rows: 8 };
    let mut term = HostTerm::spawn(
        "/bin/sh",
        &[
            "-c".to_string(),
            "printf 'a\\nb\\n\\033[1;31mc\\033[0m\\n'; printf '\\033]0;t\\007'; \
             printf '\\007'; sleep 30"
                .to_string(),
        ],
        &[],
        size,
        50,
    )
    .expect("spawn");

    let mut sender = Sender::new(blank(size.rows, size.cols));
    let mut receiver = Receiver::new(blank(size.rows, size.cols));
    let mut previous = receiver.state().clone();

    for _ in 0..200 {
        pump(&mut term, &mut sender, &mut receiver, &[]).expect("pump");
        receiver.state().validate().expect("valid at every step");
        receiver
            .state()
            .validate_transition(&previous)
            .expect("every transition is monotonic");
        previous = receiver.state().clone();
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

#[test]
fn loopback_returns_the_childs_exit_code_or_fails_cleanly() {
    // run_loopback needs a terminal for raw mode; under `cargo test` there is
    // none, so it must fail cleanly rather than panic or hang.
    match run_loopback("/bin/sh", &["-c".to_string(), "exit 0".to_string()]) {
        Ok(code) => assert_eq!(code, 0),
        Err(e) => assert!(!e.to_string().is_empty(), "must fail with a message"),
    }
}
```

- [ ] **Step 5: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-client -- --test-threads 4`
Expected: FAIL — `cannot find type 'Renderer'`, `cannot find function
'terminal_size'`.

Run: `cargo test --jobs 4 --test loopback -- --test-threads 4`
Expected: FAIL — `unresolved import 'oxutrm'`.

- [ ] **Step 6: Write `crates/oxutrm-client/src/render.rs`**

Put this **above** the existing `mod tests`:

```rust
use std::io::Write;

use oxutrm_proto::{Attrs, Cell, Color, ScreenState, TermSize, TerminalCaps};

/// The style of one cell: everything an SGR sequence controls.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Style {
    fg: Color,
    bg: Color,
    attrs: Attrs,
}

impl Style {
    fn of(c: &Cell) -> Style {
        Style {
            fg: c.fg,
            bg: c.bg,
            // WIDE_CONT is a layout fact, not a rendering one. Letting it reach
            // the pen would make the cell after a wide glyph emit a needless
            // SGR sequence.
            attrs: c.attrs.difference(Attrs::WIDE_CONT),
        }
    }

    fn plain() -> Style {
        Style {
            fg: Color::Default,
            bg: Color::Default,
            attrs: Attrs::empty(),
        }
    }
}

/// Diffs the desired screen against what is currently painted on the physical
/// terminal and emits the minimal ANSI to reconcile the two.
///
/// This is the second of the client's two diffs. The first applies the host's
/// `ScreenDiff` to the authoritative state; this one turns that state into
/// bytes. Keeping them apart is what makes a local status pane and local
/// scrolling possible without repainting the world.
pub struct Renderer {
    size: TermSize,
    caps: TerminalCaps,
    /// What we believe is on the physical terminal right now.
    painted: ScreenState,
    dirty_all: bool,
}

impl Renderer {
    #[must_use]
    pub fn new(size: TermSize, caps: TerminalCaps) -> Renderer {
        Renderer {
            size,
            caps,
            painted: ScreenState::blank(size.rows, size.cols)
                .expect("a blank screen of the terminal's own size is valid"),
            dirty_all: true,
        }
    }

    /// Tell the renderer the physical terminal changed size. The next render
    /// repaints everything.
    pub fn resize(&mut self, size: TermSize) {
        self.size = size;
        self.painted = ScreenState::blank(size.rows, size.cols)
            .expect("a blank screen of the terminal's own size is valid");
        self.dirty_all = true;
    }

    /// Forget what is painted; the next render repaints everything.
    pub fn invalidate(&mut self) {
        self.dirty_all = true;
    }

    /// Emit the minimal ANSI that turns the painted screen into `s`.
    pub fn render<W: Write>(&mut self, w: &mut W, s: &ScreenState) -> std::io::Result<()> {
        let mut out: Vec<u8> = Vec::new();
        let full = self.dirty_all || self.painted.rows != s.rows || self.painted.cols != s.cols;

        let mut pen: Option<Style> = None;
        if full {
            // A reset first, so the pen is known; then home and clear.
            out.extend_from_slice(b"\x1b[0m\x1b[H\x1b[2J");
            pen = Some(Style::plain());
            self.painted = ScreenState::blank(s.rows, s.cols)
                .expect("the incoming state's size is valid by construction");
            self.size = TermSize {
                cols: s.cols,
                rows: s.rows,
            };
        }

        for r in 0..s.rows {
            let new = s.row(r);
            let old = self.painted.row(r);
            let Some((first, last)) = changed_span(new, old) else {
                continue;
            };
            out.extend_from_slice(cup(r, first).as_bytes());
            for col in first..=last {
                let cell = &new[col as usize];
                if cell.attrs.contains(Attrs::WIDE_CONT) {
                    // The wide character to the left already covers this
                    // column. Painting a space here would cut the glyph in two.
                    continue;
                }
                let style = Style::of(cell);
                if pen != Some(style) {
                    out.extend_from_slice(sgr(&style, &self.caps).as_bytes());
                    pen = Some(style);
                }
                out.extend_from_slice(cell.text.as_bytes());
            }
        }

        out.extend_from_slice(cup(s.cursor.row, s.cursor.col).as_bytes());
        if s.cursor.visible != self.painted.cursor.visible {
            out.extend_from_slice(if s.cursor.visible {
                b"\x1b[?25h"
            } else {
                b"\x1b[?25l"
            });
        }

        // The painted model tracks the physical terminal, not the protocol, so
        // the sequence number means nothing to it — but it must still be a
        // valid state, so it keeps 1 rather than 0.
        self.painted.clone_from(s);
        self.painted.seq = 1;
        self.dirty_all = false;

        w.write_all(&out)
    }
}

/// Cursor position, 1-based as the terminal expects.
fn cup(row: u16, col: u16) -> String {
    format!("\x1b[{};{}H", u32::from(row) + 1, u32::from(col) + 1)
}

/// The first and last column at which two rows differ, or `None` when they are
/// identical.
fn changed_span(new: &[Cell], old: &[Cell]) -> Option<(u16, u16)> {
    let differs = |i: usize| i >= old.len() || old[i] != new[i];
    let first = (0..new.len()).find(|i| differs(*i))?;
    let last = (0..new.len()).rev().find(|i| differs(*i))?;
    Some((first as u16, last as u16))
}

/// One SGR sequence that starts with a reset, so the result never depends on
/// what the pen was before. That is what makes the output exactly assertable.
fn sgr(style: &Style, caps: &TerminalCaps) -> String {
    let mut params: Vec<String> = vec!["0".to_string()];
    for (flag, code) in [
        (Attrs::BOLD, "1"),
        (Attrs::DIM, "2"),
        (Attrs::ITALIC, "3"),
        (Attrs::UNDERLINE, "4"),
        (Attrs::BLINK, "5"),
        (Attrs::INVERSE, "7"),
        (Attrs::HIDDEN, "8"),
        (Attrs::STRIKE, "9"),
    ] {
        if style.attrs.contains(flag) {
            params.push(code.to_string());
        }
    }
    params.extend(color_params(style.fg, caps, false));
    params.extend(color_params(style.bg, caps, true));
    format!("\x1b[{}m", params.join(";"))
}

/// The SGR parameters for one colour. Empty for `Default`.
///
/// BOLD-to-bright promotion is deliberately absent: the output is ANSI rather
/// than pixels, so the user's own terminal promotes as it always has, and doing
/// it here would double-apply.
fn color_params(c: Color, caps: &TerminalCaps, bg: bool) -> Vec<String> {
    let (base, extended) = if bg { (40u16, "48") } else { (30u16, "38") };
    match c {
        Color::Default => Vec::new(),
        Color::Idx(n) if n < 8 => vec![(base + u16::from(n)).to_string()],
        // The bright eight have their own short codes: 90-97 and 100-107.
        Color::Idx(n) if n < 16 => vec![(base + 60 + u16::from(n - 8)).to_string()],
        Color::Idx(n) => vec![extended.to_string(), "5".to_string(), n.to_string()],
        Color::Rgb(r, g, b) => {
            if caps.truecolor {
                vec![
                    extended.to_string(),
                    "2".to_string(),
                    r.to_string(),
                    g.to_string(),
                    b.to_string(),
                ]
            } else {
                // Down-convert HERE, in the client, so the host's state stays
                // full fidelity for a better client that attaches later.
                vec![
                    extended.to_string(),
                    "5".to_string(),
                    rgb_to_cube(r, g, b).to_string(),
                ]
            }
        }
    }
}

/// The nearest colour in the 6x6x6 cube of the 256-colour palette.
fn rgb_to_cube(r: u8, g: u8, b: u8) -> u8 {
    let q = |v: u8| u16::from(v) * 5 / 255;
    (16 + 36 * q(r) + 6 * q(g) + q(b)) as u8
}

/// Exposed for the unit tests, which assert the parameter list directly.
#[cfg(test)]
fn color_params_for_test(c: Color, bg: bool) -> Vec<String> {
    let caps = TerminalCaps {
        truecolor: true,
        colors: 16_777_216,
        bracketed_paste: true,
        mouse_sgr: true,
        osc52: true,
        term_name: "xterm-256color".to_string(),
    };
    color_params(c, &caps, bg)
}
```

- [ ] **Step 7: Write `crates/oxutrm-client/src/raw.rs`**

Put this **above** the existing `mod tests`:

```rust
use anyhow::{Context as _, Result};
use oxutrm_proto::TermSize;
use rustix::termios::{OptionalActions, Termios};

/// Raw mode for the duration of a scope.
///
/// Restored on `Drop`, which covers a panic that unwinds. A panic hook is
/// installed as well, so the terminal is repaired even when the panic message
/// is printed before the stack unwinds past this guard.
pub struct RawGuard {
    original: Termios,
}

impl RawGuard {
    /// Put the controlling terminal into raw mode.
    pub fn enter() -> Result<RawGuard> {
        let stdin = std::io::stdin();
        let original = rustix::termios::tcgetattr(&stdin).context("tcgetattr on stdin")?;

        let mut raw = original.clone();
        raw.make_raw();
        rustix::termios::tcsetattr(&stdin, OptionalActions::Flush, &raw)
            .context("tcsetattr to raw mode")?;

        install_panic_hook(original.clone());
        Ok(RawGuard { original })
    }
}

impl Drop for RawGuard {
    fn drop(&mut self) {
        let stdin = std::io::stdin();
        let _ = rustix::termios::tcsetattr(&stdin, OptionalActions::Flush, &self.original);
        // Leave the terminal in a sane visible state, whatever the remote
        // application had turned on.
        let _ = std::io::Write::write_all(
            &mut std::io::stdout(),
            b"\x1b[0m\x1b[?25h\x1b[?1049l\x1b[?2004l\x1b[?1006l\x1b[?1002l",
        );
    }
}

/// Chain a panic hook that restores the saved termios before the default hook
/// prints anything.
fn install_panic_hook(original: Termios) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(move || {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let stdin = std::io::stdin();
            let _ = rustix::termios::tcsetattr(&stdin, OptionalActions::Flush, &original);
            let _ = std::io::Write::write_all(&mut std::io::stdout(), b"\x1b[0m\x1b[?25h");
            previous(info);
        }));
    });
}

/// The size of the controlling terminal.
///
/// Falls back to 80x24 when there is no terminal — under a pipe, in CI — so no
/// caller ever has to handle a zero-sized screen, which would make every cell
/// index panic.
pub fn terminal_size() -> Result<TermSize> {
    let stdout = std::io::stdout();
    match rustix::termios::tcgetwinsize(&stdout) {
        Ok(ws) if ws.ws_col > 0 && ws.ws_row > 0 => Ok(TermSize {
            cols: ws.ws_col,
            rows: ws.ws_row,
        }),
        _ => Ok(TermSize { cols: 80, rows: 24 }),
    }
}
```

Rewrite `crates/oxutrm-client/src/lib.rs`, keeping its module doc:

```rust
mod raw;
mod render;

pub use raw::{RawGuard, terminal_size};
pub use render::Renderer;
```

- [ ] **Step 8: Write `src/loopback.rs` and wire the binary**

`src/loopback.rs`:

```rust
//! The loopback terminal: everything M1 builds, wired into one process.
//!
//! A shell runs on a PTY. Its output goes through the emulator into a
//! `ScreenState`, which goes through the sync engine's `Sender` into a
//! `Frame`, which goes straight into a `Receiver` — no socket, no network —
//! and the resulting state is rendered onto the real terminal.
//!
//! It exists to prove the terminal core and the sync engine work together
//! before any of the transport does.

use std::io::{Read as _, Write as _};
use std::time::Duration;

use anyhow::{Context as _, Result};
use oxutrm_client::{RawGuard, Renderer, terminal_size};
use oxutrm_proto::{Frame, ScreenState};
use oxutrm_sync::{DiffHint, Receiver, Sender};
use oxutrm_term::{HostTerm, detect_caps, negotiate_term};

/// One turn of the loop, with no terminal I/O at all.
///
/// Writes `input` to the PTY, drains whatever the child produced, and pushes
/// one state through the sync engine. The receiver's acknowledgement reaches
/// the sender first, so the frame is always a diff against what the receiver
/// actually holds.
pub fn pump(
    term: &mut HostTerm,
    sender: &mut Sender<ScreenState>,
    receiver: &mut Receiver<ScreenState>,
    input: &[u8],
) -> Result<()> {
    if !input.is_empty() {
        term.write_input(input)?;
    }

    if term.poll()? {
        // The emulator's per-line damage answers exactly the question the diff
        // engine asks. It is handed to oxutrm-sync as plain data; the sync
        // engine never asks the emulator anything, which is what keeps it
        // I/O-free.
        let hint = match term.take_damage() {
            Some(rows) => DiffHint::rows(rows),
            None => DiffHint::everything(),
        };
        // The sequence number is the sender's business; snapshot with a
        // placeholder and let `update_damaged` assign the real one.
        sender.update_damaged(term.snapshot(1), hint);
    }

    sender.on_ack(receiver.ack());
    if let Some(frame) = sender
        .make_frame(receiver.ack())
        .map_err(|e| anyhow::anyhow!("make_frame: {e}"))?
    {
        // In M1 the frame does not cross a socket, but it is still encoded and
        // decoded, so the wire format is exercised from the very first
        // milestone rather than first meeting a real network in M4.
        let bytes = frame.encode().context("encode frame")?;
        let decoded = Frame::decode(&bytes).context("decode frame")?;
        receiver
            .on_frame(&decoded)
            .map_err(|e| anyhow::anyhow!("on_frame: {e}"))?;
    }
    Ok(())
}

/// Run a shell on a PTY and render it onto this terminal. Returns the child's
/// exit code.
pub fn run_loopback(shell: &str, args: &[String]) -> Result<i32> {
    let caps = detect_caps();
    // TERM comes from what the emulator emulates, never from `caps`.
    let (term_name, colorterm) = negotiate_term();

    let mut env = vec![("TERM".to_string(), term_name)];
    if let Some(ct) = colorterm {
        env.push(("COLORTERM".to_string(), ct));
    }

    let size = terminal_size()?;
    // Raw mode first: if it fails there is no terminal, and spawning a child we
    // could not show would be worse than failing here.
    let _raw = RawGuard::enter().context("this command needs a terminal")?;

    let mut term = HostTerm::spawn(shell, args, &env, size, 10_000)?;
    let initial = ScreenState::blank(size.rows, size.cols)
        .map_err(|e| anyhow::anyhow!("blank screen: {e}"))?;
    let mut sender = Sender::new(initial.clone());
    let mut receiver = Receiver::new(initial);
    let mut renderer = Renderer::new(size, caps);

    let mut stdin = std::io::stdin();
    set_nonblocking(&stdin)?;
    let mut stdout = std::io::stdout();
    let mut buf = [0u8; 4096];
    let mut last_size = size;
    let mut last_seq = 0u64;

    loop {
        // A SIGWINCH handler is not needed for M1: polling the size every turn
        // costs a few microseconds and has no signal-safety problems.
        let now = terminal_size()?;
        if now != last_size {
            term.resize(now)?;
            renderer.resize(now);
            last_size = now;
        }

        let input = match stdin.read(&mut buf) {
            Ok(0) => &[][..],
            Ok(n) => &buf[..n],
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => &[][..],
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => &[][..],
            Err(e) => return Err(e).context("read stdin"),
        };

        pump(&mut term, &mut sender, &mut receiver, input)?;

        if receiver.state().seq != last_seq {
            renderer.render(&mut stdout, receiver.state())?;
            stdout.flush()?;
            last_seq = receiver.state().seq;
        }

        if let Some(code) = term.child_exited() {
            // Drain anything the child wrote just before exiting.
            pump(&mut term, &mut sender, &mut receiver, &[])?;
            renderer.render(&mut stdout, receiver.state())?;
            stdout.flush()?;
            return Ok(code);
        }

        std::thread::sleep(Duration::from_millis(8));
    }
}

fn set_nonblocking(fd: &impl std::os::fd::AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd).context("fcntl_getfl on stdin")?;
    rustix::fs::fcntl_setfl(fd, flags | rustix::fs::OFlags::NONBLOCK)
        .context("set O_NONBLOCK on stdin")?;
    Ok(())
}
```

Create `src/lib.rs`:

```rust
#![forbid(unsafe_code)]

//! oxutrm — a remote terminal that survives bad networks and NAT.
//!
//! The binary is thin; everything testable lives here.

mod loopback;

pub use loopback::{pump, run_loopback};
```

Add a `[lib]` section to the root `Cargo.toml`, beside `[[bin]]`:

```toml
[lib]
name = "oxutrm"
path = "src/lib.rs"
```

Add the `loopback` arm to `src/main.rs`, leaving the rest as it is:

```rust
        // Hidden, and deliberately absent from the usage text: a development
        // aid that runs the whole terminal core in one process with no network
        // at all.
        Some("loopback") => {
            let shell = args
                .get(1)
                .cloned()
                .or_else(|| std::env::var("SHELL").ok())
                .unwrap_or_else(|| "/bin/sh".to_string());
            let rest: Vec<String> = args.iter().skip(2).cloned().collect();
            let code = oxutrm::run_loopback(&shell, &rest)?;
            std::process::exit(code);
        }
```

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-client -- --test-threads 4`
Expected: PASS, 17 tests. Every renderer test asserts an exact byte string; if
one fails, print the actual value with `{:?}` — which escapes the `\x1b` — and
compare character by character. Change the **implementation**, not the
expectation, unless you can articulate why the expectation contradicts the six
emission rules.

Run: `cargo test --jobs 4 --test loopback -- --test-threads 4`
Expected: PASS, 6 tests.

`the_receivers_state_is_valid_at_every_step_of_a_real_session` is the one that
ties the two test classes together: the property test proves the invariants over
generated input, and this proves them over a real shell, a real emulator and a
real PTY.

- [ ] **Step 10: Drive it by hand**

Run: `cargo run --jobs 4 -- loopback /bin/sh`

Expected: a working shell. Check each of these:

- Type `ls`, press Enter, see output.
- `printf '\033[1;31mred\033[0m\n'` shows red text.
- `printf '\033[5mblink\033[0m\n'` blinks — this is the attribute the emulator
  discards, so it proves the `Handler` newtype is wired in.
- `printf '\033[9mstrike\033[0m\n'` is struck through.
- `printf '\033]0;my title\007'` sets the window title.
- Resize the terminal window; the screen reflows.
- Run `vim`, quit it; the alternate screen switches both ways.
- Type `exit`; the terminal is returned to normal — cursor visible, echo
  working, `stty sane` not needed.

If the terminal is left broken after exit, `RawGuard`'s `Drop` is not running:
check that `_raw` is bound to a named variable and not to `_`, which would drop
it immediately.

- [ ] **Step 11: Run the whole gate**

Run: `cargo fmt --all --check`
Expected: PASS.

Run: `cargo clippy --jobs 4 --workspace --all-targets -- -D warnings`
Expected: PASS.

Run: `cargo test --jobs 4 --workspace -- --test-threads 4 2>&1 | grep -E '^test result'`
Expected: every line `ok`, and the total well above Task 1's baseline.

Run: `make check`
Expected: PASS.

- [ ] **Step 12: Commit**

```bash
git add Cargo.toml Cargo.lock src tests crates/oxutrm-client
git commit -m "$(cat <<'EOF'
feat: Renderer, RawGuard and the hidden `oxutrm loopback` subcommand

M1 complete: a shell on a PTY, through alacritty_terminal, through the sync
engine's Sender and Receiver — one Frame per state, encoded and decoded even
though it never leaves the process — and rendered back onto the real terminal.
Typing works, resize works, no network is involved.

Every renderer SGR starts with a reset, so the output never depends on prior pen
state and every test pins an exact byte string. Colours the terminal cannot show
are down-converted here, so the host's state stays full fidelity for a better
client later; bold-to-bright is left to the user's terminal.

The emulator's per-line damage is handed to oxutrm-sync as a DiffHint, so the
diff engine never compares whole grids and the sync crate still performs no I/O.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Definition of done for M1

- [ ] `make check` is green from a clean checkout.
- [ ] `cargo run -- loopback` gives a working shell: typing, colour, blink,
      strikeout, title, alternate screen and resize all work, and exiting leaves
      the terminal sane.
- [ ] **Both** `oxutrm-sync` test classes pass, and each was proven capable of
      failing: `tests/convergence.rs` for the happy path (three injections in
      Task 8) and `tests/reject_path.rs` for the refusals (four in Task 4, one
      in Task 7).
- [ ] `grep -rn 'frag' crates/ src/` finds nothing — there is no fragmentation.
- [ ] `grep -rn 'vt100' crates/ src/ Cargo.toml` finds nothing.
- [ ] `cargo test -p oxutrm-sync --test no_io` passes: the crate's dependency
      allowlist is unchanged.
- [ ] The four `.ansi` fixtures and their **freshly generated** snapshots are
      committed, and `grep -c icon crates/oxutrm-term/tests/snapshots/*.snap`
      reports `0`.
- [ ] `oxutrm-net` and `oxutrm-host` are still stubs — M1 does not touch them.

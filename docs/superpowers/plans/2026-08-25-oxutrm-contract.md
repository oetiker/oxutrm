# oxutrm — Global Constraints and Interface Contract

> Shared by every milestone plan. Every task's requirements implicitly include
> this file. Types named here are **normative**: do not rename, do not change
> signatures without updating this file first.

**Spec:** `docs/superpowers/specs/2026-08-25-oxutrm-design.md`

---

## Global Constraints

- **Binary and product name is `oxutrm`.** Not `oxuterm`. Crates are
  `oxutrm-proto`, `oxutrm-sync`, `oxutrm-term`, `oxutrm-net`, `oxutrm-host`,
  `oxutrm-client`. The checkout directory is `oxuterm` for historical reasons;
  nothing inside it uses that spelling.
- **Rust edition 2024**, workspace at the repo root, one binary `src/main.rs`.
  `alacritty_terminal` 0.26 is edition 2024 with MSRV 1.85, so that floor applies to
  the whole build; there is nothing to gain from staying on 2021.
- **Cap all parallelism at 4**: `cargo build --jobs 4`,
  `cargo test --jobs 4 -- --test-threads 4`. The build machine is shared.
- **Workspace root `Cargo.toml` must contain:**
  ```toml
  [profile.dev]
  debug = "line-tables-only"
  split-debuginfo = "unpacked"
  ```
- **`oxutrm-sync` performs no I/O.** No `std::net`, no `std::fs`, no `tokio`,
  no clock access. This is enforced by review; a violation fails the task.
- **English** for all identifiers, comments, and documentation.
- **`anyhow::Result`** at binary and crate-boundary level; concrete error enums
  (via `thiserror`) inside `oxutrm-sync` and `oxutrm-proto` where callers must
  discriminate.
- **No key material is ever written to disk**, in any crate, at any time.
- **Every task ends green**: `cargo clippy --all-targets -- -D warnings` and
  `cargo test --jobs 4 -- --test-threads 4` both pass before committing.
- **Commit messages** end with:
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`

## Dependency versions (pin these exact majors)

| Crate | Version | Used by |
|---|---|---|
| `quinn` | `0.11` | net |
| `rustls` | whatever `quinn` 0.11 re-exports | net |
| `rcgen` | `0.13` | net (self-signed cert) |
| `alacritty_terminal` | `0.26`, `default-features = false` (serde is a default we do not need). Edition 2024, **MSRV 1.85**, Apache-2.0. Re-exports `vte`. | term — the emulator, on BOTH ends |
| `rustix` | `1` (features `process`, `termios`, `stdio`, `fs`) | term, host, client |
| `rustix-openpty` | `0.2` | term |
| `stun_codec` | `0.4` | net — ALL ICE checks and keepalives |
| `stunclient` | `0.4` | net — pre-QUIC discovery ONLY; its API has no MESSAGE-INTEGRITY |
| `bytecodec` | `0.5` | net |
| `crab_nat` | `0.8` | net |
| `igd-next` | `0.17` | net |
| `tokio` | `1` (features `rt-multi-thread`, `net`, `time`, `macros`, `io-util`, `sync`, `process`) | net, host, client |
| `postcard` | `1` (feature `use-std`) | proto, sync |
| `serde` | `1` (feature `derive`) | proto, sync, term |
| `serde_json` | `1` | proto (signalling only) |
| `ruzstd` | `0.8` | sync |
| `bitflags` | `2` | term |
| `anyhow` | `1` | all binaries |
| `thiserror` | `2` | proto, sync |
| `rand` | `0.9` | net, host |
| `sha2` | `0.10` | net |
| `hmac` | `0.12` | net (STUN MESSAGE-INTEGRITY) |
| `hkdf` | `0.12` | net (direction-labelled ICE credentials) |
| `sha1` | `0.10` | net (STUN MESSAGE-INTEGRITY is HMAC-SHA1) |
| `netdev` | `0.46` | net — default-gateway discovery (`crab_nat` needs the gateway and ships no discovery). Chosen over `/proc/net/route`, which is Linux-only, because §1.2 scopes the project to Unix. |
| `base64` | `0.22` | proto |
| `unicode-width` | `0.2` | term, client |
| `compact_str` | `0.10` | term — `CellText`, inline for <=24 bytes |
| `libc` | `0.2` | host; client — **only** `sigaction`, to restore the terminal when the client is killed. rustix has no stable binding for installing a handler (`rustix::runtime` is explicitly unstable), and that one gap is why `oxutrm-client` is `deny(unsafe_code)` rather than `forbid`. |
| `proptest` | `1` | sync (dev) |
| `insta` | `1` | term (dev, snapshots) |

---

## Interface Contract

These signatures are normative. A task that consumes a type from another crate
uses **exactly** these names.

### `oxutrm-proto`

```rust
pub const PROTO_VERSION: u32 = 1;

/// 128-bit session identifier. Display and FromStr are 32 lowercase hex chars.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SessionId(pub [u8; 16]);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct TermSize { pub cols: u16, pub rows: u16 }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CandidateKind { Host, PortMapped, ServerReflexive, PeerReflexive }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NatType { None, EndpointIndependent, AddressDependent, Symmetric, Unknown }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Rung { Ipv6Direct, PortMapped, StunPunch, Birthday, SshTunnel }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub addr: std::net::SocketAddr,
    pub kind: CandidateKind,
    pub priority: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalCaps {
    pub truecolor: bool,
    pub colors: u32,          // 8, 16, 256 or 16_777_216
    pub bracketed_paste: bool,
    pub mouse_sgr: bool,
    pub osc52: bool,
    pub term_name: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PathDescription {
    pub rung: Rung,
    pub local: std::net::SocketAddr,
    pub remote: std::net::SocketAddr,
    pub probes_sent: u32,
    pub nat_type: NatType,
    pub rtt_ms: u32,
    pub mtu: u16,
}

// ---- signalling over SSH: newline-delimited serde_json ----

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Signal {
    HostHello {
        proto: u32,
        session_id: String,             // hex
        /// Which attach generation this is. Both `seq` counters reset to 1 at
        /// every attach, so the two ends must agree on the generation;
        /// otherwise a host already serving a session cannot tell a second
        /// `--attach` from the current one. Signalling and meta.json only —
        /// never per-frame, since each attach is a distinct QUIC connection.
        attach_id: u64,
        cert_spki_sha256: String,       // base64
        psk: String,                    // base64, 32 bytes
        candidates: Vec<Candidate>,
        nat_type: NatType,
        bound_port: u16,
        /// The host's INTENT, not the outcome. `HostHello` is written BEFORE
        /// the ladder runs — the candidates travel in it — so at this point
        /// nobody yet knows which rung will be nominated. False here only when
        /// the host already knows it cannot detach.
        ///
        /// Actual detachability is settled LATER, by the nominated rung, and
        /// that is the only place it becomes `SessionMeta.detachable`. A
        /// session that daemonized on intent and then landed on rung 4 would
        /// have closed the very SSH descriptors it needs to carry its data.
        detachable: bool,
    },
    ClientHello {
        proto: u32,
        candidates: Vec<Candidate>,
        nat_type: NatType,
        caps: TerminalCaps,
        size: TermSize,
    },
    CandidateUpdate { candidates: Vec<Candidate> },
    Established { path: PathDescription },
    Failed { reason: String },
}

/// Reads/writes `Signal` as newline-delimited JSON.
pub fn write_signal<W: std::io::Write>(w: &mut W, s: &Signal) -> Result<(), ProtoError>;
pub fn read_signal<R: std::io::BufRead>(r: &mut R) -> Result<Signal, ProtoError>;

// ---- datagram framing: postcard ----

pub const FLAG_ZSTD: u8 = 0x01;

#[derive(Clone, Debug, Serialize, Deserialize)]
/// FIELD ORDER IS WIRE-SIGNIFICANT. postcard serialises in declaration order,
/// so reordering these silently breaks interoperability with no useful error.
/// Do not tidy this struct, and do not re-add fragmentation fields — see the
/// channel-selection note below for why they are gone.
pub struct Frame {
    pub my_state: u64,
    pub from_state: u64,
    /// The highest state OF THE PEER'S that we have applied. **Zero is legal
    /// and means "nothing of yours yet" (R5)** — it is never a real sequence
    /// number, so it cannot collide with I3.
    pub ack_state: u64,
    pub flags: u8,
    pub payload: Vec<u8>,
}

// ---- CHANNEL SELECTION: which transport carries this Frame ----
//
// There is NO datagram fragmentation. It was specified, reviewed, and removed:
// a state of F fragments needs all F to arrive with no retransmission, so
// delivery probability is (1-p)^F. A 200x60 truecolor full state is ~125
// fragments — 28% delivery at 1% loss, 0.16% at 5%. Worse, the ring-miss
// recovery path is OBLIGED to send a full state, so the mechanism most needed
// after a burst of loss was the one least able to survive it.
//
// Instead, size picks the channel:
//
//   fits in one datagram  -> QUIC unreliable datagram. The common case:
//                            incremental diffs and keystrokes. Latency wins,
//                            and a loss costs nothing because the next diff
//                            re-diffs from the same ack and contains it.
//
//   larger                -> a FRESH unidirectional QUIC stream, reliable and
//                            ordered. A full state is a recovery mechanism, not
//                            a latency-critical one, so reliability is right.
//
//   no datagrams at all   -> NOTHING is sent, and the sender says so once, out
//                            loud. `max_datagram_size()` returning `None` means
//                            the peer never advertised
//                            `max_datagram_frame_size`; it is not a missing
//                            number to guess at, and it is not a cue to put
//                            every frame on a stream.
//
// That last line is a behavioural rule, not a definition, and it was added
// because the code did the opposite: with datagrams off it fell through to the
// stream path silently, so keystrokes and diffs alike each took a fresh
// unidirectional stream, one at a time, at one frame per pacing interval, with
// everything offered in between dropped. That is a terminal that feels
// mysteriously broken instead of a configuration bug anyone can find, and it
// converts the recovery channel into the whole transport. Both ends of oxutrm
// set both datagram buffer sizes, so `None` means the config grew a hole or the
// peer is not oxutrm — and neither is something to paper over. A refusal is
// still not fatal: the session survives, as every send failure must.
//
// The rule that keeps "never stale, never behind" true on the stream path:
// if a newer state becomes current while such a stream is still in flight,
// RESET_STREAM it and open a new stream for the CURRENT state. Never queue.
// The receiver then gets nothing rather than something out of date.
//
// "In flight" means STILL BEING WRITTEN, never "was started once". A writer
// that finished, failed, or was reset stops counting immediately, so a state
// that does not advance can be offered again after a lost attempt. Held the
// other way — as it was — a stream that could not even be opened pinned its
// state for ever, every state got exactly one attempt in its whole life, and
// retry did not exist; a long-finished stream was also reported as superseded
// by the next one, which made the outcome unable to distinguish a real reset.
//
// Streams may complete out of order; the receiver applies one only if its
// `my_state` is newer than what it holds. `Frame`'s sequence numbers already
// answer that, so no extra machinery is needed.

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError>;
    pub fn decode(bytes: &[u8]) -> Result<Frame, ProtoError>;
}

// ---- stream messages: postcard ----

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ControlMsg {
    SessionInfo { session_id: String, shell: String, created_unix: u64 },
    CapsUpdate(TerminalCaps),
    StatusRequest,
    StatusReply(PathDescription),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScrollbackReq { pub from_line: u64, pub to_line: u64 }

#[derive(thiserror::Error, Debug)]
pub enum ProtoError {
    #[error("protocol version mismatch: peer {peer}, ours {ours}")]
    VersionMismatch { peer: u32, ours: u32 },
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
```

### `oxutrm-term`

```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Color { Default, Idx(u8), Rgb(u8, u8, u8) }

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
    pub struct Attrs: u16 {
        const BOLD      = 0b0000_0001;
        const ITALIC    = 0b0000_0010;
        const UNDERLINE = 0b0000_0100;
        const INVERSE   = 0b0000_1000;
        const BLINK     = 0b0001_0000;
        const STRIKE    = 0b0010_0000;
        const DIM       = 0b0100_0000;
        const HIDDEN    = 0b1000_0000;
        /// Right-hand half of a double-width character (`Flags::WIDE_CHAR_SPACER`).
        const WIDE_CONT = 0b0001_0000_0000;
        // BLINK is NOT a native alacritty flag: vte parses SGR 5/6/25 but
        // `Term::terminal_attribute` drops them. It is recovered by a newtype
        // wrapping `Term` that implements `vte::ansi::Handler`, forwards every
        // method, and intercepts those three into a parallel blink plane keyed
        // by `term.grid().cursor.point`. STRIKE and HIDDEN ARE native.
        //
        // v1 maps all five alacritty underline variants (UNDERLINE,
        // DOUBLE_UNDERLINE, UNDERCURL, DOTTED_UNDERLINE, DASHED_UNDERLINE) onto
        // UNDERLINE. Styles and per-cell underline colour (SGR 58/59) are
        // available for a later milestone.
    }
}

/// Inline for up to 24 bytes, so a cell holding one ASCII char — or one char
/// plus a combining mark — allocates NOTHING. This matters because the design
/// keeps a ring of 32 states: with `String` an 80x24 session would hold roughly
/// 61,000 live heap allocations. Wire encoding is identical (both serialise as
/// a str), so this alias can be swapped in one line without a protocol change.
pub type CellText = compact_str::CompactString;

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cell { pub text: CellText, pub fg: Color, pub bg: Color, pub attrs: Attrs }
impl Default for Cell;                 // " ", Default, Default, empty
impl Cell { pub fn blank() -> Cell; }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum CursorShape { Block, Underline, Bar }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cursor { pub row: u16, pub col: u16, pub visible: bool, pub shape: CursorShape }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum MouseMode { Off, Press, PressRelease, ButtonMotion, AnyMotion }

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Modes {
    pub alt_screen: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseMode,
    pub app_cursor: bool,
    pub app_keypad: bool,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ScreenState {
    /// Starts at 1. Zero is reserved as the "full state" sentinel.
    /// Resets to 1 at every attach.
    pub seq: u64,
    pub rows: u16,
    pub cols: u16,
    pub cells: Vec<Cell>,          // rows * cols, row-major, len is exact
    pub cursor: Cursor,
    pub modes: Modes,
    pub title: String,
    // NOTE: no `icon`. OSC 1 is SILENTLY DROPPED by vte — there is no `b"1"`
    // arm in osc_dispatch and no Handler method. Verified empirically.
    pub bell: u32,
    pub scrollback_len: u64,
}

impl ScreenState {
    pub fn blank(rows: u16, cols: u16) -> ScreenState;
    pub fn cell(&self, row: u16, col: u16) -> &Cell;
    pub fn row(&self, row: u16) -> &[Cell];

    /// Checks I1-I3 — every invariant that a single state can show. Called by
    /// every constructor. A comment is not a constraint anyone checks — this
    /// is.
    pub fn validate(&self) -> Result<(), ApplyError>;

    /// Checks I5 and I6, which only exist BETWEEN two states. Calls
    /// `validate` on `self` first, so it is a superset.
    ///
    /// Its production caller is `Receiver::on_frame`, which runs it after
    /// `apply` with the pre-application state as `previous`. That call is
    /// normative: without it I5 and I6 are enforced NOWHERE, which is exactly
    /// what was true until it was added — the doc claimed the sync layer ran
    /// it and only `oxutrm-proto`'s own tests ever did.
    ///
    /// A failure is a REJECTED FRAME, never a fatal error: `on_frame` applies
    /// to a clone, so the state and the ack are untouched.
    pub fn validate_transition(&self, previous: &ScreenState) -> Result<(), ApplyError>;
}

// ---- ScreenState INVARIANTS — enforced, not merely documented ----
//
// I1. `cells.len() == rows as usize * cols as usize`, EXACTLY. Row-major.
//     A diff that would break this is rejected with
//     `ApplyError::LengthMismatch`, never applied partially.
// I2. `cursor.row < rows` and `cursor.col < cols`. A diff carrying an
//     out-of-range cursor is rejected, not clamped: clamping would hide a
//     real desynchronisation between the two ends.
// I3. `seq >= 1`. Zero is the full-state sentinel and is never a real state.
// I4. `title` is set from OSC 0 and OSC 2 ONLY. There is no icon field:
//     `vte` silently drops OSC 1.
// I5. `bell` is a MONOTONIC counter, never a flag and never reset. The client
//     rings once per increment, so a reset would ring the terminal once for
//     every bell in the session's history. Enforced on the receiving side by
//     `validate_transition`, via `Receiver::on_frame`.
// I6. `scrollback_len` counts lines that NEVER travel in a datagram. The lines
//     themselves are fetched on a stream. It is synthesized by accumulating
//     `saturating_sub` of `Term::history_size()`, which saturates at capacity.
//     It never shrinks; enforced the same way as I5.
//     NOTE: it is deliberately NOT `history_size()` itself, which both
//     saturates and FALLS when the emulator is reset. `HostTerm` only ever
//     `saturating_add`s it, which is what makes enforcing I6 safe: a healthy
//     session cannot produce a frame this rejects. The scrollback FETCH path
//     is what reconciles the counter with the lines actually still held.
// I7. **THE DIMENSIONS ARE BOUNDED, AND THE BOUND IS CHECKED BEFORE THE
//     ALLOCATION.** `rows as usize * cols as usize <= MAX_SCREEN_CELLS`
//     (262_144) and `rows, cols <= MAX_SCREEN_DIM` (2048).
//
//     I1 to I6 constrain what a screen may CONTAIN and say nothing about how
//     BIG it may be, and that gap was a remote memory bomb: `rows` and `cols`
//     are `u16` and a `Cell` is ~40 bytes, so a peer-supplied `TermSize` in a
//     resize diff of a few bytes named 4.29e9 cells — about 170 GB — and both
//     ends keep a ring of states. Since R4 the RECEIVER keeps one too, so the
//     I7 ceiling is multiplied by `STATE_RING` on the parsing side as well:
//     I7 is load-bearing twice over, and without it R4 would have turned a
//     170 GB single allocation into a 5.4 TB one.
//
//     The ORDER is the rule, not a detail. Every other invariant here is
//     checked on a state that already exists, because building one is cheap
//     and being wrong about it is what costs. For I7 the building IS the cost,
//     so `TermSize::check_bounds()` is called BEFORE `vec![Cell::blank(); ..]`
//     at every site that allocates from a peer-chosen size:
//       * `ScreenState::blank`
//       * the `d.resize` arm of `ScreenState::apply` (`oxutrm-sync`)
//     `ScreenState::validate` checks it as well, so no oversized state can
//     exist however it was constructed — but validation alone is NOT
//     sufficient, and a future implementer who moves the check into `validate`
//     and deletes the pre-allocation call has reintroduced the bug with every
//     test still green except the one that says so.
//
//     A rejected resize is a REJECTED FRAME like any other: `ApplyError::
//     ScreenTooLarge`, the state untouched, the session alive.
//
//     The numbers are a ceiling, not a policy on window sizes. A 4K display at
//     a 6-pixel font is about 400x120 — 48,000 cells — so the cap is roughly
//     five times the largest terminal anyone runs, and one state is capped at
//     about 10 MB.

// I8. **TEXT THAT IS PAINTED IS TEXT, NOT CONTROL — AND ITS LENGTH IS BOUNDED
//     BEFORE IT IS EXPANDED.** No `Cell::text` and no `title` may contain a
//     scalar value in C0 (U+0000..=U+001F), U+007F, or C1 (U+0080..=U+009F).
//     A `Cell::text` is at most `MAX_CELL_TEXT` (32) bytes; a `title` is at
//     most `MAX_TITLE` (512) bytes.
//
//     I1 to I7 constrain the SHAPE of a screen — lengths, cursor, sequence,
//     dimensions — and say nothing about what a cell may CONTAIN. That gap is
//     the whole trust model inverted. The client renders a `ScreenState` by
//     writing escape sequences to the user's REAL terminal:
//     `Renderer::write_cells` appends `cell.text` verbatim and
//     `Renderer::write_title` interpolates `title` between `\x1b]0;` and BEL.
//     A host is not trusted — it is the machine the user connected to, and it
//     may be compromised — so a cell holding `\x1b]52;c;...\x07` is not a
//     character on a grid, it is a command to the terminal the user is sitting
//     in front of, and it writes their clipboard. Nothing on screen shows it:
//     the escape consumes itself and paints nothing. **The grid must be a
//     picture, never a channel.**
//
//     REJECTED, NOT STRIPPED, for I2's reason. Filtering the control bytes out
//     would paint a screen the host did not send and leave the two ends
//     silently disagreeing about every column after it. A refusal is visible:
//     `ApplyError::ControlInText` / `ApplyError::TextTooLong`, a REJECTED FRAME
//     like any other, the state untouched, the session alive.
//
//     The ORDER is the rule here too, exactly as in I7. The length half is
//     checked BEFORE the run is expanded, not after: a `Run` clones its cells
//     `repeat + 1` times, so one 60 MiB cell in a run repeated across a
//     2048-column row is about 123 GiB — built in the receiver, then
//     concatenated again into the renderer's single output buffer.
//     `MAX_DECOMPRESSED` bounds the frame and I7 bounds the cell COUNT;
//     neither bounds the bytes per cell, and the product is where the bomb
//     lives. This is the resize bomb's shape entering through the one
//     dimension I7 does not measure.
//
//     32 bytes is a ceiling, not a policy: the longest thing a cell
//     legitimately holds is a base character plus its combining marks, which
//     `HostTerm` builds from `cell.zerowidth()`. Measured against real scripts,
//     the widest cluster found is 15 bytes — a Tibetan stack, a Hebrew word
//     with nikud and cantillation, a Devanagari cluster with four marks — so 32
//     is a 2x margin, pinned by a test so a regression reads as a lost margin
//     rather than as a mystery. It IS a deliberate cut below what Unicode
//     permits: UAX #15's Stream-Safe Text Format tolerates 30 non-starters
//     after a base, which would need 94 bytes. No script needs that, and the
//     cut is what the host-side fit below exists to make safe. 512 is
//     comfortably past any real window title.
//
//     **The host MAINTAINS I8; it does not merely check it.** This is the site
//     that is easy to forget and the reasoning is not what it first appears:
//     `alacritty_terminal` puts NO cap on `Cell::zerowidth()` —
//     `push_zerowidth` is an unbounded `Vec::push` — so a program printing
//     combining marks in a loop grows one cell without limit. That is ugly
//     output, not an attack, and an honest host would then ship a state its own
//     peer must refuse, freezing the session for reasons no log explains. So
//     the host TRUNCATES while building the cell rather than asserting on the
//     finished one: filtering a megabyte of marks down to 32 bytes still costs
//     the megabyte if you assemble it first. The title is fitted where the
//     `ScreenState` is built, so every path in the crate inherits it.
//
//     Truncation at the source is NOT the "stripping" this invariant forbids,
//     and the distinction is the whole point. The host is fitting text it
//     AUTHORED, before any sequence number names it; the client is refusing
//     text a PEER sent. One buys liveness for honest hosts, the other buys
//     safety for the user, and a hostile host simply would not run the first.
//     Never move the host's fit to the client side.
//
//     The diff's TITLE is checked at the TOP of `ScreenState::apply`, not where
//     it is assigned. The assignment sits below the row loop, so checking there
//     would leave a half-painted screen behind on a refusal, and `ApplyError`
//     promises that nothing was applied. One line of ordering keeps that true.
//
//     I8 does NOT cover exit-time restoration, and that hole is still open. A
//     hostile host can set `alt_screen`, mouse reporting and a hidden cursor,
//     all of which are legitimate mid-session, and then drop the connection.
//     `RawGuard` restores termios and emits no escape at all, so the user is
//     returned to a shell on the alternate buffer with SGR mouse reports on
//     every twitch. No content invariant fixes that; it needs a guaranteed
//     teardown emission — leave alternate buffer, all mouse modes off, cursor
//     shown, SGR reset, bracketed paste off — on the same paths `RawGuard`
//     already covers. It must be writable from a signal handler, which
//     constrains it to a single `write(2)` of a `const` byte string: no
//     allocation, async-signal-safe, the discipline `restore_and_reraise`
//     already follows.

/// PTY + `alacritty_terminal::term::Term`, fed by a re-exported
/// `alacritty_terminal::vte::ansi::Processor`. Owns the child process.
///
/// Feed bytes with `processor.advance(&mut term, bytes)` — `Term` IS the vte
/// `Handler`. Title, bell and OSC 52 arrive as `Event`s through an
/// `EventListener`, whose single method takes `&self`, so the listener needs
/// interior mutability. OSC 52 payloads arrive ALREADY base64-decoded; set
/// `Config::osc52 = Osc52::CopyPaste` to receive paste requests.
///
/// Scrollback is native and O(1): negative `Line` indices reach history.
/// Do NOT hand-roll a ring. `Term::resize` genuinely REFLOWS the primary grid
/// (never the alternate one) and is lossless in both directions.
///
/// THREE hard obligations, each easy to miss:
///   1. `Index<Point>` has only a `debug_assert` — out of range PANICS in debug
///      and reads garbage in release. Route EVERY access through one checked
///      accessor that clamps with `Point::grid_clamp(&dims, Boundary::Grid)`.
///   2. The crate ships NO default palette. `Term::colors()` is an OSC 4/10/11
///      OVERRIDE table, all-`None` by default. oxutrm supplies its own 269-entry
///      table and consults `colors()` only as an override layer. Layout, all
///      ranges half-open: 0..16 named, 16..232 cube (216), 232..256 grayscale
///      (24), 256 fg, 257 bg, 258 cursor, 259..267 the eight dim variants,
///      267 bright foreground, 268 dim foreground. Sums to exactly 269.
///      There is no dim BACKGROUND — `NamedColor` ends
///      `BrightForeground, DimForeground`.
///      DIM/BOLD-to-bright promotion is the RENDERER's job, not the crate's.
///   3. There is NO monotonic scrolled-off counter; `history_size()` saturates.
///      Synthesize `scrollback_len` by accumulating `saturating_sub` of
///      `history_size()` across each `advance()`.
///
/// Use `Term::damage()` / `reset_damage()` for per-line dirty ranges instead of
/// comparing whole grids — it answers exactly the question the diff engine asks.
pub struct HostTerm { /* private */ }

impl HostTerm {
    pub fn spawn(
        shell: &str,
        args: &[String],
        env: &[(String, String)],
        size: TermSize,
        scrollback: usize,
    ) -> anyhow::Result<HostTerm>;

    /// Write user input to the PTY.
    pub fn write_input(&mut self, bytes: &[u8]) -> anyhow::Result<()>;

    /// Resize the PTY and the emulator.
    pub fn resize(&mut self, size: TermSize) -> anyhow::Result<()>;

    /// Drain whatever the PTY has ready, without blocking.
    /// Returns true if the screen changed.
    pub fn poll(&mut self) -> anyhow::Result<bool>;

    /// Build a state carrying the given sequence number.
    pub fn snapshot(&self, seq: u64) -> ScreenState;

    /// Scrollback lines [from, to) as rendered cell rows.
    pub fn scrollback(&self, from: u64, to: u64) -> Vec<Vec<Cell>>;

    pub fn child_exited(&mut self) -> Option<i32>;
}

/// Detect the local terminal's capabilities from the environment.
pub fn detect_caps() -> TerminalCaps;

/// Derived SOLELY from what `alacritty_terminal` emulates. The client's
/// capabilities must NOT
/// influence this: the child's TERM cannot change when a differently-capable
/// client reattaches, and down-converting here would permanently degrade the
/// host's state. All capability adaptation happens in the client.
pub fn negotiate_term() -> (String /*TERM*/, Option<String> /*COLORTERM*/);
```

### `oxutrm-sync`

```rust
/// The depth of BOTH rings, and they are coupled. The sender can only name a
/// base within its last `STATE_RING` updates, so a receiver cap of at least
/// `STATE_RING` is what guarantees the named base is still held. Lowering the
/// receiver's cap alone silently reinstates the R4 defect — and HIDES it,
/// because in that regime ring exhaustion degrades every frame to a full state
/// and a full state applies unconditionally (R3). The rescue masks the bug.
pub const STATE_RING: usize = 32;

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum ApplyError {
    #[error("diff base {base} does not match current state {current}")]
    BaseMismatch { base: u64, current: u64 },
    #[error("diff refers to row {row} outside {rows} rows")]
    OutOfBounds { row: u16, rows: u16 },
    #[error("decode: {0}")]
    Decode(String),
    #[error("cells length {len} does not match {rows}x{cols}")]
    LengthMismatch { len: usize, rows: u16, cols: u16 },
    #[error("cursor ({row},{col}) outside {rows}x{cols}")]
    CursorOutOfBounds { row: u16, col: u16, rows: u16, cols: u16 },
}

/// A replicated value. No I/O, no clocks, no allocation assumptions.
pub trait SyncState: Clone {
    type Diff: serde::Serialize + serde::de::DeserializeOwned;
    fn seq(&self) -> u64;
    fn set_seq(&mut self, seq: u64);
    /// Diff that turns `base` into `self`.
    fn diff_from(&self, base: &Self) -> Self::Diff;
    fn apply(&mut self, base: u64, target: u64, d: &Self::Diff) -> Result<(), ApplyError>;
    /// This value's own invariants.
    fn validate(&self) -> Result<(), ApplyError>;
    /// The invariants that exist only BETWEEN two states. `Receiver::on_frame`
    /// calls THIS after `apply`, not `validate`, with the state being replaced
    /// as `previous`. The default implementation is `self.validate()`, so a
    /// state with no transition rules needs nothing; `ScreenState` overrides
    /// it to enforce I5 and I6.
    fn validate_transition(&self, previous: &Self) -> Result<(), ApplyError> { self.validate() }
    /// A diff from nothing: used when the peer's ack has left the ring.
    fn full_diff(&self) -> Self::Diff;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
/// The `cells` sequence is emitted `repeat + 1` times consecutively, starting
/// at `start_col`. `repeat == 0` therefore means "emit `cells` exactly once".
pub struct Run { pub start_col: u16, pub repeat: u16, pub cells: Vec<Cell> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RowPatch { pub row: u16, pub runs: Vec<Run> }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScreenDiff {
    // NOTE: base/target live in `Frame` only. Never duplicate them here.
    pub resize: Option<TermSize>,
    pub rows: Vec<RowPatch>,
    pub cursor: Option<Cursor>,
    pub modes: Option<Modes>,
    pub title: Option<String>,
    pub bell: Option<u32>,
    pub scrollback_len: Option<u64>,
}

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct InputState { pub seq: u64, pub pending: Vec<u8>, pub size: TermSize }

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InputDiff {
    // NOTE: base/target live in `Frame` only. Never duplicate them here.
    /// Bytes the host has consumed; dropped from the FRONT of `pending`.
    pub consumed: u64,
    pub appended: Vec<u8>,
    pub size: Option<TermSize>,
}
// apply() is defined as: drop `consumed` bytes from the front, THEN append
// `appended`. Getting this order wrong writes consumed input to the PTY twice.

impl SyncState for ScreenState { type Diff = ScreenDiff; }
impl SyncState for InputState  { type Diff = InputDiff; }

/// Keeps a ring of recent states and emits diffs against the peer's ack.
pub struct Sender<S: SyncState> { /* private */ }

impl<S: SyncState> Sender<S> {
    pub fn new(initial: S) -> Sender<S>;
    /// Replace the current state. Assigns the next sequence number.
    pub fn update(&mut self, next: S);
    pub fn on_ack(&mut self, peer_saw: u64);
    pub fn current(&self) -> &S;
    /// `None` when the peer is already up to date.
    /// Compresses with zstd when that shrinks the payload; sets FLAG_ZSTD.
    pub fn make_frame(&self, ack_state: u64) -> Result<Option<Frame>, ApplyError>;
}

/// Keeps a ring of recent states too, so a diff can name a base it has left.
pub struct Receiver<S: SyncState> { /* private */ }

impl<S: SyncState> Receiver<S> {
    pub fn new(initial: S) -> Receiver<S>;
    /// Returns true when the state advanced. Stale or duplicate frames
    /// return Ok(false) — they are never an error.
    ///
    /// Applies to a CLONE, then runs `validate_transition` on the result with
    /// the state being replaced as `previous`. Nothing is committed unless
    /// both succeed, so an `Err` is a rejection that leaves the state and the
    /// ack exactly as they were. A rejected frame NEVER disconnects the
    /// session; the host and client loops log it and go on.
    pub fn on_frame(&mut self, f: &Frame) -> Result<bool, ApplyError>;
    pub fn state(&self) -> &S;
    /// The sequence number to put in our outgoing `ack_state`.
    /// ZERO until the peer's first frame has been applied — see R5.
    pub fn ack(&self) -> u64;
    /// The peer's `ack_state` from the last frame we accepted.
    pub fn peer_ack(&self) -> u64;
}

// ---- THE FIRST FRAME OF AN ATTACH — a rule whose absence cost a real bug ----
//
// Both ends independently construct an initial state numbered 1, holding
// COMPLETELY DIFFERENT content: the host's comes from the live emulator, the
// client's from a blank screen. A sequence number says which GENERATION, never
// which CONTENT, so nothing notices the collision.
//
// R1. The host's first frame of every attach IS a full state — a fresh `Sender`
//     has `peer_saw == 0`, finds no ring entry for 0, and takes the `full_diff`
//     branch with `from_state == 0`.
//
// R2. **A full state (`from_state == 0`) MUST apply even when `my_state` does
//     not exceed the seq the receiver currently holds.** This is the half that
//     was missing. The ordinary staleness test `my_state > state.seq()` rejects
//     the legitimate first full state at seq 1, after which the client keeps its
//     invented blank screen and every later diff arrives against a base it never
//     reached. The session deadlocks until ring eviction accidentally rescues it,
//     which is why the symptom looked like flakiness.
//
// R3. Because the sentinel applies against any held seq, a full state is also
//     the protocol's universal recovery: whenever the peer's ack has left the
//     ring, `from_state == 0` re-synchronises unconditionally.
//
// Do not "optimise" R2 back into a plain `>` comparison.
//
// R4. **A diff applies against whichever state the receiver HELD at
//     `from_state`, not only against the one it holds now.** The `Receiver`
//     keeps a ring for the same reason the `Sender` does. The sender diffs
//     against the newest state the peer ACKNOWLEDGED, and an ack takes a round
//     trip, so every frame sent inside that window names a base the receiver
//     has already left. Requiring `from_state == state.seq()` throws all of
//     them away — and each one carries a state strictly NEWER than the one
//     being shown. Measured under the runaway-writer flood: 44 of 89 screen
//     frames dropped at one round trip of ack latency, 63 of 72 at eight, with
//     the client's screen running 15 generations behind the host. It never
//     deadlocks, because the sender re-diffs from the same ack, which is
//     exactly why it was invisible.
//
//     The ring is self-pruning: a frame's `from_state` reveals which base the
//     sender is still working from, and nothing older can be named again.
//     Steady state is two entries **while diffs are flowing**. The prune is
//     keyed on `from_state`, and a full state carries `from_state == 0`, so a
//     RUN OF FULL STATES PRUNES NOTHING and the ring fills to `STATE_RING`.
//     That run is exactly the R3 recovery regime, so the reassuring number is
//     false in precisely the regime that matters. `STATE_RING` is the cap and
//     in that regime it is the only bound: 32 x 262_144 cells x ~40 B is about
//     320 MiB per side, which I7's ceilings make survivable and nothing else
//     does. A base older than the ring is still a `BaseMismatch` — refused,
//     never guessed at.
//
// R5. **A receiver MUST NOT acknowledge a state it invented.** `ack()` is 0
//     until the peer's first frame has been applied. This is R1 seen from the
//     acknowledging side: the receiver's initial state is numbered 1 like
//     everyone else's, and acknowledging that 1 tells the sender "I hold YOUR
//     state 1", so the sender diffs against a state the receiver has never
//     seen and never can. Zero means "I have nothing of yours" — true, and the
//     one value `make_frame` cannot find in its ring, so it sends the full
//     state R1 requires. Without R5 the very first frame after an attach is
//     unapplicable, and the session is rescued only by ring eviction later.
//
//     **R5 corollary — a reattach MUST construct a NEW `Sender`/`Receiver`
//     pair on both ends.** `Sender::on_ack` is monotonic, so a fresh
//     receiver's `ack() == 0` cannot pull a live sender's `peer_saw` back down.
//     The attach keys already require both `seq` counters to reset to 1 at
//     every attach; this is the same rule stated where it can be violated.
//     `HostSession::spawn` builds its sender once, so a reattach path that
//     REUSES it gets a client correctly acking 0 against a sender whose
//     `peer_saw` is still high, and every host frame is unapplicable until ring
//     eviction accidentally rescues it. Recorded now, before M3's reattach is
//     written, because R5 turns a pre-existing hazard into a certainty.
//
// R4 and R5 are the same lesson as R2, and it is the lesson of this whole
// section: a sequence number names a GENERATION, never a piece of content, and
// it means nothing at all unless both ends agree who AUTHORED that generation.

/// Trim consumed input after the host acknowledges it.
impl InputState {
    pub fn append(&self, bytes: &[u8], size: TermSize) -> InputState;
    pub fn consume(&self, n: usize) -> InputState;
}
```

### `oxutrm-net`

```rust
#[derive(Clone, Debug)]
pub struct NetConfig {
    pub stun_servers: Vec<String>,
    pub prefer_port: u16,              // 443
    pub enable_port_mapping: bool,
    pub enable_birthday: bool,
    pub birthday_sockets: u16,         // 256
    pub birthday_ports: u16,           // 256
    pub birthday_budget: std::time::Duration,
    pub gather_timeout: std::time::Duration,
}
impl Default for NetConfig;

/// Bind the session socket, preferring UDP/443, dual-stack where possible.
pub fn bind_socket(cfg: &NetConfig) -> anyhow::Result<std::net::UdpSocket>;

/// Local interface addresses as `CandidateKind::Host`.
pub fn local_candidates(socket: &std::net::UdpSocket) -> Vec<Candidate>;

/// NAT-PMP, then PCP, then UPnP-IGD. Refreshed for the session's life.
pub struct PortMapping { /* private */ }
impl PortMapping {
    pub async fn acquire(local_port: u16, cfg: &NetConfig) -> Option<(PortMapping, Candidate)>;
}
impl Drop for PortMapping;             // releases the mapping

/// Query several STUN servers from `socket`; classify the NAT by comparing
/// the mapped addresses they report.
///
/// TWO probes decide, a third refines. P1 is the first server; P2 is a server
/// at a genuinely DIFFERENT IP, and a mapping that differs between the two is
/// `Symmetric`. P3 is a second port on P1's IP and separates
/// `AddressDependent` from `Symmetric` — but it runs ONLY when
/// `cfg.stun_servers` names such a port (two entries, same resolved IP,
/// different ports). It is never `P1.port() + 1`.
///
/// CHANGED, and why: P3 used to be the guess `P1.port() + 1`, and no server in
/// `NetConfig::default()` answers there, so P3 always timed out and the
/// `Symmetric` arm — the one thing that sends the ladder straight to rung 3 —
/// was unreachable outside the tests, which stood a responder up on `port + 1`
/// to manufacture it. `P1` vs `P2` differing therefore now yields `Symmetric`
/// where it used to yield `Unknown`; without P3 that verdict is merged with
/// `AddressDependent`, which is deliberate. Rung 2's premise is a reusable
/// server-reflexive candidate, and a per-destination mapping has already
/// broken it, so the merged verdict skips a rung that could not have worked;
/// the old `Unknown` instead burned the whole gather budget on it.
pub async fn stun_discover(
    socket: &tokio::net::UdpSocket,
    cfg: &NetConfig,
) -> (Vec<Candidate>, NatType);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IceRole { Controlling, Controlled }   // client is Controlling

pub enum IceEvent {
    NewLocalCandidate(Candidate),
    Nominated { local: std::net::SocketAddr, remote: std::net::SocketAddr, rung: Rung, probes: u32 },
    Failed(String),
}

/// ICE connectivity checks: STUN Binding Requests with MESSAGE-INTEGRITY
/// keyed by the shared PSK.
/// The client is ALWAYS `IceRole::Controlling`, and only the controlling side
/// nominates — otherwise asymmetric loss makes the two sides nominate different
/// pairs. Direction-labelled credentials are derived from the shared psk with
/// HKDF-SHA256, info strings `"oxutrm ice c2h"` and `"oxutrm ice h2c"`, so a
/// side can tell its own reflected check from a genuine peer check.
///
/// Nomination MUST complete BEFORE QUIC starts. QUIC connection migration only
/// lets a client change its own LOCAL address; there is no protocol mechanism
/// and no quinn API to repoint an established connection at a different REMOTE
/// address. A better path found later is lost for that attach.
pub struct IceAgent { /* private */ }
impl IceAgent {
    pub fn new(psk: [u8; 32], role: IceRole, cfg: NetConfig) -> IceAgent;
    pub fn add_local(&mut self, c: Candidate);
    pub fn add_remote(&mut self, c: Candidate);
    pub async fn run(&mut self, socket: std::sync::Arc<tokio::net::UdpSocket>) -> IceEvent;
}

/// True when the datagram is STUN rather than QUIC.
/// STUN: top two bits are 00. QUIC: fixed bit (0x40) is set.
pub fn is_stun(datagram: &[u8]) -> bool;

/// quinn owns the socket's recv loop, so STUN and QUIC CANNOT both call recv
/// on the same socket — they race and steal each other's packets. This wrapper
/// peels STUN off the front and passes everything else to quinn. Construct the
/// endpoint with `Endpoint::new_with_abstract_socket`, never `Endpoint::new`.
pub struct StunDemuxSocket { /* private */ }
impl quinn::AsyncUdpSocket for StunDemuxSocket { /* ... */ }
impl StunDemuxSocket {
    pub fn new(inner: std::sync::Arc<tokio::net::UdpSocket>) -> (StunDemuxSocket,
        tokio::sync::mpsc::Receiver<(Vec<u8>, std::net::SocketAddr)>);
}

/// The default gateway, needed because `crab_nat` takes the gateway address
/// and ships no discovery of its own. Use `netdev::get_default_gateway()`:
/// netlink on Linux, the route socket on the BSDs and macOS.
pub fn default_gateway() -> Option<std::net::IpAddr>;

/// Self-signed certificate plus the SHA-256 of its SPKI.
pub fn generate_cert() -> anyhow::Result<(rustls::pki_types::CertificateDer<'static>,
                                          rustls::pki_types::PrivateKeyDer<'static>,
                                          [u8; 32])>;

pub async fn quic_server(
    socket: std::net::UdpSocket,
    cert: rustls::pki_types::CertificateDer<'static>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::Endpoint>;

/// The pinning verifier checks the SPKI hash AND performs real TLS 1.3
/// signature verification by delegating to the default provider. Stubbing
/// `verify_tls12_signature` / `verify_tls13_signature` / `supported_verify_schemes`
/// to `Ok(())` — the usual copy-paste — throws away proof that the peer holds
/// the private key and reduces pinning to merely knowing the certificate bytes.
/// rustls 0.23 also needs an explicit `CryptoProvider` installed before
/// `QuicClientConfig::try_from` will succeed.
pub async fn quic_client(
    socket: std::net::UdpSocket,
    peer: std::net::SocketAddr,
    expect_spki_sha256: [u8; 32],
) -> anyhow::Result<quinn::Connection>;
```

// **QUIC MUST AUTHENTICATE THE CLIENT, AND IT MUST DO SO IN THE SAME CHANGE
// THAT FIRST WIRES THE DATA PATH — never afterwards.**
//
// `quic_server` is built `.with_no_client_auth()`. Pinning is one-directional
// by design, and the PSK gates PATH NOMINATION, not the QUIC handshake, so the
// gate people assume protects the port does not touch it: `IceAgent` verifies
// every check with PSK-derived credentials, but it never sees a QUIC packet.
// `StunDemuxSocket::poll_recv` splits the batch on `is_stun` and hands every
// non-STUN datagram to quinn FROM ANY SOURCE ADDRESS, with no reference to the
// nominated pair. Nomination decides where WE send; it decides nothing about
// what quinn accepts.
//
// What an unauthenticated connection reaches is not a probe surface, it is the
// session: inbound frames go to `on_frame` and the accumulated bytes are
// written to the PTY — keystroke injection into the user's shell — and a naive
// accept loop calls `HostSession::spawn`, which starts a fresh shell as the ssh
// user. That is remote code execution, not a lesser harm.
//
// The exposure TODAY is nil, because there is no production caller: both
// `--serve` and connect return "not wired up yet", and every call site of
// `quic_server`/`quic_client` is a test. That is exactly why this is cheap now
// and expensive later — the whole cost is test churn across eleven call sites.
//
// Do not argue this away with "the punched port is a moving target". The
// canonical oxutrm host is a public-IP server with NO NAT filtering, the socket
// prefers a FIXED port (443) before an ephemeral one, and the host's
// server-reflexive candidate — the exact IP:port — is handed to one of four
// third-party STUN operators by default. An on-path attacker or a malicious
// STUN server has it outright.
//
// The mechanism is SYMMETRIC SPKI PINNING, mirroring the verifier that already
// exists: the client generates a throwaway cert with `generate_cert()` and
// sends its fingerprint on the ssh channel; the host pins it in a
// `ClientCertVerifier`. The ordering already works — `HostHello` is written
// first, the client replies with `ClientHello`, and QUIC is built only AFTER
// nomination — so the host holds the fingerprint well before it needs it.
// `ClientHello` gains `cert_spki_sha256` and `PROTO_VERSION` goes to 2.
//
//   * **The three signature methods MUST delegate to the provider**, for the
//     identical reason given above for the server verifier. Stubbing them here
//     reproduces the same hole pointing the other way.
//   * A TLS external PSK is NOT available — rustls 0.23 exposes no such API.
//     An application-level first-frame check over the TLS exporter would work
//     and needs no proto change, but it leaves the attacker with a completed
//     handshake and open streams, and puts the whole guarantee in a "do not
//     read a frame or spawn anything before this check" ORDERING RULE. This
//     codebase makes such orderings structural instead — `DetachPermit` and
//     `Detached` exist for exactly that reason. Mutual pinning pushes the
//     rejection into the handshake, where the accept simply fails and nothing
//     downstream is reachable by mistake.
//
// Against the design precedent: Mosh authenticates EVERY datagram with a key
// passed over ssh and accepts a roaming peer only after a datagram
// authenticates. oxutrm's pinning authenticates the host to the client only; in
// the client-to-host direction it currently has no equivalent by any other
// means. Mutual pinning makes it equivalent or stronger — TLS 1.3 mutual auth,
// AEAD on every packet, and QUIC path validation for the roam.
//
// Cheap hardening for the SAME accept loop, none of which substitutes for the
// above: refuse an `Incoming` whose `remote_address()` is not the nominated
// remote (this does not break roaming — a roam reuses the connection via path
// validation, not a new handshake); prefer `ignore()` over `refuse()` so a port
// scanner gets silence; accept exactly ONE connection per attach and then stop,
// because a second inbound connection is always wrong under this model; and
// wire address validation against spoofed-source floods. There is no rate limit
// today and no accept path to hold one, so build it with the accept loop.
//
// **The PSK seam is still OPEN and must be closed by the same change.** The PSK
// and the host fingerprint are minted base64 at one end and consumed as raw
// `[u8; 32]` at the other, and NO base64 decoder exists anywhere in the tree to
// join them — the two encode sites are the only non-test base64 calls there
// are. Closing the seam already requires writing that decode path for two
// values; the client fingerprint is a third on the same path, so it costs one
// more call to a helper that has to be written regardless.

### `oxutrm-host`

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    /// The current attach generation, mirrored from `HostHello.attach_id`.
    pub attach_id: u64,
    pub pid: u32,
    pub created_unix: u64,
    pub shell: String,
    pub size: TermSize,
    /// Settled by the NOMINATED RUNG, never at handshake time:
    /// `detachable = rung != Rung::SshTunnel`. A rung-4 session tunnels QUIC
    /// over the SSH connection for its whole life, so it cannot close those
    /// descriptors, cannot daemonize, dies with its SSH, and cannot be
    /// reattached. Daemonization happens only AFTER this is settled.
    pub detachable: bool,
}

/// `$XDG_RUNTIME_DIR/oxutrm/<id>/` — dir 0700, files 0600. Never holds keys.
/// `/run/user/<uid>` is DESTROYED at logout unless lingering is enabled, which
/// would make every detached session unreachable. `dir()` therefore checks
/// `loginctl show-user <uid> --property=Linger` and falls back to
/// `$HOME/.local/state/oxutrm/`, warning loudly when it does.
pub struct Registry;
impl Registry {
    pub fn dir() -> anyhow::Result<std::path::PathBuf>;
    /// Prunes stale entries. The `$HOME` fallback is a real filesystem, not a
    /// tmpfs, so a reboot no longer clears them: an entry is stale when the pid
    /// is gone OR the pid now belongs to an unrelated process, checked against
    /// the recorded creation time.
    pub fn list() -> anyhow::Result<Vec<SessionMeta>>;
    pub fn socket_path(id: &str) -> anyhow::Result<std::path::PathBuf>;
}

pub struct RegistryGuard { /* private */ }
impl RegistryGuard {
    pub fn register(meta: &SessionMeta) -> anyhow::Result<RegistryGuard>;
}
impl Drop for RegistryGuard;           // removes the directory

/// Double fork, setsid, chdir /, close every inherited descriptor,
/// reopen 0/1/2 on /dev/null. Must be called only after HostHello is flushed.
///
/// Kept for callers with nothing left to say over the ssh pipes. `host --serve`
/// cannot use it: see the two phases below.
pub fn daemonize() -> anyhow::Result<()>;

/// Detaching is TWO operations, and they are not safe at the same moment.
/// Forking must happen before any thread exists; the rung that decides whether
/// severing is allowed cannot be known without an async ICE ladder, which needs
/// a runtime, whose threads do not survive a fork. Welded together they are
/// unsatisfiable. Split, the ladder runs BETWEEN them.
///
/// Phase 1: fork, setsid, fork, umask. NO DESCRIPTOR IS TOUCHED, so it needs no
/// DetachPermit — forking is harmless for every rung, rung 4 included. Designed
/// to be the first statement of `host --serve`, which is what makes "fork
/// before any thread exists" structural rather than remembered.
///
/// The grandchild keeps 0/1/2 — sshd's pipes. sshd sends exit-status when the
/// process it spawned exits (the fork parent, immediately) but does not close
/// the channel until stdout and stderr reach EOF, so the local ssh stays alive
/// for the whole handshake and ladder. That is what bidirectional candidate
/// exchange requires. Do NOT move descriptor closure into this phase.
pub fn detach_process() -> anyhow::Result<Detached>;

/// Proof that detach_process already ran in THIS process. Severing before
/// forking closes ssh's pipes while still in ssh's session holding ssh's
/// controlling terminal: ssh exits, SIGHUP arrives, the session dies.
pub struct Detached { /* private */ }

/// Phase 2: chdir /, close every inherited descriptor, reopen 0/1/2 on
/// /dev/null. Call after Established is flushed. THIS is what DetachPermit
/// gates, and it is the operation the permit's own documentation describes:
/// a rung-4 session never obtains one and therefore keeps its pipes for life.
/// When this returns, ssh sees EOF and exits — the session is now detached.
pub fn sever_from_ssh(detached: Detached, permit: DetachPermit) -> anyhow::Result<()>;

// There is deliberately no `transport::Path` here. One was written — an enum
// over "datagram path" and "rung-4 tunnel", exposing a `max_payload() -> usize`
// so that nobody could write `max_datagram_size().unwrap_or(1200)` — and it
// acquired no production caller, while the real send site in `src/link.rs` did
// the very thing it forbade. It was removed rather than wired in, for two
// reasons beyond its being unused.
//
// Its `max_payload` was decided once, when the path was built. On a live QUIC
// connection that is wrong: the datagram limit shrinks with the path MTU, so a
// cached one survives a migration that invalidated it. `FrameSink::send` asks
// the connection per frame instead.
//
// And its tunnel half described a wire nothing speaks. `oxutrm_net::ice` never
// nominates `Rung::SshTunnel`, so rung 4 has no implementation to be abstracted
// from; its framing and its size limit will be written next to the code that
// carries them, where the size question gets asked again with a real answer.
//
// The rule the type existed to hold now lives in `FrameSink::send`, on the path
// a frame actually takes, with a test on that path. This is the lesson from the
// fragmentation removal restated: a rule that lives only in prose or in an
// abstraction nobody calls is a rule nobody implements.

// When rung 4 IS built, its framing MUST carry both properties the removed code
// had, because they are the reason it was worth writing and they are easy to
// omit when rewriting from scratch:
//   1. An explicit length prefix. The ssh channel is a byte stream with no
//      message boundaries of its own, so the tunnel must supply them.
//   2. The length is validated against the maximum BEFORE any allocation, so a
//      corrupt or hostile prefix cannot ask for four gigabytes. Validate, then
//      allocate — never the reverse.
// Recorded here rather than kept as uncalled code, because this project has
// twice found that an abstraction with no caller drifts away from the rule the
// live code follows.

// The `RungRunner` trait that once lived in `oxutrm-host::ladder` is removed for
// the same reason, and `ladder` now holds the connection POLICY only. The trait
// never appeared in this contract: an agent invented it, its only implementor
// was its own test double, and it could not have carried a real rung. Its
// `status_line` was an unauthorised second copy of the one specified under
// `oxutrm-client` below, and a wrong one — it printed every punched rung as
// "IPv4", including an IPv6 server-reflexive path.
//
// `oxutrm-host` MUST NOT depend on `oxutrm-net`. The driver belongs in the root
// binary, which already depends on both; its manifest records why this crate's
// narrowness is deliberate.
//
// When the ladder driver IS built, it MUST carry four properties. The first
// three are why the trait could not have worked, and are easy to lose when
// rewriting from scratch:
//   1. ONE SOCKET. Rungs 0 to 2 are not three attempts; they are three
//      candidate CLASSES on one socket, and `IceAgent` already races every pair
//      on it and reports which `Rung` the winner belonged to. Racing them as
//      separate futures means concurrent receive loops on one socket stealing
//      each other's datagrams — the failure `StunDemuxSocket` exists to
//      prevent. NAT mappings are per-socket: an address learned on any other
//      socket names a hole our traffic will never emerge from.
//   2. THE NOMINATION RETURNS THE SOCKET, not merely an address. Rung 3 punches
//      with a fresh socket and `birthday_blast` returns it, because the mapping
//      belongs to that socket and no other, and QUIC must adopt that exact one.
//      A result type of `SocketAddr`s silently drops it and leaves an address
//      describing a hole nothing owns.
//   3. NO MTU BEFORE THE HANDSHAKE. Path MTU is a `quinn` property discovered
//      after the handshake, which is strictly after nomination ends. A per-rung
//      `mtu` field can only be guessed, and a guess renders as a plausible
//      status line for a number nobody measured. The removed code carried one,
//      and a test asserted the fabricated value back.
//   4. EVERY RUNG'S REASON SURVIVES A TOTAL FAILURE, and a SKIPPED rung reads
//      differently from a FAILED one. "Connection failed" is useless; the rung
//      that got closest is the one worth reading, and which rungs were never
//      attempted is the first thing a bug report needs. `LadderPlan.skipped`
//      already carries the reasons as data, but nothing aggregates or renders
//      them, and the one test that held this property went with the trait.
//      This is the only requirement here that WAS tested and is now not.
```

### `oxutrm-client`

```rust
/// Diffs the desired screen against what is currently painted and emits
/// the minimal ANSI. Owns no terminal state beyond that model.
pub struct Renderer { /* private */ }
impl Renderer {
    pub fn new(size: TermSize, caps: TerminalCaps) -> Renderer;
    pub fn resize(&mut self, size: TermSize);
    /// Forget what is painted; the next render repaints everything.
    pub fn invalidate(&mut self);
    pub fn render<W: std::io::Write>(&mut self, w: &mut W, s: &ScreenState) -> std::io::Result<()>;
}

/// Raw mode on entry, restored on Drop, on panic, and on a catchable
/// termination signal.
pub struct RawGuard { /* private */ }
impl RawGuard { pub fn enter() -> anyhow::Result<RawGuard>; }
impl Drop for RawGuard;

pub fn terminal_size() -> anyhow::Result<TermSize>;

/// One connect-time line, then silence.
pub fn status_line(path: &PathDescription) -> String;
```

**A full repaint states every mode, in whichever direction.** When the painted
model is unknown — a first render, a resize, an `invalidate` after a path
migration — `render` emits `\x1b[?1049h` *or* `\x1b[?1049l`, the bracketed-paste
mode either way when the terminal has it, and the full mouse sequence even for
`MouseMode::Off`. "Nothing is known about the terminal" is not "the terminal is
in the default state". Emitting only the modes that wanted turning *on* is what
left a resized-then-quit vim behind on the alternate buffer, still reporting the
mouse, with the user's shell unusable and no way back but a blind `reset`.

**A mode that swaps the physical screen buffer forces a repaint, and is emitted
before it.** Crossing `alt_screen` invalidates the painted model in the same
pass: every cell it holds belongs to the buffer being left, so a diff against it
would paint nothing onto the buffer the user is now looking at. The bell is read
before that drop, so a ring is not lost to the swap.

**The painted model is committed only after a successful, complete write.** On
any error from `write_all` — including the short write `write_all` reports as
`WriteZero` — the model is invalidated and the next render repaints. A model
that claims a screen state the terminal never received poisons every later diff
permanently. This is the render-path form of the standing rule that a rejected
frame costs a repaint, never a session.

**`RawGuard::enter` claims SIGTERM, SIGINT, SIGHUP and SIGQUIT.** The handler
restores the terminal, resets the signal to its default disposition and
re-raises, so the process still dies of the signal it was sent and a waiting
parent sees the truth. `SIGKILL` and `SIGSTOP` cannot be caught, so `kill -9`
still leaves the terminal raw; nothing can change that. This is why the client
depends on `libc`: `sigaction` has no safe binding in this tree, and the crate
is `deny(unsafe_code)` with that single documented exception rather than
`forbid`.

**On an 8-colour terminal, only `Idx(8..16)` reaches the bright half.** An
application that said "bright red" gets the traditional bold-plus-base-colour
rendering. A cube or grey index (`>= 16`) carries no brightness signal — it says
no more than the RGB it is defined as — so it folds with the high bit masked
off, exactly as `Rgb` always did. Without that mask a dark teal landed on 8 and
the renderer promoted it to bold, putting text in a heavier font than the
application ever asked for.

---

## Milestone map

| Milestone | Plan file | Crates touched |
|---|---|---|
| **M1** loopback terminal | `2026-08-25-oxutrm-M1-terminal-core.md` | proto, sync, term, client |
| **M2** QUIC over punched socket | `2026-08-25-oxutrm-M2-transport.md` | net |
| **M3** SSH bootstrap and sessions | `2026-08-25-oxutrm-M3-sessions.md` | proto, host, main |
| **M4** joined up | `2026-08-25-oxutrm-M4-integration.md` | all |

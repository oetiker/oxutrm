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
| `zstd` | `0.13` | sync |
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
pub struct Frame {
    pub my_state: u64,
    pub from_state: u64,
    pub ack_state: u64,
    pub flags: u8,
    /// 0-based fragment index within this target state.
    pub frag_index: u16,
    /// Total fragments for this target state. 1 means unfragmented.
    pub frag_count: u16,
    pub payload: Vec<u8>,
}

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

#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Cell { pub text: String, pub fg: Color, pub bg: Color, pub attrs: Attrs }
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
}

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
///      table and consults `colors()` only as an override layer.
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
pub const STATE_RING: usize = 32;

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum ApplyError {
    #[error("diff base {base} does not match current state {current}")]
    BaseMismatch { base: u64, current: u64 },
    #[error("diff refers to row {row} outside {rows} rows")]
    OutOfBounds { row: u16, rows: u16 },
    #[error("decode: {0}")]
    Decode(String),
}

/// A replicated value. No I/O, no clocks, no allocation assumptions.
pub trait SyncState: Clone {
    type Diff: serde::Serialize + serde::de::DeserializeOwned;
    fn seq(&self) -> u64;
    fn set_seq(&mut self, seq: u64);
    /// Diff that turns `base` into `self`.
    fn diff_from(&self, base: &Self) -> Self::Diff;
    fn apply(&mut self, base: u64, target: u64, d: &Self::Diff) -> Result<(), ApplyError>;
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

pub struct Receiver<S: SyncState> { /* private */ }

impl<S: SyncState> Receiver<S> {
    pub fn new(initial: S) -> Receiver<S>;
    /// Returns true when the state advanced. Stale or duplicate frames
    /// return Ok(false) — they are never an error.
    pub fn on_frame(&mut self, f: &Frame) -> Result<bool, ApplyError>;
    pub fn state(&self) -> &S;
    /// The sequence number to put in our outgoing `ack_state`.
    pub fn ack(&self) -> u64;
    /// The peer's `ack_state` from the last frame we accepted.
    pub fn peer_ack(&self) -> u64;
}

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
/// the mapped ports they report.
/// THREE probes, not two: two servers at different IPs, plus a second port on
/// the FIRST server's IP. Two probes can only separate `EndpointIndependent`
/// from the rest; they cannot tell `AddressDependent` from `Symmetric`.
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

### `oxutrm-host`

```rust
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub pid: u32,
    pub created_unix: u64,
    pub shell: String,
    pub size: TermSize,
    /// False for a rung-4 (SSH-tunnelled) session: it cannot daemonize,
    /// so it dies with its SSH connection and cannot be reattached.
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
pub fn daemonize() -> anyhow::Result<()>;
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

/// Raw mode on entry, restored on Drop and on panic.
pub struct RawGuard { /* private */ }
impl RawGuard { pub fn enter() -> anyhow::Result<RawGuard>; }
impl Drop for RawGuard;

pub fn terminal_size() -> anyhow::Result<TermSize>;

/// One connect-time line, then silence.
pub fn status_line(path: &PathDescription) -> String;
```

---

## Milestone map

| Milestone | Plan file | Crates touched |
|---|---|---|
| **M1** loopback terminal | `2026-08-25-oxutrm-M1-terminal-core.md` | proto, sync, term, client |
| **M2** QUIC over punched socket | `2026-08-25-oxutrm-M2-transport.md` | net |
| **M3** SSH bootstrap and sessions | `2026-08-25-oxutrm-M3-sessions.md` | proto, host, main |
| **M4** joined up | `2026-08-25-oxutrm-M4-integration.md` | all |

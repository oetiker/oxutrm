# oxutrm M3 — SSH Bootstrap, Signalling and Sessions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make oxutrm sessions real — created over SSH, surviving the SSH channel's death, listed, detached from and reattached to — with the terminal and the QUIC transport deliberately stubbed.

**Architecture:** `oxutrm-proto` gains the signalling half of the wire: a `Signal`
enum carried as newline-delimited JSON over the SSH child's stdin/stdout, with a
hard protocol-version check and tolerance for SSH banners and motd noise before
the first message. `oxutrm-host` gains everything a session needs to outlive its
creator: `daemonize()` (double fork, `setsid`, `chdir("/")`, every inherited
descriptor closed), a `$XDG_RUNTIME_DIR/oxutrm/<id>/` registry with strict
permissions and pid-based pruning, a per-session Unix socket, and fresh key
material generated per attach and never written to disk. `src/main.rs` wires the
three roles together: `oxutrm <ssh-target>`, `oxutrm host --serve`,
`oxutrm host --list`, `oxutrm host --attach <id>`.

**Tech Stack:** Rust 2021, `tokio` (process, net, time, io-util), `serde_json`
for signalling, `libc` for `fork`/`setsid`/`dup2`/`kill`, `rand` 0.9 for key
material, `base64` 0.22, `thiserror` 2, `anyhow` 1, `tempfile` 3 (dev).

**Spec:** `docs/superpowers/specs/2026-08-25-oxutrm-design.md` — §4 (bootstrap and
signalling), §9 (host design), §11 (security model).

**Contract:** `docs/superpowers/plans/2026-08-25-oxutrm-contract.md` — normative
types and global constraints. Read it before starting any task.

---

## Global Constraints

Copied from the contract. Every task's requirements implicitly include these.

- **Binary and product name is `oxutrm`.** Not `oxuterm`. Crates are
  `oxutrm-proto`, `oxutrm-sync`, `oxutrm-term`, `oxutrm-net`, `oxutrm-host`,
  `oxutrm-client`. The checkout directory is `oxuterm` for historical reasons;
  nothing inside it uses that spelling.
- **Rust edition 2021**, workspace at the repo root, one binary `src/main.rs`.
- **Cap all parallelism at 4**: `cargo build --jobs 4`,
  `cargo test --jobs 4 -- --test-threads 4`. The build machine is shared.
- **Workspace root `Cargo.toml` must contain:**
  ```toml
  [profile.dev]
  debug = "line-tables-only"
  split-debuginfo = "unpacked"
  ```
- **`oxutrm-sync` performs no I/O.** Not touched by this milestone.
- **English** for all identifiers, comments, and documentation.
- **`anyhow::Result`** at binary and crate-boundary level; concrete error enums
  (via `thiserror`) inside `oxutrm-sync` and `oxutrm-proto` where callers must
  discriminate.
- **No key material is ever written to disk**, in any crate, at any time.
  Task 12 enforces this with a test.
- **Every task ends green**: `cargo clippy --all-targets -- -D warnings` and
  `cargo test --jobs 4 -- --test-threads 4` both pass before committing.
- **Commit messages** end with:
  `Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>`

## Prerequisites

M1 has created the workspace and `oxutrm-proto` with `PROTO_VERSION`,
`SessionId`, `TermSize`, `Candidate`, `CandidateKind`, `NatType`, `Rung`,
`TerminalCaps`, `PathDescription`, `Frame` and `ProtoError`. This plan **adds**
to `oxutrm-proto` and **creates** `oxutrm-host`; it never renames an existing
type.

## Deviations from the contract, deliberate and flagged

These are additions, not changes. Nothing in the contract is renamed or
re-signatured.

1. `ProtoError` gains a `Closed` variant. A relay loop must tell "the peer hung
   up cleanly" apart from "the peer sent rubbish"; folding both into
   `Malformed` would make a normal detach look like a protocol bug.
2. `oxutrm-proto` gains `parse_signal_line`, `looks_like_signal`,
   `check_version` and `read_signal_skip_preamble` alongside the contract's
   `read_signal`. The async side of the house (tokio pipes) cannot use a
   `std::io::BufRead`, so the line parser is factored out and shared.
3. `Registry`, `RegistryGuard` gain `*_in` / `*_at` variants taking an explicit
   root directory. `Registry::dir()` reads `$XDG_RUNTIME_DIR`, and mutating
   process environment from tests is racy under `--test-threads 4`. The
   contract's no-argument functions remain, implemented in terms of these.
4. `oxutrm-host` depends on `libc = "0.2"` (fork, setsid, dup2, kill, _exit),
   `base64 = "0.22"` (psk encoding) and dev-depends on `tempfile = "3"`. The
   contract lists `rustix` for host; `libc` is used instead because `fork` is
   not in rustix's stable surface, and mixing two process crates for one
   `daemonize()` is worse than picking one.

## Stubs, and what M4 replaces

M3 proves the session machinery, not the pixels. Three things are honest stubs
with a named replacement:

| Stub in M3 | Replaced in M4 by |
|---|---|
| `KeyMaterial::fresh()` invents the certificate fingerprint from the CSPRNG | the real SHA-256 of the SPKI from `oxutrm_net::generate_cert()` |
| `Session::host_hello` advertises no candidates, `NatType::Unknown`, `bound_port: 0` | candidates from `oxutrm_net::local_candidates` / `stun_discover` |
| `StubShell` reports a shell that never exits | `oxutrm_term::HostTerm`, whose `child_exited()` drives `ShellHandle::exit_status` |

The `ShellHandle` trait exists precisely so M4 swaps the implementation without
touching the lifecycle loop.

---

## File Structure

**`oxutrm-proto` (modified):**

- `crates/oxutrm-proto/src/signal.rs` — *created*. The `Signal` enum, NDJSON
  read/write, version check, preamble skipping. One responsibility: the SSH
  signalling channel's format.
- `crates/oxutrm-proto/src/lib.rs` — *modified*. `pub mod signal;`,
  `pub use signal::*;`, and the `ProtoError::Closed` variant.
- `crates/oxutrm-proto/Cargo.toml` — *modified*. `serde_json` dependency.

**`oxutrm-host` (created):**

- `crates/oxutrm-host/src/lib.rs` — module declarations and re-exports only.
- `crates/oxutrm-host/src/registry.rs` — `SessionMeta`, `Registry`,
  `RegistryGuard`, pid liveness. Owns everything that touches
  `$XDG_RUNTIME_DIR`.
- `crates/oxutrm-host/src/daemon.rs` — `daemonize()` and nothing else. The
  highest-risk file in the milestone; kept alone so it can be read whole.
- `crates/oxutrm-host/src/keys.rs` — `KeyMaterial`, `new_session_id()`. Owns
  every byte that must never reach a disk.
- `crates/oxutrm-host/src/ndjson.rs` — async `Signal` read/write over tokio
  streams. Shared by the SSH wrapper and the session socket.
- `crates/oxutrm-host/src/ssh.rs` — `SshCommand`, `SignalLink`, `SshError`,
  `bootstrap()`. The local wrapper's half. It lives in `oxutrm-host` because
  both ends of the bootstrap speak the same three messages in the same order,
  and splitting them across crates would duplicate the sequence.
- `crates/oxutrm-host/src/session.rs` — `SessionConfig`, `ShellHandle`,
  `StubShell`, `Session`, `SessionEnd`, `serve_attach`, `run_session`,
  `relay_attach`.
- `crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs` — test fixture: a process
  that daemonizes and reports its descriptors.
- `crates/oxutrm-host/src/bin/oxutrm-fake-ssh.rs` — test fixture: a local
  program that behaves like `ssh <target> oxutrm host --serve`, banner included.
- `crates/oxutrm-host/tests/registry.rs`, `tests/daemonize.rs`,
  `tests/ssh_bootstrap.rs`, `tests/attach.rs`, `tests/no_keys_on_disk.rs`.

Fixture binaries live under `src/bin/` and not `examples/` because Cargo sets
`CARGO_BIN_EXE_<name>` for integration tests only for `[[bin]]` targets. All
tests that launch a fixture must therefore live in `crates/oxutrm-host/tests/`.

**Root binary (modified):**

- `src/main.rs` — subcommand dispatch for the three roles.
- `Cargo.toml` — `oxutrm-host` added to `[workspace.members]` and to the binary's
  dependencies.

---

### Task 1: `Signal` and newline-delimited JSON

**Files:**
- Create: `crates/oxutrm-proto/src/signal.rs`
- Modify: `crates/oxutrm-proto/src/lib.rs`
- Modify: `crates/oxutrm-proto/Cargo.toml`
- Test: `crates/oxutrm-proto/src/signal.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `PROTO_VERSION: u32`, `Candidate`, `NatType`, `TerminalCaps`,
  `TermSize`, `PathDescription`, `Rung`, `CandidateKind`, `ProtoError` — all
  already in `oxutrm-proto` from M1.
- Produces:
  ```rust
  pub enum Signal {
      HostHello { proto: u32, session_id: String, cert_spki_sha256: String,
                  psk: String, candidates: Vec<Candidate>, nat_type: NatType,
                  bound_port: u16 },
      ClientHello { proto: u32, candidates: Vec<Candidate>, nat_type: NatType,
                    caps: TerminalCaps, size: TermSize },
      CandidateUpdate { candidates: Vec<Candidate> },
      Established { path: PathDescription },
      Failed { reason: String },
  }
  impl Signal { pub fn proto(&self) -> Option<u32>; }
  pub fn write_signal<W: std::io::Write>(w: &mut W, s: &Signal) -> Result<(), ProtoError>;
  pub fn read_signal<R: std::io::BufRead>(r: &mut R) -> Result<Signal, ProtoError>;
  pub fn parse_signal_line(line: &str) -> Result<Signal, ProtoError>;
  pub fn looks_like_signal(line: &str) -> bool;
  // ProtoError gains: Closed
  ```

- [ ] **Step 1: Add `serde_json` to `oxutrm-proto`**

In `crates/oxutrm-proto/Cargo.toml`, under `[dependencies]`:

```toml
serde_json = "1"
```

- [ ] **Step 2: Add the `Closed` variant to `ProtoError`**

In `crates/oxutrm-proto/src/lib.rs`, inside the existing `enum ProtoError`, add:

```rust
    #[error("signalling stream closed by peer")]
    Closed,
```

- [ ] **Step 3: Write the failing test**

Create `crates/oxutrm-proto/src/signal.rs` containing only this test module for
now (the code above it comes in step 5):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CandidateKind, PathDescription, Rung, TermSize, TerminalCaps};
    use std::io::BufReader;

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

    fn every_variant() -> Vec<Signal> {
        let cand = Candidate {
            addr: "192.0.2.7:443".parse().unwrap(),
            kind: CandidateKind::ServerReflexive,
            priority: 1_000,
        };
        vec![
            Signal::HostHello {
                proto: PROTO_VERSION,
                session_id: "00112233445566778899aabbccddeeff".to_string(),
                cert_spki_sha256: "YWJjZGVmZ2hpamtsbW5vcHFyc3R1dnd4eXoxMjM0NTY=".to_string(),
                psk: "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=".to_string(),
                candidates: vec![cand.clone()],
                nat_type: NatType::EndpointIndependent,
                bound_port: 443,
            },
            Signal::ClientHello {
                proto: PROTO_VERSION,
                candidates: vec![cand.clone()],
                nat_type: NatType::Symmetric,
                caps: caps(),
                size: TermSize { cols: 120, rows: 40 },
            },
            Signal::CandidateUpdate { candidates: vec![cand] },
            Signal::Established {
                path: PathDescription {
                    rung: Rung::Ipv6Direct,
                    local: "[2001:db8::1]:443".parse().unwrap(),
                    remote: "[2001:db8::2]:443".parse().unwrap(),
                    probes_sent: 3,
                    nat_type: NatType::None,
                    rtt_ms: 11,
                    mtu: 1452,
                },
            },
            Signal::Failed { reason: "no UDP path and no SSH tunnel".to_string() },
        ]
    }

    #[test]
    fn round_trips_every_variant_through_one_stream() {
        let mut buf: Vec<u8> = Vec::new();
        for s in every_variant() {
            write_signal(&mut buf, &s).expect("write");
        }
        let mut r = BufReader::new(buf.as_slice());
        let mut got = Vec::new();
        for _ in 0..5 {
            got.push(read_signal(&mut r).expect("read"));
        }
        let want = every_variant();
        assert_eq!(got.len(), want.len());
        for (a, b) in got.iter().zip(want.iter()) {
            assert_eq!(format!("{a:?}"), format!("{b:?}"));
        }
    }

    #[test]
    fn each_message_is_exactly_one_line() {
        let mut buf: Vec<u8> = Vec::new();
        for s in every_variant() {
            write_signal(&mut buf, &s).expect("write");
        }
        let text = String::from_utf8(buf).expect("utf8");
        assert_eq!(text.lines().count(), 5, "one line per Signal");
        assert!(text.ends_with('\n'), "every line is terminated");
    }

    #[test]
    fn variants_are_tagged_with_t() {
        let mut buf: Vec<u8> = Vec::new();
        write_signal(&mut buf, &Signal::Failed { reason: "x".into() }).expect("write");
        let text = String::from_utf8(buf).expect("utf8");
        assert!(text.contains(r#""t":"Failed""#), "tag missing in {text}");
    }

    #[test]
    fn a_reason_containing_a_newline_still_frames_as_one_line() {
        let mut buf: Vec<u8> = Vec::new();
        write_signal(&mut buf, &Signal::Failed { reason: "line one\nline two".into() })
            .expect("write");
        let text = String::from_utf8(buf.clone()).expect("utf8");
        assert_eq!(text.lines().count(), 1, "embedded newline must be escaped");
        let mut r = BufReader::new(buf.as_slice());
        match read_signal(&mut r).expect("read") {
            Signal::Failed { reason } => assert_eq!(reason, "line one\nline two"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn end_of_stream_is_closed_not_malformed() {
        let empty: &[u8] = b"";
        let mut r = BufReader::new(empty);
        match read_signal(&mut r) {
            Err(ProtoError::Closed) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn a_json_line_that_is_not_a_signal_is_malformed() {
        let bytes: &[u8] = b"{\"t\":\"Nonsense\"}\n";
        let mut r = BufReader::new(bytes);
        match read_signal(&mut r) {
            Err(ProtoError::Malformed(m)) => assert!(m.contains("signal json"), "{m}"),
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn looks_like_signal_ignores_leading_whitespace() {
        assert!(looks_like_signal("  {\"t\":\"Failed\"}"));
        assert!(!looks_like_signal("Welcome to Ubuntu 24.04.1 LTS"));
        assert!(!looks_like_signal(""));
    }
}
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: FAIL — `crates/oxutrm-proto/src/signal.rs` is not a module yet, or
`cannot find function write_signal`.

- [ ] **Step 5: Write the implementation**

Put this **above** the test module in `crates/oxutrm-proto/src/signal.rs`:

```rust
//! The SSH signalling channel: newline-delimited JSON on the SSH child's
//! stdin and stdout.
//!
//! JSON rather than a binary format because this channel is low-volume,
//! human-debuggable, and version skew here must fail loudly (spec §4.2).

use crate::{Candidate, NatType, PathDescription, ProtoError, TermSize, TerminalCaps, PROTO_VERSION};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Signal {
    /// host -> client, first line.
    HostHello {
        proto: u32,
        /// 32 lowercase hex characters.
        session_id: String,
        /// base64 of the SHA-256 of the host certificate's SPKI.
        cert_spki_sha256: String,
        /// base64 of 32 CSPRNG bytes. Never written to disk, on either side.
        psk: String,
        candidates: Vec<Candidate>,
        nat_type: NatType,
        bound_port: u16,
    },
    /// client -> host, first line.
    ClientHello {
        proto: u32,
        candidates: Vec<Candidate>,
        nat_type: NatType,
        caps: TerminalCaps,
        size: TermSize,
    },
    /// Either direction, repeatable until the link is up.
    CandidateUpdate { candidates: Vec<Candidate> },
    /// Either direction, terminates signalling.
    Established { path: PathDescription },
    Failed { reason: String },
}

impl Signal {
    /// The protocol version a message carries, where it carries one.
    pub fn proto(&self) -> Option<u32> {
        match self {
            Signal::HostHello { proto, .. } | Signal::ClientHello { proto, .. } => Some(*proto),
            _ => None,
        }
    }
}

/// True when a line could be a `Signal` rather than SSH noise.
pub fn looks_like_signal(line: &str) -> bool {
    line.trim_start().starts_with('{')
}

/// Serialise one `Signal` and terminate it with a newline.
///
/// `serde_json` escapes newlines inside strings, so a message can never
/// straddle two lines however hostile its contents.
pub fn write_signal<W: std::io::Write>(w: &mut W, s: &Signal) -> Result<(), ProtoError> {
    let line = serde_json::to_string(s)
        .map_err(|e| ProtoError::Malformed(format!("encoding signal: {e}")))?;
    w.write_all(line.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()?;
    Ok(())
}

/// Parse one already-read line. Shared by the blocking and async readers.
pub fn parse_signal_line(line: &str) -> Result<Signal, ProtoError> {
    let s: Signal = serde_json::from_str(line.trim())
        .map_err(|e| ProtoError::Malformed(format!("signal json: {e}")))?;
    Ok(s)
}

/// Read one `Signal`. `ProtoError::Closed` on a clean end of stream.
pub fn read_signal<R: std::io::BufRead>(r: &mut R) -> Result<Signal, ProtoError> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Err(ProtoError::Closed);
    }
    parse_signal_line(&line)
}
```

- [ ] **Step 6: Declare the module**

In `crates/oxutrm-proto/src/lib.rs`, next to the other module declarations:

```rust
pub mod signal;
pub use signal::{looks_like_signal, parse_signal_line, read_signal, write_signal, Signal};
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: PASS, 7 new tests.

- [ ] **Step 8: Lint**

Run: `cargo clippy --all-targets --jobs 4 -- -D warnings`
Expected: no warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/oxutrm-proto
git commit -m "feat(proto): Signal enum and newline-delimited JSON signalling

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Hard protocol-version check

**Files:**
- Modify: `crates/oxutrm-proto/src/signal.rs`
- Test: `crates/oxutrm-proto/src/signal.rs` (inline tests)

**Interfaces:**
- Consumes: `Signal`, `parse_signal_line`, `read_signal`, `PROTO_VERSION`,
  `ProtoError::VersionMismatch { peer: u32, ours: u32 }` from Task 1 and M1.
- Produces:
  ```rust
  pub fn check_version(s: &Signal) -> Result<(), ProtoError>;
  ```
  and the guarantee that `parse_signal_line` — and therefore every reader built
  on it — rejects a `HostHello` or `ClientHello` whose `proto` is not
  `PROTO_VERSION`. Messages without a `proto` field are never version-checked.

- [ ] **Step 1: Write the failing test**

Append these tests inside the existing `mod tests` in
`crates/oxutrm-proto/src/signal.rs`:

```rust
    /// A peer one version ahead of us. Forged as raw JSON, because
    /// `write_signal` deliberately does not validate what it is asked to send.
    fn skewed_host_hello_line() -> String {
        format!(
            concat!(
                r#"{{"t":"HostHello","proto":{},"session_id":"00112233445566778899aabbccddeeff","#,
                r#""cert_spki_sha256":"YWJj","psk":"ZGVm","candidates":[],"#,
                r#""nat_type":"Unknown","bound_port":443}}"#
            ),
            PROTO_VERSION + 1
        )
    }

    #[test]
    fn version_skew_fails_loudly_and_names_both_versions() {
        let line = skewed_host_hello_line();
        let err = parse_signal_line(&line).expect_err("skew must not parse");
        match err {
            ProtoError::VersionMismatch { peer, ours } => {
                assert_eq!(peer, PROTO_VERSION + 1);
                assert_eq!(ours, PROTO_VERSION);
                let shown = ProtoError::VersionMismatch { peer, ours }.to_string();
                assert!(shown.contains(&peer.to_string()), "message hides the peer version: {shown}");
                assert!(shown.contains(&ours.to_string()), "message hides our version: {shown}");
            }
            other => panic!("version skew must be VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn version_skew_is_not_silently_skipped_by_the_reader() {
        let line = skewed_host_hello_line() + "\n";
        let mut r = BufReader::new(line.as_bytes());
        assert!(
            matches!(read_signal(&mut r), Err(ProtoError::VersionMismatch { .. })),
            "read_signal must surface the skew, never swallow it"
        );
    }

    #[test]
    fn an_older_peer_also_fails() {
        let line = format!(
            concat!(
                r#"{{"t":"ClientHello","proto":0,"candidates":[],"nat_type":"Unknown","#,
                r#""caps":{{"truecolor":false,"colors":16,"bracketed_paste":false,"#,
                r#""mouse_sgr":false,"osc52":false,"term_name":"vt100"}},"#,
                r#""size":{{"cols":80,"rows":24}}}}"#
            )
        );
        match parse_signal_line(&line) {
            Err(ProtoError::VersionMismatch { peer, ours }) => {
                assert_eq!(peer, 0);
                assert_eq!(ours, PROTO_VERSION);
            }
            other => panic!("expected VersionMismatch, got {other:?}"),
        }
    }

    #[test]
    fn messages_without_a_version_are_not_version_checked() {
        let line = r#"{"t":"CandidateUpdate","candidates":[]}"#;
        assert!(parse_signal_line(line).is_ok());
        assert!(check_version(&Signal::Failed { reason: "x".into() }).is_ok());
    }

    #[test]
    fn a_matching_version_passes() {
        let s = Signal::CandidateUpdate { candidates: vec![] };
        assert!(check_version(&s).is_ok());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: FAIL — `cannot find function check_version`, and
`version_skew_fails_loudly_and_names_both_versions` panics with
"version skew must be VersionMismatch, got ..." because `parse_signal_line`
currently accepts anything that deserialises.

- [ ] **Step 3: Write the implementation**

In `crates/oxutrm-proto/src/signal.rs`, add `check_version` and call it from
`parse_signal_line`:

```rust
/// Hard version check (spec §4.2): a mismatch is a loud failure, never a
/// downgrade and never a warning. Messages that carry no version pass.
pub fn check_version(s: &Signal) -> Result<(), ProtoError> {
    match s.proto() {
        Some(peer) if peer != PROTO_VERSION => Err(ProtoError::VersionMismatch {
            peer,
            ours: PROTO_VERSION,
        }),
        _ => Ok(()),
    }
}
```

Replace the body of `parse_signal_line` with:

```rust
pub fn parse_signal_line(line: &str) -> Result<Signal, ProtoError> {
    let s: Signal = serde_json::from_str(line.trim())
        .map_err(|e| ProtoError::Malformed(format!("signal json: {e}")))?;
    check_version(&s)?;
    Ok(s)
}
```

- [ ] **Step 4: Export it**

In `crates/oxutrm-proto/src/lib.rs`, extend the re-export:

```rust
pub use signal::{
    check_version, looks_like_signal, parse_signal_line, read_signal, write_signal, Signal,
};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: PASS, 12 tests in `signal`.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-proto
git commit -m "feat(proto): hard protocol version check on signalling

Version skew fails loudly with both versions named, in the reader as
well as the parser.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Skipping the SSH preamble

**Files:**
- Modify: `crates/oxutrm-proto/src/signal.rs`
- Test: `crates/oxutrm-proto/src/signal.rs` (inline tests)

**Interfaces:**
- Consumes: `looks_like_signal`, `parse_signal_line`, `read_signal`,
  `ProtoError::Closed` from Tasks 1 and 2.
- Produces:
  ```rust
  pub fn read_signal_skip_preamble<R: std::io::BufRead>(r: &mut R) -> Result<Signal, ProtoError>;
  ```
  Skips lines that do not look like a `Signal` (SSH banners, motd, `stty`
  complaints), then parses the first line that does. A line that *does* look
  like a `Signal` but fails to parse — including one that fails the version
  check — is an error, never skipped.

**Why the asymmetry:** a version-skewed `HostHello` is valid JSON. If the
skipper swallowed unparseable JSON lines it would silently discard exactly the
message whose failure must be loudest. The cost is that a motd line beginning
with `{` breaks the bootstrap; that is the safe direction to fail in, and it is
documented on the function.

- [ ] **Step 1: Write the failing test**

Append inside `mod tests`:

```rust
    const REAL_WORLD_PREAMBLE: &str = concat!(
        "Welcome to Ubuntu 24.04.1 LTS (GNU/Linux 6.8.0-40-generic x86_64)\n",
        "\n",
        " * Documentation:  https://help.ubuntu.com\n",
        "Last login: Tue Aug 25 17:33:01 2026 from 192.0.2.1\n",
        "stty: 'standard input': Inappropriate ioctl for device\n",
    );

    #[test]
    fn skips_a_banner_and_motd_before_the_first_signal() {
        let mut input = String::from(REAL_WORLD_PREAMBLE);
        let mut line = Vec::new();
        write_signal(&mut line, &Signal::Failed { reason: "after the noise".into() }).unwrap();
        input.push_str(&String::from_utf8(line).unwrap());

        let mut r = BufReader::new(input.as_bytes());
        match read_signal_skip_preamble(&mut r).expect("must find the signal") {
            Signal::Failed { reason } => assert_eq!(reason, "after the noise"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn after_the_preamble_subsequent_reads_are_strict() {
        let mut input = String::from("Last login: whenever\n");
        for s in [
            Signal::Failed { reason: "first".into() },
            Signal::Failed { reason: "second".into() },
        ] {
            let mut line = Vec::new();
            write_signal(&mut line, &s).unwrap();
            input.push_str(&String::from_utf8(line).unwrap());
        }
        let mut r = BufReader::new(input.as_bytes());
        assert!(matches!(read_signal_skip_preamble(&mut r), Ok(Signal::Failed { .. })));
        // The plain reader picks up from the next line with no skipping.
        match read_signal(&mut r).expect("second") {
            Signal::Failed { reason } => assert_eq!(reason, "second"),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn only_noise_ends_as_closed() {
        let mut r = BufReader::new(REAL_WORLD_PREAMBLE.as_bytes());
        assert!(
            matches!(read_signal_skip_preamble(&mut r), Err(ProtoError::Closed)),
            "a stream of pure noise ends closed, not malformed"
        );
    }

    #[test]
    fn skipping_never_swallows_version_skew() {
        let input = format!("Welcome to Ubuntu\n{}\n", skewed_host_hello_line());
        let mut r = BufReader::new(input.as_bytes());
        assert!(
            matches!(read_signal_skip_preamble(&mut r), Err(ProtoError::VersionMismatch { .. })),
            "a skewed hello must stop the skipper, not be treated as noise"
        );
    }

    #[test]
    fn skipping_never_swallows_malformed_json() {
        let input = "motd line\n{\"t\":\"HostHello\"}\n";
        let mut r = BufReader::new(input.as_bytes());
        assert!(
            matches!(read_signal_skip_preamble(&mut r), Err(ProtoError::Malformed(_))),
            "a truncated signal must be reported, not skipped"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: FAIL — `cannot find function read_signal_skip_preamble in this scope`.

- [ ] **Step 3: Write the implementation**

In `crates/oxutrm-proto/src/signal.rs`:

```rust
/// Read the first `Signal`, discarding whatever the remote login printed
/// first: banners, motd, `stty` complaints on a non-tty.
///
/// A line that looks like a signal (it starts with `{`) is parsed strictly:
/// malformed JSON and version skew are reported, never skipped. The corollary
/// is that a motd line starting with `{` will break the bootstrap. That is the
/// safe direction: silently discarding a bad `HostHello` would hide the one
/// failure that must be loud.
pub fn read_signal_skip_preamble<R: std::io::BufRead>(r: &mut R) -> Result<Signal, ProtoError> {
    loop {
        let mut line = String::new();
        if r.read_line(&mut line)? == 0 {
            return Err(ProtoError::Closed);
        }
        if !looks_like_signal(&line) {
            continue;
        }
        return parse_signal_line(&line);
    }
}
```

Extend the re-export in `crates/oxutrm-proto/src/lib.rs`:

```rust
pub use signal::{
    check_version, looks_like_signal, parse_signal_line, read_signal, read_signal_skip_preamble,
    write_signal, Signal,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-proto -- --test-threads 4`
Expected: PASS, 17 tests in `signal`.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-proto
git commit -m "feat(proto): tolerate SSH banners before the first signal

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: The `oxutrm-host` crate and the registry directory

**Files:**
- Create: `crates/oxutrm-host/Cargo.toml`
- Create: `crates/oxutrm-host/src/lib.rs`
- Create: `crates/oxutrm-host/src/registry.rs`
- Modify: `Cargo.toml` (workspace members)
- Test: `crates/oxutrm-host/tests/registry.rs`

**Interfaces:**
- Consumes: `oxutrm_proto::TermSize`.
- Produces:
  ```rust
  pub struct SessionMeta {
      pub session_id: String,
      pub pid: u32,
      pub created_unix: u64,
      pub shell: String,
      pub size: TermSize,
  }                                     // Clone, Debug, Serialize, Deserialize

  pub const REGISTRY_SUBDIR: &str = "oxutrm";

  pub struct Registry;
  impl Registry {
      pub fn dir() -> anyhow::Result<std::path::PathBuf>;
      pub fn dir_at(base: &std::path::Path) -> std::path::PathBuf;
      pub fn socket_path(id: &str) -> anyhow::Result<std::path::PathBuf>;
      pub fn socket_path_in(dir: &std::path::Path, id: &str) -> std::path::PathBuf;
  }
  pub fn pid_alive(pid: u32) -> bool;
  pub fn now_unix() -> u64;
  ```

- [ ] **Step 1: Create the crate manifest**

`crates/oxutrm-host/Cargo.toml`:

```toml
[package]
name = "oxutrm-host"
version = "0.1.0"
edition = "2021"

[dependencies]
oxutrm-proto = { path = "../oxutrm-proto" }
anyhow = "1"
thiserror = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
rand = "0.9"
base64 = "0.22"
libc = "0.2"
tokio = { version = "1", features = [
    "rt-multi-thread", "net", "time", "macros", "io-util", "sync", "process",
] }

[dev-dependencies]
tempfile = "3"

[[bin]]
name = "oxutrm-daemon-probe"
path = "src/bin/oxutrm-daemon-probe.rs"

[[bin]]
name = "oxutrm-fake-ssh"
path = "src/bin/oxutrm-fake-ssh.rs"
```

The two `[[bin]]` targets are test fixtures, created in Tasks 7 and 8. Add them
to the manifest now but create the files in those tasks; until then, comment out
both `[[bin]]` blocks so the crate builds. Uncomment each as its task creates it.

Add `"crates/oxutrm-host"` to `[workspace.members]` in the root `Cargo.toml`.

- [ ] **Step 2: Write the failing test**

`crates/oxutrm-host/tests/registry.rs`:

```rust
use oxutrm_host::{now_unix, pid_alive, Registry, SessionMeta, REGISTRY_SUBDIR};
use oxutrm_proto::TermSize;

fn meta(id: &str, pid: u32) -> SessionMeta {
    SessionMeta {
        session_id: id.to_string(),
        pid,
        created_unix: now_unix(),
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
    }
}

#[test]
fn dir_at_appends_the_oxutrm_subdirectory() {
    let base = std::path::Path::new("/run/user/1000");
    assert_eq!(
        Registry::dir_at(base),
        std::path::Path::new("/run/user/1000").join(REGISTRY_SUBDIR)
    );
}

#[test]
fn socket_path_in_is_dir_id_sock() {
    let dir = std::path::Path::new("/run/user/1000/oxutrm");
    assert_eq!(
        Registry::socket_path_in(dir, "deadbeef"),
        std::path::Path::new("/run/user/1000/oxutrm/deadbeef/sock")
    );
}

#[test]
fn our_own_pid_is_alive_and_pid_zero_is_not() {
    assert!(pid_alive(std::process::id()));
    assert!(!pid_alive(0));
}

#[test]
fn a_reaped_child_is_not_alive() {
    let mut child = std::process::Command::new("/bin/true")
        .spawn()
        .expect("spawn /bin/true");
    let pid = child.id();
    child.wait().expect("wait");
    assert!(!pid_alive(pid), "pid {pid} was reaped and must read as dead");
}

#[test]
fn session_meta_round_trips_through_json() {
    let m = meta("00112233445566778899aabbccddeeff", 4242);
    let text = serde_json::to_string(&m).expect("encode");
    let back: SessionMeta = serde_json::from_str(&text).expect("decode");
    assert_eq!(back.session_id, m.session_id);
    assert_eq!(back.pid, m.pid);
    assert_eq!(back.shell, m.shell);
    assert_eq!(back.size, m.size);
}
```

Add `serde_json = "1"` is already a dependency; tests use it through the crate's
own dependency graph by declaring it in `[dev-dependencies]` as well:

```toml
[dev-dependencies]
tempfile = "3"
serde_json = "1"
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: FAIL — `unresolved import oxutrm_host::Registry`.

- [ ] **Step 4: Write the implementation**

`crates/oxutrm-host/src/registry.rs`:

```rust
//! The session registry: `$XDG_RUNTIME_DIR/oxutrm/<session-id>/` holding a
//! `sock` and a `meta.json` (spec §9.2). It never holds key material.

use anyhow::{anyhow, Context};
use oxutrm_proto::TermSize;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const REGISTRY_SUBDIR: &str = "oxutrm";
pub const META_FILE: &str = "meta.json";
pub const SOCK_FILE: &str = "sock";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub pid: u32,
    pub created_unix: u64,
    pub shell: String,
    pub size: TermSize,
}

/// Seconds since the Unix epoch. Saturates rather than panicking on a clock
/// set before 1970.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when a process with this pid exists and we could signal it.
///
/// `kill(pid, 0)` performs the permission and existence check without
/// delivering anything. `EPERM` means the process exists but belongs to
/// somebody else, which still counts as alive.
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

pub struct Registry;

impl Registry {
    /// `$XDG_RUNTIME_DIR/oxutrm`.
    pub fn dir() -> anyhow::Result<PathBuf> {
        let base = match std::env::var_os("XDG_RUNTIME_DIR") {
            Some(v) if !v.is_empty() => PathBuf::from(v),
            _ => {
                return Err(anyhow!(
                    "XDG_RUNTIME_DIR is not set, so oxutrm has no per-user runtime \
                     directory to keep sessions in. On systemd hosts this usually \
                     means the login session is not a real one: try \
                     `loginctl enable-linger $USER`, or set XDG_RUNTIME_DIR yourself."
                ))
            }
        };
        Ok(Self::dir_at(&base))
    }

    pub fn dir_at(base: &Path) -> PathBuf {
        base.join(REGISTRY_SUBDIR)
    }

    pub fn socket_path(id: &str) -> anyhow::Result<PathBuf> {
        Ok(Self::socket_path_in(&Self::dir()?, id))
    }

    pub fn socket_path_in(dir: &Path, id: &str) -> PathBuf {
        dir.join(id).join(SOCK_FILE)
    }
}

/// Create a directory owned by this user alone, whatever the umask says.
pub(crate) fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("creating {}", path.display()))?;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("chmod 0700 {}", path.display()))?;
    Ok(())
}

/// Write a file readable by this user alone, whatever the umask says.
pub(crate) fn write_private_file(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    f.flush()?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("chmod 0600 {}", path.display()))?;
    Ok(())
}
```

`crates/oxutrm-host/src/lib.rs`:

```rust
//! Session hosting: the registry, daemonizing, key material, and the SSH
//! bootstrap both ends of a session speak.

pub mod registry;

pub use registry::{
    now_unix, pid_alive, Registry, SessionMeta, META_FILE, REGISTRY_SUBDIR, SOCK_FILE,
};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: PASS, 5 tests.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add Cargo.toml crates/oxutrm-host
git commit -m "feat(host): oxutrm-host crate with the registry directory layout

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: `RegistryGuard` — registration, permissions, removal on drop

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Test: `crates/oxutrm-host/tests/registry.rs`

**Interfaces:**
- Consumes: `SessionMeta`, `Registry`, `create_private_dir`,
  `write_private_file`, `META_FILE`, `SOCK_FILE` from Task 4.
- Produces:
  ```rust
  pub struct RegistryGuard { /* private */ }
  impl RegistryGuard {
      pub fn register(meta: &SessionMeta) -> anyhow::Result<RegistryGuard>;
      pub fn register_in(root: &std::path::Path, meta: &SessionMeta) -> anyhow::Result<RegistryGuard>;
      pub fn dir(&self) -> &std::path::Path;
      pub fn socket_path(&self) -> std::path::PathBuf;
      pub fn meta_path(&self) -> std::path::PathBuf;
      /// Rewrite `meta.json` after the pid changes (i.e. after `daemonize`).
      pub fn update(&self, meta: &SessionMeta) -> anyhow::Result<()>;
  }
  impl Drop for RegistryGuard;   // removes the session directory
  ```

- [ ] **Step 1: Write the failing test**

Append to `crates/oxutrm-host/tests/registry.rs`:

```rust
use oxutrm_host::RegistryGuard;
use std::os::unix::fs::PermissionsExt;

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path)
        .unwrap_or_else(|e| panic!("stat {}: {e}", path.display()))
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn registering_creates_a_private_directory_and_a_private_meta_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("aaaabbbbccccddddeeeeffff00001111", std::process::id());

    let guard = RegistryGuard::register_in(&root, &m).expect("register");

    assert_eq!(mode_of(&root), 0o700, "registry root must be 0700");
    assert_eq!(mode_of(guard.dir()), 0o700, "session directory must be 0700");
    assert_eq!(mode_of(&guard.meta_path()), 0o600, "meta.json must be 0600");
    assert_eq!(guard.dir(), root.join(&m.session_id));
    assert_eq!(guard.socket_path(), root.join(&m.session_id).join("sock"));

    let text = std::fs::read_to_string(guard.meta_path()).expect("read meta");
    let back: SessionMeta = serde_json::from_str(&text).expect("decode meta");
    assert_eq!(back.session_id, m.session_id);
    assert_eq!(back.pid, m.pid);
}

#[test]
fn a_strict_umask_does_not_loosen_the_bits() {
    // The permissions are set explicitly after creation, so they hold whatever
    // the process umask happens to be.
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("11112222333344445555666677778888", std::process::id());
    let guard = RegistryGuard::register_in(&root, &m).expect("register");
    assert_eq!(mode_of(guard.dir()) & 0o077, 0, "no group or other bits");
    assert_eq!(mode_of(&guard.meta_path()) & 0o077, 0, "no group or other bits");
}

#[test]
fn dropping_the_guard_removes_the_session_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("99998888777766665555444433332222", std::process::id());

    let dir = {
        let guard = RegistryGuard::register_in(&root, &m).expect("register");
        let dir = guard.dir().to_path_buf();
        // A socket file left behind by a live session must not stop removal.
        std::fs::write(guard.socket_path(), b"").expect("touch sock");
        assert!(dir.exists());
        dir
    };
    assert!(!dir.exists(), "Drop must remove {}", dir.display());
    assert!(root.exists(), "the registry root itself stays");
}

#[test]
fn registering_the_same_id_twice_fails_rather_than_stealing_it() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let m = meta("0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f0f", std::process::id());
    let _first = RegistryGuard::register_in(&root, &m).expect("first register");
    assert!(
        RegistryGuard::register_in(&root, &m).is_err(),
        "a second live registration of one id must fail"
    );
}

#[test]
fn update_rewrites_the_pid() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let mut m = meta("abcdabcdabcdabcdabcdabcdabcdabcd", 1);
    let guard = RegistryGuard::register_in(&root, &m).expect("register");
    m.pid = std::process::id();
    guard.update(&m).expect("update");
    let text = std::fs::read_to_string(guard.meta_path()).expect("read");
    let back: SessionMeta = serde_json::from_str(&text).expect("decode");
    assert_eq!(back.pid, std::process::id());
    assert_eq!(mode_of(&guard.meta_path()), 0o600, "still 0600 after rewrite");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: FAIL — `unresolved import oxutrm_host::RegistryGuard`.

- [ ] **Step 3: Write the implementation**

Append to `crates/oxutrm-host/src/registry.rs`:

```rust
/// Owns one `$XDG_RUNTIME_DIR/oxutrm/<id>/` directory for as long as the
/// session lives. Dropping it removes the directory, so a session that exits
/// cleanly leaves nothing behind for `--list` to prune.
pub struct RegistryGuard {
    dir: PathBuf,
}

impl RegistryGuard {
    pub fn register(meta: &SessionMeta) -> anyhow::Result<RegistryGuard> {
        Self::register_in(&Registry::dir()?, meta)
    }

    pub fn register_in(root: &Path, meta: &SessionMeta) -> anyhow::Result<RegistryGuard> {
        create_private_dir(root)?;
        let dir = root.join(&meta.session_id);
        // `create_dir` and not `create_dir_all`: an existing directory means
        // another live session already owns this id, and taking it over would
        // delete that session's socket on drop.
        std::fs::create_dir(&dir)
            .with_context(|| format!("creating session directory {}", dir.display()))?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0700 {}", dir.display()))?;
        }
        let guard = RegistryGuard { dir };
        guard.update(meta)?;
        Ok(guard)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn socket_path(&self) -> PathBuf {
        self.dir.join(SOCK_FILE)
    }

    pub fn meta_path(&self) -> PathBuf {
        self.dir.join(META_FILE)
    }

    /// Rewrite `meta.json`. Called after `daemonize()`, because forking twice
    /// changes the pid that `--list` prunes on.
    pub fn update(&self, meta: &SessionMeta) -> anyhow::Result<()> {
        let text = serde_json::to_vec_pretty(meta).context("encoding meta.json")?;
        write_private_file(&self.meta_path(), &text)
    }
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        // Best effort: there is nothing sensible to do on failure at drop time,
        // and `--list` prunes whatever a crash leaves behind.
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
```

Extend `crates/oxutrm-host/src/lib.rs`:

```rust
pub use registry::{
    now_unix, pid_alive, Registry, RegistryGuard, SessionMeta, META_FILE, REGISTRY_SUBDIR,
    SOCK_FILE,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: PASS, 10 tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): RegistryGuard with 0700/0600 bits and removal on drop

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: `Registry::list` with pid-based pruning

**Files:**
- Modify: `crates/oxutrm-host/src/registry.rs`
- Test: `crates/oxutrm-host/tests/registry.rs`

**Interfaces:**
- Consumes: `Registry`, `SessionMeta`, `pid_alive`, `META_FILE` from Tasks 4-5.
- Produces:
  ```rust
  impl Registry {
      pub fn list() -> anyhow::Result<Vec<SessionMeta>>;
      pub fn list_in(dir: &std::path::Path) -> anyhow::Result<Vec<SessionMeta>>;
  }
  ```
  Entries are returned oldest first by `created_unix`. An entry whose pid is
  gone is **removed from disk** and omitted. A directory with no readable
  `meta.json` is left alone and omitted, because it may belong to a session
  that is mid-registration.

- [ ] **Step 1: Write the failing test**

Append to `crates/oxutrm-host/tests/registry.rs`:

```rust
/// Plant a registry entry without going through RegistryGuard, so the test can
/// choose the pid and skip Drop.
fn plant(root: &std::path::Path, id: &str, pid: u32, created: u64) -> std::path::PathBuf {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).expect("create entry dir");
    let m = SessionMeta {
        session_id: id.to_string(),
        pid,
        created_unix: created,
        shell: "/bin/bash".to_string(),
        size: TermSize { cols: 80, rows: 24 },
    };
    std::fs::write(dir.join("meta.json"), serde_json::to_vec(&m).unwrap()).expect("write meta");
    std::fs::write(dir.join("sock"), b"").expect("write sock");
    dir
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new("/bin/true").spawn().expect("spawn");
    let pid = child.id();
    child.wait().expect("wait");
    pid
}

#[test]
fn list_returns_live_sessions_oldest_first() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    std::fs::create_dir_all(&root).expect("root");
    plant(&root, "22222222222222222222222222222222", std::process::id(), 2_000);
    plant(&root, "11111111111111111111111111111111", std::process::id(), 1_000);

    let listed = Registry::list_in(&root).expect("list");
    let ids: Vec<&str> = listed.iter().map(|m| m.session_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "11111111111111111111111111111111",
            "22222222222222222222222222222222"
        ],
        "oldest first"
    );
}

#[test]
fn list_prunes_entries_whose_process_is_gone() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    std::fs::create_dir_all(&root).expect("root");
    let live = plant(&root, "aaaa1111aaaa1111aaaa1111aaaa1111", std::process::id(), 10);
    let dead = plant(&root, "bbbb2222bbbb2222bbbb2222bbbb2222", dead_pid(), 20);

    let listed = Registry::list_in(&root).expect("list");

    assert_eq!(listed.len(), 1, "only the live session survives: {listed:?}");
    assert_eq!(listed[0].session_id, "aaaa1111aaaa1111aaaa1111aaaa1111");
    assert!(live.exists(), "the live entry stays on disk");
    assert!(!dead.exists(), "the dead entry is removed from disk");
}

#[test]
fn list_of_a_missing_registry_is_empty_not_an_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path()).join("never-created");
    assert!(Registry::list_in(&root).expect("list").is_empty());
}

#[test]
fn a_directory_without_meta_is_ignored_and_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let half = root.join("cccc3333cccc3333cccc3333cccc3333");
    std::fs::create_dir_all(&half).expect("create");
    assert!(Registry::list_in(&root).expect("list").is_empty());
    assert!(half.exists(), "a half-built entry is somebody else's business");
}

#[test]
fn a_corrupt_meta_is_ignored_and_kept() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let bad = root.join("dddd4444dddd4444dddd4444dddd4444");
    std::fs::create_dir_all(&bad).expect("create");
    std::fs::write(bad.join("meta.json"), b"not json at all").expect("write");
    assert!(Registry::list_in(&root).expect("list").is_empty());
    assert!(bad.exists());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: FAIL — `no function or associated item named list_in found for struct Registry`.

- [ ] **Step 3: Write the implementation**

Inside `impl Registry` in `crates/oxutrm-host/src/registry.rs`:

```rust
    /// Every live session, oldest first. Entries whose process is gone are
    /// removed from disk as a side effect (spec §9.2).
    pub fn list() -> anyhow::Result<Vec<SessionMeta>> {
        Self::list_in(&Self::dir()?)
    }

    pub fn list_in(dir: &Path) -> anyhow::Result<Vec<SessionMeta>> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => {
                return Err(e).with_context(|| format!("reading registry {}", dir.display()))
            }
        };

        let mut live = Vec::new();
        for entry in entries {
            let entry = entry.with_context(|| format!("reading registry {}", dir.display()))?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let text = match std::fs::read_to_string(path.join(META_FILE)) {
                Ok(t) => t,
                // No meta yet, or unreadable: leave it alone. A session that is
                // still registering owns this directory, not us.
                Err(_) => continue,
            };
            let meta: SessionMeta = match serde_json::from_str(&text) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if pid_alive(meta.pid) {
                live.push(meta);
            } else {
                let _ = std::fs::remove_dir_all(&path);
            }
        }
        live.sort_by_key(|m| (m.created_unix, m.session_id.clone()));
        Ok(live)
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: PASS, 15 tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): Registry::list with pid-based pruning of dead entries

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: `daemonize()` — the descriptor that must not survive

**Files:**
- Create: `crates/oxutrm-host/src/daemon.rs`
- Create: `crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Modify: `crates/oxutrm-host/Cargo.toml` (uncomment the probe `[[bin]]`)
- Test: `crates/oxutrm-host/tests/daemonize.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  ```rust
  /// Double fork, setsid, chdir /, close every inherited descriptor,
  /// reopen 0/1/2 on /dev/null. Returns `Ok(())` only in the final grandchild;
  /// the two intermediate processes never return from this function.
  pub fn daemonize() -> anyhow::Result<()>;
  ```

**This is the highest-risk item in the milestone.** If one descriptor inherited
from SSH survives, closing the laptop lid kills the session — the exact failure
the project exists to prevent (spec §4.3). Four rules the implementation must
obey, each of which is a real bug if broken:

1. The intermediate processes exit with `libc::_exit(0)`, never `std::process::exit`
   and never by returning. `_exit` runs no destructor and no `atexit` handler,
   so the `RegistryGuard` a caller may already hold does not delete the session
   directory when the fork parent goes away.
2. `daemonize()` must be called **before** any thread is created — a tokio
   runtime included. `fork` copies only the calling thread; a runtime built
   beforehand wakes up in the child with its worker threads gone and deadlocks.
3. It must be called **after** `HostHello` is flushed, because it closes the
   pipes that message travels on.
4. The per-session Unix socket must be bound **after** it, because binding
   before means closing the listener a moment later.

- [ ] **Step 1: Write the failing test**

`crates/oxutrm-host/tests/daemonize.rs`:

```rust
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Wait for the probe to write its report, up to `limit`.
fn wait_for(path: &std::path::Path, limit: Duration) -> String {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            // The probe writes the whole report in one go and ends with a newline.
            if text.ends_with('\n') {
                return text;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the daemonized process never wrote {}", path.display());
}

fn fd_targets(report: &str) -> Vec<(i32, String)> {
    report
        .lines()
        .filter_map(|l| l.strip_prefix("fd="))
        .filter_map(|l| l.split_once(" -> "))
        .map(|(n, t)| (n.parse::<i32>().expect("fd number"), t.to_string()))
        .collect()
}

fn field<'a>(report: &'a str, key: &str) -> &'a str {
    report
        .lines()
        .find_map(|l| l.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in report:\n{report}"))
}

#[test]
fn the_daemon_outlives_its_parent_and_keeps_no_inherited_descriptor() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("report.txt");

    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm-daemon-probe"))
        .arg(&report_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the probe");

    let child_pid = child.id();
    let status = child.wait().expect("wait for the probe's first process");
    assert!(status.success(), "the fork parent must exit 0, got {status:?}");

    // The probe sleeps before reporting, so the report cannot exist yet. This
    // is what makes the next assertion mean "outlived its parent".
    assert!(
        !report_path.exists(),
        "the probe reported before its parent died; the test proves nothing"
    );

    // The pipes we handed the probe must have been closed by daemonize, so
    // reading them returns EOF rather than blocking forever.
    let mut out = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut out)
        .expect("read stdout");
    assert!(out.is_empty(), "the daemon wrote to the inherited stdout: {out:?}");

    let report = wait_for(&report_path, Duration::from_secs(10));

    let ppid: u32 = field(&report, "ppid").parse().expect("ppid");
    assert_ne!(ppid, child_pid, "still parented to the process ssh waited on");
    assert_ne!(ppid, std::process::id(), "still parented to the test harness");

    let targets = fd_targets(&report);
    assert!(!targets.is_empty(), "the probe reported no descriptors at all");

    for (fd, target) in &targets {
        assert!(
            !target.starts_with("pipe:"),
            "fd {fd} still points at an inherited pipe ({target}); \
             closing the laptop lid would kill this session"
        );
        assert!(
            !target.contains(".marker"),
            "fd {fd} still points at the file held before daemonizing ({target})"
        );
    }

    for std_fd in [0, 1, 2] {
        let (_, target) = targets
            .iter()
            .find(|(n, _)| *n == std_fd)
            .unwrap_or_else(|| panic!("fd {std_fd} is missing entirely:\n{report}"));
        assert_eq!(target, "/dev/null", "fd {std_fd} must be reopened on /dev/null");
    }
}

#[test]
fn the_daemon_starts_a_new_session_and_sits_in_the_root_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let report_path = tmp.path().join("report2.txt");

    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm-daemon-probe"))
        .arg(&report_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn the probe");
    child.wait().expect("wait");

    let report = wait_for(&report_path, Duration::from_secs(10));
    let pid = field(&report, "pid");
    let sid = field(&report, "sid");
    assert_eq!(sid, pid, "the daemon must lead its own session (setsid)");
    assert_eq!(field(&report, "cwd"), "/", "the daemon must chdir to /");
}
```

- [ ] **Step 2: Write the probe fixture**

`crates/oxutrm-host/src/bin/oxutrm-daemon-probe.rs`:

```rust
//! Test fixture. Holds an open file, daemonizes, then reports what survived.
//!
//! Not part of the product: it exists so `tests/daemonize.rs` can assert on a
//! real daemonized process rather than on a mock.

use std::io::Write;
use std::os::fd::AsRawFd;

fn main() {
    let report = std::env::args()
        .nth(1)
        .expect("usage: oxutrm-daemon-probe <report-path>");
    let marker = format!("{report}.marker");

    // Stand in for the descriptors ssh leaves behind: opened before
    // daemonizing, deliberately leaked so nothing but daemonize can close it.
    let held = std::fs::File::create(&marker).expect("create the marker file");
    let held_fd = held.as_raw_fd();
    std::mem::forget(held);

    oxutrm_host::daemonize().expect("daemonize");

    // Outlive the parent, so writing the report proves independence.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let mut lines = Vec::new();
    let pid = std::process::id();
    lines.push(format!("pid={pid}"));
    lines.push(format!("ppid={}", unsafe { libc::getppid() }));
    lines.push(format!("sid={}", unsafe { libc::getsid(0) }));
    lines.push(format!("held_fd={held_fd}"));
    lines.push(format!(
        "cwd={}",
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string())
    ));
    for entry in std::fs::read_dir("/proc/self/fd").expect("read /proc/self/fd") {
        let entry = entry.expect("fd entry");
        let n = entry.file_name().to_string_lossy().to_string();
        let target = std::fs::read_link(entry.path())
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "?".to_string());
        lines.push(format!("fd={n} -> {target}"));
    }

    // Written last and in one call, so the test never reads a half report.
    // The descriptor for this file is opened after the enumeration above, so
    // it does not appear in the listing.
    let mut f = std::fs::File::create(&report).expect("create the report");
    write!(f, "{}\n", lines.join("\n")).expect("write the report");
}
```

Uncomment the `[[bin]] name = "oxutrm-daemon-probe"` block in
`crates/oxutrm-host/Cargo.toml`.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host --test daemonize -- --test-threads 4`
Expected: FAIL to compile — `cannot find function daemonize in crate oxutrm_host`.

- [ ] **Step 4: Write the implementation**

`crates/oxutrm-host/src/daemon.rs`:

```rust
//! Detaching a session from the SSH connection that created it (spec §4.3).

use anyhow::{anyhow, Context};

/// Double fork, `setsid`, `chdir("/")`, close every inherited descriptor and
/// reopen 0/1/2 on `/dev/null`.
///
/// Returns `Ok(())` only in the final grandchild. The two intermediate
/// processes call `_exit(0)` and never return, so the caller's stack, its
/// destructors and its `atexit` handlers do not run in them — in particular a
/// `RegistryGuard` held across this call does not delete the session directory
/// when the fork parent goes away.
///
/// Call it:
/// - **after** `HostHello` has been written and flushed, because it closes the
///   pipes that message travels on;
/// - **before** any thread exists, tokio runtimes included, because `fork`
///   copies only the calling thread;
/// - **before** binding the session's Unix socket, because it closes every
///   descriptor above 2.
pub fn daemonize() -> anyhow::Result<()> {
    // First fork: the process ssh is waiting on returns immediately, so ssh
    // sees a clean exit and the shell prompt comes back.
    match unsafe { libc::fork() } {
        -1 => {
            return Err(std::io::Error::last_os_error()).context("first fork");
        }
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }

    // New session, no controlling terminal. A hangup on the old terminal can
    // no longer reach us.
    if unsafe { libc::setsid() } == -1 {
        return Err(std::io::Error::last_os_error()).context("setsid");
    }

    // Second fork: the resulting process is not a session leader, so it can
    // never acquire a controlling terminal by opening one.
    match unsafe { libc::fork() } {
        -1 => {
            return Err(std::io::Error::last_os_error()).context("second fork");
        }
        0 => {}
        _ => unsafe { libc::_exit(0) },
    }

    std::env::set_current_dir("/").context("chdir to /")?;
    // Anything this process creates from here on is its own business.
    unsafe { libc::umask(0o077) };

    close_inherited_descriptors()?;
    reopen_standard_descriptors()?;
    Ok(())
}

/// Close every descriptor above 2. This is the step that decides whether
/// closing the laptop lid kills the session.
fn close_inherited_descriptors() -> anyhow::Result<()> {
    // Collect first and close afterwards: closing while the directory handle
    // is open would invalidate the iterator. The handle's own descriptor is in
    // the collected list and is already closed by the time the loop reaches
    // it, which is harmless because nothing opens a new descriptor in between,
    // so the number cannot have been reused.
    let fds: Vec<i32> = {
        let dir = std::fs::read_dir("/proc/self/fd").context(
            "reading /proc/self/fd; oxutrm needs /proc mounted to detach safely",
        )?;
        dir.filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse::<i32>().ok()))
            .collect()
    };
    for fd in fds {
        if fd > 2 {
            unsafe { libc::close(fd) };
        }
    }
    Ok(())
}

/// Point 0, 1 and 2 at `/dev/null`, so a stray `println!` cannot write into
/// whatever descriptor number is reused next.
fn reopen_standard_descriptors() -> anyhow::Result<()> {
    let null = unsafe { libc::open(c"/dev/null".as_ptr(), libc::O_RDWR) };
    if null < 0 {
        return Err(std::io::Error::last_os_error()).context("opening /dev/null");
    }
    for target in 0..=2 {
        if unsafe { libc::dup2(null, target) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(null) };
            return Err(anyhow!("pointing fd {target} at /dev/null: {e}"));
        }
    }
    if null > 2 {
        unsafe { libc::close(null) };
    }
    Ok(())
}
```

`c"/dev/null"` is a C string literal, stable since Rust 1.77. If the toolchain
in use is older, replace that line with:

```rust
    let path = std::ffi::CString::new("/dev/null").expect("no interior nul");
    let null = unsafe { libc::open(path.as_ptr(), libc::O_RDWR) };
```

Add to `crates/oxutrm-host/src/lib.rs`:

```rust
pub mod daemon;
pub use daemon::daemonize;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host --test daemonize -- --test-threads 4`
Expected: PASS, 2 tests.

If `the_daemon_outlives_its_parent_and_keeps_no_inherited_descriptor` reports an
fd pointing at `pipe:`, the descriptor loop is wrong — do not relax the
assertion, fix the loop.

- [ ] **Step 6: Run the whole suite and lint**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Run: `cargo clippy --all-targets --jobs 4 -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/oxutrm-host
git commit -m "feat(host): daemonize with double fork and full descriptor closure

Proved by a probe binary that enumerates /proc/self/fd after detaching:
no inherited pipe survives, 0/1/2 are /dev/null, and the process outlives
the one ssh waited on.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: Async signalling over tokio streams, and the fake `ssh`

**Files:**
- Create: `crates/oxutrm-host/src/ndjson.rs`
- Create: `crates/oxutrm-host/src/bin/oxutrm-fake-ssh.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Modify: `crates/oxutrm-host/Cargo.toml` (uncomment the fake-ssh `[[bin]]`)
- Test: `crates/oxutrm-host/src/ndjson.rs` (inline tests)

**Interfaces:**
- Consumes: `oxutrm_proto::{parse_signal_line, looks_like_signal, write_signal,
  Signal, ProtoError, PROTO_VERSION, NatType, PathDescription, Rung, TermSize,
  TerminalCaps}`.
- Produces:
  ```rust
  pub async fn write_signal_async<W: tokio::io::AsyncWrite + Unpin>(
      w: &mut W, s: &Signal,
  ) -> Result<(), ProtoError>;

  /// `skip_preamble` discards lines that do not look like a Signal before the
  /// first message; it never discards a line that does.
  pub async fn read_signal_async<R: tokio::io::AsyncBufRead + Unpin>(
      r: &mut R, skip_preamble: bool,
  ) -> Result<Signal, ProtoError>;
  ```
  plus the `oxutrm-fake-ssh` fixture binary, whose behaviour is selected by
  `$OXUTRM_FAKE_SSH_MODE`: `ok` (default), `skew`, `missing_binary`,
  `ssh_failed`, `no_hello`. Every mode prints a login banner first.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-host/src/ndjson.rs` with this test module (the code above
it comes in step 2):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::PROTO_VERSION;
    use tokio::io::BufReader;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    #[test]
    fn writes_and_reads_one_signal() {
        rt().block_on(async {
            let mut buf: Vec<u8> = Vec::new();
            write_signal_async(&mut buf, &Signal::Failed { reason: "nope".into() })
                .await
                .expect("write");
            assert!(buf.ends_with(b"\n"), "must be newline terminated");

            let mut r = BufReader::new(buf.as_slice());
            match read_signal_async(&mut r, false).await.expect("read") {
                Signal::Failed { reason } => assert_eq!(reason, "nope"),
                other => panic!("wrong variant: {other:?}"),
            }
        });
    }

    #[test]
    fn skips_the_login_banner_when_asked() {
        rt().block_on(async {
            let mut input = b"Welcome to Ubuntu 24.04.1 LTS\nLast login: today\n".to_vec();
            write_signal_async(&mut input, &Signal::Failed { reason: "found".into() })
                .await
                .expect("write");

            let mut r = BufReader::new(input.as_slice());
            match read_signal_async(&mut r, true).await.expect("read") {
                Signal::Failed { reason } => assert_eq!(reason, "found"),
                other => panic!("wrong variant: {other:?}"),
            }
        });
    }

    #[test]
    fn refuses_the_banner_when_not_asked() {
        rt().block_on(async {
            let input = b"Welcome to Ubuntu 24.04.1 LTS\n".to_vec();
            let mut r = BufReader::new(input.as_slice());
            assert!(
                matches!(
                    read_signal_async(&mut r, false).await,
                    Err(ProtoError::Malformed(_))
                ),
                "without skipping, noise is an error"
            );
        });
    }

    #[test]
    fn end_of_stream_is_closed() {
        rt().block_on(async {
            let empty: &[u8] = b"";
            let mut r = BufReader::new(empty);
            assert!(matches!(
                read_signal_async(&mut r, true).await,
                Err(ProtoError::Closed)
            ));
        });
    }

    #[test]
    fn skipping_still_surfaces_version_skew() {
        rt().block_on(async {
            let line = format!(
                concat!(
                    r#"{{"t":"HostHello","proto":{},"session_id":"00","cert_spki_sha256":"YQ==","#,
                    r#""psk":"Yg==","candidates":[],"nat_type":"Unknown","bound_port":443}}"#
                ),
                PROTO_VERSION + 1
            );
            let input = format!("motd\n{line}\n");
            let mut r = BufReader::new(input.as_bytes());
            assert!(matches!(
                read_signal_async(&mut r, true).await,
                Err(ProtoError::VersionMismatch { .. })
            ));
        });
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: FAIL — `file not found for module ndjson`, then once declared,
`cannot find function write_signal_async`.

- [ ] **Step 3: Write the implementation**

Put this above the test module in `crates/oxutrm-host/src/ndjson.rs`:

```rust
//! `Signal` over tokio streams: the SSH child's pipes and the session's Unix
//! socket. The framing and the parsing live in `oxutrm-proto`; only the
//! asynchronous line reading is here.

use oxutrm_proto::{looks_like_signal, parse_signal_line, write_signal, ProtoError, Signal};
use tokio::io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// Serialise one `Signal` and flush it. Flushing matters: the peer is waiting
/// for this line before it will say anything back.
pub async fn write_signal_async<W: AsyncWrite + Unpin>(
    w: &mut W,
    s: &Signal,
) -> Result<(), ProtoError> {
    let mut line: Vec<u8> = Vec::new();
    write_signal(&mut line, s)?;
    w.write_all(&line).await?;
    w.flush().await?;
    Ok(())
}

/// Read one `Signal`.
///
/// With `skip_preamble`, lines that do not look like a signal are discarded —
/// SSH banners, motd, `stty` complaints on a non-tty. A line that does look
/// like one is always parsed strictly, so malformed JSON and version skew are
/// reported rather than skipped.
pub async fn read_signal_async<R: tokio::io::AsyncBufRead + Unpin>(
    r: &mut R,
    skip_preamble: bool,
) -> Result<Signal, ProtoError> {
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).await? == 0 {
            return Err(ProtoError::Closed);
        }
        if skip_preamble && !looks_like_signal(&line) {
            continue;
        }
        return parse_signal_line(&line);
    }
}
```

Add to `crates/oxutrm-host/src/lib.rs`:

```rust
pub mod ndjson;
pub use ndjson::{read_signal_async, write_signal_async};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: PASS, 5 new tests.

- [ ] **Step 5: Write the fake `ssh` fixture**

`crates/oxutrm-host/src/bin/oxutrm-fake-ssh.rs`:

```rust
//! Test fixture. Behaves like `ssh <target> oxutrm host --serve` without
//! needing an SSH server, a network, or a second machine.
//!
//! Every mode prints a login banner first, because a real login shell does and
//! the wrapper must survive it. The mode is chosen with $OXUTRM_FAKE_SSH_MODE:
//!
//! | mode             | behaviour                                          |
//! |------------------|----------------------------------------------------|
//! | `ok` (default)   | banner, HostHello, expects ClientHello, Established |
//! | `skew`           | banner, HostHello one protocol version ahead        |
//! | `missing_binary` | banner, "command not found" on stderr, exit 127     |
//! | `ssh_failed`     | banner, an ssh error on stderr, exit 255            |
//! | `no_hello`       | banner, then a clean exit 0 saying nothing          |

use std::io::Write;

use oxutrm_proto::{
    read_signal_skip_preamble, write_signal, NatType, PathDescription, Rung, Signal, PROTO_VERSION,
};

fn main() {
    let mode = std::env::var("OXUTRM_FAKE_SSH_MODE").unwrap_or_else(|_| "ok".to_string());
    let mut out = std::io::stdout();

    writeln!(out, "Welcome to Ubuntu 24.04.1 LTS (GNU/Linux 6.8.0-40-generic x86_64)").unwrap();
    writeln!(out, "Last login: Tue Aug 25 17:33:01 2026 from 192.0.2.1").unwrap();
    writeln!(out, "stty: 'standard input': Inappropriate ioctl for device").unwrap();
    out.flush().unwrap();

    match mode.as_str() {
        "missing_binary" => {
            eprintln!("bash: line 1: oxutrm: command not found");
            std::process::exit(127);
        }
        "ssh_failed" => {
            eprintln!("ssh: connect to host example.invalid port 22: Connection refused");
            std::process::exit(255);
        }
        "no_hello" => std::process::exit(0),
        _ => {}
    }

    let proto = if mode == "skew" { PROTO_VERSION + 1 } else { PROTO_VERSION };
    let hello = Signal::HostHello {
        proto,
        session_id: std::env::var("OXUTRM_FAKE_SSH_ID")
            .unwrap_or_else(|_| "00112233445566778899aabbccddeeff".to_string()),
        cert_spki_sha256: "Y2VydGZpbmdlcnByaW50".to_string(),
        psk: std::env::var("OXUTRM_FAKE_SSH_PSK").unwrap_or_else(|_| "cHNrYnl0ZXM=".to_string()),
        candidates: Vec::new(),
        nat_type: NatType::Unknown,
        bound_port: 443,
    };
    write_signal(&mut out, &hello).expect("write HostHello");

    let stdin = std::io::stdin();
    let mut r = stdin.lock();
    let client = read_signal_skip_preamble(&mut r).expect("read ClientHello");
    assert!(
        matches!(client, Signal::ClientHello { .. }),
        "expected a ClientHello, got {client:?}"
    );

    let established = Signal::Established {
        path: PathDescription {
            rung: Rung::SshTunnel,
            local: "127.0.0.1:1".parse().unwrap(),
            remote: "127.0.0.1:2".parse().unwrap(),
            probes_sent: 0,
            nat_type: NatType::Unknown,
            rtt_ms: 1,
            mtu: 1200,
        },
    };
    write_signal(&mut out, &established).expect("write Established");
}
```

Uncomment the `[[bin]] name = "oxutrm-fake-ssh"` block in
`crates/oxutrm-host/Cargo.toml`.

- [ ] **Step 6: Verify the fixture builds and runs**

Run: `cargo build --jobs 4 -p oxutrm-host`
Run: `OXUTRM_FAKE_SSH_MODE=missing_binary ./target/debug/oxutrm-fake-ssh; echo "exit=$?"`
Expected: the banner on stdout, `bash: line 1: oxutrm: command not found` on
stderr, `exit=127`.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): async NDJSON signalling and a fake ssh fixture

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: The SSH wrapper — spawning, handshaking, and failing usefully

**Files:**
- Create: `crates/oxutrm-host/src/ssh.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Test: `crates/oxutrm-host/tests/ssh_bootstrap.rs`

**Interfaces:**
- Consumes: `read_signal_async`, `write_signal_async` (Task 8); the
  `oxutrm-fake-ssh` fixture (Task 8); `oxutrm_proto::{Signal, ProtoError,
  TerminalCaps, TermSize, PathDescription, NatType, Candidate, PROTO_VERSION}`.
- Produces:
  ```rust
  #[derive(Clone, Debug)]
  pub struct SshCommand { pub program: String, pub args: Vec<String>, pub envs: Vec<(String, String)> }
  impl SshCommand {
      pub fn serve(target: &str) -> SshCommand;
      pub fn attach(target: &str, id: &str) -> SshCommand;
      pub fn list(target: &str) -> SshCommand;
      pub fn with_program(self, program: &str) -> SshCommand;
      pub fn with_env(self, key: &str, value: &str) -> SshCommand;
  }

  #[derive(thiserror::Error, Debug)]
  pub enum SshError {
      SshNotFound { program: String },
      RemoteBinaryMissing { stderr: String },
      SshFailed { code: i32, stderr: String },
      NoHello,
      Unexpected { expected: &'static str, got: String },
      Refused { reason: String },
      Proto(ProtoError),
      Io(std::io::Error),
  }

  pub struct SignalLink { /* private */ }
  impl SignalLink {
      pub async fn spawn(cmd: &SshCommand) -> Result<SignalLink, SshError>;
      pub async fn recv(&mut self) -> Result<Signal, SshError>;
      pub async fn send(&mut self, s: &Signal) -> Result<(), SshError>;
      pub async fn finish(self) -> Result<(), SshError>;
  }

  #[derive(Clone, Debug)]
  pub struct Bootstrap {
      pub session_id: String,
      pub psk_b64: String,
      pub cert_spki_b64: String,
      pub bound_port: u16,
      pub host_candidates: Vec<Candidate>,
      pub host_nat_type: NatType,
      pub path: PathDescription,
  }

  /// HostHello in, ClientHello out, Established back. The link is returned
  /// still open, because M4 keeps exchanging CandidateUpdate on it.
  pub async fn bootstrap(
      cmd: &SshCommand, caps: TerminalCaps, size: TermSize,
  ) -> Result<(SignalLink, Bootstrap), SshError>;
  ```

**The injection point:** `SshCommand::program` defaults to `$OXUTRM_SSH` when
set and `"ssh"` otherwise. Tests point it at the `oxutrm-fake-ssh` fixture, so
the whole wrapper is exercised with no SSH server, no network and no second
machine. The fixture emits a banner first, so banner tolerance is covered by
every test in this task rather than by one special case.

- [ ] **Step 1: Write the failing test**

`crates/oxutrm-host/tests/ssh_bootstrap.rs`:

```rust
use oxutrm_host::ssh::{bootstrap, SshCommand, SshError};
use oxutrm_proto::{TermSize, TerminalCaps};

fn fake() -> SshCommand {
    SshCommand::serve("example.invalid").with_program(env!("CARGO_BIN_EXE_oxutrm-fake-ssh"))
}

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

fn size() -> TermSize {
    TermSize { cols: 100, rows: 30 }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

#[test]
fn bootstraps_through_a_login_banner() {
    rt().block_on(async {
        let cmd = fake()
            .with_env("OXUTRM_FAKE_SSH_MODE", "ok")
            .with_env("OXUTRM_FAKE_SSH_ID", "0123456789abcdef0123456789abcdef");
        let (link, boot) = bootstrap(&cmd, caps(), size()).await.expect("bootstrap");
        assert_eq!(boot.session_id, "0123456789abcdef0123456789abcdef");
        assert_eq!(boot.bound_port, 443);
        assert_eq!(boot.path.rtt_ms, 1);
        link.finish().await.expect("clean finish");
    });
}

#[test]
fn a_missing_ssh_binary_says_so() {
    rt().block_on(async {
        let cmd = fake().with_program("/nonexistent/definitely-not-ssh");
        match bootstrap(&cmd, caps(), size()).await {
            Err(SshError::SshNotFound { program }) => {
                assert!(program.contains("definitely-not-ssh"), "{program}");
            }
            other => panic!("expected SshNotFound, got {other:?}"),
        }
    });
}

#[test]
fn a_missing_remote_binary_is_distinguished_from_an_ssh_failure() {
    rt().block_on(async {
        let cmd = fake().with_env("OXUTRM_FAKE_SSH_MODE", "missing_binary");
        match bootstrap(&cmd, caps(), size()).await {
            Err(SshError::RemoteBinaryMissing { stderr }) => {
                assert!(stderr.contains("command not found"), "{stderr}");
                let shown = SshError::RemoteBinaryMissing { stderr }.to_string();
                assert!(
                    shown.contains("oxutrm") && shown.contains("remote"),
                    "the message must tell the user what to install where: {shown}"
                );
            }
            other => panic!("expected RemoteBinaryMissing, got {other:?}"),
        }
    });
}

#[test]
fn an_ssh_failure_reports_the_exit_code_and_what_ssh_said() {
    rt().block_on(async {
        let cmd = fake().with_env("OXUTRM_FAKE_SSH_MODE", "ssh_failed");
        match bootstrap(&cmd, caps(), size()).await {
            Err(SshError::SshFailed { code, stderr }) => {
                assert_eq!(code, 255);
                assert!(stderr.contains("Connection refused"), "{stderr}");
            }
            other => panic!("expected SshFailed, got {other:?}"),
        }
    });
}

#[test]
fn a_silent_clean_exit_is_no_hello() {
    rt().block_on(async {
        let cmd = fake().with_env("OXUTRM_FAKE_SSH_MODE", "no_hello");
        match bootstrap(&cmd, caps(), size()).await {
            Err(SshError::NoHello) => {}
            other => panic!("expected NoHello, got {other:?}"),
        }
    });
}

#[test]
fn version_skew_reaches_the_caller_loudly() {
    rt().block_on(async {
        let cmd = fake().with_env("OXUTRM_FAKE_SSH_MODE", "skew");
        match bootstrap(&cmd, caps(), size()).await {
            Err(SshError::Proto(oxutrm_proto::ProtoError::VersionMismatch { peer, ours })) => {
                assert_ne!(peer, ours);
            }
            other => panic!("expected a VersionMismatch, got {other:?}"),
        }
    });
}

#[test]
fn the_remote_command_is_oxutrm_host_serve() {
    let cmd = SshCommand::serve("user@example.invalid");
    assert_eq!(cmd.program, "ssh");
    assert!(cmd.args.iter().any(|a| a == "user@example.invalid"));
    let tail = cmd.args.join(" ");
    assert!(tail.ends_with("oxutrm host --serve"), "{tail}");
    let attach = SshCommand::attach("h", "abc");
    assert!(attach.args.join(" ").ends_with("oxutrm host --attach abc"));
    let list = SshCommand::list("h");
    assert!(list.args.join(" ").ends_with("oxutrm host --list"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host --test ssh_bootstrap -- --test-threads 4`
Expected: FAIL to compile — `unresolved import oxutrm_host::ssh`.

- [ ] **Step 3: Write the implementation**

`crates/oxutrm-host/src/ssh.rs`:

```rust
//! Driving `ssh` and speaking the bootstrap over its pipes (spec §4).
//!
//! oxutrm never parses `~/.ssh/config`. It shells out to `ssh` and assumes
//! `ssh <target>` already works, by whatever means the user has arranged.

use std::process::Stdio;
use std::sync::{Arc, Mutex};

use oxutrm_proto::{
    Candidate, NatType, PathDescription, ProtoError, Signal, TermSize, TerminalCaps, PROTO_VERSION,
};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::ndjson::{read_signal_async, write_signal_async};

/// Overridden by `$OXUTRM_SSH`. This is the injection point tests use.
pub const SSH_PROGRAM_ENV: &str = "OXUTRM_SSH";

#[derive(Clone, Debug)]
pub struct SshCommand {
    pub program: String,
    pub args: Vec<String>,
    pub envs: Vec<(String, String)>,
}

fn ssh_program() -> String {
    match std::env::var(SSH_PROGRAM_ENV) {
        Ok(p) if !p.is_empty() => p,
        _ => "ssh".to_string(),
    }
}

impl SshCommand {
    fn remote(target: &str, tail: &[&str]) -> SshCommand {
        // -T: no pseudo-terminal. The channel carries JSON, not a login shell.
        let mut args = vec!["-T".to_string(), target.to_string(), "oxutrm".to_string()];
        args.extend(tail.iter().map(|s| s.to_string()));
        SshCommand { program: ssh_program(), args, envs: Vec::new() }
    }

    pub fn serve(target: &str) -> SshCommand {
        Self::remote(target, &["host", "--serve"])
    }

    pub fn attach(target: &str, id: &str) -> SshCommand {
        Self::remote(target, &["host", "--attach", id])
    }

    pub fn list(target: &str) -> SshCommand {
        Self::remote(target, &["host", "--list"])
    }

    pub fn with_program(mut self, program: &str) -> SshCommand {
        self.program = program.to_string();
        self
    }

    pub fn with_env(mut self, key: &str, value: &str) -> SshCommand {
        self.envs.push((key.to_string(), value.to_string()));
        self
    }
}

#[derive(thiserror::Error, Debug)]
pub enum SshError {
    #[error("cannot run {program:?}: is OpenSSH installed and on PATH? \
             (set OXUTRM_SSH to choose a different program)")]
    SshNotFound { program: String },
    #[error("the remote host has no `oxutrm` on its PATH — install oxutrm there, \
             the same binary you are running locally. ssh said: {stderr}")]
    RemoteBinaryMissing { stderr: String },
    #[error("ssh exited with status {code} before the session was established: {stderr}")]
    SshFailed { code: i32, stderr: String },
    #[error("the remote end closed the connection without sending a HostHello")]
    NoHello,
    #[error("expected {expected} from the remote end, got {got}")]
    Unexpected { expected: &'static str, got: String },
    #[error("the remote end refused the session: {reason}")]
    Refused { reason: String },
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub struct SignalLink {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: Arc<Mutex<String>>,
    /// The first read tolerates banners; later reads do not.
    saw_first: bool,
}

impl SignalLink {
    pub async fn spawn(cmd: &SshCommand) -> Result<SignalLink, SshError> {
        let mut builder = tokio::process::Command::new(&cmd.program);
        builder
            .args(&cmd.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // If the wrapper dies, ssh should not linger holding the session's
            // signalling channel open.
            .kill_on_drop(true);
        for (k, v) in &cmd.envs {
            builder.env(k, v);
        }

        let mut child = builder.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                SshError::SshNotFound { program: cmd.program.clone() }
            } else {
                SshError::Io(e)
            }
        })?;

        let stdin = child.stdin.take().expect("stdin was piped");
        let stdout = child.stdout.take().expect("stdout was piped");
        let mut child_stderr = child.stderr.take().expect("stderr was piped");

        // Drained in the background so ssh cannot block on a full stderr pipe,
        // and so the text is available when a failure needs explaining.
        let stderr = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&stderr);
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = child_stderr.read_to_end(&mut buf).await;
            if let Ok(mut s) = sink.lock() {
                s.push_str(&String::from_utf8_lossy(&buf));
            }
        });

        Ok(SignalLink {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
            saw_first: false,
        })
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }

    /// Read one `Signal`. On a closed stream, wait for the child and turn its
    /// exit status into an error the user can act on.
    pub async fn recv(&mut self) -> Result<Signal, SshError> {
        let skip = !self.saw_first;
        match read_signal_async(&mut self.stdout, skip).await {
            Ok(s) => {
                self.saw_first = true;
                Ok(s)
            }
            Err(ProtoError::Closed) => Err(self.classify_exit().await),
            Err(e) => Err(SshError::Proto(e)),
        }
    }

    pub async fn send(&mut self, s: &Signal) -> Result<(), SshError> {
        write_signal_async(&mut self.stdin, s).await?;
        Ok(())
    }

    /// Why did the far end go quiet?
    async fn classify_exit(&mut self) -> SshError {
        let status = match self.child.wait().await {
            Ok(s) => s,
            Err(e) => return SshError::Io(e),
        };
        // Give the stderr drain a moment to finish after the child exits.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let stderr = self.stderr_text();
        match status.code() {
            Some(0) => SshError::NoHello,
            // A POSIX shell exits 127 when the command does not exist. That is
            // the far more common cause here than any ssh failure, and it has a
            // fix the user can act on.
            Some(127) => SshError::RemoteBinaryMissing { stderr },
            Some(code) => {
                if stderr.contains("command not found") || stderr.contains("oxutrm: not found") {
                    SshError::RemoteBinaryMissing { stderr }
                } else {
                    SshError::SshFailed { code, stderr }
                }
            }
            None => SshError::SshFailed { code: -1, stderr },
        }
    }

    /// Wait for ssh to exit, mapping a non-zero status to an error.
    pub async fn finish(mut self) -> Result<(), SshError> {
        let status = self.child.wait().await?;
        if status.success() {
            Ok(())
        } else {
            Err(SshError::SshFailed {
                code: status.code().unwrap_or(-1),
                stderr: self.stderr_text(),
            })
        }
    }
}

/// What the client learns from the bootstrap.
#[derive(Clone, Debug)]
pub struct Bootstrap {
    pub session_id: String,
    /// base64. Held in memory only, never written anywhere.
    pub psk_b64: String,
    pub cert_spki_b64: String,
    pub bound_port: u16,
    pub host_candidates: Vec<Candidate>,
    pub host_nat_type: NatType,
    pub path: PathDescription,
}

/// Spawn ssh, exchange hellos, and wait for the link to come up.
///
/// The link is returned still open: M4 keeps exchanging `CandidateUpdate` on
/// it while the ICE ladder runs, and closes it once QUIC is up (spec §4.1).
pub async fn bootstrap(
    cmd: &SshCommand,
    caps: TerminalCaps,
    size: TermSize,
) -> Result<(SignalLink, Bootstrap), SshError> {
    let mut link = SignalLink::spawn(cmd).await?;

    let hello = link.recv().await?;
    let (session_id, cert_spki_b64, psk_b64, host_candidates, host_nat_type, bound_port) =
        match hello {
            Signal::HostHello {
                session_id,
                cert_spki_sha256,
                psk,
                candidates,
                nat_type,
                bound_port,
                ..
            } => (session_id, cert_spki_sha256, psk, candidates, nat_type, bound_port),
            Signal::Failed { reason } => return Err(SshError::Refused { reason }),
            other => {
                return Err(SshError::Unexpected {
                    expected: "HostHello",
                    got: format!("{other:?}"),
                })
            }
        };

    link.send(&Signal::ClientHello {
        proto: PROTO_VERSION,
        // M3 has no network layer: M4 fills these from oxutrm-net.
        candidates: Vec::new(),
        nat_type: NatType::Unknown,
        caps,
        size,
    })
    .await?;

    loop {
        match link.recv().await? {
            Signal::Established { path } => {
                return Ok((
                    link,
                    Bootstrap {
                        session_id,
                        psk_b64,
                        cert_spki_b64,
                        bound_port,
                        host_candidates,
                        host_nat_type,
                        path,
                    },
                ))
            }
            Signal::Failed { reason } => return Err(SshError::Refused { reason }),
            // Candidates keep arriving while the ladder runs; M3 has none to
            // act on, so they are collected and ignored until Established.
            Signal::CandidateUpdate { .. } => continue,
            other => {
                return Err(SshError::Unexpected {
                    expected: "Established",
                    got: format!("{other:?}"),
                })
            }
        }
    }
}
```

Add to `crates/oxutrm-host/src/lib.rs`:

```rust
pub mod ssh;
pub use ssh::{bootstrap, Bootstrap, SignalLink, SshCommand, SshError};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host --test ssh_bootstrap -- --test-threads 4`
Expected: PASS, 7 tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): SSH wrapper with banner tolerance and useful failures

A missing remote binary, a missing local ssh and an ssh failure are three
different errors with three different fixes.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Key material, fresh per attach, never on disk

**Files:**
- Create: `crates/oxutrm-host/src/keys.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Test: `crates/oxutrm-host/src/keys.rs` (inline tests)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces:
  ```rust
  /// 32 CSPRNG bytes for the ICE credential and QUIC PSK binding, plus the
  /// SHA-256 of the host certificate's SPKI. Neither ever reaches a disk.
  pub struct KeyMaterial { /* private */ }
  impl KeyMaterial {
      pub fn fresh() -> KeyMaterial;
      pub fn psk(&self) -> &[u8; 32];
      pub fn psk_b64(&self) -> String;
      pub fn cert_spki_sha256(&self) -> &[u8; 32];
      pub fn cert_spki_b64(&self) -> String;
  }
  impl Drop for KeyMaterial;              // zeroes both arrays
  impl std::fmt::Debug for KeyMaterial;   // prints no bytes

  /// 128 bits from the CSPRNG as 32 lowercase hex characters.
  pub fn new_session_id() -> String;
  ```

**M4 note:** `fresh()` draws the certificate fingerprint from the CSPRNG because
M3 has no certificate. M4 replaces that draw with the real SHA-256 of the SPKI
from `oxutrm_net::generate_cert()`; the psk stays exactly as it is.

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-host/src/keys.rs` with this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    #[test]
    fn a_session_id_is_32_lowercase_hex_characters() {
        let id = new_session_id();
        assert_eq!(id.len(), 32, "128 bits as hex: {id}");
        assert!(
            id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "not lowercase hex: {id}"
        );
    }

    #[test]
    fn session_ids_do_not_repeat() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1_000 {
            assert!(seen.insert(new_session_id()), "the CSPRNG repeated itself");
        }
    }

    #[test]
    fn a_psk_is_32_bytes_and_never_the_same_twice() {
        let a = KeyMaterial::fresh();
        let b = KeyMaterial::fresh();
        assert_eq!(a.psk().len(), 32);
        assert_ne!(a.psk(), b.psk(), "every attach gets its own psk");
        assert_ne!(
            a.cert_spki_sha256(),
            b.cert_spki_sha256(),
            "every attach gets its own certificate"
        );
        assert_ne!(a.psk(), &[0u8; 32], "an all-zero psk means the CSPRNG did nothing");
    }

    #[test]
    fn base64_decodes_back_to_the_bytes() {
        let k = KeyMaterial::fresh();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(k.psk_b64())
            .expect("valid base64");
        assert_eq!(decoded.as_slice(), k.psk().as_slice());
        let decoded_cert = base64::engine::general_purpose::STANDARD
            .decode(k.cert_spki_b64())
            .expect("valid base64");
        assert_eq!(decoded_cert.as_slice(), k.cert_spki_sha256().as_slice());
    }

    #[test]
    fn debug_never_prints_the_bytes() {
        let k = KeyMaterial::fresh();
        let shown = format!("{k:?}");
        let first = format!("{:02x}", k.psk()[0]);
        assert!(!shown.contains(&k.psk_b64()), "Debug leaked the psk: {shown}");
        assert!(
            !shown.contains(&first) || shown.contains("redacted"),
            "Debug must not render key bytes: {shown}"
        );
        assert!(shown.contains("redacted"), "{shown}");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: FAIL — `file not found for module keys`, then `cannot find function
new_session_id`.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/oxutrm-host/src/keys.rs`:

```rust
//! Key material for one attach. Spec §4.2 and §11: 32 bytes from the OS
//! CSPRNG, fresh for every attach, and never written to disk on either side.

use base64::Engine;
use rand::RngCore;

pub struct KeyMaterial {
    psk: [u8; 32],
    cert_spki_sha256: [u8; 32],
}

impl KeyMaterial {
    /// Fresh material for one attach. A psk from an earlier session can
    /// therefore never reattach (spec §11).
    ///
    /// M3 has no certificate, so the fingerprint is drawn from the CSPRNG too.
    /// M4 replaces that draw with the SHA-256 of the SPKI that
    /// `oxutrm_net::generate_cert()` produces; nothing else here changes.
    pub fn fresh() -> KeyMaterial {
        let mut psk = [0u8; 32];
        let mut cert_spki_sha256 = [0u8; 32];
        let mut rng = rand::rng();
        rng.fill_bytes(&mut psk);
        rng.fill_bytes(&mut cert_spki_sha256);
        KeyMaterial { psk, cert_spki_sha256 }
    }

    pub fn psk(&self) -> &[u8; 32] {
        &self.psk
    }

    pub fn psk_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.psk)
    }

    pub fn cert_spki_sha256(&self) -> &[u8; 32] {
        &self.cert_spki_sha256
    }

    pub fn cert_spki_b64(&self) -> String {
        base64::engine::general_purpose::STANDARD.encode(self.cert_spki_sha256)
    }
}

impl Drop for KeyMaterial {
    fn drop(&mut self) {
        // Best effort: shrinks the window in which a core dump or a reused
        // allocation could expose the psk.
        self.psk.fill(0);
        self.cert_spki_sha256.fill(0);
    }
}

impl std::fmt::Debug for KeyMaterial {
    /// Deliberately prints nothing useful. A psk in a log file is a psk on a
    /// disk.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyMaterial { psk: <redacted>, cert_spki_sha256: <redacted> }")
    }
}

/// 128 bits from the CSPRNG, rendered as 32 lowercase hex characters.
pub fn new_session_id() -> String {
    let mut bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut bytes);
    let mut out = String::with_capacity(32);
    for b in bytes {
        use std::fmt::Write;
        let _ = write!(out, "{b:02x}");
    }
    out
}
```

Add `base64 = "0.22"` to `[dev-dependencies]` as well, so the inline test can
decode. Add to `crates/oxutrm-host/src/lib.rs`:

```rust
pub mod keys;
pub use keys::{new_session_id, KeyMaterial};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: PASS, 5 new tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): per-attach key material that never leaves memory

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 11: The `Session` and its lifecycle

**Files:**
- Create: `crates/oxutrm-host/src/session.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Test: `crates/oxutrm-host/src/session.rs` (inline tests)

**Interfaces:**
- Consumes: `SessionMeta`, `RegistryGuard`, `now_unix` (Tasks 4-6);
  `KeyMaterial`, `new_session_id` (Task 10);
  `oxutrm_proto::{Signal, TermSize, NatType, PathDescription, Rung, PROTO_VERSION}`.
- Produces:
  ```rust
  /// Whatever is running under the session. M3 stubs it; M4 supplies a
  /// HostTerm-backed implementation whose `child_exited()` drives this.
  pub trait ShellHandle: Send {
      fn exit_status(&mut self) -> Option<i32>;
  }

  pub struct StubShell { /* private */ }
  impl StubShell {
      pub fn running() -> StubShell;
      pub fn exited(code: i32) -> StubShell;
      pub fn set_exited(&mut self, code: i32);
  }

  #[derive(Clone, Debug)]
  pub struct SessionConfig {
      pub shell: String,
      pub size: TermSize,
      /// `None` means never, which is the default (spec §9.3).
      pub idle_timeout: Option<std::time::Duration>,
  }
  impl Default for SessionConfig;
  pub fn default_shell() -> String;

  #[derive(Clone, Copy, PartialEq, Eq, Debug)]
  pub enum SessionEnd { ShellExited(i32), IdleTimeout }

  pub struct Session { /* private */ }
  impl Session {
      pub fn create(cfg: SessionConfig, shell: Box<dyn ShellHandle>) -> Session;
      pub fn id(&self) -> &str;
      pub fn meta(&self) -> &SessionMeta;
      pub fn attach_count(&self) -> u64;
      pub fn set_size(&mut self, size: TermSize);
      pub fn refresh_pid(&mut self);
      pub fn publish(&self, root: &std::path::Path) -> anyhow::Result<RegistryGuard>;
      pub fn begin_attach(&mut self) -> KeyMaterial;
      pub fn host_hello(&self, keys: &KeyMaterial) -> Signal;
      pub fn note_activity(&mut self, now: std::time::Instant);
      pub fn poll_end(&mut self, now: std::time::Instant) -> Option<SessionEnd>;
  }
  ```

- [ ] **Step 1: Write the failing test**

Create `crates/oxutrm-host/src/session.rs` with this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn cfg() -> SessionConfig {
        SessionConfig {
            shell: "/bin/bash".to_string(),
            size: TermSize { cols: 80, rows: 24 },
            idle_timeout: None,
        }
    }

    #[test]
    fn a_new_session_has_a_hex_id_and_our_pid() {
        let s = Session::create(cfg(), Box::new(StubShell::running()));
        assert_eq!(s.id().len(), 32);
        assert_eq!(s.meta().pid, std::process::id());
        assert_eq!(s.meta().shell, "/bin/bash");
        assert_eq!(s.meta().size, TermSize { cols: 80, rows: 24 });
        assert_eq!(s.attach_count(), 0);
    }

    #[test]
    fn the_default_idle_timeout_is_never() {
        assert!(
            SessionConfig::default().idle_timeout.is_none(),
            "spec 9.3: the default idle timeout is never"
        );
        let mut s = Session::create(cfg(), Box::new(StubShell::running()));
        let far_future = Instant::now() + Duration::from_secs(86_400 * 7);
        assert_eq!(
            s.poll_end(far_future),
            None,
            "a week detached must cost the session nothing"
        );
    }

    #[test]
    fn an_idle_session_ends_once_the_timeout_passes() {
        let mut c = cfg();
        c.idle_timeout = Some(Duration::from_secs(60));
        let mut s = Session::create(c, Box::new(StubShell::running()));
        let start = Instant::now();
        assert_eq!(s.poll_end(start + Duration::from_secs(59)), None);
        assert_eq!(
            s.poll_end(start + Duration::from_secs(61)),
            Some(SessionEnd::IdleTimeout)
        );
    }

    #[test]
    fn activity_pushes_the_idle_deadline_out() {
        let mut c = cfg();
        c.idle_timeout = Some(Duration::from_secs(60));
        let mut s = Session::create(c, Box::new(StubShell::running()));
        let start = Instant::now();
        s.note_activity(start + Duration::from_secs(50));
        assert_eq!(s.poll_end(start + Duration::from_secs(100)), None);
        assert_eq!(
            s.poll_end(start + Duration::from_secs(111)),
            Some(SessionEnd::IdleTimeout)
        );
    }

    #[test]
    fn the_session_ends_when_the_shell_exits() {
        let mut s = Session::create(cfg(), Box::new(StubShell::exited(3)));
        assert_eq!(s.poll_end(Instant::now()), Some(SessionEnd::ShellExited(3)));
    }

    #[test]
    fn a_shell_exit_beats_an_idle_timeout() {
        let mut c = cfg();
        c.idle_timeout = Some(Duration::from_secs(1));
        let mut s = Session::create(c, Box::new(StubShell::exited(0)));
        assert_eq!(
            s.poll_end(Instant::now() + Duration::from_secs(10)),
            Some(SessionEnd::ShellExited(0)),
            "the shell's exit is the more informative answer"
        );
    }

    #[test]
    fn every_attach_gets_fresh_key_material() {
        let mut s = Session::create(cfg(), Box::new(StubShell::running()));
        let first = s.begin_attach();
        let second = s.begin_attach();
        assert_ne!(
            first.psk(),
            second.psk(),
            "spec 11: a stolen psk from an earlier attach must not reattach"
        );
        assert_ne!(first.cert_spki_sha256(), second.cert_spki_sha256());
        assert_eq!(s.attach_count(), 2);
    }

    #[test]
    fn host_hello_carries_this_sessions_id_and_the_given_keys() {
        let mut s = Session::create(cfg(), Box::new(StubShell::running()));
        let keys = s.begin_attach();
        match s.host_hello(&keys) {
            Signal::HostHello { proto, session_id, psk, cert_spki_sha256, .. } => {
                assert_eq!(proto, PROTO_VERSION);
                assert_eq!(session_id, s.id());
                assert_eq!(psk, keys.psk_b64());
                assert_eq!(cert_spki_sha256, keys.cert_spki_b64());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn publishing_writes_the_registry_entry() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = crate::Registry::dir_at(tmp.path());
        let s = Session::create(cfg(), Box::new(StubShell::running()));
        let guard = s.publish(&root).expect("publish");
        assert_eq!(guard.dir(), root.join(s.id()));
        let listed = crate::Registry::list_in(&root).expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].session_id, s.id());
    }

    #[test]
    fn set_size_is_reflected_in_the_published_meta() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = crate::Registry::dir_at(tmp.path());
        let mut s = Session::create(cfg(), Box::new(StubShell::running()));
        s.set_size(TermSize { cols: 200, rows: 60 });
        let _guard = s.publish(&root).expect("publish");
        let listed = crate::Registry::list_in(&root).expect("list");
        assert_eq!(listed[0].size, TermSize { cols: 200, rows: 60 });
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: FAIL — `file not found for module session`, then `cannot find struct
Session`.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/oxutrm-host/src/session.rs`:

```rust
//! One session: what it is, when it ends, and what it tells an attaching
//! client (spec §9).

use std::path::Path;
use std::time::{Duration, Instant};

use oxutrm_proto::{NatType, Signal, TermSize, PROTO_VERSION};

use crate::keys::{new_session_id, KeyMaterial};
use crate::registry::{now_unix, RegistryGuard, SessionMeta};

/// Whatever the session is hosting.
///
/// M3 stubs this: the milestone proves the session machinery, not the pixels.
/// M4 supplies an implementation backed by `oxutrm_term::HostTerm`, forwarding
/// `HostTerm::child_exited()`.
pub trait ShellHandle: Send {
    /// `Some(code)` once the shell has exited, `None` while it runs.
    fn exit_status(&mut self) -> Option<i32>;
}

pub struct StubShell {
    status: Option<i32>,
}

impl StubShell {
    pub fn running() -> StubShell {
        StubShell { status: None }
    }

    pub fn exited(code: i32) -> StubShell {
        StubShell { status: Some(code) }
    }

    pub fn set_exited(&mut self, code: i32) {
        self.status = Some(code);
    }
}

impl ShellHandle for StubShell {
    fn exit_status(&mut self) -> Option<i32> {
        self.status
    }
}

#[derive(Clone, Debug)]
pub struct SessionConfig {
    pub shell: String,
    pub size: TermSize,
    /// `None` means never. That is the default, and it is the point of the
    /// product: staying detached for a week must cost nothing (spec §9.3).
    pub idle_timeout: Option<Duration>,
}

impl Default for SessionConfig {
    fn default() -> Self {
        SessionConfig {
            shell: default_shell(),
            size: TermSize { cols: 80, rows: 24 },
            idle_timeout: None,
        }
    }
}

pub fn default_shell() -> String {
    match std::env::var("SHELL") {
        Ok(s) if !s.is_empty() => s,
        _ => "/bin/sh".to_string(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionEnd {
    ShellExited(i32),
    IdleTimeout,
}

pub struct Session {
    meta: SessionMeta,
    cfg: SessionConfig,
    shell: Box<dyn ShellHandle>,
    last_activity: Instant,
    attaches: u64,
}

impl Session {
    pub fn create(cfg: SessionConfig, shell: Box<dyn ShellHandle>) -> Session {
        let meta = SessionMeta {
            session_id: new_session_id(),
            pid: std::process::id(),
            created_unix: now_unix(),
            shell: cfg.shell.clone(),
            size: cfg.size,
        };
        Session {
            meta,
            cfg,
            shell,
            last_activity: Instant::now(),
            attaches: 0,
        }
    }

    pub fn id(&self) -> &str {
        &self.meta.session_id
    }

    pub fn meta(&self) -> &SessionMeta {
        &self.meta
    }

    pub fn attach_count(&self) -> u64 {
        self.attaches
    }

    pub fn set_size(&mut self, size: TermSize) {
        self.meta.size = size;
    }

    /// After `daemonize()` the pid has changed twice. `--list` prunes on this
    /// number, so it must be corrected before the entry is published.
    pub fn refresh_pid(&mut self) {
        self.meta.pid = std::process::id();
    }

    pub fn publish(&self, root: &Path) -> anyhow::Result<RegistryGuard> {
        RegistryGuard::register_in(root, &self.meta)
    }

    /// Fresh key material for one attach (spec §11: fresh keys per attach).
    pub fn begin_attach(&mut self) -> KeyMaterial {
        self.attaches += 1;
        self.last_activity = Instant::now();
        KeyMaterial::fresh()
    }

    /// The first line of the signalling exchange.
    ///
    /// M3 advertises no candidates and no bound port: there is no network
    /// layer yet. M4 fills these from `oxutrm-net` before this is sent.
    pub fn host_hello(&self, keys: &KeyMaterial) -> Signal {
        Signal::HostHello {
            proto: PROTO_VERSION,
            session_id: self.meta.session_id.clone(),
            cert_spki_sha256: keys.cert_spki_b64(),
            psk: keys.psk_b64(),
            candidates: Vec::new(),
            nat_type: NatType::Unknown,
            bound_port: 0,
        }
    }

    pub fn note_activity(&mut self, now: Instant) {
        self.last_activity = now;
    }

    /// Has the session finished? Checked on a timer by `run_session`.
    pub fn poll_end(&mut self, now: Instant) -> Option<SessionEnd> {
        // The shell's exit is the more informative answer, so it is checked
        // first even when the idle deadline has also passed.
        if let Some(code) = self.shell.exit_status() {
            return Some(SessionEnd::ShellExited(code));
        }
        match self.cfg.idle_timeout {
            Some(limit) if now.saturating_duration_since(self.last_activity) > limit => {
                Some(SessionEnd::IdleTimeout)
            }
            _ => None,
        }
    }
}
```

Add to `crates/oxutrm-host/src/lib.rs`:

```rust
pub mod session;
pub use session::{
    default_shell, Session, SessionConfig, SessionEnd, ShellHandle, StubShell,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host -- --test-threads 4`
Expected: PASS, 10 new tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): Session lifecycle with idle timeout defaulting to never

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

### Task 12: The session socket — serving attaches and relaying into them

**Files:**
- Modify: `crates/oxutrm-host/src/session.rs`
- Modify: `crates/oxutrm-host/src/lib.rs`
- Test: `crates/oxutrm-host/tests/attach.rs`

**Interfaces:**
- Consumes: `Session`, `SessionEnd`, `SessionConfig`, `StubShell` (Task 11);
  `read_signal_async`, `write_signal_async` (Task 8); `KeyMaterial` (Task 10).
- Produces:
  ```rust
  /// Serve one attaching client: fresh keys, HostHello out, ClientHello in,
  /// Established back.
  pub async fn serve_attach(
      session: &mut Session, stream: tokio::net::UnixStream,
  ) -> anyhow::Result<()>;

  /// Accept attaches until the shell exits or the session goes idle.
  pub async fn run_session(
      session: Session, listener: tokio::net::UnixListener,
  ) -> anyhow::Result<SessionEnd>;

  /// The `--attach` side: connect to a live session's socket and pump bytes
  /// between it and the SSH pipes, flushing every chunk.
  pub async fn relay_attach<I, O>(sock: &std::path::Path, input: I, output: O) -> anyhow::Result<()>
  where I: tokio::io::AsyncRead + Unpin + Send + 'static,
        O: tokio::io::AsyncWrite + Unpin + Send + 'static;
  ```

**Why a byte pump and not a `Signal` pump:** the attaching process is a courier.
It has no key material, makes no decisions, and must not need a protocol update
when a new `Signal` variant appears. Both real endpoints — the client and the
session — do the parsing.

- [ ] **Step 1: Write the failing test**

`crates/oxutrm-host/tests/attach.rs`:

```rust
use std::time::Duration;

use oxutrm_host::session::{relay_attach, run_session, SessionConfig, SessionEnd, StubShell};
use oxutrm_host::{read_signal_async, write_signal_async, Registry, Session};
use oxutrm_proto::{NatType, Signal, TermSize, TerminalCaps, PROTO_VERSION};
use tokio::io::BufReader;
use tokio::net::{UnixListener, UnixStream};

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

fn client_hello() -> Signal {
    Signal::ClientHello {
        proto: PROTO_VERSION,
        candidates: Vec::new(),
        nat_type: NatType::Unknown,
        caps: caps(),
        size: TermSize { cols: 132, rows: 43 },
    }
}

/// Connect, do one full attach handshake, and return the psk that was offered.
async fn attach_once(sock: &std::path::Path) -> String {
    let stream = UnixStream::connect(sock).await.expect("connect");
    let (r, mut w) = stream.into_split();
    let mut r = BufReader::new(r);

    let psk = match read_signal_async(&mut r, false).await.expect("HostHello") {
        Signal::HostHello { proto, psk, .. } => {
            assert_eq!(proto, PROTO_VERSION);
            psk
        }
        other => panic!("expected HostHello, got {other:?}"),
    };
    write_signal_async(&mut w, &client_hello()).await.expect("ClientHello");
    match read_signal_async(&mut r, false).await.expect("Established") {
        Signal::Established { .. } => {}
        other => panic!("expected Established, got {other:?}"),
    }
    psk
}

#[tokio::test]
async fn reattaching_generates_fresh_key_material_every_time() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let session = Session::create(SessionConfig::default(), Box::new(StubShell::running()));
    let guard = session.publish(&root).expect("publish");
    let sock = guard.socket_path();
    let listener = UnixListener::bind(&sock).expect("bind");

    let runner = tokio::spawn(run_session(session, listener));

    let first = attach_once(&sock).await;
    let second = attach_once(&sock).await;
    let third = attach_once(&sock).await;

    assert_ne!(first, second, "the second attach must not reuse the first psk");
    assert_ne!(second, third);
    assert_ne!(first, third);

    runner.abort();
}

#[tokio::test]
async fn the_session_ends_when_the_shell_exits() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let mut cfg = SessionConfig::default();
    cfg.idle_timeout = None;
    let session = Session::create(cfg, Box::new(StubShell::exited(7)));
    let guard = session.publish(&root).expect("publish");
    let listener = UnixListener::bind(guard.socket_path()).expect("bind");

    let end = tokio::time::timeout(Duration::from_secs(5), run_session(session, listener))
        .await
        .expect("run_session must return promptly")
        .expect("run_session");
    assert_eq!(end, SessionEnd::ShellExited(7));
}

#[tokio::test]
async fn an_idle_session_times_out_when_configured_to() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let mut cfg = SessionConfig::default();
    cfg.idle_timeout = Some(Duration::from_millis(200));
    let session = Session::create(cfg, Box::new(StubShell::running()));
    let guard = session.publish(&root).expect("publish");
    let listener = UnixListener::bind(guard.socket_path()).expect("bind");

    let end = tokio::time::timeout(Duration::from_secs(5), run_session(session, listener))
        .await
        .expect("run_session must return promptly")
        .expect("run_session");
    assert_eq!(end, SessionEnd::IdleTimeout);
}

#[tokio::test]
async fn the_relay_carries_a_whole_handshake_between_pipes_and_the_socket() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = Registry::dir_at(tmp.path());
    let session = Session::create(SessionConfig::default(), Box::new(StubShell::running()));
    let guard = session.publish(&root).expect("publish");
    let sock = guard.socket_path();
    let listener = UnixListener::bind(&sock).expect("bind");
    let runner = tokio::spawn(run_session(session, listener));

    // Stand in for the SSH pipes the attaching process is given.
    let (client_side, relay_input) = tokio::io::duplex(8192);
    let (relay_output, client_reads) = tokio::io::duplex(8192);
    let sock_for_relay = sock.clone();
    let relay = tokio::spawn(async move {
        relay_attach(&sock_for_relay, relay_input, relay_output).await
    });

    let (cr, mut cw) = tokio::io::split(client_side);
    let mut cr = BufReader::new(cr);
    // The relay only carries bytes, so the client reads from the other duplex.
    drop(cr);
    let mut down = BufReader::new(client_reads);

    let psk = match read_signal_async(&mut down, false).await.expect("HostHello") {
        Signal::HostHello { psk, .. } => psk,
        other => panic!("expected HostHello, got {other:?}"),
    };
    assert!(!psk.is_empty());
    write_signal_async(&mut cw, &client_hello()).await.expect("ClientHello");
    match read_signal_async(&mut down, false).await.expect("Established") {
        Signal::Established { .. } => {}
        other => panic!("expected Established, got {other:?}"),
    }

    relay.abort();
    runner.abort();
}

#[tokio::test]
async fn attaching_to_a_dead_session_fails_with_a_useful_message() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let sock = tmp.path().join("nothing-here").join("sock");
    let (_, input) = tokio::io::duplex(64);
    let (output, _) = tokio::io::duplex(64);
    let err = relay_attach(&sock, input, output)
        .await
        .expect_err("there is no session there");
    let text = format!("{err:#}");
    assert!(text.contains("sock"), "the message must name the socket: {text}");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --jobs 4 -p oxutrm-host --test attach -- --test-threads 4`
Expected: FAIL to compile — `cannot find function run_session`. Add
`tokio = { version = "1", features = ["macros", "rt-multi-thread"] }` to
`[dev-dependencies]` so `#[tokio::test]` is available.

- [ ] **Step 3: Write the implementation**

Append to `crates/oxutrm-host/src/session.rs`:

```rust
use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::ndjson::{read_signal_async, write_signal_async};
use oxutrm_proto::{PathDescription, Rung};

/// Serve one attaching client: fresh keys out, capabilities in.
///
/// M3 stops there and reports a stub path, because there is no QUIC yet. M4
/// continues from the same point into the ICE ladder and only then reports the
/// real `PathDescription`.
pub async fn serve_attach(session: &mut Session, stream: UnixStream) -> anyhow::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    let keys = session.begin_attach();
    write_signal_async(&mut writer, &session.host_hello(&keys))
        .await
        .context("sending HostHello")?;

    match read_signal_async(&mut reader, false)
        .await
        .context("reading ClientHello")?
    {
        Signal::ClientHello { size, .. } => session.set_size(size),
        other => anyhow::bail!("expected a ClientHello, got {other:?}"),
    }

    let path = PathDescription {
        rung: Rung::SshTunnel,
        local: "127.0.0.1:0".parse().expect("literal address"),
        remote: "127.0.0.1:0".parse().expect("literal address"),
        probes_sent: 0,
        nat_type: NatType::Unknown,
        rtt_ms: 0,
        mtu: 1200,
    };
    write_signal_async(&mut writer, &Signal::Established { path })
        .await
        .context("sending Established")?;

    session.note_activity(Instant::now());
    // `keys` is dropped here, zeroing the psk. M4 hands it to the ICE agent
    // instead and drops it when the QUIC connection is up.
    Ok(())
}

/// How often the lifecycle conditions are re-checked.
const LIFECYCLE_TICK: Duration = Duration::from_millis(100);

/// Accept attaches until the shell exits or the session goes idle (spec §9.3).
///
/// Attaches are served one at a time. §9.5 keeps multiple simultaneous clients
/// possible in the state model, but M3 does not implement them.
pub async fn run_session(
    mut session: Session,
    listener: UnixListener,
) -> anyhow::Result<SessionEnd> {
    let mut tick = tokio::time::interval(LIFECYCLE_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _addr) = accepted.context("accepting on the session socket")?;
                // A client that misbehaves loses its attach, not the session.
                // There is nowhere to log it: after daemonize, stderr is
                // /dev/null.
                let _ = serve_attach(&mut session, stream).await;
            }
            _ = tick.tick() => {
                if let Some(end) = session.poll_end(Instant::now()) {
                    return Ok(end);
                }
            }
        }
    }
}

/// Copy in one direction, flushing every chunk.
///
/// `tokio::io::copy` flushes only at end of stream, which deadlocks an
/// interactive handshake: the client would wait for a `HostHello` sitting in
/// the relay's buffer.
async fn pump<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut r: R,
    mut w: W,
) -> std::io::Result<()> {
    let mut buf = [0u8; 8192];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).await?;
        w.flush().await?;
    }
    let _ = w.shutdown().await;
    Ok(())
}

/// `oxutrm host --attach <id>`: connect to a live session and carry bytes
/// between it and the SSH pipes.
///
/// Deliberately a byte courier and not a `Signal` parser: it holds no key
/// material, makes no decisions, and needs no change when a new `Signal`
/// variant appears.
pub async fn relay_attach<I, O>(sock: &Path, input: I, output: O) -> anyhow::Result<()>
where
    I: AsyncRead + Unpin + Send + 'static,
    O: AsyncWrite + Unpin + Send + 'static,
{
    let stream = UnixStream::connect(sock).await.with_context(|| {
        format!(
            "no live session listening on {} — `oxutrm host --list` shows what is running",
            sock.display()
        )
    })?;
    let (sock_read, sock_write) = stream.into_split();

    let up = tokio::spawn(pump(input, sock_write));
    let down = pump(sock_read, output).await;
    up.abort();
    down.context("relaying session output")?;
    Ok(())
}
```

Extend the re-export in `crates/oxutrm-host/src/lib.rs`:

```rust
pub use session::{
    default_shell, relay_attach, run_session, serve_attach, Session, SessionConfig, SessionEnd,
    ShellHandle, StubShell,
};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --jobs 4 -p oxutrm-host --test attach -- --test-threads 4`
Expected: PASS, 5 tests.

- [ ] **Step 5: Run the whole suite, lint and commit**

```bash
cargo test --jobs 4 -p oxutrm-host -- --test-threads 4
cargo clippy --all-targets --jobs 4 -- -D warnings
git add crates/oxutrm-host
git commit -m "feat(host): session socket, attach handshake and the attach relay

Every attach mints fresh key material, so a psk from an earlier attach
cannot reattach.

Co-Authored-By: Claude Opus 5 (1M context) <noreply@anthropic.com>"
```

---

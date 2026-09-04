//! `oxutrm host --attach <id>` through the real entry point.
//!
//! `run_host_attach` (`src/main.rs`) is the one thing Task 4 actually built:
//! the wiring from the CLI's `--attach <id>` arm through to
//! `oxutrm_host::attach::connect_to_session`'s error, surfaced to the user on
//! stderr. Everything `connect_to_session` and `relay_signals` do was already
//! covered before that task, by `crates/oxutrm-host/tests/attach.rs` — what
//! was NOT covered was `run_host_attach` itself: the `.context(...)` calls
//! around `resolve_registry_root()` and `connect_to_session`, and the
//! `Registry::dir_at(&root.base)` path actually being the right one.
//!
//! This has to run the real, compiled binary rather than call `run_host`
//! in-process from a unit test in `src/main.rs`. `run_host_attach` calls
//! `oxutrm_host::resolve_registry_root()`, which reads `$OXUTRM_STATE_DIR`
//! from the process environment with no injection seam — and `src/main.rs`
//! is `#![forbid(unsafe_code)]`, while `std::env::set_var` is an `unsafe fn`
//! (soundness: mutating the environment races any other thread reading it).
//! So a unit test inside `main.rs` cannot control where `resolve_registry_root`
//! looks. Spawning the binary and setting `OXUTRM_STATE_DIR` on the *child's*
//! environment needs no unsafe code anywhere, and is the pattern
//! `tests/serve_exits.rs` already established for the same reason (there, to
//! reach `--serve` past its ssh-pipe fork).

use std::process::Command;

use oxutrm_host::{Registry, RegistryGuard, SessionMeta, now_unix};
use oxutrm_proto::TermSize;

/// A private directory under `/tmp`, not `std::env::temp_dir()`.
///
/// On macOS that is `/var/folders/<hash>/T/`, long enough that appending
/// `oxutrm/<32-char id>/sock` — which `--attach` would build if it ever got
/// past the unknown-id check — risks the 100-byte `sun_path` limit. This test
/// never binds a socket, but the registry directory it writes is exactly what
/// a real session's would be, so it is built the same safe way.
fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxu-host-attach-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("a private directory for this test");
    dir
}

fn a_session(id: &str) -> SessionMeta {
    SessionMeta {
        session_id: id.to_owned(),
        attach_id: 1,
        // The registry treats a dead pid's entry as stale and prunes it on
        // read, so it must belong to something alive for the whole test —
        // this process itself.
        pid: std::process::id(),
        created_unix: now_unix(),
        shell: "/bin/bash".to_owned(),
        size: TermSize { cols: 80, rows: 24 },
        detachable: true,
    }
}

/// A mistyped id must reach `connect_to_session`'s error — the one that lists
/// what actually IS there — through `run_host_attach`'s own error handling,
/// not fail earlier or later with something less useful.
#[test]
fn attaching_to_a_typo_through_the_real_binary_lists_what_is_actually_there() {
    let state = tempdir("typo");

    // Registered directly, the same way `oxutrm host --serve` would after
    // `daemonize`: no listener is bound, because `connect_to_session` checks
    // the registry before it ever touches a socket, and this test is about
    // that first check.
    let root_dir = Registry::dir_at(&state);
    let _guard =
        RegistryGuard::register_in(&root_dir, &a_session("realone")).expect("register realone");

    let output = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .arg("host")
        .arg("--attach")
        .arg("typo")
        .env("OXUTRM_STATE_DIR", &state)
        .output()
        .expect("running oxutrm host --attach");

    assert!(
        !output.status.success(),
        "attaching to an id that does not exist must fail"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("realone"),
        "the error does not name the session that IS there, so the user has \
         nothing to correct their typo against: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&state);
}

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

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use oxutrm_host::{Registry, RegistryGuard, SessionMeta, now_unix};
use oxutrm_proto::TermSize;

/// A private directory under `/tmp`, not `std::env::temp_dir()`.
///
/// On macOS `temp_dir()` is `/var/folders/<hash>/T/`, long enough that
/// appending `oxutrm/<32-char id>/sock` — which `--attach` builds once it is
/// past the unknown-id check — risks the 100-byte `sun_path` limit. The
/// docstring said this from the day it was written while the body called
/// `std::env::temp_dir()` anyway; `tempdir_in("/tmp")` is what makes it true,
/// and is the same pattern `crates/oxutrm-host/tests/attach.rs` uses.
///
/// A `TempDir` guard rather than a `PathBuf` and a manual `remove_dir_all`:
/// the manual form is skipped by a panicking assertion, which is exactly when
/// the directory is left behind, and these tests write registry entries
/// carrying this process's own pid.
fn tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("oxu-host-attach-")
        .tempdir_in("/tmp")
        .expect("a private directory under /tmp for this test")
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
    // Held for the whole test: dropping the guard removes the directory.
    let dir = tempdir();
    let state = dir.path();

    // Registered directly, the same way `oxutrm host --serve` would after
    // `daemonize`: no listener is bound, because `connect_to_session` checks
    // the registry before it ever touches a socket, and this test is about
    // that first check.
    let root_dir = Registry::dir_at(state);
    let _guard =
        RegistryGuard::register_in(&root_dir, &a_session("realone")).expect("register realone");

    let output = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .arg("host")
        .arg("--attach")
        .arg("typo")
        .env("OXUTRM_STATE_DIR", state)
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
}

/// How long the child is given to exit once the far end has hung up. The
/// failing case does not take longer — it takes for ever.
const EXIT_WITHIN: Duration = Duration::from_secs(5);

/// How long the test waits for the child to reach the socket at all.
const CONNECT_WITHIN: Duration = Duration::from_secs(20);

/// `--attach` must let go of this terminal when the session hangs up.
///
/// `run_host_attach` calls `runtime.shutdown_background()` rather than letting
/// the runtime drop, and the reason is not stylistic. `tokio::io::stdin()` is
/// served from the blocking pool; a thread parked in `read(2)` is not
/// reachable by `abort()`, and a normally-dropped runtime JOINS that pool. So
/// when the session end hangs up, the `select!` in `run_host_attach` finishes
/// and the process would then wait — for ever — on a read of this terminal
/// that only the user typing a byte could release. From the user's side that
/// is a command that will not come back.
///
/// This is the branch's one live correctness question and it was defended only
/// by prose. It cannot be observed from inside the process: the hang IS the
/// process failing to end, so the observation has to be made from outside one.
/// Hence the subprocess, and hence — exactly as in `tests/serve_exits.rs`, and
/// for exactly the same reason — **a stdin pipe the test holds open and never
/// writes to**. Closing it would hand that blocking read an EOF and release
/// it, which is precisely what hides the failure.
#[test]
fn attaching_lets_go_of_this_terminal_when_the_session_hangs_up() {
    let dir = tempdir();
    let state = dir.path();

    // A registered session with a socket that is really bound — this test is
    // about what happens AFTER a successful connect, so the connect has to
    // succeed. The pid on the record is this process's, so the registry does
    // not prune the entry out from under the child.
    let root_dir = Registry::dir_at(state);
    let guard = RegistryGuard::register_in(&root_dir, &a_session("hangup")).expect("register");
    let listener = std::os::unix::net::UnixListener::bind(guard.socket_path())
        .expect("binding the session socket");
    listener
        .set_nonblocking(true)
        .expect("a listener this test can give up on");

    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .arg("host")
        .arg("--attach")
        .arg("hangup")
        .env("OXUTRM_STATE_DIR", state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("running oxutrm host --attach");

    // Bound, not dropped. See the note above: this pipe staying open for the
    // whole test is the entire fixture.
    let _to_child = child.stdin.take().expect("the child's stdin");

    let deadline = Instant::now() + CONNECT_WITHIN;
    let conn = loop {
        match listener.accept() {
            Ok((s, _)) => break s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the child never reached the session socket"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => panic!("accepting the child's connection: {e}"),
        }
    };

    // The session hangs up. The child's socket-to-stdout relay reads end of
    // file and finishes; its stdin-to-socket relay is still parked on the pipe
    // above, and abandoning that is the whole question.
    drop(conn);

    let deadline = Instant::now() + EXIT_WITHIN;
    loop {
        match child.try_wait().expect("waiting on the child") {
            Some(_) => break,
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!(
                    "`oxutrm host --attach` did not exit within {EXIT_WITHIN:?} of \
                     the session hanging up: it is waiting for the blocking stdin \
                     read that only a keystroke could release"
                );
            }
        }
    }
}

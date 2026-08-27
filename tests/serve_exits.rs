//! `oxutrm host --serve` must let go of ssh when it is finished with it.
//!
//! This file does not inspect the code: it spawns the real binary the way ssh
//! does — a child whose 0, 1 and 2 are pipes — plays the client's half of the
//! handshake, and then watches for the grandchild's end of the pipe to close.
//!
//! # Why the *grandchild*, and why stdout
//!
//! `--serve` double forks. The process ssh waits on `_exit(0)`s at once, so
//! `child.wait()` here returns immediately and proves nothing at all. The one
//! thing observable from outside is the pipe: the grandchild inherited it, so
//! stdout reaching EOF is the grandchild being gone, and nothing else is.
//!
//! # Why stdin is deliberately left OPEN
//!
//! Closing it would hide the failure this file exists to catch. The host reads
//! signalling on descriptor 0 while the ladder runs, and closing our end hands
//! that read an EOF, which releases anything parked on it. A real ssh session
//! does not close its end just because the ladder failed — the user is still
//! sitting there — so neither does this test.
//!
//! Measured, not theorised: with `tokio::io::stdin()`'s blocking read
//! outstanding, the process sat in `futex_do_wait` inside runtime shutdown
//! while a worker thread stayed in `anon_pipe_read`, indefinitely. From the
//! user's side that is an `ssh` that never returns.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use oxutrm_proto::{Candidate, ClientSpki, NatType, PROTO_VERSION, Signal, TermSize, TerminalCaps};

/// Generous on purpose. The host gathers candidates against real STUN servers
/// with `NetConfig`'s three-second budget, and this test must not fail on a
/// slow resolver — it is about whether the process ends, not how fast.
const GIVE_UP: Duration = Duration::from_secs(45);

fn a_client_hello(candidates: Vec<Candidate>) -> Signal {
    Signal::ClientHello {
        proto: PROTO_VERSION,
        cert_spki_sha256: ClientSpki::new([3u8; 32]),
        candidates,
        nat_type: NatType::Symmetric,
        caps: TerminalCaps {
            truecolor: true,
            colors: 16_777_216,
            bracketed_paste: true,
            mouse_sgr: true,
            osc52: true,
            term_name: "xterm-256color".to_owned(),
        },
        size: TermSize {
            cols: 100,
            rows: 30,
        },
    }
}

/// A client that offers no candidate at all cannot be reached on any rung, so
/// every one of the five is accounted for and the ladder gives up. That is the
/// shortest honest route to "the session is over" — and the route on which a
/// host that never lets go of ssh leaves the user's terminal hanging.
#[test]
fn a_session_whose_ladder_fails_lets_go_of_the_ssh_pipes() {
    let state = tempdir("serve-exits");
    let mut child = Command::new(env!("CARGO_BIN_EXE_oxutrm"))
        .arg("host")
        .arg("--serve")
        .env("OXUTRM_STATE_DIR", &state)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawning the host");

    // Kept alive for the whole test. Dropping it closes ssh's end of the pipe,
    // which is exactly the thing that would mask the failure.
    let mut to_host = child.stdin.take().expect("the host's stdin");
    let from_host = child.stdout.take().expect("the host's stdout");
    let mut stderr = child.stderr.take().expect("the host's stderr");
    // Drained on a thread of its own: the grandchild holds this pipe too, and
    // reading it to EOF on the main thread would be the same wait this test is
    // trying to bound.
    std::thread::spawn(move || {
        let mut sink = Vec::new();
        let _ = stderr.read_to_end(&mut sink);
    });

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(from_host);
        let mut lines = Vec::new();
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                // EOF: the grandchild is gone and let go of the pipe.
                Ok(0) => break,
                Ok(_) => {
                    let trimmed = line.trim().to_owned();
                    if trimmed.starts_with('{') {
                        let _ = tx.send(trimmed.clone());
                    }
                    lines.push(trimmed);
                }
                Err(_) => break,
            }
        }
        let _ = tx.send("<eof>".to_owned());
    });

    let hello = rx
        .recv_timeout(GIVE_UP)
        .expect("the host must offer a HostHello");
    assert!(
        hello.contains("HostHello"),
        "the first message was not the host's offer: {hello}"
    );

    let mut encoded = Vec::new();
    oxutrm_proto::write_signal(&mut encoded, &a_client_hello(Vec::new()))
        .expect("encoding the client's hello");
    to_host.write_all(&encoded).expect("sending our hello");
    to_host.flush().expect("flushing our hello");

    let failure = rx
        .recv_timeout(GIVE_UP)
        .expect("the host must say why no rung worked");
    assert!(
        failure.contains("Failed"),
        "the host did not report the ladder's verdict: {failure}"
    );

    let end = rx
        .recv_timeout(GIVE_UP)
        .expect("the host never let go of the ssh pipes after giving up");
    assert_eq!(
        end, "<eof>",
        "expected the pipe to close after the failure, got {end}"
    );

    // Only now, and only so a leaked process cannot outlive a failing run.
    let _ = child.kill();
    let _ = child.wait();
    drop(to_host);
    let _ = std::fs::remove_dir_all(&state);
}

fn tempdir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "oxutrm-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("a private directory for this test");
    dir
}

//! The session's Unix socket: bound once the ssh handshake and (where the
//! rung allows it) the sever are done, and accepted on for the rest of the
//! session's life.
//!
//! `Registry::socket_path` has been computed, registered and advertised since
//! the registry existed, and nothing has ever bound it — a second
//! `oxutrm host --attach` had a path to dial and nothing listening on it.
//! This module is what binds it.
//!
//! The exchange run here is exactly [`crate::attach_exchange::run_attach_exchange`],
//! the same function the first connect runs over ssh's pipes. Spec 5.1's
//! binding rule is that reattachment must not be a second code path, and this
//! module does not add one — it only supplies a different pair of pipes.

use std::time::Duration;

use oxutrm_host::registry::SessionMeta;
use oxutrm_net::NetConfig;

use crate::attach_exchange::Attached;

/// How long one attach attempt may hold the accept loop.
///
/// The exchange runs inline, one at a time, on purpose — see the comment in
/// the loop. Nothing inside it bounds the wait for the client's hello:
/// `read_signal_async` waits for a `ClientHello` that a peer which connects
/// and then stays alive and silent will never send, and the session goes on
/// advertising a socket it will never answer on again. The only remedy was
/// killing the session.
///
/// Ninety seconds, and generous on purpose. Every budget inside the exchange
/// is smaller and each is the right one for its own step —
/// [`crate::accept::ACCEPT_TIMEOUT`] and `oxutrm_net::CONNECT_TIMEOUT` are
/// thirty seconds each, and the candidate gather has three — so this is not a
/// second opinion about any of them. It is the outer bound on a whole attempt,
/// sized to sit clear of their sum so that a real attach over a slow link is
/// never the thing it cuts off.
pub(crate) const ATTACH_TIMEOUT: Duration = Duration::from_secs(90);

/// Serve second attaches for the life of the session.
///
/// Never returns on its own: it is spawned, and dropped when the session ends.
///
/// `attach_timeout` is a parameter rather than a constant read here so the
/// guard for it can be exercised in a second instead of in ninety. It is not a
/// knob: the one production call site passes [`ATTACH_TIMEOUT`], and there is
/// no flag, environment variable or configuration field behind it.
pub(crate) async fn serve_attaches(
    listener: tokio::net::UnixListener,
    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
    cfg: NetConfig,
    attach_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Attached>,
) {
    loop {
        let stream = match listener.accept().await {
            Ok((s, _)) => s,
            // A failed accept is not a reason to stop answering the door.
            Err(_) => continue,
        };
        let (r, w) = stream.into_split();

        // One attach at a time, deliberately. Two concurrent exchanges would
        // both bump the generation and race to hand the session a link, and
        // the loser's shell would be a live connection nobody owns.
        //
        // Which is exactly why the attempt is bounded: serial and unbounded
        // means one stalled peer closes the door for the life of the session.
        let mut m = meta.lock().await;
        let outcome = tokio::time::timeout(
            attach_timeout,
            crate::attach_exchange::run_attach_exchange(
                tokio::io::BufReader::new(r),
                w,
                &mut m,
                &cfg,
            ),
        )
        .await;
        let attached = match outcome {
            Ok(Ok(a)) => a,
            // The attempt failed. The running session is untouched: it never
            // heard about this, which is the whole point of the arm firing
            // only on a completed link.
            Ok(Err(_)) => continue,
            // A timed-out attempt is a failed attempt and nothing more: the
            // registry is not written, the session never hears about it, and
            // the door is open again on the next lap. Said aloud on stderr,
            // because "the socket stopped answering" is otherwise indis-
            // tinguishable from a wedged session.
            Err(_) => {
                eprintln!(
                    "oxutrm: an attach attempt got no further than {}s and was \
                     given up on; the session is still accepting",
                    attach_timeout.as_secs()
                );
                continue;
            }
        };
        // `update`'s own doc: "after every attach, because attach_id moves".
        let _ = guard.update(&m);
        drop(m);

        if tx.send(attached).await.is_err() {
            // The session is gone; so is the reason to keep listening.
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session record as R4 finds it, before any attach. Mirrors
    /// `attach_exchange::tests::fresh_meta` — duplicated rather than shared,
    /// because both are `#[cfg(test)]`-private to their own module and
    /// neither is worth a third module just to hold one struct literal.
    fn fresh_meta(session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_owned(),
            attach_id: 0,
            pid: std::process::id(),
            created_unix: 0,
            shell: "/bin/sh".to_owned(),
            size: oxutrm_proto::TermSize { cols: 80, rows: 24 },
            detachable: false,
        }
    }

    /// A configuration that reaches no network at all.
    ///
    /// Mirrors `attach_exchange::tests::stun_free` and exists for the same two
    /// reasons. Public STUN servers are a list of hopes, not of requirements
    /// (`stun_discover`'s own words), so an empty list is a supported
    /// configuration and not a test firing probes at the public internet on
    /// every `make check` — `run_attach_exchange` calls `stun_discover` at R5,
    /// before the hello exchange, so these tests were doing exactly that.
    ///
    /// And it is what makes them deterministic. With a three-second gather
    /// budget in front of the code under test, "the exchange has finished" and
    /// "the test looked" are not reliably ordered, which is how a guard below
    /// once passed under an injected bug: it read `meta.json` while the
    /// exchange was still probing. Without STUN the attempt fails at once on
    /// the closed write half.
    fn stun_free() -> NetConfig {
        NetConfig {
            stun_servers: vec![],
            enable_port_mapping: false,
            enable_birthday: false,
            ..Default::default()
        }
    }

    /// Long enough that a live loop always answers, short enough that a dead
    /// one does not hold the suite up. Nothing real waits on it: with
    /// [`stun_free`] the exchange reaches R6 in microseconds.
    const ANSWER_WITHIN: Duration = Duration::from_secs(5);

    /// How long "nothing arrived" is observed for.
    const QUIET: Duration = Duration::from_millis(500);

    /// Connect, and read what the accept loop says back.
    ///
    /// **This, and not a bare `connect`, is how "the door is still open" is
    /// observed.** `UnixStream::connect` succeeds off the kernel's listen
    /// backlog whether or not anything is still calling `accept`, so it cannot
    /// tell a live loop from a dead one — a socket whose owner has stopped
    /// accepting takes connections just the same, until the backlog fills. A
    /// `HostHello` cannot be produced that way: only a loop that went round,
    /// accepted, and ran the exchange as far as R6 writes one.
    ///
    /// The write half is bound rather than dropped, so the connection is not
    /// shut down before the loop has answered on it.
    async fn hello_off(sock: &std::path::Path) -> oxutrm_proto::Signal {
        let stream = tokio::net::UnixStream::connect(sock)
            .await
            .expect("connecting to the session socket");
        let (r, _w) = stream.into_split();
        let mut r = tokio::io::BufReader::new(r);
        tokio::time::timeout(
            ANSWER_WITHIN,
            oxutrm_host::signalling::read_signal_async(&mut r),
        )
        .await
        .expect("the accept loop never answered; it is not accepting any more")
        .expect("the accept loop answered with something that is not a Signal")
    }

    /// A failed attach attempt must not disturb the running session.
    ///
    /// The arm only ever fires on a COMPLETED link, so a client that connects
    /// and hangs up costs the session nothing — no message on the channel, and
    /// the listener still accepting afterwards.
    ///
    /// Both halves of that used to be unobservable. `try_recv` ran microseconds
    /// after the drop, before the loop could have finished handling it, so it
    /// read "empty" whatever `serve_attaches` did; and the second `connect`
    /// proved only that the kernel has a backlog. Both are completed
    /// observations now: a wait that must elapse, and an answer that must
    /// arrive.
    #[tokio::test]
    async fn an_abandoned_attempt_sends_nothing_and_leaves_the_door_open() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("door");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            guard,
            std::sync::Arc::clone(&meta),
            stun_free(),
            ATTACH_TIMEOUT,
            tx,
        ));

        // Connect and hang up without saying anything.
        drop(
            tokio::net::UnixStream::connect(&sock)
                .await
                .expect("connect"),
        );

        // Nothing reaches the session, and the loop has had [`QUIET`] to be
        // wrong about that. An elapsed wait is the passing case; the two ways
        // of not elapsing are different defects and say so separately.
        match tokio::time::timeout(QUIET, rx.recv()).await {
            Err(_) => {}
            Ok(Some(_)) => {
                panic!("an attempt that never completed handed a link to the session")
            }
            Ok(None) => panic!(
                "the accept loop dropped its sender: it stopped serving on a \
                 failed attempt instead of going round again"
            ),
        }

        // And the door is still open. See [`hello_off`] for why a second
        // `connect` is not that observation.
        let hello = hello_off(&sock).await;
        assert!(
            matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
            "the loop answered the second connection with {hello:?} instead of \
             an offer"
        );
        task.abort();
    }

    /// A peer that connects and then says nothing must not close the door for
    /// the life of the session.
    ///
    /// The exchange runs inline in the accept loop, one at a time, by design.
    /// Nothing inside it bounded the wait for a `ClientHello`, so a peer that
    /// stayed alive and silent parked the loop for ever: the session went on
    /// advertising a socket it would never answer on again, silently, and the
    /// only remedy was killing the session.
    ///
    /// A short timeout is passed in rather than [`ATTACH_TIMEOUT`] so this
    /// costs a fifth of a second instead of ninety. The code it exercises is
    /// the same.
    #[tokio::test]
    async fn a_stalled_attach_gives_up_and_the_door_opens_again() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("stall");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            std::sync::Arc::clone(&guard),
            std::sync::Arc::clone(&meta),
            stun_free(),
            Duration::from_millis(200),
            tx,
        ));

        // Alive and silent, which is the case that has no other rescue: the
        // connection is HELD, so the exchange's read of the client's hello
        // never returns and nothing about the peer being gone can free the
        // loop.
        let stalled = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connecting to the session socket");

        // The loop has to come back to `accept` on its own clock.
        let hello = hello_off(&sock).await;
        assert!(
            matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
            "the loop answered the second connection with {hello:?} instead of \
             an offer"
        );

        // A timed-out attempt is a failed attempt, and nothing more.
        let on_disk: SessionMeta =
            serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json is there"))
                .expect("meta.json parses");
        assert_eq!(
            on_disk.attach_id, 0,
            "an attempt that timed out still advertised its generation"
        );

        drop(stalled);
        task.abort();
    }

    /// A failed attempt must not advertise a generation that never served.
    ///
    /// `begin_attach` bumps `attach_id` at R4, before the exchange can fail,
    /// so the in-memory record moves even when nothing comes of it. The
    /// registry is only written on success — otherwise `--list` and a
    /// reconnecting client would name a generation that no link ever ran as.
    #[tokio::test]
    async fn a_failed_attempt_does_not_write_its_generation_to_the_registry() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("gen");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            std::sync::Arc::clone(&guard),
            std::sync::Arc::clone(&meta),
            stun_free(),
            ATTACH_TIMEOUT,
            tx,
        ));

        // Connect and hang up: R4 runs, R7 fails.
        drop(
            tokio::net::UnixStream::connect(&sock)
                .await
                .expect("connect"),
        );

        // Ordered by the loop itself, not by a sleep. The exchange is serial —
        // one attach at a time is this loop's whole design — so a second
        // connection being ANSWERED is proof that the first attempt is over
        // and has written whatever it was going to write. The fixed 50 ms
        // sleep this replaces was not proof of anything: with a three-second
        // gather budget in front of it, it is how this guard once passed under
        // an injected bug, reading `meta.json` while the exchange was still
        // probing.
        let hello = hello_off(&sock).await;
        assert!(
            matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
            "the first attempt never finished, so nothing below is ordered \
             after it: {hello:?}"
        );

        let on_disk: SessionMeta =
            serde_json::from_slice(&std::fs::read(guard.meta_path()).expect("meta.json is there"))
                .expect("meta.json parses");
        assert_eq!(
            on_disk.attach_id, 0,
            "the registry advertises generation {} but no link ever ran as it; \
             a client told to expect it would be waiting for a host that is \
             not there",
            on_disk.attach_id
        );
        task.abort();
    }

    /// The listener's `Arc` clone of the guard has to be gone before the
    /// session drops its own, or the session directory outlives the session.
    ///
    /// This is the shape `serve()` (`src/serve.rs`) ends on, and it is the
    /// shape rather than that call site that can be reached from here.
    /// `abort()` only SCHEDULES cancellation: without the await, the spawned
    /// future — and its clone of the guard — is still alive when `drop(guard)`
    /// runs, `RegistryGuard::drop` decrements two to one instead of one to
    /// zero, `remove_dir_all` never runs, and the session leaves an entry
    /// behind for `--list` to prune. Worse, `run_host_serve`'s comment cites
    /// that cleanup as the reason it is allowed to `shutdown_background()` and
    /// walk away.
    ///
    /// Asserted on the directory — the side effect — rather than on a strong
    /// count, which is the mechanism.
    #[tokio::test]
    async fn awaiting_the_aborted_listener_is_what_lets_the_guard_clean_up() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("clean");
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(dir.path(), &start).expect("register"),
        );
        let session_dir = guard.dir().to_path_buf();
        let meta = std::sync::Arc::new(tokio::sync::Mutex::new(start));

        let task = tokio::spawn(serve_attaches(
            listener,
            std::sync::Arc::clone(&guard),
            meta,
            stun_free(),
            ATTACH_TIMEOUT,
            tx,
        ));
        assert!(session_dir.exists(), "the fixture registered nothing");

        // Exactly what `serve()` does when the shell exits.
        task.abort();
        let _ = task.await;
        drop(guard);

        assert!(
            !session_dir.exists(),
            "the session ended and {} is still there: the listener task was \
             still holding a clone of the guard, so its Drop never ran",
            session_dir.display()
        );
    }
}

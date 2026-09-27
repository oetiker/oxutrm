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
//!
//! The socket is one of two ways in. The other is the door: a standby
//! requested on a live link's control stream ([`crate::control`]) is handed
//! here as a pair of pipes, and runs the same exchange in the same loop.

use std::time::Duration;

use oxutrm_host::registry::SessionMeta;
use oxutrm_net::NetConfig;

use crate::attach_exchange::Attached;
use crate::control::{DoorRequest, Role};

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
/// `doors` brings standby requests from the control streams; `door_tx` is its
/// sender, handed to the control server started on every link this hands the
/// session, so that link can carry the next request.
///
/// `attach_timeout` is a parameter rather than a constant read here so the
/// guard for it can be exercised in a second instead of in ninety. It is not a
/// knob: the one production call site passes [`ATTACH_TIMEOUT`], and there is
/// no flag, environment variable or configuration field behind it.
#[expect(
    clippy::too_many_arguments,
    reason = "each is a distinct thing the loop is handed once; a struct \
              around them would only be a second name for this signature"
)]
pub(crate) async fn serve_attaches(
    listener: tokio::net::UnixListener,
    guard: std::sync::Arc<oxutrm_host::RegistryGuard>,
    meta: std::sync::Arc<tokio::sync::Mutex<SessionMeta>>,
    cfg: NetConfig,
    attach_timeout: Duration,
    mut doors: tokio::sync::mpsc::Receiver<DoorRequest>,
    door_tx: tokio::sync::mpsc::Sender<DoorRequest>,
    tx: tokio::sync::mpsc::Sender<Attached>,
) {
    // A socket attach that pre-empted a standby's exchange, served next.
    let mut preempting: Option<tokio::net::UnixStream> = None;
    loop {
        // Two ways in, one door. The Unix socket brings ssh-relayed attaches
        // (primary: newest attach wins); a control stream brings a standby.
        // Both are served here, serially, under the one meta lock, for the
        // reason the comment below gives: two concurrent exchanges would both
        // bump the generation and race to hand the session a link.
        //
        // `doors` never closes while this runs, because `door_tx` is held
        // right here; the `Some` pattern is only there to say so.
        let (reader, writer, role): Pipes = match preempting.take() {
            Some(s) => primary_pipes(s),
            None => tokio::select! {
                r = listener.accept() => match r {
                    Ok((s, _)) => primary_pipes(s),
                    // A failed accept is not a reason to stop answering the door.
                    Err(_) => continue,
                },
                Some(d) = doors.recv() => (d.reader, d.writer, d.role),
            },
        };

        // One attach at a time, deliberately. Two concurrent exchanges would
        // both bump the generation and race to hand the session a link, and
        // the loser's shell would be a live connection nobody owns.
        //
        // Which is exactly why the attempt is bounded: serial and unbounded
        // means one stalled peer closes the door for the life of the session.
        let mut m = meta.lock().await;
        let exchange = tokio::time::timeout(
            attach_timeout,
            crate::attach_exchange::run_attach_exchange(reader, writer, &mut m, &cfg),
        );
        // A standby gives way to a primary. The standby is an insurance
        // policy on a link that still works; a socket attach is the client's
        // ssh rebuild, or a user taking the session over, and either one is
        // the thing that matters now. A standby whose control stream went
        // silent would otherwise hold the door for the whole of
        // `attach_timeout`, longer than a rebuild is prepared to wait. The
        // dropped attempt is a failed attempt like any other: the registry is
        // not written and the session never hears of it. A primary is never
        // pre-empted: newest attach wins among primaries by running in turn.
        let outcome = if role == Role::Standby {
            tokio::select! {
                o = exchange => o,
                Ok((s, _)) = listener.accept() => {
                    preempting = Some(s);
                    continue;
                }
            }
        } else {
            exchange.await
        };
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

        // The exchange cannot know why it was run; the way in does.
        let mut attached = attached;
        attached.role = role;
        let conn = attached.link.sink.connection().clone();

        if tx.send(attached).await.is_err() {
            // The session is gone; so is the reason to keep listening.
            return;
        }
        // Every link the session is handed can carry the next standby
        // request and answer probes. The server dies with the connection.
        // Started only once the session has the link, so a link nobody took
        // is not held open by a server of its own.
        crate::control::serve_control(conn, door_tx.clone());
    }
}

/// An attach's two pipes, and what the attach is for.
type Pipes = (
    Box<dyn tokio::io::AsyncBufRead + Unpin + Send>,
    Box<dyn tokio::io::AsyncWrite + Unpin + Send>,
    Role,
);

/// The pipes of an attach that came in over the Unix socket.
fn primary_pipes(s: tokio::net::UnixStream) -> Pipes {
    let (r, w) = s.into_split();
    (
        Box::new(tokio::io::BufReader::new(r)),
        Box::new(w),
        Role::Primary,
    )
}

/// Stop answering the door once the shell has exited.
///
/// Both halves are needed. [`serve_attaches`] never returns by itself, so
/// without the `abort()` the await is for ever: the daemon outlives its shell,
/// its entry stays on `--list`, and the next reattach completes an exchange
/// into a session with nothing left to send.
///
/// And `abort()` only SCHEDULES cancellation: the future — and with it the
/// listener's `Arc` clone of the guard — is dropped by the runtime at some
/// later point on some worker. Without the await, the caller's `drop(guard)`
/// decrements from two to one, the guard's `Drop` never runs, and whether the
/// abandoned task's destructor beats `shutdown_background()` is a race.
/// Awaiting an aborted handle returns as soon as the runtime has dropped the
/// future, and `serve_attaches` touches no blocking pool, so this is prompt.
pub(crate) async fn close_the_door(task: tokio::task::JoinHandle<()>) {
    task.abort();
    let _ = task.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attach_exchange::fixtures::{fresh_meta, stun_free};

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
        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
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
            door_rx,
            door_tx,
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
        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
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
            door_rx,
            door_tx,
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
        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
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
            door_rx,
            door_tx,
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

    /// A standby comes in through the door, runs the very same exchange, and
    /// comes out marked as a standby — while an attach over the Unix socket
    /// comes out as a primary.
    ///
    /// The role is what the session acts on: a standby is parked, a primary
    /// takes over. A standby that came out as a primary would displace the
    /// live link and close it as taken over, which ends the client. So both
    /// exchanges are driven to completion by the real client half,
    /// `connect::establish`, and the role is read off the `Attached` the
    /// session would receive — the socket case first, so "every attach says
    /// Standby" cannot pass either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_door_request_runs_the_same_exchange_and_carries_its_role() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("roles");
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
            door_rx,
            door_tx.clone(),
            tx,
        ));
        let size = oxutrm_proto::TermSize { cols: 80, rows: 24 };
        let within = Duration::from_secs(10);

        // Over the Unix socket: a primary.
        let stream = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connecting to the session socket");
        let (cr, cw) = stream.into_split();
        let _primary = tokio::time::timeout(
            within,
            crate::connect::establish(tokio::io::BufReader::new(cr), cw, size, &stun_free(), None),
        )
        .await
        .expect("the client exchange over the socket must not hang")
        .expect("the client exchange over the socket completes");
        let attached = tokio::time::timeout(within, rx.recv())
            .await
            .expect("the socket attach never reached the session")
            .expect("the listener dropped its sender");
        assert_eq!(
            attached.role,
            Role::Primary,
            "an ssh-relayed attach must take over, not park"
        );

        // Through the door: the same exchange over a different pipe.
        let (client_side, host_side) = tokio::io::duplex(64 * 1024);
        let (hr, hw) = tokio::io::split(host_side);
        door_tx
            .send(DoorRequest {
                reader: Box::new(tokio::io::BufReader::new(hr)),
                writer: Box::new(hw),
                role: Role::Standby,
            })
            .await
            .unwrap();
        let (cr, cw) = tokio::io::split(client_side);
        let _standby = tokio::time::timeout(
            within,
            crate::connect::establish(tokio::io::BufReader::new(cr), cw, size, &stun_free(), None),
        )
        .await
        .expect("the client exchange through the door must not hang")
        .expect("the client exchange through the door completes");
        let attached = tokio::time::timeout(within, rx.recv())
            .await
            .expect("the door attach never reached the session")
            .expect("the listener dropped its sender");
        assert_eq!(
            attached.role,
            Role::Standby,
            "a standby came out of the door as a takeover: it would displace \
             the live link and end the client"
        );
        task.abort();
    }

    /// A standby's exchange gives way to an attach over the socket.
    ///
    /// The door is serial, and a standby whose control stream went silent
    /// never errors: without pre-emption it would hold the door for the whole
    /// of [`ATTACH_TIMEOUT`], ninety seconds, while the client's ssh rebuild —
    /// the attach that matters — queued behind it. The real timeout is passed
    /// on purpose, so nothing but pre-emption can free the loop in time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_socket_attach_preempts_a_stalled_standby() {
        let dir = tempfile::tempdir().expect("a temp dir");
        let sock = dir.path().join("sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
        let start = fresh_meta("preempt");
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
            door_rx,
            door_tx.clone(),
            tx,
        ));
        let within = Duration::from_secs(10);

        // A standby whose peer hears the offer and then says nothing, held
        // open so nothing about the peer being gone can end the attempt.
        let (client_side, host_side) = tokio::io::duplex(64 * 1024);
        let (hr, hw) = tokio::io::split(host_side);
        door_tx
            .send(DoorRequest {
                reader: Box::new(tokio::io::BufReader::new(hr)),
                writer: Box::new(hw),
                role: Role::Standby,
            })
            .await
            .unwrap();
        let (cr, _cw) = tokio::io::split(client_side);
        let mut cr = tokio::io::BufReader::new(cr);
        // Before: the standby's exchange is under way, holding the door.
        let offer =
            tokio::time::timeout(within, oxutrm_host::signalling::read_signal_async(&mut cr))
                .await
                .expect("the standby's exchange never started")
                .expect("the standby's exchange sent something that is not a Signal");
        assert!(
            matches!(offer, oxutrm_proto::Signal::HostHello { .. }),
            "{offer:?}"
        );

        // After: a socket attach gets through anyway, and as a primary.
        let stream = tokio::net::UnixStream::connect(&sock)
            .await
            .expect("connecting to the session socket");
        let (sr, sw) = stream.into_split();
        tokio::time::timeout(
            within,
            crate::connect::establish(
                tokio::io::BufReader::new(sr),
                sw,
                oxutrm_proto::TermSize { cols: 80, rows: 24 },
                &stun_free(),
                None,
            ),
        )
        .await
        .expect(
            "the socket attach waited behind a stalled standby: the rebuild \
             it stands for would queue for the whole attach timeout",
        )
        .expect("the socket attach completes");
        let attached = tokio::time::timeout(within, rx.recv())
            .await
            .expect("the socket attach never reached the session")
            .expect("the listener dropped its sender");
        assert_eq!(attached.role, Role::Primary);
        task.abort();
    }

    /// The listener's `Arc` clone of the guard has to be gone before the
    /// session drops its own, or the session directory outlives the session.
    ///
    /// `serve()` (`src/serve.rs`) ends on [`close_the_door`], and so does
    /// this test; the listener must also be gone at all, which the timeout
    /// asserts. `abort()` only SCHEDULES cancellation: without the await, the spawned
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
        let (door_tx, door_rx) = tokio::sync::mpsc::channel(1);
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
            door_rx,
            door_tx,
            tx,
        ));
        assert!(session_dir.exists(), "the fixture registered nothing");

        // Exactly what `serve()` does when the shell exits -- the same call,
        // not a copy of it. A copy is how `serve()` lost its `abort()` while
        // this test, aborting on its own, stayed green: the listener never
        // returns by itself, so the session's daemon outlived its shell,
        // stayed on `--list`, and handed the next reattach a dead link.
        tokio::time::timeout(Duration::from_secs(5), close_the_door(task))
            .await
            .expect(
                "the listener was still running after the shell exited: the \
                 daemon outlives its session and keeps accepting attaches",
            );
        drop(guard);

        assert!(
            !session_dir.exists(),
            "the session ended and {} is still there: the listener task was \
             still holding a clone of the guard, so its Drop never ran",
            session_dir.display()
        );
    }
}

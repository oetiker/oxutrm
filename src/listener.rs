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

/// Accept on the session's Unix socket for the life of the session, and
/// hand every connection to the door ([`crate::door::serve`]) on a task of
/// its own. Never returns on its own: it is spawned, and stopped with
/// [`close_the_door`].
///
/// A task per connection is the point (switcher spec §2.2): a connection
/// that stalls before saying what it wants holds its own task, never the
/// accept, and a `Myself` is answered while an attach is under way.
pub(crate) async fn accept_doors(
    listener: tokio::net::UnixListener,
    door: std::sync::Arc<crate::door::Door>,
) {
    loop {
        match listener.accept().await {
            Ok((s, _)) => {
                let (r, w) = s.into_split();
                tokio::spawn(crate::door::serve(
                    std::sync::Arc::clone(&door),
                    crate::door::Via::Socket,
                    tokio::io::BufReader::new(r),
                    w,
                ));
            }
            // A failed accept is not a reason to stop answering the door.
            Err(_) => tokio::task::yield_now().await,
        }
    }
}

/// Run attaches for the life of the session, one at a time.
///
/// Never returns on its own: it is spawned, and dropped when the session ends.
///
/// `inbox` brings what the doors were asked to attach: primaries -- an
/// ssh-relayed `--attach`, a client switching here -- and standbys from a
/// client's control stream. Each completed link goes to the session on `tx`,
/// and gets a control server of its own through `door`, so it can carry the
/// next request.
///
/// `attach_timeout` is a parameter rather than a constant read here so the
/// guard for it can be exercised in a second instead of in ninety. It is not a
/// knob: the one production call site passes [`ATTACH_TIMEOUT`], and there is
/// no flag, environment variable or configuration field behind it.
pub(crate) async fn serve_attaches(
    door: std::sync::Arc<crate::door::Door>,
    cfg: NetConfig,
    attach_timeout: Duration,
    mut inbox: crate::door::AttachInbox,
    tx: tokio::sync::mpsc::Sender<Attached>,
) {
    // The exchange's working record: `begin_attach` moves its generation at
    // R4, before the exchange can fail. Seeded from the door's record, which
    // is what the session's first attach already wrote.
    let mut meta: SessionMeta = door.meta();
    // A primary that pre-empted a standby's exchange, served next.
    let mut preempting: Option<DoorRequest> = None;
    loop {
        // A primary first: newest attach wins among primaries by running in
        // turn, and a standby is only an insurance policy.
        let req = match preempting.take() {
            Some(r) => r,
            None => tokio::select! {
                biased;
                Some(r) = inbox.primary.recv() => r,
                Some(r) = inbox.standby.recv() => r,
                else => return,
            },
        };
        let role = req.role;

        // One attach at a time, deliberately. Two concurrent exchanges would
        // both bump the generation and race to hand the session a link, and
        // the loser's shell would be a live connection nobody owns.
        //
        // Which is exactly why the attempt is bounded: serial and unbounded
        // means one stalled peer closes the door for the life of the session.
        let exchange = tokio::time::timeout(
            attach_timeout,
            crate::attach_exchange::run_attach_exchange(req.reader, req.writer, &mut meta, &cfg),
        );
        // A standby gives way to a primary. The standby is an insurance
        // policy on a link that still works; a primary is the client's ssh
        // rebuild, a user taking the session over, or a client switching
        // here, and any of them is the thing that matters now. A standby
        // whose control stream went silent would otherwise hold the loop for
        // the whole of `attach_timeout`, longer than a rebuild is prepared to
        // wait. The dropped attempt is a failed attempt like any other: the
        // registry is not written and the session never hears of it. A
        // primary is never pre-empted.
        let outcome = if role == Role::Standby {
            tokio::select! {
                o = exchange => o,
                Some(r) = inbox.primary.recv() => {
                    preempting = Some(r);
                    continue;
                }
            }
        } else {
            exchange.await
        };
        let mut attached = match outcome {
            Ok(Ok(a)) => a,
            // The attempt failed. The running session is untouched: it never
            // heard about this, which is the whole point of the arm firing
            // only on a completed link.
            Ok(Err(_)) => continue,
            // A timed-out attempt is a failed attempt and nothing more: the
            // registry is not written, the session never hears about it, and
            // the loop takes the next request. Said aloud on stderr, because
            // "the socket stopped answering" is otherwise indistinguishable
            // from a wedged session.
            Err(_) => {
                eprintln!(
                    "oxutrm: an attach attempt got no further than {}s and was \
                     given up on; the session is still accepting",
                    attach_timeout.as_secs()
                );
                continue;
            }
        };
        door.record_attach(&meta);

        // The exchange cannot know why it was run; the way in does.
        attached.role = role;
        let conn = attached.link.sink.connection().clone();

        if tx.send(attached).await.is_err() {
            // The session is gone; so is the reason to keep listening.
            return;
        }
        // Every link the session is handed can carry the next request and
        // answer probes. The server dies with the connection. Started only
        // once the session has the link, so a link nobody took is not held
        // open by a server of its own.
        crate::control::serve_control(conn, std::sync::Arc::clone(&door));
    }
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
    use crate::door::{AttachQueue, Door, attach_queue};
    use oxutrm_host::signalling::write_line_async;
    use oxutrm_proto::{Open, Request};

    /// Long enough that a live loop always answers, short enough that a dead
    /// one does not hold the suite up. Nothing real waits on it: with
    /// [`stun_free`] the exchange reaches R6 in microseconds.
    const ANSWER_WITHIN: Duration = Duration::from_secs(5);

    /// How long "nothing arrived" is observed for.
    const QUIET: Duration = Duration::from_millis(500);

    /// A session's doors as `serve.rs` opens them: registered as `meta` in a
    /// temp registry, its socket accepted on, its attach loop running.
    struct Doors {
        _dir: tempfile::TempDir,
        registry: std::path::PathBuf,
        sock: std::path::PathBuf,
        door: Option<std::sync::Arc<Door>>,
        queue: AttachQueue,
        guard_dir: std::path::PathBuf,
        rx: tokio::sync::mpsc::Receiver<Attached>,
        accept: tokio::task::JoinHandle<()>,
        attaches: tokio::task::JoinHandle<()>,
    }

    fn doors(meta: SessionMeta, attach_timeout: Duration) -> Doors {
        let dir = tempfile::tempdir().expect("a temp dir");
        let registry = dir.path().to_path_buf();
        let guard = std::sync::Arc::new(
            oxutrm_host::RegistryGuard::register_in(&registry, &meta).expect("register"),
        );
        let sock = guard.socket_path();
        let guard_dir = guard.dir().to_path_buf();
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");
        let (queue, inbox) = attach_queue();
        let door = Door::new(
            registry.clone(),
            meta,
            Some(guard),
            crate::host_session::Presence::default(),
            Some(queue.clone()),
        );
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let accept = tokio::spawn(accept_doors(listener, std::sync::Arc::clone(&door)));
        let attaches = tokio::spawn(serve_attaches(
            std::sync::Arc::clone(&door),
            stun_free(),
            attach_timeout,
            inbox,
            tx,
        ));
        Doors {
            _dir: dir,
            registry,
            sock,
            door: Some(door),
            queue,
            guard_dir,
            rx,
            accept,
            attaches,
        }
    }

    /// Connect, ask for a primary attach, and return the stream's halves.
    async fn open_attach(
        sock: &std::path::Path,
    ) -> (
        tokio::io::BufReader<tokio::net::unix::OwnedReadHalf>,
        tokio::net::unix::OwnedWriteHalf,
    ) {
        let stream = tokio::net::UnixStream::connect(sock)
            .await
            .expect("connecting to the session socket");
        let (r, mut w) = stream.into_split();
        write_line_async(
            &mut w,
            &Open::new(Request::Attach {
                role: Role::Primary,
            }),
        )
        .await
        .expect("asking to attach");
        (tokio::io::BufReader::new(r), w)
    }

    /// Ask for an attach, and read what the loop says back.
    ///
    /// **This, and not a bare `connect`, is how "the door is still open" is
    /// observed.** `UnixStream::connect` succeeds off the kernel's listen
    /// backlog whether or not anything is still calling `accept`, so it cannot
    /// tell a live loop from a dead one. A `HostHello` cannot be produced that
    /// way: only a loop that went round and ran the exchange as far as R6
    /// writes one.
    ///
    /// The write half is bound rather than dropped, so the connection is not
    /// shut down before the loop has answered on it.
    async fn hello_off(sock: &std::path::Path) -> oxutrm_proto::Signal {
        let (mut r, _w) = open_attach(sock).await;
        tokio::time::timeout(
            ANSWER_WITHIN,
            oxutrm_host::signalling::read_signal_async(&mut r),
        )
        .await
        .expect("the attach loop never answered; it is not accepting any more")
        .expect("the attach loop answered with something that is not a Signal")
    }

    fn on_disk(d: &Doors) -> SessionMeta {
        serde_json::from_slice(
            &std::fs::read(d.guard_dir.join(oxutrm_host::META_FILE)).expect("meta.json is there"),
        )
        .expect("meta.json parses")
    }

    /// A failed attach attempt must not disturb the running session: a
    /// client that asks and hangs up costs nothing -- no message on the
    /// channel, and the loop still attaching afterwards.
    #[tokio::test]
    async fn an_abandoned_attempt_sends_nothing_and_leaves_the_door_open() {
        let mut d = doors(
            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
            ATTACH_TIMEOUT,
        );

        drop(open_attach(&d.sock).await);

        match tokio::time::timeout(QUIET, d.rx.recv()).await {
            Err(_) => {}
            Ok(Some(_)) => {
                panic!("an attempt that never completed handed a link to the session")
            }
            Ok(None) => panic!(
                "the attach loop dropped its sender: it stopped serving on a \
                 failed attempt instead of going round again"
            ),
        }

        let hello = hello_off(&d.sock).await;
        assert!(
            matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
            "the loop answered the second attach with {hello:?} instead of an offer"
        );
    }

    /// A peer that asks to attach and then says nothing must not close the
    /// door for the life of the session. A short timeout is passed in rather
    /// than [`ATTACH_TIMEOUT`] so this costs a fifth of a second instead of
    /// ninety. The code it exercises is the same.
    #[tokio::test]
    async fn a_stalled_attach_gives_up_and_the_door_opens_again() {
        let d = doors(
            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
            Duration::from_millis(200),
        );
        // Alive and silent: the connection is HELD, so the exchange's read of
        // the client's hello never returns.
        let stalled = open_attach(&d.sock).await;

        let hello = hello_off(&d.sock).await;
        assert!(
            matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
            "the loop answered the second attach with {hello:?} instead of an offer"
        );
        assert_eq!(
            on_disk(&d).attach_id,
            0,
            "an attempt that timed out still advertised its generation"
        );
        drop(stalled);
    }

    /// A failed attempt must not advertise a generation that never served.
    /// `begin_attach` bumps `attach_id` at R4, before the exchange can fail;
    /// the registry is only written on success.
    #[tokio::test]
    async fn a_failed_attempt_does_not_write_its_generation_to_the_registry() {
        let d = doors(
            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
            ATTACH_TIMEOUT,
        );

        // Ask and hang up: R4 runs, R7 fails.
        drop(open_attach(&d.sock).await);

        // Ordered by the loop itself, not by a sleep: it is serial, so a
        // second attach being ANSWERED is proof that the first attempt is
        // over and has written whatever it was going to write.
        let hello = hello_off(&d.sock).await;
        assert!(
            matches!(hello, oxutrm_proto::Signal::HostHello { .. }),
            "the first attempt never finished, so nothing below is ordered \
             after it: {hello:?}"
        );
        assert_eq!(
            on_disk(&d).attach_id,
            0,
            "the registry advertises a generation no link ever ran as"
        );
    }

    /// A standby comes in through the queue, runs the very same exchange,
    /// and comes out marked as a standby -- while an attach over the socket
    /// comes out as a primary. A standby that came out as a primary would
    /// displace the live link and end the client.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_queued_standby_runs_the_same_exchange_and_carries_its_role() {
        let mut d = doors(
            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
            ATTACH_TIMEOUT,
        );
        let size = oxutrm_proto::TermSize { cols: 80, rows: 24 };
        let within = Duration::from_secs(10);

        let (cr, cw) = open_attach(&d.sock).await;
        let primary = tokio::time::timeout(
            within,
            crate::connect::establish(cr, cw, size, &stun_free(), None),
        )
        .await
        .expect("the client exchange over the socket must not hang")
        .expect("the client exchange over the socket completes");
        let attached = tokio::time::timeout(within, d.rx.recv())
            .await
            .expect("the socket attach never reached the session")
            .expect("the loop dropped its sender");
        assert_eq!(attached.role, Role::Primary);

        let (client_side, host_side) = tokio::io::duplex(64 * 1024);
        let (hr, hw) = tokio::io::split(host_side);
        d.queue
            .standby
            .send(DoorRequest {
                reader: Box::new(tokio::io::BufReader::new(hr)),
                writer: Box::new(hw),
                role: Role::Standby,
            })
            .await
            .unwrap();
        let (cr, cw) = tokio::io::split(client_side);
        let standby = tokio::time::timeout(
            within,
            crate::connect::establish(tokio::io::BufReader::new(cr), cw, size, &stun_free(), None),
        )
        .await
        .expect("the standby exchange must not hang")
        .expect("the standby exchange completes");
        let attached = tokio::time::timeout(within, d.rx.recv())
            .await
            .expect("the standby never reached the session")
            .expect("the loop dropped its sender");
        assert_eq!(attached.role, Role::Standby);
        assert_eq!(on_disk(&d).attach_id, 2, "both generations were recorded");

        // Every link handed over carries a control server of its own.
        let (_unserved_host, unserved) = crate::link::fixtures::link_pair().await;
        assert!(!crate::control::probe(unserved.sink.connection().clone(), 1).await);
        assert!(crate::control::probe(standby.link.sink.connection().clone(), 2).await);
        assert!(crate::control::probe(primary.link.sink.connection().clone(), 3).await);
    }

    /// A standby's exchange gives way to a primary. The real timeout is
    /// passed on purpose, so nothing but pre-emption can free the loop in
    /// time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_primary_preempts_a_stalled_standby() {
        let mut d = doors(
            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
            ATTACH_TIMEOUT,
        );
        let within = Duration::from_secs(10);

        let (client_side, host_side) = tokio::io::duplex(64 * 1024);
        let (hr, hw) = tokio::io::split(host_side);
        d.queue
            .standby
            .send(DoorRequest {
                reader: Box::new(tokio::io::BufReader::new(hr)),
                writer: Box::new(hw),
                role: Role::Standby,
            })
            .await
            .unwrap();
        let (cr, _cw) = tokio::io::split(client_side);
        let mut cr = tokio::io::BufReader::new(cr);
        let offer =
            tokio::time::timeout(within, oxutrm_host::signalling::read_signal_async(&mut cr))
                .await
                .expect("the standby's exchange never started")
                .expect("the standby's exchange sent something that is not a Signal");
        assert!(matches!(offer, oxutrm_proto::Signal::HostHello { .. }));

        let (sr, sw) = open_attach(&d.sock).await;
        tokio::time::timeout(
            within,
            crate::connect::establish(
                sr,
                sw,
                oxutrm_proto::TermSize { cols: 80, rows: 24 },
                &stun_free(),
                None,
            ),
        )
        .await
        .expect("the primary waited behind a stalled standby")
        .expect("the primary completes");
        let attached = tokio::time::timeout(within, d.rx.recv())
            .await
            .expect("the primary never reached the session")
            .expect("the loop dropped its sender");
        assert_eq!(attached.role, Role::Primary);
    }

    /// `Myself` is answered while the session is in the middle of an attach
    /// (switcher spec §2.2): it never waits on the serial loop.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn myself_is_answered_while_an_attach_is_under_way() {
        let d = doors(
            crate::door::fixtures::meta("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("build")),
            ATTACH_TIMEOUT,
        );
        // An attach that has its hello and will say nothing more: the loop
        // is held for the whole attach timeout.
        let (mut r, _w) = open_attach(&d.sock).await;
        let _ = oxutrm_host::signalling::read_signal_async(&mut r).await;

        let begun = std::time::Instant::now();
        let answer = crate::door::fixtures::ask(&d.sock, Request::Myself).await;
        assert!(
            matches!(&answer, oxutrm_proto::Answer::Reply(oxutrm_proto::Reply::Entry(e)) if e.name.as_ref().map(oxutrm_proto::Name::as_str) == Some("build")),
            "{answer:?}"
        );
        assert!(
            begun.elapsed() < Duration::from_secs(1),
            "Myself waited on the attach: {:?}",
            begun.elapsed()
        );
    }

    /// A `Sessions` fetch asks each sibling `Myself`; it must not cancel a
    /// sibling's standby exchange, which a fetch through the attach loop
    /// would pre-empt (switcher spec §2.2).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_sessions_fetch_does_not_cancel_a_siblings_standby_search() {
        let mut sibling = doors(
            crate::door::fixtures::meta("a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60", Some("logs")),
            ATTACH_TIMEOUT,
        );
        // This session, in the same registry, asking.
        let (me, _inbox, _) = crate::door::fixtures::registered(
            &sibling.registry,
            crate::door::fixtures::meta("3ff1218f5e0c4b7d9a1c2e3f40516273", Some("build")),
        );
        let me_sock = oxutrm_host::Registry::socket_path_in(
            &sibling.registry,
            "3ff1218f5e0c4b7d9a1c2e3f40516273",
        );
        crate::door::fixtures::socket_door(
            me,
            tokio::net::UnixListener::bind(&me_sock).expect("bind"),
        );

        // The sibling's standby exchange, under way.
        let (client_side, host_side) = tokio::io::duplex(64 * 1024);
        let (hr, hw) = tokio::io::split(host_side);
        sibling
            .queue
            .standby
            .send(DoorRequest {
                reader: Box::new(tokio::io::BufReader::new(hr)),
                writer: Box::new(hw),
                role: Role::Standby,
            })
            .await
            .unwrap();

        let answer = crate::door::fixtures::ask(&me_sock, Request::Sessions).await;
        let oxutrm_proto::Answer::Reply(oxutrm_proto::Reply::Sessions(list)) = answer else {
            panic!("{answer:?}");
        };
        assert_eq!(list.len(), 2, "{list:?}");

        // And the standby still completes, as a standby.
        let (cr, cw) = tokio::io::split(client_side);
        tokio::time::timeout(
            Duration::from_secs(10),
            crate::connect::establish(
                tokio::io::BufReader::new(cr),
                cw,
                oxutrm_proto::TermSize { cols: 80, rows: 24 },
                &stun_free(),
                None,
            ),
        )
        .await
        .expect("the standby hung")
        .expect("the standby was cancelled by the fetch");
        let attached = sibling.rx.recv().await.expect("the standby landed");
        assert_eq!(attached.role, Role::Standby);
    }

    /// The tasks' clones of the door -- and through it of the guard -- have
    /// to be gone before the session drops its own, or the session
    /// directory outlives the session. `abort()` only SCHEDULES
    /// cancellation; [`close_the_door`] awaits it.
    #[tokio::test]
    async fn awaiting_the_aborted_tasks_is_what_lets_the_guard_clean_up() {
        let mut d = doors(
            fresh_meta("3ff1218f5e0c4b7d9a1c2e3f40516273"),
            ATTACH_TIMEOUT,
        );
        assert!(d.guard_dir.exists(), "the fixture registered nothing");

        let accept = std::mem::replace(&mut d.accept, tokio::spawn(async {}));
        let attaches = std::mem::replace(&mut d.attaches, tokio::spawn(async {}));
        tokio::time::timeout(Duration::from_secs(5), async {
            close_the_door(accept).await;
            close_the_door(attaches).await;
        })
        .await
        .expect("the door's tasks were still running after the shell exited");
        drop(d.door.take());

        assert!(
            !d.guard_dir.exists(),
            "the session ended and {} is still there: a task was still \
             holding the door, so the guard's Drop never ran",
            d.guard_dir.display()
        );
    }
}

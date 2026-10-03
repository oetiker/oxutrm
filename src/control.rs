//! The control stream: a QUIC bidirectional stream on a live link, carrying
//! ordinary `Signal` lines (spec §2).
//!
//! After `Established`, ssh is gone (`serve.rs`'s sever), and this is the only
//! channel between the two ends. It is at least as well authenticated as ssh:
//! both ends of the connection it rides on are pinned by SPKI.
//!
//! Each stream is one conversation, and its first line says which:
//! `StandbyRequest` hands the stream to the session's door, where the ordinary
//! attach exchange runs over it; `Probe` is answered in place, for as long as
//! the stream stays open. Anything else is dropped.
//!
//! The client's half is [`request_standby`] and [`probe`], one stream per
//! conversation as well.

use oxutrm_host::signalling::{read_signal_async, write_signal_async};
use oxutrm_proto::Signal;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// What a completed attach is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Role {
    /// Adopt now; newest attach wins (recovery spec §6).
    Primary,
    /// Park until the client's first sync frame on it (spec §3.5).
    Standby,
}

/// A pair of pipes for the door, and what the attach that runs over them is for.
pub(crate) struct DoorRequest {
    pub reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub role: Role,
}

/// Serve control streams on `conn` until it closes.
///
/// Ends by itself when the connection does, because `accept_bi` then fails.
/// Nothing needs to stop it: every link the host drops is closed first.
pub(crate) fn serve_control(
    conn: quinn::Connection,
    door: tokio::sync::mpsc::Sender<DoorRequest>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((send, recv)) = conn.accept_bi().await {
            tokio::spawn(one_stream(send, recv, door.clone()));
        }
    })
}

/// Serve control streams on a link that has no door behind it.
///
/// A session without a Unix socket (rung 4, whose QUIC runs inside ssh) has
/// no listener to run a standby's exchange, but its hello still advertised a
/// control stream: the hello is written before the rung is known. So the
/// link is served anyway, against a door that is already closed. Probes are
/// answered; a standby request is refused at once, which the client sees as
/// its stream ending rather than as silence it would wait on for ever.
pub(crate) fn serve_control_without_door(conn: quinn::Connection) -> tokio::task::JoinHandle<()> {
    let (door, closed) = tokio::sync::mpsc::channel(1);
    drop(closed);
    serve_control(conn, door)
}

/// Longest a whole standby search may take, from opening the stream to the
/// host's verdict. Every step inside `establish` has its own budget; this is
/// the outer wall, as `ATTACH_TIMEOUT` is for the host. It matters more here
/// than it looks: the primary has no idle timeout, so a request sent into a
/// path that has just gone dark is never answered and never fails either.
pub(crate) const SEARCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(90);

/// Ask the host, over the primary, for a standby, and run the ordinary client
/// exchange over the same stream (spec §3.2).
///
/// `admit` is the filter the standby's ladder nominates through: in the
/// session, [`crate::egress::avoiding_source`] for the primary's remote.
pub(crate) async fn request_standby(
    primary: quinn::Connection,
    size: oxutrm_proto::TermSize,
    cfg: oxutrm_net::NetConfig,
    admit: oxutrm_net::RemoteFilter,
) -> anyhow::Result<crate::connect::Established> {
    use anyhow::Context as _;
    let search = async {
        let (mut send, recv) = primary
            .open_bi()
            .await
            .context("opening a control stream")?;
        write_signal_async(&mut send, &Signal::StandbyRequest)
            .await
            .context("asking for a standby")?;
        crate::connect::establish(
            tokio::io::BufReader::new(recv),
            send,
            size,
            &cfg,
            Some(admit),
        )
        .await
    };
    tokio::time::timeout(SEARCH_DEADLINE, search)
        .await
        .context("the standby search ran out of time")?
}

/// Does the far end of `conn` answer right now?
///
/// A fresh stream per probe. Opening one on a dead path succeeds locally,
/// since stream ids are ours to allocate, and it is the timeout that decides.
pub(crate) async fn probe(conn: quinn::Connection, nonce: u64) -> bool {
    let attempt = async {
        let (mut send, recv) = conn.open_bi().await.ok()?;
        write_signal_async(&mut send, &Signal::Probe { nonce })
            .await
            .ok()?;
        let mut reader = tokio::io::BufReader::new(recv);
        match read_signal_async(&mut reader).await.ok()? {
            Signal::ProbeAck { nonce: n } if n == nonce => Some(()),
            _ => None,
        }
    };
    matches!(
        tokio::time::timeout(crate::linkstate::PROBE_TIMEOUT, attempt).await,
        Ok(Some(()))
    )
}

async fn one_stream(
    mut send: quinn::SendStream,
    recv: quinn::RecvStream,
    door: tokio::sync::mpsc::Sender<DoorRequest>,
) {
    let mut reader = tokio::io::BufReader::new(recv);
    let Ok(first) = read_signal_async(&mut reader).await else {
        return;
    };
    match first {
        Signal::StandbyRequest => {
            // A busy door is waited for: the door is serial, and the attempt
            // ahead is bounded by `ATTACH_TIMEOUT`. A closed door (a session
            // with no listener, see `serve_control_without_door`, or one whose
            // listener has gone with it) drops the stream at once. The
            // client's search then fails and backs off, which is the whole of
            // the damage.
            let _ = door
                .send(DoorRequest {
                    reader: Box::new(reader),
                    writer: Box::new(send),
                    role: Role::Standby,
                })
                .await;
        }
        Signal::Probe { nonce } => {
            let mut nonce = nonce;
            loop {
                if write_signal_async(&mut send, &Signal::ProbeAck { nonce })
                    .await
                    .is_err()
                {
                    return;
                }
                match read_signal_async(&mut reader).await {
                    Ok(Signal::Probe { nonce: next }) => nonce = next,
                    _ => return,
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::link::fixtures::link_pair;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_is_answered_with_its_own_nonce() {
        let (host, client) = link_pair().await;
        let (door_tx, _door_rx) = tokio::sync::mpsc::channel(1);
        let _server = serve_control(host.sink.connection().clone(), door_tx);

        let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
        write_signal_async(&mut send, &Signal::Probe { nonce: 7 })
            .await
            .unwrap();
        let mut recv = tokio::io::BufReader::new(recv);
        let back = read_signal_async(&mut recv).await.unwrap();
        assert!(matches!(back, Signal::ProbeAck { nonce: 7 }), "{back:?}");

        // The same stream keeps answering, so one probe stream can serve an outage.
        write_signal_async(&mut send, &Signal::Probe { nonce: 8 })
            .await
            .unwrap();
        let back = read_signal_async(&mut recv).await.unwrap();
        assert!(matches!(back, Signal::ProbeAck { nonce: 8 }), "{back:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_request_is_handed_to_the_door_with_its_stream() {
        let (host, client) = link_pair().await;
        let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
        let _server = serve_control(host.sink.connection().clone(), door_tx);

        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        write_signal_async(&mut send, &Signal::StandbyRequest)
            .await
            .unwrap();
        // Something after the request must reach whoever the door hands the
        // stream to: the exchange reads the client's hello from it.
        write_signal_async(&mut send, &Signal::Probe { nonce: 1 })
            .await
            .unwrap();

        let mut req = door_rx.recv().await.expect("the door was knocked on");
        assert_eq!(req.role, Role::Standby);
        let next = read_signal_async(&mut req.reader).await.unwrap();
        assert!(matches!(next, Signal::Probe { nonce: 1 }), "{next:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn anything_else_first_is_dropped_without_reaching_the_door() {
        let (host, client) = link_pair().await;
        let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
        let _server = serve_control(host.sink.connection().clone(), door_tx);

        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        write_signal_async(
            &mut send,
            &Signal::Choose {
                choice: oxutrm_proto::Choice::New,
            },
        )
        .await
        .unwrap();
        send.finish().unwrap();
        let knocked =
            tokio::time::timeout(std::time::Duration::from_millis(500), door_rx.recv()).await;
        assert!(
            knocked.is_err(),
            "the door was knocked on: a stray line can start an attach"
        );

        // And the quiet meant something: the server was there, and a request
        // on the next stream still gets through. Without this, a server that
        // never accepted a stream at all would pass the assertion above.
        let (mut send, _recv) = client.sink.connection().open_bi().await.unwrap();
        write_signal_async(&mut send, &Signal::StandbyRequest)
            .await
            .unwrap();
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), door_rx.recv())
            .await
            .expect("the control server stopped serving after a stray stream")
            .expect("the door's sender is gone");
        assert_eq!(req.role, Role::Standby);
    }

    /// A host side that serves control streams and runs every standby request
    /// through the real exchange, returning what it produced.
    fn host_door(
        host_conn: quinn::Connection,
    ) -> tokio::sync::oneshot::Receiver<crate::attach_exchange::Attached> {
        let (door_tx, mut door_rx) = tokio::sync::mpsc::channel(1);
        serve_control(host_conn, door_tx);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let req: DoorRequest = door_rx.recv().await.expect("a request");
            let mut meta = crate::attach_exchange::fixtures::fresh_meta(SESSION);
            let cfg = crate::attach_exchange::fixtures::stun_free();
            if let Ok(mut a) =
                crate::attach_exchange::run_attach_exchange(req.reader, req.writer, &mut meta, &cfg)
                    .await
            {
                a.role = req.role;
                let _ = done_tx.send(a);
            }
        });
        done_rx
    }

    const SESSION: &str = "00112233445566778899aabbccddeeff";

    fn small() -> oxutrm_proto::TermSize {
        oxutrm_proto::TermSize { cols: 40, rows: 10 }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_search_lands_a_second_connection_to_the_same_host() {
        let (host, client) = link_pair().await;
        let done = host_door(host.sink.connection().clone());
        let cfg = crate::attach_exchange::fixtures::stun_free();
        let primary = client.sink.connection().clone();

        let est = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            request_standby(primary.clone(), small(), cfg, std::sync::Arc::new(|_| true)),
        )
        .await
        .expect("the search hung")
        .expect("the standby landed");
        let attached = done.await.expect("the host completed its side");

        assert_eq!(attached.role, Role::Standby);
        assert_eq!(
            est.session_id, SESSION,
            "the standby reached another session"
        );
        assert_ne!(
            est.link.sink.connection().stable_id(),
            primary.stable_id(),
            "the standby is the primary"
        );
        assert!(
            primary.close_reason().is_none(),
            "the search disturbed the primary"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_search_honours_the_filter() {
        let (host, client) = link_pair().await;
        let _done = host_door(host.sink.connection().clone());
        let mut cfg = crate::attach_exchange::fixtures::stun_free();
        cfg.gather_timeout = std::time::Duration::from_millis(800);

        let r = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            request_standby(
                client.sink.connection().clone(),
                small(),
                cfg,
                std::sync::Arc::new(|_| false),
            ),
        )
        .await
        .expect("the search hung");
        let Err(e) = r else {
            panic!("landed a standby on a forbidden path");
        };
        let why = format!("{e:#}");
        assert!(
            why.contains("no rung of the ladder reached the host"),
            "the search failed, but not because the filter left it no path: {why}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_of_a_served_link_is_answered() {
        let (host, client) = link_pair().await;
        let _server = serve_control_without_door(host.sink.connection().clone());
        assert!(probe(client.sink.connection().clone(), 5).await);
        // And again, on a fresh stream: one probe per conversation.
        assert!(probe(client.sink.connection().clone(), 6).await);
    }

    /// Nobody answers: the connection is up, but no control server reads the
    /// stream. The probe must come back `false` on its own clock rather than
    /// wait for a reply that is never written.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_nobody_answers_times_out_as_unanswered() {
        let (_host, client) = link_pair().await;
        let started = std::time::Instant::now();
        let answered = tokio::time::timeout(
            crate::linkstate::PROBE_TIMEOUT * 3,
            probe(client.sink.connection().clone(), 5),
        )
        .await
        .expect("the probe waited past its own timeout");
        assert!(!answered);
        assert!(started.elapsed() >= crate::linkstate::PROBE_TIMEOUT);
    }

    /// Rung 4 advertises a control stream like every other link, so it has to
    /// answer one: a standby request ends promptly instead of waiting on a
    /// door nobody opens, and probes still work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn without_a_door_a_standby_request_ends_at_once_and_probes_are_answered() {
        let (host, client) = link_pair().await;
        let _server = serve_control_without_door(host.sink.connection().clone());
        let within = std::time::Duration::from_secs(5);

        let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
        write_signal_async(&mut send, &Signal::StandbyRequest)
            .await
            .unwrap();
        let mut recv = tokio::io::BufReader::new(recv);
        let ended = tokio::time::timeout(within, read_signal_async(&mut recv))
            .await
            .expect("a standby request with no door behind it was left waiting");
        assert!(
            ended.is_err(),
            "a standby request with no door was answered: {ended:?}"
        );

        let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
        write_signal_async(&mut send, &Signal::Probe { nonce: 3 })
            .await
            .unwrap();
        let mut recv = tokio::io::BufReader::new(recv);
        let back = tokio::time::timeout(within, read_signal_async(&mut recv))
            .await
            .expect("a probe on a doorless link went unanswered")
            .unwrap();
        assert!(matches!(back, Signal::ProbeAck { nonce: 3 }), "{back:?}");
    }
}

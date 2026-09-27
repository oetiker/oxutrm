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

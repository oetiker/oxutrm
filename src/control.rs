//! The control stream: a QUIC bidirectional stream on a live link (spec §2).
//!
//! After `Established`, ssh is gone (`serve.rs`'s sever), and this is the only
//! channel between the two ends. It is at least as well authenticated as ssh:
//! both ends of the connection it rides on are pinned by SPKI.
//!
//! Each stream is one conversation, and its first line is an
//! [`oxutrm_proto::Open`] saying which. The host hands every stream to the
//! session's door ([`crate::door`]), the same dispatcher its Unix socket
//! uses: `Attach { Standby }` runs the ordinary attach exchange over the
//! stream and parks the result; `Probe` is answered in place, for as long as
//! the stream stays open; the switcher's requests are answered there too.
//!
//! The client's half is [`request_standby`] and [`probe`] here, one stream
//! per conversation as well.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::sync::Arc;

use oxutrm_host::signalling::{read_line_async, write_line_async};
pub(crate) use oxutrm_proto::Role;
use oxutrm_proto::{Open, Reply, Request};
use tokio::io::{AsyncBufRead, AsyncWrite};

use crate::door::{Door, Via};

/// A pair of pipes for the attach loop, and what the attach that runs over
/// them is for.
pub(crate) struct DoorRequest {
    pub reader: Box<dyn AsyncBufRead + Unpin + Send>,
    pub writer: Box<dyn AsyncWrite + Unpin + Send>,
    pub role: Role,
}

/// Serve control streams on `conn` through `door` until it closes.
///
/// Ends by itself when the connection does, because `accept_bi` then fails.
/// Nothing needs to stop it: every link the host drops is closed first.
pub(crate) fn serve_control(
    conn: quinn::Connection,
    door: Arc<Door>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok((send, recv)) = conn.accept_bi().await {
            tokio::spawn(crate::door::serve(
                Arc::clone(&door),
                Via::Client,
                tokio::io::BufReader::new(recv),
                send,
            ));
        }
    })
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
        write_line_async(
            &mut send,
            &Open::new(Request::Attach {
                role: Role::Standby,
            }),
        )
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
        write_line_async(&mut send, &Open::new(Request::Probe { nonce }))
            .await
            .ok()?;
        let mut reader = tokio::io::BufReader::new(recv);
        match read_line_async(&mut reader).await.ok()? {
            Reply::ProbeAck { nonce: n } if n == nonce => Some(()),
            _ => None,
        }
    };
    matches!(
        tokio::time::timeout(crate::linkstate::PROBE_TIMEOUT, attempt).await,
        Ok(Some(()))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::door::fixtures::{meta, registered};
    use crate::link::fixtures::link_pair;

    const SESSION: &str = "00112233445566778899aabbccddeeff";

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_probe_is_answered_with_its_own_nonce_and_the_stream_keeps_answering() {
        let dir = tempfile::tempdir().unwrap();
        let (door, _inbox, _) = registered(dir.path(), meta(SESSION, None));
        let (host, client) = link_pair().await;
        let _server = serve_control(host.sink.connection().clone(), door);

        let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
        let mut recv = tokio::io::BufReader::new(recv);
        for nonce in [7, 8] {
            write_line_async(&mut send, &Open::new(Request::Probe { nonce }))
                .await
                .unwrap();
            let back: Reply = read_line_async(&mut recv).await.unwrap();
            assert_eq!(back, Reply::ProbeAck { nonce });
        }
    }

    /// A host side that serves control streams and runs every standby request
    /// through the real exchange, returning what it produced.
    fn host_door(
        dir: &std::path::Path,
        host_conn: quinn::Connection,
    ) -> tokio::sync::oneshot::Receiver<crate::attach_exchange::Attached> {
        let (door, mut inbox, _) = registered(dir, meta(SESSION, None));
        serve_control(host_conn, door);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let req: DoorRequest = inbox.standby.recv().await.expect("a request");
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

    fn small() -> oxutrm_proto::TermSize {
        oxutrm_proto::TermSize { cols: 40, rows: 10 }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_standby_search_lands_a_second_connection_to_the_same_host() {
        let dir = tempfile::tempdir().unwrap();
        let (host, client) = link_pair().await;
        let done = host_door(dir.path(), host.sink.connection().clone());
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
        let dir = tempfile::tempdir().unwrap();
        let (host, client) = link_pair().await;
        let _done = host_door(dir.path(), host.sink.connection().clone());
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
    /// answer one: a standby request ends promptly instead of waiting on an
    /// attach loop nobody runs, and probes still work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn without_an_attach_loop_a_standby_request_ends_at_once_and_probes_are_answered() {
        let dir = tempfile::tempdir().unwrap();
        let door = crate::door::Door::assembled(
            dir.path().to_path_buf(),
            meta(SESSION, None),
            None,
            crate::host_session::Presence::default(),
            None,
        );
        let (host, client) = link_pair().await;
        let _server = serve_control(host.sink.connection().clone(), door);
        let within = std::time::Duration::from_secs(5);

        let (mut send, recv) = client.sink.connection().open_bi().await.unwrap();
        write_line_async(
            &mut send,
            &Open::new(Request::Attach {
                role: Role::Standby,
            }),
        )
        .await
        .unwrap();
        let mut recv = tokio::io::BufReader::new(recv);
        let ended = tokio::time::timeout(within, read_line_async::<_, Reply>(&mut recv))
            .await
            .expect("a standby request with no loop behind it was left waiting");
        assert!(
            ended.is_err(),
            "a standby request with no loop was answered: {ended:?}"
        );

        assert!(probe(client.sink.connection().clone(), 3).await);
        // And again, on a fresh stream: one probe per conversation.
        assert!(probe(client.sink.connection().clone(), 4).await);
    }
}

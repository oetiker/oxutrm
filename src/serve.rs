//! `oxutrm host --serve`: the remote half, from the fork to the shell.

use oxutrm_proto::{Candidate, NatType, PathDescription};

use oxutrm_host::signalling::write_signal_async;
use oxutrm_proto::Signal;
use tokio::io::AsyncWrite;

/// Send every candidate the ladder discovers to the peer, as a
/// `CandidateUpdate`, for as long as the ladder is still discovering them.
///
/// Ends when `learned` closes, which is what dropping the ladder's sender
/// does — so the writer comes back **unborrowed and mid-line-free**, ready for
/// `Established`. Nothing is cancelled, so no half-written line can precede it.
async fn outbound_candidates<W>(
    writer: &mut W,
    learned: &mut tokio::sync::mpsc::Receiver<Candidate>,
) -> anyhow::Result<()>
where
    W: AsyncWrite + Unpin,
{
    while let Some(candidate) = learned.recv().await {
        write_signal_async(
            writer,
            &Signal::CandidateUpdate {
                candidates: vec![candidate],
            },
        )
        .await?;
    }
    Ok(())
}

/// Feed the ladder every candidate the peer discovers after its hello.
///
/// Returns the first signal that is **not** a `CandidateUpdate`, because that
/// is the one the caller was waiting for and swallowing it would lose the end
/// of the handshake. End of stream is an error: the peer hanging up mid-race
/// is not a nomination.
async fn inbound_candidates<R>(
    reader: &mut R,
    inbound: &tokio::sync::mpsc::Sender<Candidate>,
) -> anyhow::Result<Signal>
where
    R: tokio::io::AsyncBufReadExt + Unpin,
{
    loop {
        match oxutrm_host::signalling::read_signal_async(reader).await? {
            Signal::CandidateUpdate { candidates } => {
                for c in candidates {
                    // A closed receiver means the ladder has already settled,
                    // which is not an error -- there is simply nobody left to
                    // tell. Anything else the peer sends still matters.
                    if inbound.send(c).await.is_err() {
                        break;
                    }
                }
            }
            other => return Ok(other),
        }
    }
}

/// Describe the nominated path for the status line.
///
/// **The MTU and the RTT come from the live `quinn` connection and from
/// nowhere else.** They cannot come from the [`Nomination`]: neither number
/// exists at nomination time, which is why `Nomination` deliberately has no
/// `mtu` field to copy. A constant here would render as a plausible status
/// line that is simply not true of this path.
///
/// Everything else is the ladder's own finding, copied unchanged, because the
/// connection knows nothing about which rung won or what it cost.
fn path_description(
    nomination: &crate::ladder::Nomination,
    nat: NatType,
    conn: &quinn::Connection,
) -> PathDescription {
    PathDescription {
        rung: nomination.rung,
        local: nomination.local,
        remote: nomination.remote,
        probes_sent: nomination.probes,
        nat_type: nat,
        // Saturating rather than wrapping: a link slow enough to overflow a
        // u32 of milliseconds is one the status line should report as very
        // slow, not as very fast.
        rtt_ms: u32::try_from(conn.rtt().as_millis()).unwrap_or(u32::MAX),
        mtu: conn.stats().path.current_mtu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::CandidateKind;

    fn candidate(port: u16) -> Candidate {
        Candidate {
            addr: format!("192.0.2.1:{port}").parse().expect("a test address"),
            kind: CandidateKind::PeerReflexive,
            priority: 1,
        }
    }

    /// The half the handoff warns about: the agent emits `NewLocalCandidate`
    /// whether or not anyone is listening, so a pump that drops them is silent.
    /// Deleting the `write_signal_async` call below makes this test fail and
    /// nothing else in the tree notice.
    #[tokio::test]
    async fn every_learned_candidate_reaches_the_peer_as_a_candidate_update() {
        let (mut ours, theirs) = tokio::io::duplex(64 * 1024);
        let (learned_tx, mut learned_rx) = tokio::sync::mpsc::channel(8);

        learned_tx.send(candidate(4001)).await.expect("send one");
        learned_tx.send(candidate(4002)).await.expect("send two");
        drop(learned_tx);

        outbound_candidates(&mut ours, &mut learned_rx)
            .await
            .expect("the pump must finish cleanly when the ladder is done");
        drop(ours);

        let mut peer = tokio::io::BufReader::new(theirs);
        let mut ports = Vec::new();
        while let Ok(signal) = oxutrm_host::signalling::read_signal_async(&mut peer).await {
            match signal {
                Signal::CandidateUpdate { candidates } => {
                    ports.extend(candidates.iter().map(|c| c.addr.port()));
                }
                other => panic!("the pump wrote something that is not an update: {other:?}"),
            }
        }
        assert_eq!(
            ports,
            vec![4001, 4002],
            "candidates the ladder learned never reached the peer"
        );
    }

    /// A pipe already holding `signals`, ready to be read as a peer would
    /// have written them.
    async fn peer_wrote(signals: &[Signal]) -> tokio::io::BufReader<tokio::io::DuplexStream> {
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        for s in signals {
            oxutrm_host::signalling::write_signal_async(&mut theirs, s)
                .await
                .expect("the peer writes its own signals");
        }
        drop(theirs);
        tokio::io::BufReader::new(ours)
    }

    /// The other half the handoff warns about. A candidate that turns up
    /// *during* the race is the one that rescues a one-sided port mapping, and
    /// a pump that reads it and drops it looks identical to one that works.
    #[tokio::test]
    async fn a_candidate_that_arrives_during_the_race_reaches_the_ladder() {
        let mut reader = peer_wrote(&[
            Signal::CandidateUpdate {
                candidates: vec![candidate(5001)],
            },
            Signal::CandidateUpdate {
                candidates: vec![candidate(5002)],
            },
            Signal::Failed {
                reason: "stop".to_owned(),
            },
        ])
        .await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);

        inbound_candidates(&mut reader, &tx)
            .await
            .expect("the pump must return the terminating signal");

        let mut ports = Vec::new();
        while let Ok(c) = rx.try_recv() {
            ports.push(c.addr.port());
        }
        assert_eq!(
            ports,
            vec![5001, 5002],
            "candidates that arrived mid-race never reached the ladder"
        );
    }

    // ---- the path description, and the two numbers that are not the ladder's --
    //
    // `Nomination` has no `mtu`, on purpose: at nomination time QUIC has not
    // run, so the number does not exist. The tempting mistake is a constant --
    // 1200 is the QUIC floor and reads as a real answer. These fixtures pin an
    // MTU the connection could only have got from its own configuration, so a
    // constant of any value fails.

    use std::net::SocketAddr;
    use std::sync::Arc;

    use oxutrm_net::{
        ALPN, CERT_NAME, PinnedClientSpki, PinnedSpki, generate_cert, install_crypto_provider,
        provider,
    };
    use oxutrm_proto::{ClientSpki, HostSpki, Rung};
    use quinn::rustls;

    /// Deliberately not a round number and not the QUIC floor, so no plausible
    /// constant can collide with it.
    const PINNED_MTU: u16 = 1337;

    fn transport() -> Arc<quinn::TransportConfig> {
        let mut t = quinn::TransportConfig::default();
        // Freeze the path MTU: discovery would raise it on loopback and the
        // assertion below is about WHERE the number comes from, not how big it
        // is.
        t.initial_mtu(PINNED_MTU);
        t.min_mtu(PINNED_MTU);
        t.mtu_discovery_config(None);
        Arc::new(t)
    }

    /// A real QUIC connection on loopback whose MTU is [`PINNED_MTU`].
    ///
    /// The endpoints come back because dropping them closes the connection.
    #[allow(clippy::type_complexity)]
    async fn connected() -> (quinn::Connection, (quinn::Connection, Vec<quinn::Endpoint>)) {
        install_crypto_provider();
        let (cert, key, fingerprint) = generate_cert().expect("host certificate");
        let (client_cert, client_key, client_fp) = generate_cert().expect("client certificate");
        let addr: SocketAddr = "127.0.0.1:0".parse().expect("a loopback address");

        let mut tls = rustls::ServerConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .with_client_cert_verifier(Arc::new(PinnedClientSpki::new(ClientSpki::new(client_fp))))
            .with_single_cert(vec![cert], key)
            .expect("a server config");
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut server_cfg = quinn::ServerConfig::with_crypto(Arc::new(
            quinn::crypto::rustls::QuicServerConfig::try_from(tls).expect("a QUIC server config"),
        ));
        server_cfg.transport_config(transport());

        let server_ep = quinn::Endpoint::server(server_cfg, addr).expect("a server endpoint");
        let server_addr = server_ep.local_addr().expect("the server's address");
        let accepting = {
            let ep = server_ep.clone();
            tokio::spawn(async move {
                ep.accept()
                    .await
                    .expect("a connection attempt")
                    .await
                    .expect("a completed handshake")
            })
        };

        let mut tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .expect("TLS 1.3")
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedSpki::new(HostSpki::new(fingerprint))))
            .with_client_auth_cert(vec![client_cert], client_key)
            .expect("a client config");
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let mut client_cfg = quinn::ClientConfig::new(Arc::new(
            quinn::crypto::rustls::QuicClientConfig::try_from(tls).expect("a QUIC client config"),
        ));
        client_cfg.transport_config(transport());

        let mut client_ep = quinn::Endpoint::client(addr).expect("a client endpoint");
        client_ep.set_default_client_config(client_cfg);
        let client_conn = client_ep
            .connect(server_addr, CERT_NAME)
            .expect("a connect attempt")
            .await
            .expect("a completed handshake");
        let server_conn = accepting.await.expect("the accept task");

        // The client connection is returned, not dropped: dropping it closes
        // the connection, and every assertion below reads live numbers off the
        // server side of it.
        (server_conn, (client_conn, vec![server_ep, client_ep]))
    }

    async fn nomination(rung: Rung, probes: u32) -> crate::ladder::Nomination {
        let socket = Arc::new(
            tokio::net::UdpSocket::bind("127.0.0.1:0")
                .await
                .expect("a socket for the nomination"),
        );
        crate::ladder::Nomination {
            local: socket.local_addr().expect("its own address"),
            socket,
            remote: "198.51.100.9:7777".parse().expect("a peer address"),
            rung,
            probes,
        }
    }

    /// The number `Nomination` refuses to carry. 1200 is the QUIC floor and
    /// would render as a believable "mtu 1200"; anything fabricated here is a
    /// status line the user cannot tell from a measured one.
    #[tokio::test]
    async fn the_mtu_comes_from_the_live_connection_and_not_from_a_constant() {
        let (conn, _alive) = connected().await;
        let nom = nomination(Rung::StunPunch, 12).await;

        let path = path_description(&nom, NatType::EndpointIndependent, &conn);

        assert_eq!(
            path.mtu, PINNED_MTU,
            "the MTU was not read from the connection that has one"
        );
    }

    /// The other live number. `Nomination` cannot carry it either: there is no
    /// round trip to time until QUIC has completed a handshake.
    #[tokio::test]
    async fn the_rtt_comes_from_the_live_connection() {
        let (conn, _alive) = connected().await;
        let nom = nomination(Rung::StunPunch, 12).await;

        let path = path_description(&nom, NatType::EndpointIndependent, &conn);

        assert_eq!(
            u128::from(path.rtt_ms),
            conn.rtt().as_millis(),
            "the round trip time was not read from the connection"
        );
    }

    /// The mirror image: the connection knows nothing about which rung won or
    /// what the race cost, so those must survive unchanged from the ladder.
    #[tokio::test]
    async fn the_ladders_own_findings_are_carried_through_unchanged() {
        let (conn, _alive) = connected().await;
        let nom = nomination(Rung::Birthday, 312).await;

        let path = path_description(&nom, NatType::Symmetric, &conn);

        assert_eq!(path.rung, Rung::Birthday);
        assert_eq!(path.probes_sent, 312, "the blast's cost must be reported");
        assert_eq!(path.local, nom.local);
        assert_eq!(path.remote, nom.remote);
        assert_eq!(path.nat_type, NatType::Symmetric);
    }

    /// The terminating signal is what the caller is waiting for. A pump that
    /// consumed it would leave the caller reading a stream that has nothing
    /// left to say.
    #[tokio::test]
    async fn the_signal_that_ends_the_race_is_handed_back_not_swallowed() {
        let mut reader = peer_wrote(&[Signal::Failed {
            reason: "the peer gave up".to_owned(),
        }])
        .await;
        let (tx, _rx) = tokio::sync::mpsc::channel(8);

        let signal = inbound_candidates(&mut reader, &tx)
            .await
            .expect("a terminating signal is not an error");
        match signal {
            Signal::Failed { reason } => assert_eq!(reason, "the peer gave up"),
            other => panic!("the pump handed back the wrong signal: {other:?}"),
        }
    }
}

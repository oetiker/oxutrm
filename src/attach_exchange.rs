//! The attach exchange: R4 to R10, generic over the pipes signalling runs on.
//!
//! This is the same work whether the peer is speaking over ssh's pipes (the
//! first connect, from `serve()`) or over a session's Unix socket (a
//! reattach): a fresh certificate, the STUN/ICE ladder, the hello exchange
//! and the QUIC handshake do not care which pipe carries the signalling.
//! Spec 5.1's binding rule is that reattachment must not be a second code
//! path, so this is the one function both callers run.
//!
//! R11 (`settle_detachability`), R12 (`sever_from_ssh`) and R13 (register)
//! deliberately stay out of here: they are the first connect's own, and a
//! reattach that re-ran them would sever from ssh a second time and
//! re-register a live session.

use std::sync::Arc;

use anyhow::Context as _;
use oxutrm_host::registry::SessionMeta;
use oxutrm_host::signalling::write_signal_async;
use oxutrm_net::{IceRole, NetConfig};
use oxutrm_proto::{
    Candidate, ClientSpki, HostSpki, NatType, PathDescription, Signal, TermSize, TerminalCaps,
};
use tokio::io::AsyncWrite;

use crate::accept::accept_one;
use crate::candidates::{inbound_candidates, outbound_candidates};
use crate::ladder::nominate;
use crate::link::Link;

/// One completed attach: the transport, and the two facts the caller needs
/// about it.
pub(crate) struct Attached {
    pub link: Link,
    pub path: PathDescription,
    /// The client's terminal size, from its `ClientHello`.
    pub client_size: TermSize,
}

/// R4 to R10: a fresh certificate, the STUN/ICE ladder, the hello exchange and
/// the QUIC handshake, ending the moment the client has been told the path is
/// up.
///
/// Stops there on purpose. R11, R12 and R13 — settling detachability,
/// severing from ssh, and registering the session — are the caller's: see the
/// module docs.
pub(crate) async fn run_attach_exchange<R, W>(
    reader: R,
    writer: W,
    meta: &mut SessionMeta,
    cfg: &NetConfig,
) -> anyhow::Result<Attached>
where
    R: tokio::io::AsyncBufRead + Unpin + Send + 'static,
    W: tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let mut stdin = reader;
    let mut stdout = writer;

    // R4. A fresh certificate and a fresh PSK for this attach, so key material
    // captured from a previous one cannot reattach.
    let (cert, key, spki) =
        oxutrm_net::generate_cert().context("generating this attach's certificate")?;
    let attach = oxutrm_host::begin_attach(meta, HostSpki::new(spki))
        .context("starting an attach generation")?;

    // R5. One socket for STUN, ICE and QUIC — sharing it is what lets a single
    // NAT mapping serve every rung.
    let bound = oxutrm_net::bind_socket(cfg).context("binding a UDP socket")?;
    // Both of these read the bound port off the plain socket, before it is
    // handed to the runtime -- no second descriptor on the same socket.
    let bound_port = bound.local_addr().map(|a| a.port()).unwrap_or(0);
    let mut candidates = oxutrm_net::local_candidates(&bound);
    let socket = crate::ladder::adopt(bound).context("handing the socket to the runtime")?;
    let (reflexive, nat) = oxutrm_net::stun_discover(&socket, cfg).await;
    candidates.extend(reflexive);

    // R6 and R7, in that order, and the order is the whole content of it.
    let hello = host_hello(
        &meta.session_id,
        &attach,
        candidates.clone(),
        nat,
        bound_port,
    );
    let client = exchange_hellos(&mut stdin, &mut stdout, &hello).await?;
    meta.size = client.size;

    // R8. The controlling side nominates; this side is told.
    //
    // The two candidate channels are pumped by two independent tasks rather
    // than by one `select!`, so that the outbound one can be *finished* rather
    // than cancelled: dropping the ladder's sender closes it, the task returns
    // the writer with no half-written line on it, and `Established` goes out
    // behind a clean flush.
    let (in_tx, mut in_rx) = tokio::sync::mpsc::channel(32);
    let (learned_tx, mut learned_rx) = tokio::sync::mpsc::channel(32);

    let mut inbound = tokio::spawn(async move {
        let r = inbound_candidates(&mut stdin, &in_tx).await;
        (stdin, r)
    });
    let outbound = tokio::spawn(async move {
        let r = outbound_candidates(&mut stdout, &mut learned_rx).await;
        (stdout, r)
    });

    let nomination = nominate(
        Arc::clone(&socket),
        crate::ladder::Ladder {
            psk: attach.keys.psk(),
            role: IceRole::Controlled,
            nat,
            cfg,
            local: candidates,
            remote: client.candidates,
        },
        &mut in_rx,
        &learned_tx,
    )
    .await;

    // Close the outbound pump's channel, then wait for it to drain and hand
    // the writer back. This is the ordering that keeps `Established` unmixed
    // with a truncated `CandidateUpdate`.
    drop(learned_tx);
    let (mut stdout, sent) = outbound.await.context("the outbound candidate pump")?;
    sent.context("sending our candidates to the client")?;
    inbound.abort();
    let _ = (&mut inbound).await;

    // A total failure names all five rungs, and says for each whether it was
    // skipped in advance or tried and failed. The client is told before we
    // give up, because it is the end that has a user in front of it.
    let nomination = match nomination {
        Ok(n) => n,
        Err(report) => {
            let _ = write_signal_async(
                &mut stdout,
                &Signal::Failed {
                    reason: report.to_string(),
                },
            )
            .await;
            return Err(anyhow::Error::new(report).context("no rung of the ladder produced a path"));
        }
    };

    // R9. The endpoint is up before `Established` goes out, so the client's
    // Initial is never sent into a void — and the client connects on
    // nomination, without waiting for `Established`, which is why the accept
    // comes first here.
    let (endpoint, permit, _stun_rx) =
        oxutrm_net::quic_server(&nomination.socket, cert, key, client.cert_spki_sha256)
            .await
            .context("bringing up the QUIC endpoint on the nominated socket")?;
    // `?` on purpose: under ICE the peer at the nominated address IS the
    // client, and one that fails the certificate pin will not pass it on a
    // second try. Dropping the permit ends the attach.
    let connection = accept_one(permit, nomination.remote).await?;

    // R10. The last signalling message, and the first point at which the two
    // live numbers exist.
    let path = path_description(&nomination, nat, &connection);
    write_signal_async(&mut stdout, &Signal::Established { path: path.clone() })
        .await
        .context("telling the client the path is up")?;

    Ok(Attached {
        link: Link::new(connection, endpoint, nomination.socket),
        path,
        client_size: client.size,
    })
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

/// What the client told us about itself, once its hello arrived.
///
/// A struct rather than a five-tuple because two of its fields are candidate
/// lists and fingerprints that would otherwise be swappable at the call site.
#[derive(Debug)]
struct ClientFacts {
    cert_spki_sha256: ClientSpki,
    candidates: Vec<Candidate>,
    /// The client's own NAT verdict. It does **not** select the ladder plan --
    /// `LadderPlan::for_nat` is asked about *ours*, because the plan is about
    /// what this end can do. Carried because a failed ladder is diagnosed from
    /// both verdicts and only one of them is on this machine.
    ///
    /// Nothing consumes it yet, and the allow says so rather than a fabricated
    /// call site inventing a use: what is worth knowing here is precisely that
    /// the diagnosis this field exists for has not been written.
    #[allow(dead_code)]
    nat_type: NatType,
    /// What the client's terminal can render. Recorded, and **never** used to
    /// pick `TERM`: a `TERM` narrowed to today's client would bake degraded
    /// output into the authoritative screen for the life of the session, which
    /// outlives this attach. See `negotiate_term`, which takes no arguments
    /// for exactly that reason.
    #[allow(dead_code)]
    caps: TerminalCaps,
    size: TermSize,
}

/// R6: the offer, composed before the ladder has run.
///
/// `detachable` is the host's **intent** and is therefore always true here.
/// Nobody yet knows which rung will be nominated — the candidates that decide
/// it are travelling in this very message — and the outcome is settled later by
/// `settle_detachability`, which is the only thing that may write
/// `SessionMeta.detachable`. A value computed here could only be a guess the
/// client would believe.
fn host_hello(
    session_id: &str,
    attach: &oxutrm_host::Attach,
    candidates: Vec<Candidate>,
    nat_type: NatType,
    bound_port: u16,
) -> Signal {
    Signal::HostHello {
        proto: oxutrm_proto::PROTO_VERSION,
        session_id: session_id.to_owned(),
        attach_id: attach.attach_id,
        cert_spki_sha256: attach.keys.cert_spki_sha256(),
        psk: attach.keys.psk().clone(),
        candidates,
        nat_type,
        bound_port,
        detachable: true,
    }
}

/// R6 then R7, **in that order**.
///
/// The order is the whole content of this function. The client cannot compose
/// its hello without the PSK and the fingerprint in ours, so a host that read
/// first would wait for a message the client cannot yet write, and the client
/// would wait for the message the host is refusing to send: a deadlock with no
/// error on either side, which every timeout in the tree would report as a
/// slow network.
async fn exchange_hellos<R, W>(
    reader: &mut R,
    writer: &mut W,
    hello: &Signal,
) -> anyhow::Result<ClientFacts>
where
    R: tokio::io::AsyncBufReadExt + Unpin,
    W: AsyncWrite + Unpin,
{
    write_signal_async(writer, hello)
        .await
        .context("offering the session to the client")?;

    match oxutrm_host::signalling::read_signal_async(reader)
        .await
        .context("reading the client's hello")?
    {
        Signal::ClientHello {
            cert_spki_sha256,
            candidates,
            nat_type,
            caps,
            size,
            ..
        } => Ok(ClientFacts {
            cert_spki_sha256,
            candidates,
            nat_type,
            caps,
            size,
        }),
        // The client's own words, not ours. It is the only explanation
        // anybody has for why this attach is not going to happen.
        Signal::Failed { reason } => Err(anyhow::anyhow!(
            "the client gave up before the ladder ran: {reason}"
        )),
        other => Err(anyhow::anyhow!(
            "the client answered the offer with {other:?} instead of its hello"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session record as R4 finds it, before any attach.
    fn fresh_meta(session_id: &str) -> SessionMeta {
        SessionMeta {
            session_id: session_id.to_owned(),
            attach_id: 0,
            pid: std::process::id(),
            created_unix: 0,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: false,
        }
    }

    /// R11 is the caller's, not the exchange's.
    ///
    /// `detachable` is settled from the *nominated rung* by
    /// `settle_detachability`, which the first connect calls and a reattach
    /// must not. If the exchange ever sets it, a reattach would re-decide a
    /// question that was answered once, with a rung that means something
    /// different over a Unix socket than it does over ssh.
    #[tokio::test]
    async fn the_exchange_never_settles_detachability() {
        let mut meta = fresh_meta("seam");
        // The client hangs up immediately. Dropping one end of a duplex
        // stream closes both directions, so it is the host's own `HostHello`
        // write at R6 that fails first -- the exchange never reaches R7's
        // read of the client's hello. What it did to `meta` before failing
        // is the point.
        let (client, host) = tokio::io::duplex(64);
        drop(client);
        let (r, w) = tokio::io::split(host);
        // Public STUN servers are a list of hopes, not of requirements
        // (`stun_discover`'s own words), and an empty list is a supported
        // configuration -- not this test reaching the internet for a check
        // that has nothing to do with STUN.
        let _ = run_attach_exchange(
            tokio::io::BufReader::new(r),
            w,
            &mut meta,
            &NetConfig {
                stun_servers: vec![],
                enable_port_mapping: false,
                enable_birthday: false,
                ..Default::default()
            },
        )
        .await;

        assert!(
            !meta.detachable,
            "the exchange settled detachability itself; that is R11 and it \
             belongs to the caller, or a reattach re-decides it: {meta:?}"
        );
        assert_eq!(
            meta.attach_id, 1,
            "the exchange must still bump the generation — that IS R4 — but it \
             got {}: {meta:?}",
            meta.attach_id
        );
    }

    // ---- the hello exchange, and the order that is its whole content ---------

    use oxutrm_proto::CandidateKind;

    fn candidate(port: u16) -> Candidate {
        Candidate {
            addr: format!("192.0.2.1:{port}").parse().expect("a test address"),
            kind: CandidateKind::PeerReflexive,
            priority: 1,
        }
    }

    /// A session record with a fresh attach on it, as R4 produces.
    fn attach_of(session_id: &str) -> (SessionMeta, oxutrm_host::Attach) {
        let mut meta = SessionMeta {
            session_id: session_id.to_owned(),
            attach_id: 0,
            pid: std::process::id(),
            created_unix: 0,
            shell: "/bin/sh".to_owned(),
            size: TermSize { cols: 80, rows: 24 },
            detachable: false,
        };
        let attach = oxutrm_host::begin_attach(&mut meta, HostSpki::new([7u8; 32]))
            .expect("fresh key material");
        (meta, attach)
    }

    fn a_client_hello() -> Signal {
        Signal::ClientHello {
            proto: oxutrm_proto::PROTO_VERSION,
            cert_spki_sha256: ClientSpki::new([9u8; 32]),
            candidates: vec![candidate(6001)],
            nat_type: NatType::AddressDependent,
            caps: oxutrm_term::detect_caps(),
            size: TermSize {
                cols: 132,
                rows: 43,
            },
        }
    }

    /// No deadlock is visible from one side, which is why this is asserted
    /// against a peer that behaves like the real client: it says nothing until
    /// it has read a `HostHello`, because it cannot compose its own reply
    /// without the PSK and the fingerprint inside one. A host that read first
    /// would hang here, and in the field it would look like a slow network.
    #[tokio::test]
    async fn the_host_speaks_first_so_a_client_that_waits_is_not_a_deadlock() {
        let (ours, theirs) = tokio::io::duplex(64 * 1024);
        let (mut our_r, mut our_w) = tokio::io::split(ours);
        let mut our_r = tokio::io::BufReader::new(&mut our_r);

        let client = tokio::spawn(async move {
            let (r, mut w) = tokio::io::split(theirs);
            let mut r = tokio::io::BufReader::new(r);
            let offer = oxutrm_host::signalling::read_signal_async(&mut r)
                .await
                .expect("the host must speak first");
            assert!(
                matches!(offer, Signal::HostHello { .. }),
                "the first thing on the wire was not the host's offer: {offer:?}"
            );
            oxutrm_host::signalling::write_signal_async(&mut w, &a_client_hello())
                .await
                .expect("the client replies");
            // Held: dropping the write half here would close the pipe.
            std::future::pending::<()>().await;
        });

        let (_meta, attach) = attach_of("d1e5");
        let hello = host_hello("d1e5", &attach, vec![candidate(3001)], NatType::None, 4433);

        let facts = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exchange_hellos(&mut our_r, &mut our_w, &hello),
        )
        .await
        .expect("reading before writing deadlocks both ends")
        .expect("the exchange must succeed against a well-behaved client");

        assert_eq!(facts.size.cols, 132);
        assert_eq!(facts.nat_type, NatType::AddressDependent);
        assert_eq!(facts.candidates.len(), 1);
        client.abort();
    }

    /// The offer carries this attach's own material, not the session's.
    /// Fresh keys per attach are what stop a PSK captured from a previous one
    /// being usable, and the bumped `attach_id` is what lets the two ends agree
    /// on which generation their `seq` counters belong to.
    #[test]
    fn the_offer_carries_this_attachs_own_generation_and_port() {
        let (_meta, attach) = attach_of("beef");
        let hello = host_hello("beef", &attach, vec![candidate(3001)], NatType::None, 4433);

        match hello {
            Signal::HostHello {
                proto,
                session_id,
                attach_id,
                bound_port,
                candidates,
                nat_type,
                cert_spki_sha256,
                ..
            } => {
                assert_eq!(proto, oxutrm_proto::PROTO_VERSION);
                assert_eq!(session_id, "beef");
                assert_eq!(attach_id, attach.attach_id, "a stale generation");
                assert_eq!(bound_port, 4433);
                assert_eq!(candidates.len(), 1);
                assert_eq!(nat_type, NatType::None);
                assert_eq!(cert_spki_sha256, attach.keys.cert_spki_sha256());
            }
            other => panic!("host_hello composed something else: {other:?}"),
        }
    }

    /// `detachable` here is INTENT. The ladder has not run — its candidates are
    /// inside this very message — so nothing yet knows the rung, and the
    /// outcome is settled later by `settle_detachability`. Computing a value
    /// here could only produce a guess the client would believe.
    #[test]
    fn the_offer_states_the_intent_to_detach_because_the_outcome_is_not_known_yet() {
        let (_meta, attach) = attach_of("beef");
        let hello = host_hello("beef", &attach, Vec::new(), NatType::Symmetric, 0);
        match hello {
            Signal::HostHello { detachable, .. } => assert!(
                detachable,
                "the host's offer must state its intent; the rung settles the outcome"
            ),
            other => panic!("host_hello composed something else: {other:?}"),
        }
    }

    /// A client that gives up says why. Reporting "unexpected message" instead
    /// would throw away the only explanation anyone has.
    #[tokio::test]
    async fn a_client_that_gives_up_is_reported_with_its_own_reason() {
        let (ours, mut theirs) = tokio::io::duplex(64 * 1024);
        let (mut our_r, mut our_w) = tokio::io::split(ours);
        let mut our_r = tokio::io::BufReader::new(&mut our_r);

        let (_meta, attach) = attach_of("dead");
        let hello = host_hello("dead", &attach, Vec::new(), NatType::None, 4433);

        let peer = tokio::spawn(async move {
            let mut r = tokio::io::BufReader::new(&mut theirs);
            let _ = oxutrm_host::signalling::read_signal_async(&mut r).await;
            oxutrm_host::signalling::write_signal_async(
                &mut theirs,
                &Signal::Failed {
                    reason: "no usable network interface".to_owned(),
                },
            )
            .await
            .expect("the client reports why");
            std::future::pending::<()>().await;
        });

        let error = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            exchange_hellos(&mut our_r, &mut our_w, &hello),
        )
        .await
        .expect("the exchange must not hang on a refusal")
        .expect_err("a client that gave up is not a successful exchange");

        assert!(
            format!("{error:#}").contains("no usable network interface"),
            "the client's own reason was thrown away: {error:#}"
        );
        peer.abort();
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
    use oxutrm_proto::{HostSpki, Rung};
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
}

//! The host's accept path: one connection, from one address, or nothing.
//!
//! This lives in the root binary because that is the only place that owns both
//! halves — `oxutrm-net`'s endpoint and `oxutrm-term`'s shell — and because
//! `oxutrm-host` is forbidden from ever seeing a `quinn::Endpoint`
//! (`crates/oxutrm-host/tests/no_net.rs` machine-checks that edge).
//!
//! # Why the hardening is written now, with the loop
//!
//! There is no accept path in the tree yet, so there is nowhere to add these
//! checks "later". Every one of them is cheap, and none of them substitutes
//! for the client-certificate pin in `quic_server` — that pin is what makes
//! this loop's output an authenticated peer rather than a stranger. What
//! follows is what the pin does *not* cover:
//!
//! * **Not the nominated address → `ignore()`, never `refuse()`.** ICE has
//!   already settled which remote this attach talks to. `refuse()` sends a
//!   CONNECTION_REFUSED, which tells a port scanner that something is
//!   listening on the punched port; `ignore()` drops the datagram and the
//!   scanner learns nothing. The socket's exact IP and port are handed to a
//!   third-party STUN operator by default, so "nobody knows the port" is not a
//!   defence anyone should lean on.
//! * **Unvalidated source → `retry()`.** A spoofed-source flood otherwise
//!   makes the host mint connection state and send a large response to an
//!   address that never asked. The retry token costs the honest client one
//!   extra round trip on the first attempt and nothing afterwards.
//! * **Exactly one connection per attach.** A second inbound connection is
//!   always wrong under this model: one attach is one client. Roaming does not
//!   need a second one — a roam reuses the existing connection through QUIC
//!   path validation, not a new handshake.

use std::net::SocketAddr;

use anyhow::{Context as _, Result};

/// Wait for the one connection this attach expects.
///
/// Returns as soon as a handshake from `nominated` completes. Anything else —
/// a different source address, an unvalidated one — is handled inside the loop
/// and never reaches the caller, so the caller has no third case to get wrong.
///
/// A **failed** handshake, on the other hand, does come back as `Err`. That is
/// deliberate: the mandatory client-certificate pin in `oxutrm_net::quic_server`
/// fails exactly here, and a loop that swallowed it would turn "this peer is
/// not our client" into "still waiting", which is the shape of every accept
/// loop that quietly lets the next attempt through.
pub async fn accept_one(
    endpoint: &quinn::Endpoint,
    nominated: SocketAddr,
) -> Result<quinn::Connection> {
    let nominated = oxutrm_net::unmap(nominated);
    loop {
        let Some(incoming) = endpoint.accept().await else {
            anyhow::bail!("the QUIC endpoint closed before the client connected");
        };

        // `unmap` on both sides: a dual-stack endpoint reports an IPv4 peer as
        // `::ffff:a.b.c.d`, and comparing that with the nominated `a.b.c.d`
        // would reject the very peer ICE just agreed on.

        if oxutrm_net::unmap(incoming.remote_address()) != nominated {
            // Silence, not a refusal. See the module docs.
            incoming.ignore();
            continue;
        }

        if !incoming.remote_address_validated() {
            // `retry()` hands back the `Incoming` if it could not send the
            // token; there is nothing useful to do with it but drop it
            // quietly, which is the same answer as above.
            if let Err(e) = incoming.retry() {
                e.into_incoming().ignore();
            }
            continue;
        }

        return incoming
            .await
            .context("completing the QUIC handshake with the nominated peer");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use oxutrm_net::{generate_cert, quic_client, quic_server};
    use oxutrm_proto::{ClientSpki, HostSpki, TermSize};
    use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer};

    use crate::link::Link;
    use crate::session::HostSession;

    fn size() -> TermSize {
        TermSize { cols: 80, rows: 24 }
    }

    async fn udp() -> Arc<tokio::net::UdpSocket> {
        Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    /// One client identity: what it presents, and what the host would have to
    /// have been told in order to accept it.
    struct ClientId {
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        spki: ClientSpki,
    }

    impl ClientId {
        fn new() -> ClientId {
            let (cert, key, fp) = generate_cert().unwrap();
            ClientId {
                cert,
                key,
                spki: ClientSpki::new(fp),
            }
        }

        /// A fresh pair with the same identity, because `quic_client` consumes
        /// the certificate and one identity may connect more than once in a
        /// single test.
        fn again(&self) -> (CertificateDer<'static>, PrivateKeyDer<'static>) {
            (self.cert.clone(), self.key.clone_key())
        }
    }

    /// **The spawn counter.**
    ///
    /// It counts calls to `HostSession::spawn` — the step that runs
    /// `HostTerm::spawn` → `Pty::spawn` → `Command::new(shell)` — and *not*
    /// `Ok`/`Err` from a handshake. That distinction is this repository's own
    /// lesson, from `a40ed8f`: the flood gate asserted `rejected == 0` and
    /// passed happily while the path it guarded was completely broken, because
    /// the assertion could not see the regime it was about. "The handshake
    /// failed" is an implementation detail; "a shell was started for a peer we
    /// never authorised" is the harm.
    #[derive(Clone, Default)]
    struct Shells(Arc<AtomicUsize>);

    impl Shells {
        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    /// A host endpoint that serves attach after attach on ONE endpoint,
    /// starting a real shell for every connection `accept_one` hands it.
    ///
    /// The loop is deliberately *more* permissive than production: it comes
    /// back for another attempt after a failed handshake, where a real attach
    /// takes one connection and stops. That makes "B never reaches a shell" a
    /// stronger claim, not a weaker one — B is given every chance and still
    /// gets nowhere.
    fn serve(
        endpoint: quinn::Endpoint,
        socket: Arc<tokio::net::UdpSocket>,
        nominated: SocketAddr,
        shells: Shells,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut live = Vec::new();
            loop {
                match accept_one(&endpoint, nominated).await {
                    Ok(conn) => {
                        let session = HostSession::spawn(
                            "/bin/sh",
                            size(),
                            200,
                            Link::new(conn, endpoint.clone(), Arc::clone(&socket)),
                        )
                        .expect("the shell must start once we get this far");
                        shells.0.fetch_add(1, Ordering::SeqCst);
                        // Held so the PTY is not reaped mid-test; dropped with
                        // the task.
                        live.push(session);
                    }
                    Err(_) => continue,
                }
            }
        })
    }

    /// One connection attempt, with its own socket, bounded in time.
    ///
    /// The result is discarded on purpose. In TLS 1.3 the client finishes its
    /// handshake before the server has verified the client certificate, and
    /// quinn reports `Connected` as soon as rustls stops handshaking — so this
    /// resolves `Ok` for a client the host is about to throw out. **The spawn
    /// counter is the evidence; this return value is noise.** Asserting on it
    /// is the trap this test file exists to avoid.
    async fn attempt(host_addr: SocketAddr, host_fp: [u8; 32], id: &ClientId) -> SocketAddr {
        let sock = udp().await;
        let addr = sock.local_addr().unwrap();
        let (cert, key) = id.again();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            quic_client(&sock, host_addr, HostSpki::new(host_fp), cert, key),
        )
        .await;
        addr
    }

    /// A client that connects from a socket the caller chose, so the caller
    /// knows the address before the connection is made.
    async fn attempt_from(
        sock: &Arc<tokio::net::UdpSocket>,
        host_addr: SocketAddr,
        host_fp: [u8; 32],
        id: &ClientId,
    ) {
        let (cert, key) = id.again();
        let _ = tokio::time::timeout(
            Duration::from_secs(10),
            quic_client(sock, host_addr, HostSpki::new(host_fp), cert, key),
        )
        .await;
    }

    /// Long enough for a completed handshake to have reached `HostSession::spawn`.
    ///
    /// Used only where the expected count is ZERO — a sleep that is too short
    /// makes such a test pass for the wrong reason, so the positive halves
    /// below poll until the count rises instead of sleeping.
    async fn settle() {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    async fn wait_for(shells: &Shells, n: usize) {
        for _ in 0..200 {
            if shells.count() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!(
            "expected {n} shell(s), the host started {} — the accepted half of \
             this test is what proves the pin is wired from the right place",
            shells.count()
        );
    }

    // ----------------------------------------------------------------------
    // accepts A, rejects B, on ONE endpoint
    // ----------------------------------------------------------------------

    /// The core assertion, and it needs **both** halves.
    ///
    /// A permissive pin — `with_no_client_auth()`, a stubbed verifier, a
    /// `client_auth_mandatory()` of `false` — fails the first half. A pin
    /// wired from the wrong place — the host pinning its own fingerprint, the
    /// swap that used to type-check — fails the second, because then nobody
    /// gets in and "B was rejected" would be true for a reason that has
    /// nothing to do with B.
    ///
    /// An error-variant assertion could not distinguish those two worlds:
    /// in both of them a handshake fails.
    async fn accepts_a_rejects_b(a: ClientId, b: ClientId) {
        let (cert, key, host_fp) = generate_cert().unwrap();
        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();

        // The client socket is bound first so its address can be nominated,
        // which is what a real attach has after ICE.
        let a_sock = udp().await;
        let nominated = a_sock.local_addr().unwrap();

        // Pinned to A. B is a perfectly well-formed oxutrm client with a
        // perfectly valid certificate; it is simply not the one that came in
        // over ssh.
        let (endpoint, _stun) = quic_server(&host_sock, cert, key, a.spki).await.unwrap();
        let shells = Shells::default();
        let host = serve(endpoint, Arc::clone(&host_sock), nominated, shells.clone());

        // B first, from the nominated address, so the address hardening is not
        // what rejects it — the certificate pin has to be.
        attempt_from(&a_sock, host_addr, host_fp, &b).await;
        settle().await;
        assert_eq!(
            shells.count(),
            0,
            "a client the host never pinned reached HostSession::spawn, which is \
             Command::new(shell) as the ssh user"
        );

        // A, on the same endpoint, through the same accept loop.
        attempt_from(&a_sock, host_addr, host_fp, &a).await;
        wait_for(&shells, 1).await;
        assert_eq!(shells.count(), 1);

        host.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_endpoint_accepts_the_pinned_client_and_rejects_the_other() {
        accepts_a_rejects_b(ClientId::new(), ClientId::new()).await;
    }

    /// The same test with the roles swapped, because "A is accepted" must not
    /// be a property of the order two certificates were generated in, or of
    /// which one happened to be tried first.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_holds_with_the_two_clients_swapped() {
        let first = ClientId::new();
        let second = ClientId::new();
        accepts_a_rejects_b(second, first).await;
    }

    // ----------------------------------------------------------------------
    // the accept-loop hardening
    // ----------------------------------------------------------------------

    /// ICE nominated one remote. Anything from anywhere else is dropped
    /// *silently* — the connection attempt simply never completes, and the
    /// caller learns nothing about whether a host is there.
    ///
    /// Note what this does **not** claim: it is not a security boundary on its
    /// own. A source address is forgeable and this check is defence in depth
    /// behind the certificate pin, which is the part that authenticates.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_connection_from_an_address_ice_did_not_nominate_reaches_no_shell() {
        let (cert, key, host_fp) = generate_cert().unwrap();
        let client = ClientId::new();
        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();

        // A nominated address nobody is using. The client below is correctly
        // pinned and would otherwise be let straight in, which is what makes
        // this test about the address and nothing else.
        let elsewhere: SocketAddr = "127.0.0.1:9".parse().unwrap();

        let (endpoint, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();
        let shells = Shells::default();
        let host = serve(endpoint, Arc::clone(&host_sock), elsewhere, shells.clone());

        attempt(host_addr, host_fp, &client).await;
        settle().await;
        assert_eq!(
            shells.count(),
            0,
            "a connection from an address ICE never nominated reached a shell"
        );

        host.abort();
    }

    /// The control for the test above: the same client, the same certificate,
    /// nominated this time. Without it, `accept_one` could be ignoring
    /// *everything* and the test above would still be green.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_nominated_address_is_the_one_that_gets_through() {
        let (cert, key, host_fp) = generate_cert().unwrap();
        let client = ClientId::new();
        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();
        let client_sock = udp().await;
        let nominated = client_sock.local_addr().unwrap();

        let (endpoint, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();
        let shells = Shells::default();
        let host = serve(endpoint, Arc::clone(&host_sock), nominated, shells.clone());

        attempt_from(&client_sock, host_addr, host_fp, &client).await;
        wait_for(&shells, 1).await;

        host.abort();
    }

    /// One attach is one client, so exactly one shell is started even when the
    /// same authorised client connects twice.
    ///
    /// The loop under test here is the test harness's `serve`, which keeps
    /// accepting — so what this pins is that a *second* connection is a
    /// distinct event the production caller must decline, not something
    /// `accept_one` folds into the first. `accept_one` returns one connection
    /// and then returns; there is no second connection to be had without
    /// calling it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn accept_one_returns_exactly_one_connection() {
        let (cert, key, host_fp) = generate_cert().unwrap();
        let client = ClientId::new();
        let host_sock = udp().await;
        let host_addr = host_sock.local_addr().unwrap();
        let client_sock = udp().await;
        let nominated = client_sock.local_addr().unwrap();

        let (endpoint, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();
        let shells = Shells::default();

        // One call, and then nothing accepts any more.
        let ep = endpoint.clone();
        let sock = Arc::clone(&host_sock);
        let counter = shells.clone();
        let host = tokio::spawn(async move {
            let conn = accept_one(&ep, nominated).await.expect("the first client");
            let session =
                HostSession::spawn("/bin/sh", size(), 200, Link::new(conn, ep, sock)).unwrap();
            counter.0.fetch_add(1, Ordering::SeqCst);
            // Keep it alive so the test is not measuring a dropped session.
            std::future::pending::<()>().await;
            drop(session);
        });

        attempt_from(&client_sock, host_addr, host_fp, &client).await;
        wait_for(&shells, 1).await;

        // A second attach, from a second socket, by the very client the host
        // does trust. It must still reach no shell: nothing is accepting.
        attempt(host_addr, host_fp, &client).await;
        settle().await;
        assert_eq!(
            shells.count(),
            1,
            "a second connection started a second shell on the same attach"
        );

        host.abort();
    }
}

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
//!   path validation, not a new handshake. This one is not a check inside the
//!   loop but a property of the signature: [`accept_one`] consumes an
//!   [`AcceptPermit`], `quic_server` mints exactly one per endpoint, and a
//!   second call does not compile. See that type for why the rule was moved
//!   out of this comment and into the type system.
//! * **A deadline.** Every bullet above is about a datagram that *arrived*.
//!   [`ACCEPT_TIMEOUT`] is about the one that never does.

use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context as _, Result};
use oxutrm_net::AcceptPermit;

/// How long the host waits for its one client before giving up.
///
/// **Not a tuning knob — the alternative to it is a leak.** Without a deadline
/// `Endpoint::accept()` simply never returns when no datagram ever arrives, and
/// the host is left parked on it for ever: a registered session in the
/// registry, a process holding a punched socket, and no shell behind either.
/// Nothing else in the attach path can notice, because from the accept's point
/// of view "silent" and "still coming" are the same thing.
///
/// Thirty seconds, and the number now stands on its own. It used to be
/// justified by matching the transport's `max_idle_timeout`: a handshake still
/// unfinished after thirty idle seconds was one quinn was about to abandon
/// anyway, so the deadline cut nothing short. Phase 2 set that timeout to
/// `None`, so there is no longer anything to match and quinn will abandon
/// nothing.
///
/// That makes this deadline MORE load-bearing, not less. It is now the only
/// bound on the case it always described -- no peer ever spoke, so there is no
/// connection and nothing for a transport timeout to fire on. Without it
/// `--serve` parks on `Endpoint::accept()` for ever, holding a registered
/// session, a punched socket and a shell nobody can reach; and reattach does
/// not exist yet, so such a session cannot be reclaimed, only killed by PID.
///
/// It stays generous for the case that matters. ICE has already completed
/// connectivity checks over this very path by the time anything here runs, so
/// the client is known reachable and its `ClientHello` is one round trip away;
/// the address-validation `retry()` below costs one more. Thirty seconds is
/// tens of round trips on a link bad enough to be worth keeping.
pub const ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

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
///
/// # One connection, enforced by the signature
///
/// The [`AcceptPermit`] is consumed. `oxutrm_net::quic_server` mints one per
/// endpoint, this is the only thing in the tree that takes one, and calling
/// this function twice is therefore a compile error rather than a rule in a
/// comment.
///
/// The permit comes **back** in [`AcceptFailed::retry`] when — and only when —
/// the attempt ended with a peer that reached the handshake and failed it. No
/// connection was admitted, so the attach's one connection is still unspent,
/// and a caller that wants to keep listening may. A caller that does not want
/// to is written `?`, which drops the permit and ends the attach; that is what
/// `run_host --serve` will do, because under ICE the peer at the nominated
/// address *is* the client, and a client that fails the certificate pin is not
/// going to pass it on the next try.
///
/// Nothing comes back when the deadline expires or the endpoint closes: in
/// neither case is there anything left to wait for.
pub async fn accept_one(
    permit: AcceptPermit,
    nominated: SocketAddr,
) -> Result<quinn::Connection, AcceptFailed> {
    let nominated = oxutrm_net::unmap(nominated);

    // Bound the WHOLE loop, not one `accept()`. A deadline on the individual
    // call would be no deadline at all: every `ignore()`d datagram from a
    // stranger would start it again, so anyone able to send a packet a second
    // could hold the host open indefinitely.
    let outcome =
        tokio::time::timeout(ACCEPT_TIMEOUT, accept_loop(permit.endpoint(), nominated)).await;

    match outcome {
        Ok(Ok(connection)) => Ok(connection),
        Ok(Err(Rejection::Peer(error))) => AcceptFailed::retryable(permit, error),
        Ok(Err(Rejection::Endpoint(error))) => AcceptFailed::final_(error),
        Err(_) => AcceptFailed::final_(anyhow::anyhow!(
            "no client completed a QUIC handshake from {nominated} within \
             {ACCEPT_TIMEOUT:?}"
        )),
    }
}

/// A failed accept, and — when the attach still has its one connection left to
/// spend — the permit to try again.
///
/// # Why the permit lives in the error
///
/// It is the only place it can live where a caller cannot get it wrong. On
/// success the permit is spent and there is nothing to hand back; on failure
/// whether it is spent is precisely what the caller cannot work out for itself,
/// because both outcomes look like "no connection". Putting it here makes the
/// question unaskable in the success path and unavoidable in the failure one.
///
/// A caller that does not care writes `?`: [`AcceptFailed`] is a
/// `std::error::Error`, so it converts to `anyhow::Error` and the permit is
/// dropped with it. That is the right default — under ICE the peer at the
/// nominated address *is* the client, and a client that fails the certificate
/// pin will not pass it on the next try.
pub struct AcceptFailed {
    error: anyhow::Error,
    /// `Some` only when a peer reached the handshake and failed it.
    /// Read only by [`AcceptFailed::retry`], which has no caller.
    #[allow(dead_code)]
    permit: Option<AcceptPermit>,
}

impl AcceptFailed {
    /// A peer arrived from the nominated address and failed the handshake.
    ///
    /// Nothing was admitted, so the attach's one connection is still unspent
    /// and the permit comes back with the error.
    fn retryable(permit: AcceptPermit, error: anyhow::Error) -> Result<quinn::Connection, Self> {
        Err(AcceptFailed {
            error,
            permit: Some(permit),
        })
    }

    /// Nothing is left to wait for: the deadline expired, or the endpoint is
    /// gone. The permit dies with the attempt, because there is no attempt
    /// left to make.
    fn final_(error: anyhow::Error) -> Result<quinn::Connection, Self> {
        Err(AcceptFailed {
            error,
            permit: None,
        })
    }

    /// The unspent permit, if this failure left one.
    ///
    /// Consuming `self` is the point: the error and the permit cannot both be
    /// kept, so "I am going to try again" and "I am going to report this" stay
    /// the exclusive choices they are on the wire.
    #[must_use]
    /// **No caller, on either side.** `run_host --serve` writes `?`, which is
    /// the right default under ICE -- the peer at the nominated address IS the
    /// client -- and `run_connect` never accepts at all. The affordance is
    /// kept because the alternative is worse: without it the permit would have
    /// to be dropped inside the error path, and "was the attach's one
    /// connection spent?" would become a question the caller cannot answer.
    /// Recorded here rather than deleted, and recorded as unused rather than
    /// given a fabricated call site.
    #[allow(dead_code)]
    pub fn retry(self) -> Option<AcceptPermit> {
        self.permit
    }
}

/// Delegated to the inner `anyhow::Error` — including its report and backtrace,
/// which is what an `expect_err` in a test should print.
impl std::fmt::Debug for AcceptFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.error, f)
    }
}

impl std::fmt::Display for AcceptFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.error, f)
    }
}

/// This impl is what makes `?` work on `accept_one`, and with it the "drop the
/// permit and end the attach" default described above.
impl std::error::Error for AcceptFailed {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.error.source()
    }
}

/// Why one pass of the accept ended.
///
/// The distinction is the only thing the caller cannot work out for itself,
/// and it is exactly the one that decides whether the permit survives.
enum Rejection {
    /// A peer arrived from the nominated address and failed the handshake —
    /// the certificate pin, almost always. Nothing was admitted.
    Peer(anyhow::Error),
    /// The endpoint is gone. Nothing will ever arrive on it again.
    Endpoint(anyhow::Error),
}

/// The accept itself, with no deadline and no permit: both belong to
/// [`accept_one`], which is the only caller and must stay so.
async fn accept_loop(
    endpoint: &quinn::Endpoint,
    nominated: SocketAddr,
) -> Result<quinn::Connection, Rejection> {
    loop {
        let Some(incoming) = endpoint.accept().await else {
            return Err(Rejection::Endpoint(anyhow::anyhow!(
                "the QUIC endpoint closed before the client connected"
            )));
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

        // A peer that got this far and failed is the retryable case: it
        // reached the handshake, so nothing was admitted and the permit is
        // still good. `Endpoint` is for faults with nothing left behind them.
        return incoming
            .await
            .context("completing the QUIC handshake with the nominated peer")
            .map_err(Rejection::Peer);
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
        permit: AcceptPermit,
        socket: Arc<tokio::net::UdpSocket>,
        nominated: SocketAddr,
        shells: Shells,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut live = Vec::new();
            let mut permit = permit;
            loop {
                match accept_one(permit, nominated).await {
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
                        // The permit is spent. A real attach stops here too.
                        return;
                    }
                    // The permissiveness this harness is documented to have now
                    // has a name: the permit comes back only when a peer
                    // reached the handshake and failed it, and coming back for
                    // another attempt is what B must survive and A must then
                    // get through. When it does not come back there is nothing
                    // left to wait on.
                    Err(failed) => match failed.retry() {
                        Some(unspent) => permit = unspent,
                        None => return,
                    },
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
        let (endpoint, permit, _stun) = quic_server(&host_sock, cert, key, a.spki).await.unwrap();
        let shells = Shells::default();
        let host = serve(
            endpoint,
            permit,
            Arc::clone(&host_sock),
            nominated,
            shells.clone(),
        );

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

        let (endpoint, permit, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();
        let shells = Shells::default();
        let host = serve(
            endpoint,
            permit,
            Arc::clone(&host_sock),
            elsewhere,
            shells.clone(),
        );

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

        let (endpoint, permit, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();
        let shells = Shells::default();
        let host = serve(
            endpoint,
            permit,
            Arc::clone(&host_sock),
            nominated,
            shells.clone(),
        );

        attempt_from(&client_sock, host_addr, host_fp, &client).await;
        wait_for(&shells, 1).await;

        host.abort();
    }

    /// The accept gives up on its own, and it does so at `ACCEPT_TIMEOUT`.
    ///
    /// The outer 120 s is the harness's own patience, not the thing under
    /// test: if `accept_one` had no deadline at all this would still terminate,
    /// and the assertion on `elapsed` is what tells the two apart. Under
    /// `start_paused` the clock jumps to the next deadline whenever the runtime
    /// idles, so a thirty-second wait costs no wall-clock seconds and the
    /// assertion is on the very const the code uses.
    #[tokio::test(start_paused = true)]
    async fn the_accept_gives_up_when_no_client_ever_arrives() {
        let (cert, key, _host_fp) = generate_cert().unwrap();
        let client = ClientId::new();
        let host_sock = udp().await;
        let nominated: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (_endpoint, permit, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();

        let started = tokio::time::Instant::now();
        let outcome =
            tokio::time::timeout(Duration::from_secs(120), accept_one(permit, nominated)).await;
        let elapsed = started.elapsed();
        assert!(outcome.is_ok(), "accept_one never gave up: {elapsed:?}");
        assert!(
            elapsed >= ACCEPT_TIMEOUT,
            "accept_one gave up after {elapsed:?}, short of its own ACCEPT_TIMEOUT"
        );
    }

    /// A deadline that expires spends the permit; a failed handshake does not.
    ///
    /// This is the whole point of the two constructors, and the only assertion
    /// that can tell them apart. Nothing arrived here, so there is no peer to
    /// give a second chance to — and a caller that retried on a silent endpoint
    /// would hold the attach open for ever, which is the shape the deadline
    /// exists to prevent. `accepts_a_rejects_b` pins the other half: there, the
    /// permit must come back or A could never get in behind B.
    #[tokio::test(start_paused = true)]
    async fn a_deadline_that_expires_hands_no_permit_back() {
        let (cert, key, _host_fp) = generate_cert().unwrap();
        let client = ClientId::new();
        let host_sock = udp().await;
        let nominated: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let (_endpoint, permit, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();

        let failed = accept_one(permit, nominated)
            .await
            .expect_err("nothing ever connected");
        assert!(
            failed.retry().is_none(),
            "the deadline handed the permit back, so a caller may keep an \
             attach open on an endpoint no client is using"
        );
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

        let (endpoint, permit, _stun) = quic_server(&host_sock, cert, key, client.spki)
            .await
            .unwrap();
        let shells = Shells::default();

        // One call, and then nothing accepts any more.
        let ep = endpoint.clone();
        let sock = Arc::clone(&host_sock);
        let counter = shells.clone();
        let host = tokio::spawn(async move {
            let conn = accept_one(permit, nominated)
                .await
                .expect("the first client");
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

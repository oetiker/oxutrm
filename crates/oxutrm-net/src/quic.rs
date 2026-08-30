//! QUIC over the socket the ladder punched.
//!
//! The endpoint is always built with `Endpoint::new_with_abstract_socket` over
//! a [`StunDemuxSocket`], **never** with `Endpoint::new`: `Endpoint::new` runs
//! its own receive loop on the raw socket and races every STUN receiver on it.
//!
//! ICE has already nominated a pair by the time anything here is called. QUIC
//! connection migration lets a client change its own *local* address and
//! nothing more; there is no protocol mechanism and no `quinn` API to repoint
//! an established connection at a different *remote* address, so the pair has
//! to be settled first.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use oxutrm_proto::{ClientSpki, HostSpki};
use quinn::rustls;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::demuxsock::{StunDemuxSocket, StunRx};
use crate::socketfam::to_socket_family;
use crate::tls::{CERT_NAME, PinnedClientSpki, PinnedSpki, install_crypto_provider, provider};

/// Honest ALPN.
///
/// oxutrm's packets genuinely are QUIC, so a protocol-classifying middlebox
/// sees an ordinary QUIC flow. oxutrm does not forge another party's domain in
/// SNI to hide: that is impersonation, and it buys nothing over honest framing.
pub const ALPN: &[u8] = b"oxutrm/1";

/// One megabyte each way. Screen deltas are small; the buffer only has to
/// absorb a burst.
const DATAGRAM_BUFFER: usize = 1024 * 1024;

/// The transport settings, with the one that fails silently.
///
/// **Both datagram buffer sizes must be set.** Omit either and QUIC datagrams
/// are quietly disabled: `Connection::max_datagram_size()` returns `None`,
/// `send_datagram` fails, and nothing anywhere mentions a buffer. It would
/// look like a protocol bug months later, in the screen-sync layer, which is
/// the part that would be blamed. Note the asymmetry in the two signatures —
/// `usize` here, `Option<usize>` there — it is real, not a typo.
fn transport_config() -> Arc<quinn::TransportConfig> {
    let mut t = quinn::TransportConfig::default();
    t.datagram_send_buffer_size(DATAGRAM_BUFFER);
    t.datagram_receive_buffer_size(Some(DATAGRAM_BUFFER));
    // No idle timeout, deliberately, and `None` rather than a deleted line:
    // quinn's DEFAULT is 30s, so removing the setter restores exactly what
    // this is here to remove.
    //
    // The transport must not adjudicate liveness. It has no idea what the
    // user is looking at, so all it can do about silence is kill a connection
    // that was about to recover -- which is what it did, at ~33s, for the
    // whole of phase 1. `LinkState` owns that verdict now: it raises a notice
    // at 2s, holds what is typed blind, and offers `Ctrl-\ q`. quinn warns
    // that an infinite timeout can hang a future for ever; the notice is the
    // answer to that, and it says more than a dead connection ever did.
    //
    // Consequence, stated because it is easy to miss: `conn.closed()` now
    // fires only on an explicit close or a transport error, never on silence.
    // Nothing may be built on it firing for a quiet peer -- see
    // `HostSession::attached`, which used to rely on exactly that.
    t.max_idle_timeout(None);
    // NOT redundant with the client's 0.2 Hz heartbeat, and it does not go
    // with the timeout above. The heartbeat exists so an answer is *owed*, on
    // the QUIC stream, where `LinkState` can see it. Keep-alive is what holds
    // a punched NAT binding open, which rungs 2 and 3 depend on for the whole
    // life of the connection -- and with no idle timeout, a connection can now
    // outlive a binding by a very long way.
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    Arc::new(t)
}

fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    expect_client_spki: ClientSpki,
) -> anyhow::Result<quinn::ServerConfig> {
    install_crypto_provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        // The line this file shipped with was `.with_no_client_auth()`, and
        // the builder's shape is why it survived so long: it is the *default*
        // way to get past `WantsVerifier`, it reads like boilerplate, and
        // nothing downstream of it fails.
        .with_client_cert_verifier(Arc::new(PinnedClientSpki::new(expect_client_spki)))
        .with_single_cert(vec![cert], key)
        .context("installing the session certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("building the QUIC server crypto config")?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn client_config(
    expect_host_spki: HostSpki,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::ClientConfig> {
    // Its own step, and not optional: without a process-default provider
    // `QuicClientConfig::try_from` below fails with an error that says nothing
    // about providers.
    install_crypto_provider();

    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedSpki::new(expect_host_spki)))
        // Required, not offered: the host's verifier is mandatory, so a client
        // built without a certificate cannot complete a handshake at all.
        // There is no `Option` here for the same reason there is none on the
        // host side — a client with nothing to present is not a configuration.
        .with_client_auth_cert(vec![cert], key)
        .context("installing the client's session certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .context("building the QUIC client crypto config")?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn endpoint_over(
    socket: &Arc<tokio::net::UdpSocket>,
    server: Option<quinn::ServerConfig>,
) -> anyhow::Result<(quinn::Endpoint, StunRx)> {
    let (demux, stun_rx) = StunDemuxSocket::new(socket)?;
    let runtime = quinn::default_runtime()
        .context("quinn needs an async runtime; call this from inside tokio")?;
    // `new_with_abstract_socket`, never `new`: `new` would start a second
    // receive loop on the raw socket and steal the STUN packets.
    let endpoint = quinn::Endpoint::new_with_abstract_socket(
        quinn::EndpointConfig::default(),
        server,
        demux,
        runtime,
    )
    .context("creating the QUIC endpoint")?;
    Ok((endpoint, stun_rx))
}

/// The right to accept **one** connection on the endpoint it was minted with.
///
/// One attach is one client. Roaming does not need a second connection — a
/// roam reuses the existing one through QUIC path validation, not a new
/// handshake — so a host that accepted twice would be serving a peer nobody
/// asked it to serve, with a second shell to prove it.
///
/// Until this type existed that rule lived in a module comment and in a test
/// that promised not to call the accept twice. This repository's standing
/// lesson is that **a rule written only in prose is a rule nobody implements**:
/// the reviewer who adds a retry loop around the accept is not being careless,
/// they are reading a function that looks safe to call again. So the rule is
/// now a value. [`quic_server`] mints exactly one of these per endpoint, the
/// accept consumes it by value, and there is no way to get a second one but to
/// build a second endpoint.
///
/// # What it is not
///
/// It is not a capability on `quinn::Endpoint::accept`, which stays reachable
/// to anyone holding the endpoint. It gates oxutrm's accept path, which is the
/// only path that reaches `Command::new(shell)`.
///
/// # The absence is machine-checked
///
/// A permit is spent by whatever consumes it. Using it twice does not compile:
///
/// ```compile_fail
/// use oxutrm_net::AcceptPermit;
/// fn accept(_: AcceptPermit) {}
/// fn permit() -> AcceptPermit { unimplemented!() }
/// let p = permit();
/// accept(p);
/// accept(p);
/// ```
///
/// Using it once does, which is what keeps the check above honest — a
/// `compile_fail` block passes just as happily on a typo as on the error it
/// was written for:
///
/// ```no_run
/// use oxutrm_net::AcceptPermit;
/// fn accept(_: AcceptPermit) {}
/// fn permit() -> AcceptPermit { unimplemented!() }
/// let p = permit();
/// accept(p);
/// ```
///
/// Nor can a second one be cloned out of the first — the escape hatch a
/// derive would open without anyone noticing:
///
/// ```compile_fail
/// use oxutrm_net::AcceptPermit;
/// fn needs_clone<T: Clone>(_: T) {}
/// fn permit() -> AcceptPermit { unimplemented!() }
/// needs_clone(permit());
/// ```
///
/// The same bound with a trait it *does* satisfy compiles, so the block above
/// fails for the missing `Clone` and not for the shape of the helper. `Send`
/// is not an arbitrary choice: the accept runs in a spawned task, so the
/// permit has to cross a task boundary to be usable at all.
///
/// ```no_run
/// use oxutrm_net::AcceptPermit;
/// fn needs_send<T: Send>(_: T) {}
/// fn permit() -> AcceptPermit { unimplemented!() }
/// needs_send(permit());
/// ```
///
/// And one cannot be conjured without an endpoint. The field is private, so
/// [`quic_server`] is the only place in the workspace that can name it:
///
/// ```compile_fail
/// use oxutrm_net::AcceptPermit;
/// fn endpoint() -> quinn::Endpoint { unimplemented!() }
/// let _ = AcceptPermit { endpoint: endpoint() };
/// ```
///
/// Its control, which also proves `quinn` resolves inside these doctests —
/// otherwise the block above would fail on an unresolved crate and prove
/// nothing at all:
///
/// ```no_run
/// fn endpoint() -> quinn::Endpoint { unimplemented!() }
/// fn takes(_: quinn::Endpoint) {}
/// takes(endpoint());
/// ```
#[derive(Debug)]
pub struct AcceptPermit {
    endpoint: quinn::Endpoint,
}

impl AcceptPermit {
    /// The endpoint this permit admits one connection on.
    #[must_use]
    pub fn endpoint(&self) -> &quinn::Endpoint {
        &self.endpoint
    }
}

/// Listen on a socket the ladder has already punched, accepting exactly one
/// client certificate: `expect_client_spki`, and nothing else.
///
/// The caller keeps `socket` for sending ICE keepalives, and receives the
/// peeled-off STUN on the returned [`StunRx`].
///
/// # The permit
///
/// The [`AcceptPermit`] in the middle of the tuple is the right to accept one
/// connection, and this is the only function that mints one. The endpoint
/// comes back beside it because the session layer needs the same handle
/// afterwards — `Endpoint::rebind` while roaming, and the link that carries
/// the screen — and the permit is spent by the accept.
///
/// # The fingerprint is by value, required, and has no setter
///
/// Not `Option<ClientSpki>`, not `Endpoint::set_client_pin(..)` afterwards.
/// rustls' `ServerConfig` is immutable once the endpoint exists, so the wrong
/// ordering — listen first, pin later — is not something a caller can express
/// badly; it is something there is no method to write. That is the
/// `Detached`/`DetachPermit` idiom from `oxutrm-host`'s `keys.rs` applied to a
/// transport, and it matters here because the window between "listening" and
/// "pinned" is precisely the window in which an unauthenticated peer reaches
/// `Command::new(shell)`.
///
/// The ordering this demands already exists in the plan: `ClientHello` is read
/// at R7 and the endpoint is built at R9, two steps later.
pub async fn quic_server(
    socket: &Arc<tokio::net::UdpSocket>,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
    expect_client_spki: ClientSpki,
) -> anyhow::Result<(quinn::Endpoint, AcceptPermit, StunRx)> {
    let (endpoint, stun_rx) =
        endpoint_over(socket, Some(server_config(cert, key, expect_client_spki)?))?;
    let permit = AcceptPermit {
        endpoint: endpoint.clone(),
    };
    Ok((endpoint, permit, stun_rx))
}

/// Connect to `peer`, trusting exactly `expect_host_spki` and nothing else,
/// and presenting `cert` so the host can do the same in return.
///
/// The fingerprint arrives as [`HostSpki`] rather than a bare `[u8; 32]`, and
/// that is the whole point. There are now two fingerprints on every attach and
/// both are in scope on both sides: the one the client pins, and the one the
/// client *presents*. They are the same size and the same shape, a swap here
/// would make the client pin itself, and there is no conversion between the
/// two types that would let one happen.
///
/// `cert`/`key` are a throwaway pair from [`crate::generate_cert`], minted
/// fresh per attach and never written to disk — the same call the host makes
/// for its own identity. The client's fingerprint travels to the host in
/// `ClientHello`.
///
/// The endpoint comes back alongside the connection because `quinn` drives the
/// socket from it and M4 needs the same handle for `Endpoint::rebind` when the
/// local address changes while roaming.
pub async fn quic_client(
    socket: &Arc<tokio::net::UdpSocket>,
    peer: SocketAddr,
    expect_host_spki: HostSpki,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<(quinn::Connection, quinn::Endpoint, StunRx)> {
    let (mut endpoint, stun_rx) = endpoint_over(socket, None)?;
    endpoint.set_default_client_config(client_config(expect_host_spki, cert, key)?);

    let local = endpoint
        .local_addr()
        .context("reading the endpoint's local address")?;
    let peer = to_socket_family(&local, peer);
    let connection = endpoint
        .connect(peer, CERT_NAME)
        .context("starting the QUIC handshake")?
        .await
        .context("completing the QUIC handshake")?;

    Ok((connection, endpoint, stun_rx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tls::generate_cert;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// The transport must not adjudicate liveness. quinn's DEFAULT is 30s, so
    /// deleting the setter would silently restore exactly what this removes --
    /// this asserts the negotiated value, not the absence of a line of code.
    #[test]
    fn the_transport_imposes_no_idle_timeout() {
        let cfg = transport_config();
        assert_eq!(
            format!("{cfg:?}").contains("max_idle_timeout: Some"),
            false,
            "an idle timeout is still set; the client will still die on silence: {cfg:?}"
        );
    }

    /// Keep-alive is NOT redundant with the client's heartbeat: it is what
    /// holds a punched NAT binding open, which rungs 2 and 3 depend on for the
    /// life of the connection. Removing the idle timeout must not take it too.
    #[test]
    fn keep_alive_survives_the_idle_timeout_going() {
        let cfg = transport_config();
        assert!(
            format!("{cfg:?}").contains("keep_alive_interval: Some"),
            "keep-alive went with the idle timeout; punched NAT bindings will lapse: {cfg:?}"
        );
    }

    async fn socket() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
    }

    /// A client identity: its certificate, its key, and the fingerprint the
    /// host has to be told about before it can listen.
    struct ClientId {
        cert: CertificateDer<'static>,
        key: PrivateKeyDer<'static>,
        spki: ClientSpki,
    }

    fn client_id() -> ClientId {
        let (cert, key, fp) = generate_cert().unwrap();
        ClientId {
            cert,
            key,
            spki: ClientSpki::new(fp),
        }
    }

    /// A server that accepts one connection and echoes whatever it is given,
    /// on both channels.
    fn echo_server(endpoint: quinn::Endpoint) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let Some(incoming) = endpoint.accept().await else {
                return;
            };
            let Ok(conn) = incoming.await else { return };

            let datagrams = conn.clone();
            tokio::spawn(async move {
                while let Ok(d) = datagrams.read_datagram().await {
                    if datagrams.send_datagram(d).is_err() {
                        return;
                    }
                }
            });

            while let Ok((mut send, mut recv)) = conn.accept_bi().await {
                let Ok(got) = recv.read_to_end(1 << 20).await else {
                    return;
                };
                if send.write_all(&got).await.is_err() {
                    return;
                }
                let _ = send.finish();
            }
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_datagram_goes_round_trip() {
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let me = client_id();
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _permit, _stun) =
            quic_server(&server_sock, cert, key, me.spki).await.unwrap();
        let server = echo_server(endpoint);

        let client_sock = socket().await;
        let (conn, _ep, _stun) = quic_client(
            &client_sock,
            server_addr,
            HostSpki::new(fingerprint),
            me.cert,
            me.key,
        )
        .await
        .unwrap();

        assert!(
            conn.max_datagram_size().is_some(),
            "datagrams are silently disabled unless BOTH buffer sizes are set"
        );
        assert_eq!(conn.remote_address(), server_addr);

        conn.send_datagram(bytes::Bytes::from_static(b"oxutrm datagram"))
            .unwrap();
        let back = tokio::time::timeout(Duration::from_secs(10), conn.read_datagram())
            .await
            .expect("the echo must arrive")
            .unwrap();
        assert_eq!(&back[..], b"oxutrm datagram");

        conn.close(0u32.into(), b"done");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stream_goes_round_trip_alongside_the_datagrams() {
        // A 50,000-line scrollback fetch must never delay a keystroke, so
        // both channels have to exist on one connection.
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let me = client_id();
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _permit, _stun) =
            quic_server(&server_sock, cert, key, me.spki).await.unwrap();
        let server = echo_server(endpoint);

        let client_sock = socket().await;
        let (conn, _ep, _stun) = quic_client(
            &client_sock,
            server_addr,
            HostSpki::new(fingerprint),
            me.cert,
            me.key,
        )
        .await
        .unwrap();

        let payload = vec![0xABu8; 64 * 1024];
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&payload).await.unwrap();
        send.finish().unwrap();
        let back = tokio::time::timeout(Duration::from_secs(10), recv.read_to_end(1 << 20))
            .await
            .expect("the stream echo must arrive")
            .unwrap();
        assert_eq!(back, payload);

        // And a datagram still works on the same connection afterwards.
        conn.send_datagram(bytes::Bytes::from_static(b"still here"))
            .unwrap();
        let d = tokio::time::timeout(Duration::from_secs(10), conn.read_datagram())
            .await
            .expect("datagram")
            .unwrap();
        assert_eq!(&d[..], b"still here");

        conn.close(0u32.into(), b"done");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_pinned_to_a_different_certificate_is_refused() {
        let (cert, key, _fp) = generate_cert().unwrap();
        let (_other_cert, _other_key, other_fp) = generate_cert().unwrap();
        let me = client_id();

        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _permit, _stun) =
            quic_server(&server_sock, cert, key, me.spki).await.unwrap();
        let server = tokio::spawn(async move {
            if let Some(incoming) = endpoint.accept().await {
                let _ = incoming.await;
            }
        });

        let client_sock = socket().await;
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            quic_client(
                &client_sock,
                server_addr,
                HostSpki::new(other_fp),
                me.cert,
                me.key,
            ),
        )
        .await
        .expect("must not hang");

        assert!(
            result.is_err(),
            "the pin is the only thing that grants trust"
        );
        server.abort();
    }

    /// The other direction, and the one this crate shipped without.
    ///
    /// **Asserted on the HOST side, deliberately.** In TLS 1.3 the client
    /// finishes its own handshake before the server has verified the client
    /// certificate, and quinn reports `Connected` as soon as rustls stops
    /// handshaking — so `quic_client(..).await` here very likely resolves
    /// `Ok` and the connection dies a moment later. An `is_err()` on the
    /// client would be a coin flip dressed up as an assertion. The host's
    /// `incoming.await` is where the refusal is deterministic.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_certificate_the_host_did_not_pin_is_refused_by_the_host() {
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let expected = client_id();
        let stranger = client_id();
        assert_ne!(expected.spki.as_bytes(), stranger.spki.as_bytes());

        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        // Pinned to `expected`, and the stranger is the one who calls.
        let (endpoint, _permit, _stun) = quic_server(&server_sock, cert, key, expected.spki)
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let incoming = endpoint.accept().await.expect("an inbound connection");
            incoming.await
        });

        let client_sock = socket().await;
        // Whatever this resolves to is not the evidence; see the doc comment.
        let _ = tokio::time::timeout(
            Duration::from_secs(15),
            quic_client(
                &client_sock,
                server_addr,
                HostSpki::new(fingerprint),
                stranger.cert,
                stranger.key,
            ),
        )
        .await;

        let accepted = tokio::time::timeout(Duration::from_secs(15), server)
            .await
            .expect("the host must not hang")
            .expect("the accept task must not panic");
        let shown = format!("{accepted:?}");
        assert!(
            accepted.is_err(),
            "an unpinned client completed the handshake, which is one step from a shell: {shown}"
        );
    }

    /// The control for the test above. Without it, a `quic_server` that
    /// refused *everything* — a mis-wired pin, an ALPN typo, a broken
    /// certificate — would look exactly as green.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_client_the_host_did_pin_is_accepted_by_the_host() {
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let expected = client_id();
        let pinned = expected.spki;

        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _permit, _stun) =
            quic_server(&server_sock, cert, key, pinned).await.unwrap();
        let server = tokio::spawn(async move {
            let incoming = endpoint.accept().await.expect("an inbound connection");
            incoming.await
        });

        let client_sock = socket().await;
        let (conn, _ep, _stun) = tokio::time::timeout(
            Duration::from_secs(15),
            quic_client(
                &client_sock,
                server_addr,
                HostSpki::new(fingerprint),
                expected.cert,
                expected.key,
            ),
        )
        .await
        .expect("must not hang")
        .expect("the pinned client must connect");

        let accepted = tokio::time::timeout(Duration::from_secs(15), server)
            .await
            .expect("the host must not hang")
            .expect("the accept task must not panic");
        assert!(
            accepted.is_ok(),
            "the pinned client was refused: {accepted:?}"
        );
        conn.close(0u32.into(), b"done");
    }

    /// A client built the way this file's `client_config` used to build one:
    /// no certificate at all.
    ///
    /// This is what `client_auth_mandatory()` is for. With it `false` — a
    /// one-word change that overrides a rustls default and would read like a
    /// decision — this peer would be let in, and every other test in the
    /// workspace would still pass, because a well-behaved client presents its
    /// certificate whether or not it was required to.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_client_that_presents_no_certificate_at_all_is_refused_by_the_host() {
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let expected = client_id();

        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _permit, _stun) = quic_server(&server_sock, cert, key, expected.spki)
            .await
            .unwrap();
        let server = tokio::spawn(async move {
            let incoming = endpoint.accept().await.expect("an inbound connection");
            incoming.await
        });

        // Built by hand rather than through `quic_client`, because
        // `quic_client` structurally cannot produce this any more — which is
        // the point, and also the reason the anonymous client has to be
        // written out here to be tested against.
        install_crypto_provider();
        let mut tls = rustls::ClientConfig::builder_with_provider(provider())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedSpki::new(HostSpki::new(fingerprint))))
            .with_no_client_auth();
        tls.alpn_protocols = vec![ALPN.to_vec()];
        let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
        cfg.transport_config(transport_config());

        let client_sock = socket().await;
        let (mut client_ep, _stun) = endpoint_over(&client_sock, None).unwrap();
        client_ep.set_default_client_config(cfg);
        let _ = tokio::time::timeout(
            Duration::from_secs(15),
            client_ep.connect(server_addr, CERT_NAME).unwrap(),
        )
        .await;

        let accepted = tokio::time::timeout(Duration::from_secs(15), server)
            .await
            .expect("the host must not hang")
            .expect("the accept task must not panic");
        assert!(
            accepted.is_err(),
            "an anonymous client completed the handshake: {accepted:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_survives_stun_arriving_on_the_same_socket_throughout() {
        // The reason StunDemuxSocket exists, end to end: a handshake, a stream
        // and a datagram while STUN keeps landing on both sockets.
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let me = client_id();
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let client_sock = socket().await;
        let client_addr = client_sock.local_addr().unwrap();

        let (endpoint, _permit, mut server_stun) =
            quic_server(&server_sock, cert, key, me.spki).await.unwrap();
        let server = echo_server(endpoint);

        let noise = tokio::spawn(async move {
            let s = UdpSocket::bind("127.0.0.1:0").await.unwrap();
            let mut d = vec![0x00, 0x01, 0x00, 0x00];
            d.extend_from_slice(&crate::STUN_MAGIC_COOKIE);
            d.extend_from_slice(&[0xA1; 12]);
            loop {
                let _ = s.send_to(&d, server_addr).await;
                let _ = s.send_to(&d, client_addr).await;
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let (conn, _ep, _client_stun) = tokio::time::timeout(
            Duration::from_secs(20),
            quic_client(
                &client_sock,
                server_addr,
                HostSpki::new(fingerprint),
                me.cert,
                me.key,
            ),
        )
        .await
        .expect("the handshake must not be starved by the STUN traffic")
        .unwrap();

        conn.send_datagram(bytes::Bytes::from_static(b"through the noise"))
            .unwrap();
        let back = tokio::time::timeout(Duration::from_secs(10), conn.read_datagram())
            .await
            .expect("the echo must arrive")
            .unwrap();
        assert_eq!(&back[..], b"through the noise");

        // The STUN was not lost, it was diverted.
        let diverted = tokio::time::timeout(Duration::from_secs(5), server_stun.recv())
            .await
            .expect("STUN must reach the channel")
            .expect("channel open");
        assert!(crate::is_stun(&diverted.0));

        // And the caller can still send on the socket quinn is using.
        client_sock
            .send_to(&[0x00, 0x01], server_addr)
            .await
            .unwrap();

        noise.abort();
        conn.close(0u32.into(), b"done");
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_alpn_is_the_honest_one() {
        assert_eq!(ALPN, b"oxutrm/1");
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let me = client_id();
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _permit, _stun) =
            quic_server(&server_sock, cert, key, me.spki).await.unwrap();
        let server = echo_server(endpoint);

        let client_sock = socket().await;
        let (conn, _ep, _stun) = quic_client(
            &client_sock,
            server_addr,
            HostSpki::new(fingerprint),
            me.cert,
            me.key,
        )
        .await
        .unwrap();
        assert_eq!(
            conn.handshake_data()
                .and_then(|d| d.downcast::<quinn::crypto::rustls::HandshakeData>().ok())
                .and_then(|d| d.protocol),
            Some(ALPN.to_vec()),
            "the negotiated protocol must be what we advertised"
        );
        conn.close(0u32.into(), b"done");
        server.abort();
    }
}

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
use oxutrm_proto::SpkiSha256;
use quinn::rustls;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::demuxsock::{StunDemuxSocket, StunRx};
use crate::socketfam::to_socket_family;
use crate::tls::{CERT_NAME, PinnedSpki, install_crypto_provider, provider};

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
    t.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).expect("30s is representable"),
    ));
    t.keep_alive_interval(Some(Duration::from_secs(10)));
    Arc::new(t)
}

fn server_config(
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<quinn::ServerConfig> {
    install_crypto_provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .context("installing the session certificate")?;
    tls.alpn_protocols = vec![ALPN.to_vec()];

    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .context("building the QUIC server crypto config")?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(transport_config());
    Ok(cfg)
}

fn client_config(expect_spki_sha256: SpkiSha256) -> anyhow::Result<quinn::ClientConfig> {
    // Its own step, and not optional: without a process-default provider
    // `QuicClientConfig::try_from` below fails with an error that says nothing
    // about providers.
    install_crypto_provider();

    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .context("selecting TLS 1.3")?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedSpki::new(*expect_spki_sha256.as_bytes())))
        .with_no_client_auth();
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

/// Listen on a socket the ladder has already punched.
///
/// The caller keeps `socket` for sending ICE keepalives, and receives the
/// peeled-off STUN on the returned [`StunRx`].
pub async fn quic_server(
    socket: &Arc<tokio::net::UdpSocket>,
    cert: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
) -> anyhow::Result<(quinn::Endpoint, StunRx)> {
    endpoint_over(socket, Some(server_config(cert, key)?))
}

/// Connect to `peer`, trusting exactly `expect_spki_sha256` and nothing else.
///
/// The fingerprint arrives as the wire crate's [`SpkiSha256`] rather than a
/// bare `[u8; 32]`, and that is the whole point: it is the same 32 bytes the
/// host put in `HostHello`, carried in a type that a `Psk` — also 32 bytes,
/// also on that message — cannot be mistaken for.
///
/// The endpoint comes back alongside the connection because `quinn` drives the
/// socket from it and M4 needs the same handle for `Endpoint::rebind` when the
/// local address changes while roaming.
pub async fn quic_client(
    socket: &Arc<tokio::net::UdpSocket>,
    peer: SocketAddr,
    expect_spki_sha256: SpkiSha256,
) -> anyhow::Result<(quinn::Connection, quinn::Endpoint, StunRx)> {
    let (mut endpoint, stun_rx) = endpoint_over(socket, None)?;
    endpoint.set_default_client_config(client_config(expect_spki_sha256)?);

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

    async fn socket() -> Arc<UdpSocket> {
        Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap())
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
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _stun) = quic_server(&server_sock, cert, key).await.unwrap();
        let server = echo_server(endpoint);

        let client_sock = socket().await;
        let (conn, _ep, _stun) =
            quic_client(&client_sock, server_addr, SpkiSha256::new(fingerprint))
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
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _stun) = quic_server(&server_sock, cert, key).await.unwrap();
        let server = echo_server(endpoint);

        let client_sock = socket().await;
        let (conn, _ep, _stun) =
            quic_client(&client_sock, server_addr, SpkiSha256::new(fingerprint))
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

        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _stun) = quic_server(&server_sock, cert, key).await.unwrap();
        let server = tokio::spawn(async move {
            if let Some(incoming) = endpoint.accept().await {
                let _ = incoming.await;
            }
        });

        let client_sock = socket().await;
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            quic_client(&client_sock, server_addr, SpkiSha256::new(other_fp)),
        )
        .await
        .expect("must not hang");

        assert!(
            result.is_err(),
            "the pin is the only thing that grants trust"
        );
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn quic_survives_stun_arriving_on_the_same_socket_throughout() {
        // The reason StunDemuxSocket exists, end to end: a handshake, a stream
        // and a datagram while STUN keeps landing on both sockets.
        let (cert, key, fingerprint) = generate_cert().unwrap();
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let client_sock = socket().await;
        let client_addr = client_sock.local_addr().unwrap();

        let (endpoint, mut server_stun) = quic_server(&server_sock, cert, key).await.unwrap();
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
            quic_client(&client_sock, server_addr, SpkiSha256::new(fingerprint)),
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
        let server_sock = socket().await;
        let server_addr = server_sock.local_addr().unwrap();
        let (endpoint, _stun) = quic_server(&server_sock, cert, key).await.unwrap();
        let server = echo_server(endpoint);

        let client_sock = socket().await;
        let (conn, _ep, _stun) =
            quic_client(&client_sock, server_addr, SpkiSha256::new(fingerprint))
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

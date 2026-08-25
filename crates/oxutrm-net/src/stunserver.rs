//! A minimal STUN Binding server.
//!
//! This is a **production module**, not a test fixture. Spec §11 makes the
//! STUN server list configurable "so a user who objects can point at their
//! own", and §12 requires that CI not depend on the public internet. One small
//! responder serves both: every unit test in this crate binds one, and a
//! privacy-minded user can run their own.
//!
//! It answers Binding Requests with `XOR-MAPPED-ADDRESS` and nothing else. It
//! deliberately does **not** implement `MESSAGE-INTEGRITY`: this is the
//! unauthenticated discovery half of the design, where the only question is
//! "what address did my packet appear to come from". Authenticated
//! connectivity checks are a different mechanism built on `stun_codec` +
//! `hmac`, and they never talk to a server like this one.

use std::net::SocketAddr;
use std::sync::Arc;

use bytecodec::{DecodeExt, EncodeExt};
use stun_codec::rfc5389::attributes::XorMappedAddress;
use stun_codec::rfc5389::{Attribute, methods::BINDING};
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder};
use tokio::net::UdpSocket;

use crate::{is_stun, unmap};

/// How the responder reports the address it saw.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MappingBehaviour {
    /// Report the source address exactly as seen. This is what a real STUN
    /// server does.
    Truthful,
    /// Report the source IP but a fixed, made-up port.
    ///
    /// Two responders started with two different values look to a client
    /// exactly like a symmetric NAT — a different external port per
    /// destination — without needing a kernel NAT at all. That is what lets
    /// the classifier's whole truth table be exercised in a unit test.
    RewritePort(u16),
}

/// A running responder. Dropping it stops the server.
pub struct StunResponder {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl StunResponder {
    /// Bind an ephemeral loopback port.
    pub async fn start(behaviour: MappingBehaviour) -> anyhow::Result<StunResponder> {
        StunResponder::start_on("127.0.0.1:0".parse().expect("a literal address"), behaviour).await
    }

    /// Bind a specific address, for a harness that needs a known one.
    pub async fn start_on(
        bind: SocketAddr,
        behaviour: MappingBehaviour,
    ) -> anyhow::Result<StunResponder> {
        let socket = Arc::new(UdpSocket::bind(bind).await?);
        let addr = socket.local_addr()?;
        let task = tokio::spawn(async move { serve(socket, behaviour).await });
        Ok(StunResponder { addr, task })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The `host:port` string [`crate::NetConfig::stun_servers`] wants.
    pub fn server_string(&self) -> String {
        self.addr.to_string()
    }
}

impl Drop for StunResponder {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(socket: Arc<UdpSocket>, behaviour: MappingBehaviour) {
    let mut buf = vec![0u8; 2048];
    loop {
        let (n, from) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(_) => return,
        };
        // Anything that is not STUN is silently dropped: this socket is not a
        // general-purpose service and must never be usable as a reflector for
        // arbitrary payloads.
        if !is_stun(&buf[..n]) {
            continue;
        }
        if let Some(reply) = build_reply(&buf[..n], unmap(from), behaviour)
            && socket.send_to(&reply, from).await.is_err()
        {
            return;
        }
    }
}

/// Decode a Binding Request and encode the matching Success Response.
///
/// Separated from the socket so the message handling is testable directly.
fn build_reply(datagram: &[u8], from: SocketAddr, behaviour: MappingBehaviour) -> Option<Vec<u8>> {
    let decoded = MessageDecoder::<Attribute>::new()
        .decode_from_bytes(datagram)
        .ok()?
        .ok()?;

    // Only Binding Requests get an answer. An Indication expects none, and
    // answering a Response would be a loop.
    if decoded.class() != MessageClass::Request || decoded.method() != BINDING {
        return None;
    }

    let reported = match behaviour {
        MappingBehaviour::Truthful => from,
        MappingBehaviour::RewritePort(port) => SocketAddr::new(from.ip(), port),
    };

    // The transaction id is echoed, which is what lets a client match a reply
    // to its own request rather than to a stray datagram.
    let mut reply = Message::<Attribute>::new(
        MessageClass::SuccessResponse,
        BINDING,
        decoded.transaction_id(),
    );
    reply.add_attribute(XorMappedAddress::new(reported));

    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(reply)
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use stun_codec::TransactionId;

    fn binding_request(id: [u8; 12]) -> Vec<u8> {
        let msg = Message::<Attribute>::new(MessageClass::Request, BINDING, TransactionId::new(id));
        MessageEncoder::<Attribute>::new()
            .encode_into_bytes(msg)
            .expect("encode")
    }

    fn mapped_address(reply: &[u8]) -> SocketAddr {
        let msg = MessageDecoder::<Attribute>::new()
            .decode_from_bytes(reply)
            .expect("decode")
            .expect("well-formed");
        assert_eq!(msg.class(), MessageClass::SuccessResponse);
        msg.attributes()
            .find_map(|a| match a {
                Attribute::XorMappedAddress(x) => Some(x.address()),
                _ => None,
            })
            .expect("XOR-MAPPED-ADDRESS")
    }

    /// Ask a running responder what address it saw, from `socket`.
    async fn query(socket: &UdpSocket, server: SocketAddr, id: [u8; 12]) -> SocketAddr {
        socket
            .send_to(&binding_request(id), server)
            .await
            .expect("send");
        let mut buf = vec![0u8; 2048];
        let (n, _) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            socket.recv_from(&mut buf),
        )
        .await
        .expect("the responder answered in time")
        .expect("recv");
        mapped_address(&buf[..n])
    }

    #[tokio::test]
    async fn a_truthful_responder_reports_the_address_it_saw() {
        let server = StunResponder::start(MappingBehaviour::Truthful)
            .await
            .expect("start");
        let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let ours = sock.local_addr().expect("local_addr");

        let seen = query(&sock, server.addr(), [1; 12]).await;
        assert_eq!(
            seen, ours,
            "a truthful responder must echo our real address"
        );
    }

    #[tokio::test]
    async fn a_rewriting_responder_reports_the_port_it_was_told_to() {
        let server = StunResponder::start(MappingBehaviour::RewritePort(40_001))
            .await
            .expect("start");
        let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let ours = sock.local_addr().expect("local_addr");

        let seen = query(&sock, server.addr(), [2; 12]).await;
        assert_eq!(seen.ip(), ours.ip());
        assert_eq!(seen.port(), 40_001);
        assert_ne!(seen.port(), ours.port());
    }

    /// The property the classifier's tests depend on: two responders with
    /// different rewrite ports are indistinguishable from a symmetric NAT.
    #[tokio::test]
    async fn two_rewriting_responders_look_like_a_symmetric_nat() {
        let a = StunResponder::start(MappingBehaviour::RewritePort(50_001))
            .await
            .expect("start a");
        let b = StunResponder::start(MappingBehaviour::RewritePort(50_002))
            .await
            .expect("start b");
        let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");

        let from_a = query(&sock, a.addr(), [3; 12]).await;
        let from_b = query(&sock, b.addr(), [4; 12]).await;
        assert_ne!(
            from_a.port(),
            from_b.port(),
            "the same socket must appear on different ports to different servers"
        );
    }

    #[tokio::test]
    async fn the_transaction_id_is_echoed_so_a_client_can_match_its_own_reply() {
        let server = StunResponder::start(MappingBehaviour::Truthful)
            .await
            .expect("start");
        let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let id = [0xAB; 12];
        sock.send_to(&binding_request(id), server.addr())
            .await
            .expect("send");

        let mut buf = vec![0u8; 2048];
        let (n, _) =
            tokio::time::timeout(std::time::Duration::from_secs(5), sock.recv_from(&mut buf))
                .await
                .expect("answered")
                .expect("recv");
        let msg = MessageDecoder::<Attribute>::new()
            .decode_from_bytes(&buf[..n])
            .expect("decode")
            .expect("well-formed");
        assert_eq!(msg.transaction_id(), TransactionId::new(id));
    }

    #[test]
    fn only_binding_requests_are_answered() {
        let from: SocketAddr = "198.51.100.7:1234".parse().unwrap();

        // A Request gets a reply.
        assert!(build_reply(&binding_request([5; 12]), from, MappingBehaviour::Truthful).is_some());

        // An Indication expects no reply, and a Response would be a loop.
        for class in [
            MessageClass::Indication,
            MessageClass::SuccessResponse,
            MessageClass::ErrorResponse,
        ] {
            let msg = Message::<Attribute>::new(class, BINDING, TransactionId::new([6; 12]));
            let bytes = MessageEncoder::<Attribute>::new()
                .encode_into_bytes(msg)
                .expect("encode");
            assert!(
                build_reply(&bytes, from, MappingBehaviour::Truthful).is_none(),
                "{class:?} must not be answered"
            );
        }
    }

    /// The responder must never become a reflector for arbitrary payloads.
    #[test]
    fn garbage_is_not_answered() {
        let from: SocketAddr = "198.51.100.7:1234".parse().unwrap();
        for junk in [
            vec![],
            vec![0x00],
            vec![0xFF; 64],
            // Right length, wrong everything else.
            vec![0x00; 40],
        ] {
            assert!(build_reply(&junk, from, MappingBehaviour::Truthful).is_none());
        }
    }

    #[tokio::test]
    async fn dropping_the_responder_stops_it() {
        let server = StunResponder::start(MappingBehaviour::Truthful)
            .await
            .expect("start");
        let addr = server.addr();
        let sock = UdpSocket::bind("127.0.0.1:0").await.expect("bind");
        let _ = query(&sock, addr, [7; 12]).await;

        drop(server);
        // Give the abort a moment to take effect.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        sock.send_to(&binding_request([8; 12]), addr)
            .await
            .expect("send");
        let mut buf = vec![0u8; 2048];
        let answered = tokio::time::timeout(
            std::time::Duration::from_millis(300),
            sock.recv_from(&mut buf),
        )
        .await;
        assert!(answered.is_err(), "a dropped responder still answered");
    }

    #[tokio::test]
    async fn the_server_string_round_trips_into_a_socket_address() {
        let server = StunResponder::start(MappingBehaviour::Truthful)
            .await
            .expect("start");
        let parsed: SocketAddr = server.server_string().parse().expect("parse");
        assert_eq!(parsed, server.addr());
    }
}

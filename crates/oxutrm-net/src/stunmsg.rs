//! ICE connectivity checks: authenticated STUN Binding messages.
//!
//! These are the packets that actually punch the hole. They are STUN Binding
//! Requests carrying `MESSAGE-INTEGRITY`, exactly as RFC 8445 connectivity
//! checks are, which buys three things at once: they demultiplex cleanly
//! against QUIC on the same socket ([`crate::is_stun`]), strangers cannot walk
//! our state machine, and the `XOR-MAPPED-ADDRESS` in a response *is*
//! peer-reflexive discovery, free.
//!
//! # Why one shared key is not enough
//!
//! `MESSAGE-INTEGRITY` keyed by a single shared secret authenticates *the
//! session*, not *the sender*. A side then cannot tell **its own reflected
//! check** from a genuine peer check — a hairpinning NAT, a confused
//! middlebox, or someone echoing our packet back all produce a request that
//! verifies perfectly. The agent records "the peer can reach me", which is
//! false, and nominates a path to itself.
//!
//! So the PSK is expanded into **two** independent credentials with
//! HKDF-SHA256, one per direction:
//!
//! ```text
//! c2h = HKDF-SHA256(ikm = psk, salt = none, info = "oxutrm ice c2h", L = 32)
//! h2c = HKDF-SHA256(ikm = psk, salt = none, info = "oxutrm ice h2c", L = 32)
//! ```
//!
//! | I am | sign requests | verify requests | sign responses | verify responses |
//! |---|---|---|---|---|
//! | `Controlling` (client) | `c2h` | `h2c` | `h2c` | `c2h` |
//! | `Controlled` (host) | `h2c` | `c2h` | `c2h` | `h2c` |
//!
//! A reflected copy of my own request is signed with my outbound credential
//! and checked against the peer's. It fails, every time. That is the point.
//!
//! # Two things that fail silently
//!
//! **The key is the password verbatim.** `stun_codec` passes
//! `password.as_bytes()` straight to HMAC-SHA1 (RFC 5389 §15.4: for short-term
//! credentials the key *is* `SASLprep(password)`, and a base64 alphabet is
//! unchanged by SASLprep). HKDF gives 32 raw bytes, which is not a `&str`, so
//! each credential is the URL-safe unpadded base64 of those bytes: 43
//! characters of `[A-Za-z0-9_-]`, split into an 8-character ufrag and a
//! 35-character password. Both clear RFC 8445's minimums (ufrag ≥ 4, password
//! ≥ 22) and neither needs escaping in a STUN `USERNAME`.
//!
//! **Attribute order is part of the signature.** The HMAC covers the message
//! as encoded *so far*, with the header length temporarily raised by 24. Every
//! other attribute must therefore already be present when `MESSAGE-INTEGRITY`
//! is added, and nothing may follow it. oxutrm deliberately sends no
//! `FINGERPRINT`: it would have to come after, changing the bytes validated
//! over, and [`crate::is_stun`] already demultiplexes without it.

use std::net::SocketAddr;

use base64::Engine as _;
use bytecodec::{DecodeExt, EncodeExt};
use stun_codec::rfc5389::attributes::{MessageIntegrity, Username, XorMappedAddress};
use stun_codec::rfc5389::{Attribute, methods::BINDING};
use stun_codec::{Message, MessageClass, MessageDecoder, MessageEncoder, TransactionId};

/// Which side drives the exchange. The client is **always** `Controlling`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IceRole {
    Controlling,
    Controlled,
}

/// Which half of the exchange a message belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    ClientToHost,
    HostToClient,
}

const INFO_C2H: &[u8] = b"oxutrm ice c2h";
const INFO_H2C: &[u8] = b"oxutrm ice h2c";

/// One direction's short-term credential.
#[derive(Clone)]
struct Credential {
    ufrag: String,
    password: String,
}

/// Two independent short-term credentials derived from the one shared PSK.
#[derive(Clone)]
pub struct IceCredentials {
    c2h: Credential,
    h2c: Credential,
}

/// Redacted: a `Debug` that printed these would put key material in every log
/// line that formats an `IceAgent`, and spec §11 says no key material is ever
/// written anywhere.
impl std::fmt::Debug for IceCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("IceCredentials(<redacted>)")
    }
}

fn derive_one(psk: &[u8; 32], info: &[u8]) -> Credential {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(None, psk);
    let mut okm = [0u8; 32];
    hk.expand(info, &mut okm)
        .expect("32 bytes is far below HKDF-SHA256's output limit");
    // URL-safe and unpadded: 43 characters of [A-Za-z0-9_-], none of which
    // needs escaping in a STUN USERNAME or is altered by SASLprep.
    let s = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(okm);
    debug_assert_eq!(s.len(), 43);
    Credential {
        ufrag: s[..8].to_string(),
        password: s[8..].to_string(),
    }
}

impl IceCredentials {
    pub fn derive(psk: &[u8; 32]) -> IceCredentials {
        IceCredentials {
            c2h: derive_one(psk, INFO_C2H),
            h2c: derive_one(psk, INFO_H2C),
        }
    }

    /// The direction this role signs its own requests in.
    pub fn outbound(role: IceRole) -> Direction {
        match role {
            IceRole::Controlling => Direction::ClientToHost,
            IceRole::Controlled => Direction::HostToClient,
        }
    }

    /// The direction this role expects the peer's requests in.
    pub fn inbound(role: IceRole) -> Direction {
        match role {
            IceRole::Controlling => Direction::HostToClient,
            IceRole::Controlled => Direction::ClientToHost,
        }
    }

    fn cred(&self, d: Direction) -> &Credential {
        match d {
            Direction::ClientToHost => &self.c2h,
            Direction::HostToClient => &self.h2c,
        }
    }

    pub fn ufrag(&self, d: Direction) -> &str {
        &self.cred(d).ufrag
    }

    pub fn password(&self, d: Direction) -> &str {
        &self.cred(d).password
    }

    /// RFC 8445 `USERNAME`: `<remote-ufrag>:<local-ufrag>`.
    pub fn username(&self, d: Direction) -> String {
        let (remote, local) = match d {
            Direction::ClientToHost => (&self.h2c, &self.c2h),
            Direction::HostToClient => (&self.c2h, &self.h2c),
        };
        format!("{}:{}", remote.ufrag, local.ufrag)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CheckKind {
    Request,
    SuccessResponse,
    /// oxutrm's stand-in for RFC 8445 `USE-CANDIDATE`.
    Nomination,
}

#[derive(Clone, Debug)]
pub struct Check {
    pub kind: CheckKind,
    pub tid: TransactionId,
    /// The `XOR-MAPPED-ADDRESS` from a response: our own address as the peer
    /// sees it, which is peer-reflexive discovery for free.
    pub reflexive: Option<SocketAddr>,
}

pub fn random_transaction_id() -> TransactionId {
    use rand::RngCore;
    let mut id = [0u8; 12];
    rand::rng().fill_bytes(&mut id);
    TransactionId::new(id)
}

/// Encode a message, signing it last. Nothing may be added after the HMAC.
fn finish(
    mut msg: Message<Attribute>,
    c: &IceCredentials,
    d: Direction,
) -> anyhow::Result<Vec<u8>> {
    let mi = MessageIntegrity::new_short_term_credential(&msg, c.password(d))
        .map_err(|e| anyhow::anyhow!("MESSAGE-INTEGRITY: {e}"))?;
    msg.add_attribute(mi);
    MessageEncoder::<Attribute>::new()
        .encode_into_bytes(msg)
        .map_err(|e| anyhow::anyhow!("encode: {e}"))
}

pub fn build_check_request(
    c: &IceCredentials,
    d: Direction,
    tid: TransactionId,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::Request, BINDING, tid);
    msg.add_attribute(Username::new(c.username(d)).map_err(|e| anyhow::anyhow!("USERNAME: {e}"))?);
    finish(msg, c, d)
}

pub fn build_check_response(
    c: &IceCredentials,
    d: Direction,
    tid: TransactionId,
    reflexive: SocketAddr,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::SuccessResponse, BINDING, tid);
    // The peer's own address as we saw it. This is what makes a reflector
    // unnecessary for learning one's public mapping.
    msg.add_attribute(XorMappedAddress::new(reflexive));
    finish(msg, c, d)
}

/// An authenticated Binding Indication that says "this pair is the one".
///
/// RFC 8445 uses a `USE-CANDIDATE` attribute, which `stun_codec`'s `rfc5389`
/// module does not define. An Indication carries the same credential and the
/// same `MESSAGE-INTEGRITY`, expects no response, and cannot be confused with
/// a check because its class differs.
pub fn build_nomination(
    c: &IceCredentials,
    d: Direction,
    tid: TransactionId,
) -> anyhow::Result<Vec<u8>> {
    let mut msg = Message::<Attribute>::new(MessageClass::Indication, BINDING, tid);
    msg.add_attribute(Username::new(c.username(d)).map_err(|e| anyhow::anyhow!("USERNAME: {e}"))?);
    finish(msg, c, d)
}

/// Parse and **verify** a datagram. `d` is the direction it is expected to
/// have been signed in.
///
/// Returns `None` for anything that is not a well-formed, correctly signed
/// Binding message — including a copy of our own packet, which is signed with
/// the other direction's credential.
pub fn parse_check(c: &IceCredentials, d: Direction, datagram: &[u8]) -> Option<Check> {
    if !crate::is_stun(datagram) {
        return None;
    }
    let msg = MessageDecoder::<Attribute>::new()
        .decode_from_bytes(datagram)
        .ok()?
        .ok()?;
    if msg.method() != BINDING {
        return None;
    }

    // An unsigned message is not a check. Rejecting here is what stops a
    // stranger advancing the state machine.
    let mi = msg.get_attribute::<MessageIntegrity>()?;
    mi.check_short_term_credential(c.password(d)).ok()?;

    let kind = match msg.class() {
        MessageClass::Request => CheckKind::Request,
        MessageClass::SuccessResponse => CheckKind::SuccessResponse,
        MessageClass::Indication => CheckKind::Nomination,
        MessageClass::ErrorResponse => return None,
    };
    let reflexive = msg
        .get_attribute::<XorMappedAddress>()
        .map(|x| crate::unmap(x.address()));

    Some(Check {
        kind,
        tid: msg.transaction_id(),
        reflexive,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PSK: [u8; 32] = [7u8; 32];

    fn creds() -> IceCredentials {
        IceCredentials::derive(&PSK)
    }

    #[test]
    fn the_two_directions_get_different_credentials() {
        let c = creds();
        assert_ne!(
            c.password(Direction::ClientToHost),
            c.password(Direction::HostToClient),
            "one shared key would let a reflected check verify"
        );
        assert_ne!(
            c.ufrag(Direction::ClientToHost),
            c.ufrag(Direction::HostToClient)
        );
    }

    #[test]
    fn derivation_is_deterministic_and_psk_dependent() {
        assert_eq!(
            IceCredentials::derive(&PSK).password(Direction::ClientToHost),
            creds().password(Direction::ClientToHost),
            "both ends must derive the same credential from the same psk"
        );
        let other = IceCredentials::derive(&[8u8; 32]);
        assert_ne!(
            other.password(Direction::ClientToHost),
            creds().password(Direction::ClientToHost)
        );
    }

    /// RFC 8445 minimums, and the character set a STUN USERNAME can carry
    /// without escaping.
    #[test]
    fn the_credentials_meet_rfc_8445_minimums_and_need_no_escaping() {
        let c = creds();
        for d in [Direction::ClientToHost, Direction::HostToClient] {
            assert_eq!(c.ufrag(d).len(), 8, "RFC 8445 wants at least 4");
            assert_eq!(c.password(d).len(), 35, "RFC 8445 wants at least 22");
            for s in [c.ufrag(d), c.password(d)] {
                assert!(
                    s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                    "{s:?} is not URL-safe base64"
                );
            }
        }
    }

    /// The one place key material could leak into a log line.
    #[test]
    fn debug_never_prints_key_material() {
        let c = creds();
        let shown = format!("{c:?}");
        assert!(!shown.contains(c.password(Direction::ClientToHost)));
        assert!(!shown.contains(c.password(Direction::HostToClient)));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn the_username_is_remote_ufrag_then_local_ufrag() {
        let c = creds();
        assert_eq!(
            c.username(Direction::ClientToHost),
            format!(
                "{}:{}",
                c.ufrag(Direction::HostToClient),
                c.ufrag(Direction::ClientToHost)
            )
        );
        // And the two directions disagree, which is what a peer checks.
        assert_ne!(
            c.username(Direction::ClientToHost),
            c.username(Direction::HostToClient)
        );
    }

    #[test]
    fn a_role_signs_outbound_and_verifies_inbound_with_opposite_directions() {
        assert_eq!(
            IceCredentials::outbound(IceRole::Controlling),
            Direction::ClientToHost
        );
        assert_eq!(
            IceCredentials::inbound(IceRole::Controlling),
            Direction::HostToClient
        );
        assert_eq!(
            IceCredentials::outbound(IceRole::Controlled),
            Direction::HostToClient
        );
        assert_eq!(
            IceCredentials::inbound(IceRole::Controlled),
            Direction::ClientToHost
        );
    }

    // ---- the wire ----

    /// Attribute ordering is part of the signature and gets it wrong
    /// silently, so pin the exact bytes.
    #[test]
    fn a_check_request_has_exactly_the_bytes_we_expect() {
        let c = creds();
        let tid = TransactionId::new([0x11; 12]);
        let bytes = build_check_request(&c, Direction::ClientToHost, tid).expect("build");

        // Header: Binding Request, and the magic cookie.
        assert_eq!(
            &bytes[0..2],
            &[0x00, 0x01],
            "class/method must be a Binding Request"
        );
        assert_eq!(&bytes[4..8], &crate::STUN_MAGIC_COOKIE);
        assert_eq!(
            &bytes[8..20],
            &[0x11; 12],
            "the transaction id is echoed by the peer"
        );

        // USERNAME (0x0006) comes first...
        assert_eq!(
            &bytes[20..22],
            &[0x00, 0x06],
            "USERNAME must precede MESSAGE-INTEGRITY"
        );
        let user_len = u16::from_be_bytes([bytes[22], bytes[23]]) as usize;
        assert_eq!(user_len, 17, "8 + ':' + 8");
        assert_eq!(
            &bytes[24..24 + user_len],
            c.username(Direction::ClientToHost).as_bytes()
        );

        // ...then MESSAGE-INTEGRITY (0x0008), 20 bytes, and NOTHING after it.
        let pad = (4 - user_len % 4) % 4;
        let mi_at = 24 + user_len + pad;
        assert_eq!(&bytes[mi_at..mi_at + 2], &[0x00, 0x08]);
        assert_eq!(
            u16::from_be_bytes([bytes[mi_at + 2], bytes[mi_at + 3]]),
            20,
            "HMAC-SHA1 is 20 bytes"
        );
        assert_eq!(
            bytes.len(),
            mi_at + 4 + 20,
            "nothing may follow MESSAGE-INTEGRITY, including FINGERPRINT"
        );

        // And the length field describes exactly the attributes.
        assert_eq!(
            u16::from_be_bytes([bytes[2], bytes[3]]) as usize,
            bytes.len() - 20
        );

        // is_stun must accept our own checks, or the demultiplexer drops them.
        assert!(crate::is_stun(&bytes));
    }

    #[test]
    fn a_correctly_signed_request_is_accepted() {
        let c = creds();
        let tid = random_transaction_id();
        let bytes = build_check_request(&c, Direction::ClientToHost, tid).expect("build");

        let got = parse_check(&c, Direction::ClientToHost, &bytes).expect("accepted");
        assert_eq!(got.kind, CheckKind::Request);
        assert_eq!(got.tid, tid);
        assert!(got.reflexive.is_none());
    }

    /// The reflection attack the direction labels exist to stop: our own
    /// request, echoed back, must not verify as the peer's.
    #[test]
    fn a_reflected_copy_of_our_own_request_is_rejected() {
        let c = creds();
        let mine = build_check_request(
            &c,
            IceCredentials::outbound(IceRole::Controlling),
            random_transaction_id(),
        )
        .expect("build");

        // As the controlling side, an inbound request is verified with the
        // INBOUND direction. Our own packet was signed with the outbound one.
        assert!(
            parse_check(&c, IceCredentials::inbound(IceRole::Controlling), &mine).is_none(),
            "a reflected check verified, so the agent could nominate a path to itself"
        );
        // The same, from the other side.
        let theirs = build_check_request(
            &c,
            IceCredentials::outbound(IceRole::Controlled),
            random_transaction_id(),
        )
        .expect("build");
        assert!(parse_check(&c, IceCredentials::inbound(IceRole::Controlled), &theirs).is_none());
    }

    #[test]
    fn a_check_signed_with_the_wrong_psk_is_rejected() {
        let ours = creds();
        let stranger = IceCredentials::derive(&[0xAB; 32]);
        let forged =
            build_check_request(&stranger, Direction::ClientToHost, random_transaction_id())
                .expect("build");
        assert!(
            parse_check(&ours, Direction::ClientToHost, &forged).is_none(),
            "a stranger's check verified, so anyone could advance our state machine"
        );
    }

    #[test]
    fn a_response_carries_the_peer_reflexive_address() {
        let c = creds();
        let tid = random_transaction_id();
        let seen: SocketAddr = "203.0.113.9:51000".parse().unwrap();
        let bytes = build_check_response(&c, Direction::HostToClient, tid, seen).expect("build");

        let got = parse_check(&c, Direction::HostToClient, &bytes).expect("accepted");
        assert_eq!(got.kind, CheckKind::SuccessResponse);
        assert_eq!(got.tid, tid);
        assert_eq!(
            got.reflexive,
            Some(seen),
            "the XOR-MAPPED-ADDRESS is how a side learns its own mapping"
        );
    }

    /// An IPv4-mapped address must be reported in the form a peer would dial.
    #[test]
    fn a_reflexive_address_is_unmapped() {
        let c = creds();
        let mapped: SocketAddr = "[::ffff:203.0.113.9]:51000".parse().unwrap();
        let bytes =
            build_check_response(&c, Direction::HostToClient, random_transaction_id(), mapped)
                .expect("build");
        let got = parse_check(&c, Direction::HostToClient, &bytes).expect("accepted");
        assert_eq!(got.reflexive, Some("203.0.113.9:51000".parse().unwrap()));
    }

    #[test]
    fn a_nomination_is_an_authenticated_indication() {
        let c = creds();
        let tid = random_transaction_id();
        let bytes = build_nomination(&c, Direction::ClientToHost, tid).expect("build");

        let got = parse_check(&c, Direction::ClientToHost, &bytes).expect("accepted");
        assert_eq!(got.kind, CheckKind::Nomination);
        assert_eq!(got.tid, tid);

        // A nomination must be distinguishable from a check by class alone.
        let req = build_check_request(&c, Direction::ClientToHost, tid).expect("build");
        assert_ne!(bytes[0..2], req[0..2]);

        // And it must still be rejected when signed with the wrong direction.
        assert!(parse_check(&c, Direction::HostToClient, &bytes).is_none());
    }

    #[test]
    fn an_unsigned_binding_request_is_not_a_check() {
        let c = creds();
        let plain =
            Message::<Attribute>::new(MessageClass::Request, BINDING, random_transaction_id());
        let bytes = MessageEncoder::<Attribute>::new()
            .encode_into_bytes(plain)
            .expect("encode");
        assert!(
            parse_check(&c, Direction::ClientToHost, &bytes).is_none(),
            "an unsigned request must never advance the state machine"
        );
    }

    #[test]
    fn a_tampered_check_is_rejected() {
        let c = creds();
        let mut bytes = build_check_request(&c, Direction::ClientToHost, random_transaction_id())
            .expect("build");
        // Flip a bit inside the USERNAME, which the HMAC covers.
        bytes[25] ^= 0x01;
        assert!(parse_check(&c, Direction::ClientToHost, &bytes).is_none());
    }

    #[test]
    fn garbage_is_not_a_check() {
        let c = creds();
        for junk in [vec![], vec![0u8], vec![0xFF; 64], vec![0u8; 40]] {
            assert!(parse_check(&c, Direction::ClientToHost, &junk).is_none());
        }
    }

    #[test]
    fn transaction_ids_are_not_predictable() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            assert!(
                seen.insert(random_transaction_id()),
                "a transaction id repeated"
            );
        }
    }
}

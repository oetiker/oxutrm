//! Self-signed certificates, and trusting exactly one of them.
//!
//! No certificate authority is involved anywhere. The host generates a fresh
//! certificate per session, the SHA-256 of its SPKI travels over SSH, and the
//! client trusts that fingerprint and nothing else. The trust chain is SSH's,
//! unchanged.
//!
//! # The hole people ship here
//!
//! A custom [`ServerCertVerifier`] has four methods, and only one of them is
//! about the certificate. The other three —
//! [`ServerCertVerifier::verify_tls12_signature`],
//! [`ServerCertVerifier::verify_tls13_signature`] and
//! [`ServerCertVerifier::supported_verify_schemes`] — are about whether the
//! peer can **prove it holds the private key**. The copy-paste that circulates
//! stubs all three to `Ok(...)`.
//!
//! That is not a small shortcut. A certificate is public: anyone who has ever
//! talked to the host has a copy, and the fingerprint travels in the clear
//! inside the SSH channel. If the signature is not checked, "I present this
//! certificate" becomes the entire authentication, and any party who has seen
//! the certificate can impersonate the host. Pinning without signature
//! verification is not weaker authentication — it is none.
//!
//! So [`PinnedSpki`] checks the fingerprint **and** hands all three signature
//! methods to the crypto provider's real implementations.
//!
//! `rustls` is reached as `quinn::rustls` throughout. The workspace
//! deliberately declares no `rustls` version of its own, so ours cannot drift
//! out of step with whatever `quinn` links against — and a mismatched `rustls`
//! fails in confusing ways at the `QuicClientConfig::try_from` boundary rather
//! than at the dependency resolver.

use std::sync::{Arc, OnceLock};

use anyhow::Context as _;
use oxutrm_proto::{ClientSpki, HostSpki};
use quinn::rustls;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error as TlsError, SignatureScheme};

/// The SAN in the generated certificate and the SNI oxutrm sends.
///
/// `.invalid` is reserved by RFC 2606 and never resolves. oxutrm does **not**
/// forge another party's domain in SNI: that is impersonation, it buys nothing
/// over honest QUIC framing, and it is out of scope. The verifier ignores the
/// name anyway — the fingerprint is the whole trust decision.
pub const CERT_NAME: &str = "oxutrm.invalid";

/// The crypto provider this crate uses, as a value.
pub fn provider() -> Arc<CryptoProvider> {
    static P: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Arc::clone(P.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())))
}

/// Install `ring` as the **process-default** provider.
///
/// A visible step rather than a hidden side effect, because it is load-bearing
/// and its absence fails somewhere else entirely: `rustls` 0.23 consults the
/// process default in places that take no builder, and
/// `quinn::crypto::rustls::QuicClientConfig::try_from` is one of them. Without
/// this, building a client config fails with an error that says nothing about
/// providers.
///
/// Idempotent, and deliberately tolerant of a provider someone else installed
/// first: theirs is as good as ours, and racing to replace it would be worse
/// than accepting it.
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// A fresh self-signed certificate, its key, and the SHA-256 of its SPKI.
///
/// The key never touches disk: it exists in the returned value and in whatever
/// `quinn` does with it, and nowhere else.
pub fn generate_cert() -> anyhow::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, [u8; 32])>
{
    let ck = rcgen::generate_simple_self_signed(vec![CERT_NAME.to_owned()])
        .context("generating a self-signed certificate")?;

    let cert: CertificateDer<'static> = ck.cert.der().clone();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der()));
    // Read back out of the certificate rather than from the key pair, so this
    // is the same code path the verifier uses. If the two ever disagreed,
    // every connection would fail with no clue why.
    let fingerprint = crate::der::spki_sha256(cert.as_ref())
        .context("the certificate we just generated has no parsable SPKI")?;

    Ok((cert, key, fingerprint))
}

// ---------------------------------------------------------------------------
// The two genuinely shared bodies
// ---------------------------------------------------------------------------
//
// Exactly two things are the same in both directions, and only those two are
// factored: the SPKI comparison, and the three provider-delegating signature
// methods. Everything else about the two verifiers differs — different traits,
// different method sets, and `ClientCertVerifier` additionally requires
// `root_hint_subjects`, which has no server-side counterpart. Sharing more
// than this would mean inventing an abstraction over two traits that rustls
// deliberately kept apart.

/// The whole trust decision: this certificate's SPKI, or nothing.
///
/// `what` names the peer, because a pinning failure that does not say which
/// end failed sends you looking at the wrong half of the connection.
fn spki_is(
    expected: &[u8; 32],
    end_entity: &CertificateDer<'_>,
    what: &'static str,
) -> Result<(), TlsError> {
    let got = crate::der::spki_sha256(end_entity.as_ref())
        .ok_or_else(|| TlsError::General(format!("{what} certificate has no parsable SPKI")))?;
    if got != *expected {
        return Err(TlsError::General(format!(
            "{what} certificate SPKI does not match the pinned fingerprint"
        )));
    }
    Ok(())
}

/// Delegated to the provider. Returning `Ok` here unconditionally is the
/// common shortcut and it throws away proof of key possession — in **either**
/// direction. Stubbing these on the client verifier reproduces the identical
/// hole the module docs describe, pointing the other way: anyone who has seen
/// the client's certificate could then attach as that client.
fn tls12_signature(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, TlsError> {
    rustls::crypto::verify_tls12_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

/// The one that actually runs: oxutrm is TLS 1.3 only.
fn tls13_signature(
    provider: &CryptoProvider,
    message: &[u8],
    cert: &CertificateDer<'_>,
    dss: &DigitallySignedStruct,
) -> Result<HandshakeSignatureValid, TlsError> {
    rustls::crypto::verify_tls13_signature(
        message,
        cert,
        dss,
        &provider.signature_verification_algorithms,
    )
}

/// If this were empty the handshake would have nothing to negotiate and the
/// signature check would never run at all.
fn verify_schemes(provider: &CryptoProvider) -> Vec<SignatureScheme> {
    provider
        .signature_verification_algorithms
        .supported_schemes()
}

/// Accepts exactly one SPKI fingerprint — and still checks the handshake
/// signature.
///
/// The **host's**, and the type says so. Both fingerprints are 32 bytes, both
/// are in scope on both sides, and handing this one the client's used to
/// compile — see [`HostSpki`].
#[derive(Debug)]
pub struct PinnedSpki {
    expected: HostSpki,
    provider: Arc<CryptoProvider>,
}

impl PinnedSpki {
    pub fn new(expected: HostSpki) -> PinnedSpki {
        PinnedSpki {
            expected,
            provider: provider(),
        }
    }
}

impl ServerCertVerifier for PinnedSpki {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // The trust root is SSH. Name, expiry, chain and OCSP are all
        // irrelevant and deliberately unchecked: the only thing that grants
        // trust is the fingerprint that arrived over SSH.
        //
        // This alone would NOT be enough - see the module docs. The signature
        // methods below are what turn "presents this certificate" into "holds
        // its private key".
        spki_is(self.expected.as_bytes(), end_entity, "host")?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tls12_signature(&self.provider, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tls13_signature(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        verify_schemes(&self.provider)
    }
}

// ---------------------------------------------------------------------------
// The other direction
// ---------------------------------------------------------------------------

/// Accepts exactly one **client** SPKI fingerprint — and still checks the
/// handshake signature.
///
/// This is the half the project shipped without, and the exposure was not a
/// probe surface. Once the host's endpoint listens on the punched socket,
/// anything that completes the handshake runs
/// `Link::new` → `HostSession::spawn` → `HostTerm::spawn` → `Pty::spawn` →
/// `Command::new(shell)`. The pre-shared key does not help: it is HKDF'd into
/// STUN credentials and never reaches TLS, so it authenticates path discovery
/// and not the endpoint. `StunDemuxSocket` hands quinn every non-STUN datagram
/// from **any** source; nomination decides where *we* send, and nothing about
/// what quinn accepts.
///
/// [`PinnedSpki`] could not be reused for this. It implements
/// [`ServerCertVerifier`]; this needs [`ClientCertVerifier`] — a different
/// trait whose `verify_client_cert` takes no server name and no OCSP response,
/// and which additionally requires [`ClientCertVerifier::root_hint_subjects`].
/// Only the two bodies that are genuinely identical are shared.
///
/// **`offer_client_auth` and `client_auth_mandatory` are not overridden.**
/// Both default to `true` in rustls, and a `false` in either would silently
/// revert this entire change while every positive test still passed — so they
/// are asserted in the tests below rather than restated here, where a wrong
/// value would look like a decision.
#[derive(Debug)]
pub struct PinnedClientSpki {
    expected: ClientSpki,
    provider: Arc<CryptoProvider>,
}

impl PinnedClientSpki {
    pub fn new(expected: ClientSpki) -> PinnedClientSpki {
        PinnedClientSpki {
            expected,
            provider: provider(),
        }
    }
}

impl ClientCertVerifier for PinnedClientSpki {
    /// Empty, deliberately. Per RFC 8446 §4.2.4 an empty
    /// `certificate_authorities` tells the client to send whatever certificate
    /// it has — which is exactly right when the trust root is a fingerprint
    /// carried over SSH rather than a CA. There is no CA here to hint at, and
    /// inventing one would only tell the peer to send something we would then
    /// refuse.
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        // As on the other side: name, expiry, chain and CA are irrelevant and
        // deliberately unchecked. The fingerprint that arrived over SSH in
        // `ClientHello` is the whole trust decision, and the signature methods
        // below are what make it mean "holds the private key" rather than
        // "has seen the certificate".
        spki_is(self.expected.as_bytes(), end_entity, "client")?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tls12_signature(&self.provider, message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        tls13_signature(&self.provider, message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        verify_schemes(&self.provider)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::internal::msgs::codec::{Codec as _, Reader};

    /// Build a `DigitallySignedStruct` from its wire form.
    ///
    /// `DigitallySignedStruct::new` is `pub(crate)` in rustls, but the type is
    /// `Codec`, and the encoding is just the scheme followed by a 16-bit
    /// length and the signature. Going through the wire form is how a real
    /// handshake produces one anyway.
    fn signed_struct(scheme: SignatureScheme, sig: &[u8]) -> DigitallySignedStruct {
        let mut bytes = Vec::new();
        scheme.encode(&mut bytes);
        bytes.extend_from_slice(&(sig.len() as u16).to_be_bytes());
        bytes.extend_from_slice(sig);
        DigitallySignedStruct::read(&mut Reader::init(&bytes)).expect("a well-formed struct")
    }

    #[test]
    fn a_generated_certificate_reports_its_own_fingerprint() {
        let (cert, _key, fp) = generate_cert().unwrap();
        assert_eq!(
            crate::der::spki_sha256(cert.as_ref()).unwrap(),
            fp,
            "the fingerprint must describe the certificate that is returned"
        );
    }

    #[test]
    fn every_attach_generates_fresh_key_material() {
        // A stolen key from an earlier session must not be able to reattach.
        let (_, _, a) = generate_cert().unwrap();
        let (_, _, b) = generate_cert().unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn the_private_key_comes_back_as_pkcs8() {
        let (_cert, key, _fp) = generate_cert().unwrap();
        assert!(matches!(key, PrivateKeyDer::Pkcs8(_)));
    }

    #[test]
    fn the_verifier_accepts_exactly_the_pinned_fingerprint() {
        let (cert, _k, fp) = generate_cert().unwrap();
        let (other, _k2, other_fp) = generate_cert().unwrap();
        assert_ne!(fp, other_fp);

        let v = PinnedSpki::new(HostSpki::new(fp));
        let name = ServerName::try_from(CERT_NAME).unwrap();
        let now = UnixTime::now();

        assert!(v.verify_server_cert(&cert, &[], &name, &[], now).is_ok());
        assert!(
            v.verify_server_cert(&other, &[], &name, &[], now).is_err(),
            "no certificate authority is involved: the pin is the whole decision"
        );
    }

    #[test]
    fn the_verifier_ignores_the_server_name() {
        let (cert, _k, fp) = generate_cert().unwrap();
        let v = PinnedSpki::new(HostSpki::new(fp));
        let wrong = ServerName::try_from("something.else.invalid").unwrap();
        assert!(
            v.verify_server_cert(&cert, &[], &wrong, &[], UnixTime::now())
                .is_ok()
        );
    }

    #[test]
    fn the_verifier_rejects_a_certificate_it_cannot_parse() {
        let v = PinnedSpki::new(HostSpki::new([0u8; 32]));
        let junk = CertificateDer::from(vec![0xFFu8; 64]);
        let name = ServerName::try_from(CERT_NAME).unwrap();
        assert!(
            v.verify_server_cert(&junk, &[], &name, &[], UnixTime::now())
                .is_err()
        );
    }

    #[test]
    fn a_forged_handshake_signature_is_rejected() {
        // THE test for the hole described in the module docs. The certificate
        // is the pinned one, so `verify_server_cert` is perfectly happy - and
        // the signature is rubbish. An implementation that stubbed
        // verify_tls13_signature to Ok(...) would accept this, which is
        // exactly how pinning gets reduced to "knows a public certificate".
        let (cert, _key, fp) = generate_cert().unwrap();
        let v = PinnedSpki::new(HostSpki::new(fp));

        assert!(
            v.verify_server_cert(
                &cert,
                &[],
                &ServerName::try_from(CERT_NAME).unwrap(),
                &[],
                UnixTime::now()
            )
            .is_ok(),
            "the certificate itself must pass, or this proves nothing"
        );

        let dss = signed_struct(SignatureScheme::ECDSA_NISTP256_SHA256, &[0u8; 64]);
        assert!(
            v.verify_tls13_signature(b"a transcript the peer never signed", &cert, &dss)
                .is_err(),
            "a signature that does not verify must be refused - anything else \
             discards proof that the peer holds the private key"
        );
        assert!(
            v.verify_tls12_signature(b"a transcript the peer never signed", &cert, &dss)
                .is_err()
        );
    }

    #[test]
    fn the_verifier_advertises_real_signature_schemes() {
        // If this were empty, the handshake would have nothing to negotiate
        // and the signature check would never run at all.
        let v = PinnedSpki::new(HostSpki::new([0u8; 32]));
        let schemes = v.supported_verify_schemes();
        assert!(!schemes.is_empty());
        assert!(
            schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256),
            "expected the scheme rcgen's default key uses, got {schemes:?}"
        );
    }

    // ---- the other direction: the host pinning the client ----

    #[test]
    fn the_client_verifier_accepts_exactly_the_pinned_fingerprint() {
        let (cert, _k, fp) = generate_cert().unwrap();
        let (other, _k2, other_fp) = generate_cert().unwrap();
        assert_ne!(fp, other_fp);

        let v = PinnedClientSpki::new(ClientSpki::new(fp));
        let now = UnixTime::now();

        assert!(v.verify_client_cert(&cert, &[], now).is_ok());
        assert!(
            v.verify_client_cert(&other, &[], now).is_err(),
            "a client certificate the host never heard about must not be accepted"
        );
    }

    #[test]
    fn the_client_verifier_rejects_a_certificate_it_cannot_parse() {
        let v = PinnedClientSpki::new(ClientSpki::new([0u8; 32]));
        let junk = CertificateDer::from(vec![0xFFu8; 64]);
        assert!(v.verify_client_cert(&junk, &[], UnixTime::now()).is_err());
    }

    /// The mirror of `a_forged_handshake_signature_is_rejected`, and it has to
    /// exist separately: the two verifiers implement different traits, so
    /// nothing about the server-side test constrains this code at all. Stubbing
    /// these three to `Ok(..)` here would reduce client pinning to "has seen
    /// the client's certificate" — and the client's certificate travels to the
    /// host on every attach.
    #[test]
    fn a_forged_client_handshake_signature_is_rejected() {
        let (cert, _key, fp) = generate_cert().unwrap();
        let v = PinnedClientSpki::new(ClientSpki::new(fp));

        assert!(
            v.verify_client_cert(&cert, &[], UnixTime::now()).is_ok(),
            "the certificate itself must pass, or this proves nothing"
        );

        let dss = signed_struct(SignatureScheme::ECDSA_NISTP256_SHA256, &[0u8; 64]);
        assert!(
            v.verify_tls13_signature(b"a transcript the peer never signed", &cert, &dss)
                .is_err(),
            "a signature that does not verify must be refused - anything else \
             discards proof that the client holds the private key"
        );
        assert!(
            v.verify_tls12_signature(b"a transcript the peer never signed", &cert, &dss)
                .is_err()
        );
    }

    #[test]
    fn the_client_verifier_advertises_real_signature_schemes() {
        let v = PinnedClientSpki::new(ClientSpki::new([0u8; 32]));
        let schemes = v.supported_verify_schemes();
        assert!(!schemes.is_empty());
        assert!(
            schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256),
            "expected the scheme rcgen's default key uses, got {schemes:?}"
        );
    }

    /// **The test that guards the whole change against a one-word reversion.**
    ///
    /// Both default to `true` and the implementation deliberately does not
    /// override them, which means there is no line of code anywhere that
    /// states the requirement — a future `fn client_auth_mandatory(&self) ->
    /// bool { false }` would look like a considered decision and would turn
    /// the certificate request into a polite suggestion. Every positive test
    /// in this repository would still pass: a well-behaved client sends its
    /// certificate regardless.
    ///
    /// `offer_client_auth` false is worse still — no CertificateRequest is
    /// sent at all, so `verify_client_cert` is never reached and the pin never
    /// runs.
    #[test]
    fn client_auth_is_both_offered_and_mandatory() {
        let v = PinnedClientSpki::new(ClientSpki::new([0u8; 32]));
        assert!(
            v.offer_client_auth(),
            "without a CertificateRequest the client is never asked, and the pin never runs"
        );
        assert!(
            v.client_auth_mandatory(),
            "a non-mandatory client auth accepts a peer that simply declines to \
             present a certificate, which is the entire hole this change closes"
        );
    }

    /// RFC 8446 §4.2.4: an empty `certificate_authorities` tells the client to
    /// send whatever certificate it has. That is right here — the trust root
    /// is a fingerprint carried over SSH, and there is no CA to name.
    #[test]
    fn the_client_verifier_hints_at_no_certificate_authorities() {
        let v = PinnedClientSpki::new(ClientSpki::new([0u8; 32]));
        assert!(v.root_hint_subjects().is_empty());
    }

    /// The failure text says which end failed. Two pins now run on every
    /// attach and they fail identically otherwise, so a message that does not
    /// name the peer sends you to the wrong half of the connection.
    #[test]
    fn a_pin_failure_names_which_peer_it_was_about() {
        let (_c, _k, fp) = generate_cert().unwrap();
        let (other, _k2, _ofp) = generate_cert().unwrap();
        let now = UnixTime::now();

        let host_err = format!(
            "{:?}",
            PinnedSpki::new(HostSpki::new(fp))
                .verify_server_cert(
                    &other,
                    &[],
                    &ServerName::try_from(CERT_NAME).unwrap(),
                    &[],
                    now
                )
                .unwrap_err()
        );
        let client_err = format!(
            "{:?}",
            PinnedClientSpki::new(ClientSpki::new(fp))
                .verify_client_cert(&other, &[], now)
                .unwrap_err()
        );
        assert!(host_err.contains("host"), "{host_err}");
        assert!(client_err.contains("client"), "{client_err}");
    }

    #[test]
    fn the_provider_is_one_shared_instance_and_installs_idempotently() {
        assert!(Arc::ptr_eq(&provider(), &provider()));
        install_crypto_provider();
        install_crypto_provider();
        assert!(
            CryptoProvider::get_default().is_some(),
            "rustls 0.23 needs a process default before QuicClientConfig::try_from"
        );
    }
}

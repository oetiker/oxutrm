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
use quinn::rustls;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

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

/// Accepts exactly one SPKI fingerprint — and still checks the handshake
/// signature.
#[derive(Debug)]
pub struct PinnedSpki {
    expected: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl PinnedSpki {
    pub fn new(expected: [u8; 32]) -> PinnedSpki {
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
        let got = crate::der::spki_sha256(end_entity.as_ref())
            .ok_or_else(|| TlsError::General("peer certificate has no parsable SPKI".into()))?;
        if got != self.expected {
            return Err(TlsError::General(
                "peer certificate SPKI does not match the pinned fingerprint".into(),
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    /// Delegated to the provider. Returning `Ok` here unconditionally is the
    /// common shortcut and it throws away proof of key possession.
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    /// The one that actually runs: oxutrm is TLS 1.3 only.
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
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

        let v = PinnedSpki::new(fp);
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
        let v = PinnedSpki::new(fp);
        let wrong = ServerName::try_from("something.else.invalid").unwrap();
        assert!(
            v.verify_server_cert(&cert, &[], &wrong, &[], UnixTime::now())
                .is_ok()
        );
    }

    #[test]
    fn the_verifier_rejects_a_certificate_it_cannot_parse() {
        let v = PinnedSpki::new([0u8; 32]);
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
        let v = PinnedSpki::new(fp);

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
        let v = PinnedSpki::new([0u8; 32]);
        let schemes = v.supported_verify_schemes();
        assert!(!schemes.is_empty());
        assert!(
            schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256),
            "expected the scheme rcgen's default key uses, got {schemes:?}"
        );
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

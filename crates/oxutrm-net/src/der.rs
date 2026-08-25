//! A minimal DER walker: just enough to pull the `SubjectPublicKeyInfo` out of
//! an X.509 certificate.
//!
//! The pinning verifier is handed the bytes the peer presented and has to hash
//! *that* certificate's public key. `rustls::pki_types` does not parse
//! certificates and `rustls-webpki` does not expose the SPKI, so this is the
//! smallest honest thing that works. It parses nothing it does not need and
//! never allocates.

use sha2::{Digest, Sha256};

/// One DER tag-length-value, split apart.
struct Tlv<'a> {
    tag: u8,
    /// Just the contents, without the tag and length.
    value: &'a [u8],
    /// Tag, length and contents together - what an outer structure would
    /// embed, and what gets hashed.
    whole: &'a [u8],
    /// Whatever follows this TLV in the buffer.
    rest: &'a [u8],
}

/// Split one DER TLV off the front of `buf`.
fn tlv(buf: &[u8]) -> Option<Tlv<'_>> {
    let tag = *buf.first()?;
    let first_len = *buf.get(1)?;
    let (len, header) = if first_len < 0x80 {
        (first_len as usize, 2usize)
    } else {
        let n = (first_len & 0x7f) as usize;
        // n == 0 is the indefinite form, which DER forbids. More than four
        // length bytes would be a certificate larger than any real one.
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *buf.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = header.checked_add(len)?;
    if end > buf.len() {
        return None;
    }
    Some(Tlv {
        tag,
        value: &buf[header..end],
        whole: &buf[..end],
        rest: &buf[end..],
    })
}

/// The complete `SubjectPublicKeyInfo` TLV of an X.509 certificate.
///
/// ```text
/// Certificate    ::= SEQUENCE { tbsCertificate, signatureAlgorithm, signature }
/// TBSCertificate ::= SEQUENCE { [0] version OPTIONAL, serialNumber, signature,
///                               issuer, validity, subject,
///                               subjectPublicKeyInfo, ... }
/// ```
pub fn spki_der(cert: &[u8]) -> Option<&[u8]> {
    const SEQUENCE: u8 = 0x30;
    const CONTEXT_0: u8 = 0xA0;

    let certificate = tlv(cert)?;
    if certificate.tag != SEQUENCE {
        return None;
    }
    let tbs = tlv(certificate.value)?;
    if tbs.tag != SEQUENCE {
        return None;
    }

    let mut rest = tbs.value;
    // [0] EXPLICIT version, optional.
    if rest.first() == Some(&CONTEXT_0) {
        rest = tlv(rest)?.rest;
    }
    // serialNumber, signature, issuer, validity, subject.
    for _ in 0..5 {
        rest = tlv(rest)?.rest;
    }
    Some(tlv(rest)?.whole)
}

/// SHA-256 of the `SubjectPublicKeyInfo`.
///
/// This is the value that travels in `Signal::HostHello.cert_spki_sha256` and
/// the only thing the client trusts.
pub fn spki_sha256(cert: &[u8]) -> Option<[u8; 32]> {
    Some(Sha256::digest(spki_der(cert)?).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spki_we_extract_is_the_one_rcgen_made() {
        // Ground truth rather than a snapshot: rcgen documents
        // public_key_der() as the complete SubjectPublicKeyInfo (RFC 5280
        // section 4.1), so the two must agree byte for byte.
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let from_cert = spki_der(ck.cert.der().as_ref()).expect("must find the SPKI");
        assert_eq!(from_cert, ck.key_pair.public_key_der().as_slice());
    }

    #[test]
    fn two_certificates_hash_differently() {
        let a = rcgen::generate_simple_self_signed(vec!["a.invalid".to_owned()]).unwrap();
        let b = rcgen::generate_simple_self_signed(vec!["b.invalid".to_owned()]).unwrap();
        assert_ne!(
            spki_sha256(a.cert.der().as_ref()).unwrap(),
            spki_sha256(b.cert.der().as_ref()).unwrap()
        );
    }

    #[test]
    fn the_hash_is_the_sha256_of_the_extracted_bytes() {
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let der = ck.cert.der();
        let expect: [u8; 32] = Sha256::digest(spki_der(der.as_ref()).unwrap()).into();
        assert_eq!(spki_sha256(der.as_ref()).unwrap(), expect);
    }

    #[test]
    fn garbage_returns_none_rather_than_panicking() {
        assert!(spki_der(&[]).is_none());
        assert!(spki_der(&[0x30]).is_none());
        assert!(spki_der(&[0x30, 0x82]).is_none());
        assert!(
            spki_der(&[0x02, 0x01, 0x00]).is_none(),
            "an INTEGER is not a certificate"
        );
        assert!(spki_der(&[0xFF; 64]).is_none());
    }

    #[test]
    fn every_truncation_of_a_real_certificate_is_survivable() {
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let d = ck.cert.der().as_ref().to_vec();
        for cut in [0usize, 1, 2, 5, 20] {
            assert!(spki_sha256(&d[..cut]).is_none(), "cut {cut} must not parse");
        }
        // The real requirement is that nothing panics anywhere along it.
        for cut in (0..d.len()).step_by(7) {
            let _ = spki_sha256(&d[..cut]);
        }
        assert!(spki_sha256(&d).is_some(), "the whole thing still parses");
    }

    #[test]
    fn a_certificate_whose_length_header_lies_is_rejected() {
        let ck = rcgen::generate_simple_self_signed(vec!["oxutrm.invalid".to_owned()]).unwrap();
        let mut d = ck.cert.der().as_ref().to_vec();
        // Bytes 2..4 are a two-byte long-form length. Claim far more content
        // than the buffer holds.
        d[2] = 0xFF;
        d[3] = 0xFF;
        assert!(spki_der(&d).is_none());
    }
}

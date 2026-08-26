//! The two 32-byte values that cross the signalling channel.
//!
//! Both the pre-shared key and the host certificate's SPKI fingerprint are
//! minted as 32 raw bytes, travel as base64 text, and are consumed as 32 raw
//! bytes again. Until these types existed the two halves of that sentence were
//! joined nowhere: the host formatted base64 `String`s into [`crate::Signal`],
//! every consumer took `[u8; 32]`, and there was no decode call anywhere in
//! the tree — not in the product, not in a test. The seam was structurally
//! open and no test could go red about it.
//!
//! So the encoding lives **in the `serde` impls and nowhere else**, which is
//! the point: this crate's standing rule is that an API with no caller is not
//! enforcement, and every signalling round trip is a caller of these impls,
//! forever. There is no second encode site to drift out of step with the
//! first, because there is no second encode site.
//!
//! # Why two types and not one alias
//!
//! [`Psk`] and [`SpkiSha256`] are both 32 bytes and have no conversion between
//! them. On the wire they were both `String`, and off it both `[u8; 32]`, so
//! handing the fingerprint to the code that wanted the PSK type-checked
//! perfectly and produced a session that could not authenticate anything. It
//! is now a compile error.
//!
//! They differ in more than name. A PSK is a secret: redacted [`Debug`],
//! zeroed on [`Drop`], compared without an early return, and its encoded form
//! is scrubbed from the stack before [`Psk::serialize`] returns. A fingerprint
//! is a digest of a public key — it is [`Copy`], and its `Debug` prints it,
//! because a pinning failure you cannot read is a pinning failure you cannot
//! diagnose.

use base64::Engine as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

/// Every value here is 32 bytes: the PSK is 32 CSPRNG bytes, and a SHA-256
/// digest is 32 bytes by definition.
pub const WIRE_KEY_LEN: usize = 32;

/// The number of padded base64 characters [`WIRE_KEY_LEN`] bytes encode to.
///
/// This is a **necessary and not a sufficient** condition, and the constant is
/// documented next to that fact because it is the trap: base64 pads to a
/// multiple of four, so 31, 32 **and** 33 bytes all encode to exactly 44
/// characters. The character count is what rejects the absurd — a megabyte of
/// well-formed base64 — before anything is decoded. The decoded byte count is
/// what actually pins the length. Both checks are load-bearing; neither is
/// redundant.
pub const WIRE_KEY_B64_LEN: usize = 44;

/// Padded standard base64, deliberately.
///
/// Not `URL_SAFE_NO_PAD`. `oxutrm-net`'s `stunmsg` uses that alphabet for the
/// STUN USERNAME, where the value has to survive SASLprep — a different
/// alphabet for a different purpose, and unifying the two would break one of
/// them.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// Encode 32 bytes into a stack buffer and hand that to the serializer.
///
/// No `String`. A key's encoded form is a copy of the key in a different
/// alphabet, and a heap `String` is a copy nobody can scrub — which is exactly
/// the caveat the old `AttachKeys::psk_base64` had to document and could not
/// fix. Here the buffer is 44 bytes of stack, and `scrub` zeroes it before it
/// goes out of scope.
///
/// The same best-effort caveat as any zeroing in safe Rust applies: without a
/// volatile write the compiler may elide a dead store, and the fence only
/// makes that unlikely. It costs one pass over 44 bytes and removes the
/// longest-lived copy, which is the one worth removing.
fn serialize_b64<S: Serializer>(
    bytes: &[u8; WIRE_KEY_LEN],
    scrub: bool,
    s: S,
) -> Result<S::Ok, S::Error> {
    let mut buf = [0u8; WIRE_KEY_B64_LEN];
    let encoded = B64
        .encode_slice(bytes, &mut buf)
        .map_err(|e| serde::ser::Error::custom(format!("encoding key material: {e}")));
    let out = match encoded {
        Ok(n) => match std::str::from_utf8(&buf[..n]) {
            Ok(text) => s.serialize_str(text),
            Err(e) => Err(serde::ser::Error::custom(format!(
                "base64 produced non-utf-8: {e}"
            ))),
        },
        Err(e) => Err(e),
    };
    if scrub {
        buf.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
    out
}

/// Decode exactly [`WIRE_KEY_LEN`] bytes, or fail.
///
/// The order matters and it is the order written here:
///
/// 1. **Length, in characters, first.** One integer comparison, before the
///    decoder sees anything. A megabyte of otherwise-valid base64 dies here.
/// 2. **Decode into a fixed 32-byte array on the stack.** No `Vec`, no
///    capacity derived from the input — so "a legal value allocates nothing,
///    and an oversized one cannot cause a large allocation" is a property of
///    this function's shape rather than of a test that happens to pass. A
///    33-byte payload — also 44 characters — is rejected right here, because
///    it does not fit.
/// 3. **Decoded length last.** 44 characters is not proof of 32 bytes: 31
///    bytes encode to 44 characters with two pad bytes, and 31 bytes fit
///    comfortably in the target. This is the only step that catches them.
fn decode_b64<E: de::Error>(text: &str, what: &str) -> Result<[u8; WIRE_KEY_LEN], E> {
    if text.len() != WIRE_KEY_B64_LEN {
        return Err(E::custom(format!(
            "{what} must be {WIRE_KEY_B64_LEN} base64 characters for {WIRE_KEY_LEN} bytes, got {}",
            text.len()
        )));
    }
    let mut out = [0u8; WIRE_KEY_LEN];
    let decoded = B64
        .decode_slice(text.as_bytes(), &mut out)
        .map_err(|e| E::custom(format!("{what} is not {WIRE_KEY_LEN} bytes of base64: {e}")))?;
    if decoded != WIRE_KEY_LEN {
        return Err(E::custom(format!(
            "{what} must decode to {WIRE_KEY_LEN} bytes, got {decoded}"
        )));
    }
    Ok(out)
}

/// Reads a base64 string as [`WIRE_KEY_LEN`] bytes. `.0` names the field, so
/// the rejection says which one was wrong.
struct B64Visitor(&'static str);

impl de::Visitor<'_> for B64Visitor {
    type Value = [u8; WIRE_KEY_LEN];

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} as {WIRE_KEY_B64_LEN} base64 characters", self.0)
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
        decode_b64(v, self.0)
    }
}

// ---------------------------------------------------------------------------
// The pre-shared key
// ---------------------------------------------------------------------------

/// The attach pre-shared key: 32 bytes from the OS CSPRNG, fresh every attach,
/// never written to disk on either side.
///
/// Not `Copy`, because it has a `Drop` that zeroes it and a value that can be
/// copied by accident is a value whose copies are not zeroed. `Clone` is
/// explicit for the same reason.
#[derive(Clone)]
pub struct Psk([u8; WIRE_KEY_LEN]);

impl Psk {
    #[must_use]
    pub const fn new(bytes: [u8; WIRE_KEY_LEN]) -> Psk {
        Psk(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; WIRE_KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for Psk {
    /// Redacted, and hand-written for that reason. A derived `Debug` would put
    /// the PSK into the first log line or error that formatted a `Signal`, and
    /// a `HostHello` is formatted in exactly that way by half the tests in
    /// this crate.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Psk(<redacted>)")
    }
}

impl PartialEq for Psk {
    /// Compares every byte, always. `[u8; 32]`'s own `PartialEq` is free to
    /// return at the first difference; a comparison that leaks where two keys
    /// first differ is a comparison worth not writing, and folding costs 32
    /// operations.
    fn eq(&self, other: &Psk) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

impl Eq for Psk {}

impl Drop for Psk {
    /// Overwrite before the memory is reused. Best effort, with the same
    /// honest limits as any zeroing in safe Rust: the allocator may already
    /// have handed the page on, and without a volatile write the compiler is
    /// entitled to elide a dead store. The fence makes elision unlikely rather
    /// than impossible.
    ///
    /// This is the *only* place a PSK is zeroed now. It used to be
    /// `AttachKeys`, which could zero its own field and nothing else — not the
    /// `String` its base64 encoder had already produced, and not the copy that
    /// came off the wire, because no copy ever came off the wire.
    fn drop(&mut self) {
        self.0.fill(0);
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

impl Serialize for Psk {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_b64(&self.0, true, s)
    }
}

impl<'de> Deserialize<'de> for Psk {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Psk, D::Error> {
        d.deserialize_str(B64Visitor("psk")).map(Psk)
    }
}

// ---------------------------------------------------------------------------
// The certificate fingerprint
// ---------------------------------------------------------------------------

/// SHA-256 of the host certificate's `SubjectPublicKeyInfo`: the one thing the
/// client trusts about the host's TLS certificate.
///
/// Public data — a digest of a public key — so `Copy`, and a `Debug` that
/// actually prints it. A pinning mismatch is the failure this value exists to
/// cause, and one you cannot read in a log is one you cannot diagnose.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SpkiSha256([u8; WIRE_KEY_LEN]);

impl SpkiSha256 {
    #[must_use]
    pub const fn new(bytes: [u8; WIRE_KEY_LEN]) -> SpkiSha256 {
        SpkiSha256(bytes)
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8; WIRE_KEY_LEN] {
        &self.0
    }
}

impl std::fmt::Debug for SpkiSha256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut buf = [0u8; WIRE_KEY_B64_LEN];
        match B64
            .encode_slice(self.0, &mut buf)
            .ok()
            .and_then(|n| std::str::from_utf8(&buf[..n]).ok())
        {
            Some(text) => write!(f, "SpkiSha256({text})"),
            None => f.write_str("SpkiSha256(<unencodable>)"),
        }
    }
}

impl Serialize for SpkiSha256 {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        serialize_b64(&self.0, false, s)
    }
}

impl<'de> Deserialize<'de> for SpkiSha256 {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<SpkiSha256, D::Error> {
        d.deserialize_str(B64Visitor("cert_spki_sha256"))
            .map(SpkiSha256)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_psk_round_trips_through_json_as_forty_four_characters() {
        let psk = Psk::new([0x5a; WIRE_KEY_LEN]);
        let text = serde_json::to_string(&psk).expect("encode");
        // Quotes plus the encoded body.
        assert_eq!(text.len(), WIRE_KEY_B64_LEN + 2, "{text}");
        let back: Psk = serde_json::from_str(&text).expect("decode");
        assert_eq!(back, psk);
    }

    #[test]
    fn a_fingerprint_round_trips_too() {
        let fp = SpkiSha256::new([0xa5; WIRE_KEY_LEN]);
        let text = serde_json::to_string(&fp).expect("encode");
        let back: SpkiSha256 = serde_json::from_str(&text).expect("decode");
        assert_eq!(back, fp);
    }

    /// The reason `WIRE_KEY_B64_LEN` cannot be the whole check, stated as an
    /// executable fact rather than a comment: three different byte lengths
    /// encode to the same 44 characters.
    #[test]
    fn thirty_one_thirty_two_and_thirty_three_bytes_all_encode_to_forty_four_characters() {
        for n in [31usize, 32, 33] {
            let encoded = B64.encode(vec![0x11u8; n]);
            assert_eq!(
                encoded.len(),
                WIRE_KEY_B64_LEN,
                "{n} bytes encoded to {} characters",
                encoded.len()
            );
        }
    }

    #[test]
    fn only_thirty_two_bytes_survives_a_decode() {
        for n in [0usize, 1, 30, 31, 33, 34, 64] {
            let encoded = B64.encode(vec![0x11u8; n]);
            let json = format!("\"{encoded}\"");
            assert!(
                serde_json::from_str::<Psk>(&json).is_err(),
                "{n} bytes was accepted as a PSK"
            );
        }
        let ok = format!("\"{}\"", B64.encode([0x11u8; WIRE_KEY_LEN]));
        assert!(serde_json::from_str::<Psk>(&ok).is_ok());
    }

    #[test]
    fn debug_never_prints_the_psk() {
        let psk = Psk::new([0x5a; WIRE_KEY_LEN]);
        let shown = format!("{psk:?}");
        assert!(shown.contains("redacted"), "{shown}");
        let encoded = B64.encode([0x5a; WIRE_KEY_LEN]);
        assert!(!shown.contains(&encoded), "the PSK reached a Debug string");
        assert!(!shown.contains("5a"), "{shown}");
    }

    /// The fingerprint is public and its `Debug` says so, which is the
    /// asymmetry the two types exist to encode.
    #[test]
    fn debug_does_print_the_fingerprint() {
        let fp = SpkiSha256::new([0xa5; WIRE_KEY_LEN]);
        let shown = format!("{fp:?}");
        assert!(
            shown.contains(&B64.encode([0xa5u8; WIRE_KEY_LEN])),
            "{shown}"
        );
    }

    #[test]
    fn the_alphabet_is_padded_standard_not_url_safe() {
        // 0xfb 0xff produces `+` and `/` in the standard alphabet and `-` and
        // `_` in the URL-safe one. `stunmsg` deliberately uses the other
        // alphabet; this pins which one is on the signalling wire.
        let mut bytes = [0u8; WIRE_KEY_LEN];
        bytes[0] = 0xfb;
        bytes[1] = 0xff;
        bytes[2] = 0xbf;
        let text = serde_json::to_string(&SpkiSha256::new(bytes)).expect("encode");
        assert!(text.contains('+') && text.contains('/'), "{text}");
        assert!(text.ends_with("=\""), "standard base64 pads: {text}");
    }
}

//! Telling a STUN datagram from a QUIC one, on a socket that carries both.
//!
//! This is the load-bearing detail behind reusing one socket for the whole
//! session. NAT mappings are per-socket, so the address STUN discovers is only
//! meaningful for the socket that discovered it — which means QUIC has to run
//! on that same socket, and something has to sort the two apart.
//!
//! A STUN message's first two bits are `00` (RFC 5389 §6). Every QUIC packet
//! sets the fixed bit `0x40`, and long-header packets set `0x80` as well. So
//! `datagram[0] & 0xC0 == 0` already separates them.
//!
//! Two further checks are made anyway — the magic cookie and the length field.
//! A false positive here feeds a live QUIC packet to the STUN decoder, and the
//! cost of being sure is four byte comparisons.

/// RFC 5389 §6. Present in every STUN message worth parsing.
pub const STUN_MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xA4, 0x42];

/// The fixed 20-byte STUN header.
pub const STUN_HEADER_LEN: usize = 20;

/// True when the datagram is STUN rather than QUIC.
pub fn is_stun(datagram: &[u8]) -> bool {
    // Anything shorter than the header cannot be a STUN message at all.
    if datagram.len() < STUN_HEADER_LEN {
        return false;
    }
    // The two most significant bits of a STUN message type are zero; QUIC's
    // fixed bit lives in exactly that space.
    if datagram[0] & 0xC0 != 0 {
        return false;
    }
    if datagram[4..8] != STUN_MAGIC_COOKIE {
        return false;
    }
    // The length field counts attribute bytes only, and attributes are padded
    // to a multiple of four, so a length that is not is a malformed message.
    let len = u16::from_be_bytes([datagram[2], datagram[3]]) as usize;
    if !len.is_multiple_of(4) {
        return false;
    }
    // And it must actually describe the datagram in hand.
    datagram.len() >= STUN_HEADER_LEN + len
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A well-formed STUN Binding Request header with no attributes.
    fn binding_request() -> Vec<u8> {
        let mut m = vec![0x00, 0x01, 0x00, 0x00];
        m.extend_from_slice(&STUN_MAGIC_COOKIE);
        m.extend_from_slice(&[0x11; 12]); // transaction id
        m
    }

    /// A Binding Success Response carrying one 12-byte attribute.
    fn binding_response() -> Vec<u8> {
        let mut m = vec![0x01, 0x01, 0x00, 0x0c];
        m.extend_from_slice(&STUN_MAGIC_COOKIE);
        m.extend_from_slice(&[0x22; 12]);
        m.extend_from_slice(&[0x33; 12]);
        m
    }

    #[test]
    fn real_stun_messages_are_recognised() {
        assert!(is_stun(&binding_request()));
        assert!(is_stun(&binding_response()));

        // An Indication and an Error Response, both first-two-bits-zero.
        let mut indication = binding_request();
        indication[1] = 0x11;
        assert!(is_stun(&indication));
        let mut error = binding_request();
        error[0..2].copy_from_slice(&[0x01, 0x11]);
        assert!(is_stun(&error));
    }

    /// The case that matters on a live connection: a QUIC short header must
    /// never reach the STUN decoder.
    #[test]
    fn a_quic_short_header_is_not_stun() {
        // 0x40 fixed bit set, spin bit clear, 1-RTT key phase 0.
        let mut p = vec![0x40];
        p.extend_from_slice(&[0xAB; 40]); // destination CID and payload
        assert!(!is_stun(&p));

        // With the spin bit and key phase set as well.
        let mut p2 = vec![0x47];
        p2.extend_from_slice(&[0xCD; 64]);
        assert!(!is_stun(&p2));
    }

    #[test]
    fn quic_long_headers_are_not_stun() {
        // Initial, Handshake, Retry and 0-RTT all set 0x80 | 0x40.
        for first in [0xC0u8, 0xC3, 0xD0, 0xE0, 0xF0, 0xFF] {
            let mut p = vec![first];
            p.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // version
            p.extend_from_slice(&[0x55; 48]);
            assert!(!is_stun(&p), "first byte {first:#04x} classified as STUN");
        }
    }

    #[test]
    fn the_awkward_lengths_are_handled_rather_than_panicking() {
        assert!(!is_stun(&[]), "an empty datagram is not STUN");
        assert!(!is_stun(&[0x00]), "a one-byte datagram is not STUN");
        for n in 0..STUN_HEADER_LEN {
            let short = vec![0x00u8; n];
            assert!(!is_stun(&short), "a {n}-byte datagram cannot be STUN");
        }
        // Exactly the header, no attributes, is valid.
        assert_eq!(binding_request().len(), STUN_HEADER_LEN);
        assert!(is_stun(&binding_request()));
    }

    #[test]
    fn the_first_two_bits_being_zero_is_not_enough_on_its_own() {
        // Right shape, wrong cookie: some other protocol, not ours.
        let mut m = binding_request();
        m[4] = 0x00;
        assert!(!is_stun(&m), "the magic cookie was not checked");
    }

    #[test]
    fn a_length_that_lies_about_the_datagram_is_rejected() {
        // Claims 64 attribute bytes but carries none.
        let mut m = binding_request();
        m[2..4].copy_from_slice(&64u16.to_be_bytes());
        assert!(!is_stun(&m), "an over-long length field was accepted");
    }

    #[test]
    fn a_length_that_is_not_a_multiple_of_four_is_rejected() {
        let mut m = binding_response();
        m[2..4].copy_from_slice(&13u16.to_be_bytes());
        assert!(!is_stun(&m), "STUN attributes are always 4-byte aligned");
    }

    /// Over the whole first byte, exactly the `00` prefix may be STUN — which
    /// is the property that makes single-socket demultiplexing safe at all.
    #[test]
    fn no_first_byte_with_a_high_bit_set_can_ever_be_stun() {
        for first in 0u8..=255 {
            let mut m = binding_request();
            m[0] = first;
            assert_eq!(
                is_stun(&m),
                first & 0xC0 == 0,
                "first byte {first:#04x} classified wrongly"
            );
        }
    }
}

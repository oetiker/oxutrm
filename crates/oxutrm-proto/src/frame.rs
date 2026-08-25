//! Datagram framing.
//!
//! Screen and input state travel as QUIC datagrams, which are unreliable by
//! design: a lost datagram costs nothing, because the next one diffs from the
//! same acknowledged base and therefore contains whatever was lost.

use serde::{Deserialize, Serialize};

use crate::ProtoError;

/// The payload is zstd-compressed.
pub const FLAG_ZSTD: u8 = 0x01;

/// One datagram.
///
/// FIELD ORDER IS WIRE-SIGNIFICANT. postcard serialises in declaration order,
/// so reordering these silently breaks interoperability with no useful error.
/// Do not tidy this struct. `frame::tests::the_encoding_is_pinned_to_exact_bytes`
/// exists to make a reorder fail loudly.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Frame {
    /// The state this datagram describes.
    pub my_state: u64,
    /// The peer-acknowledged state it is a diff against. 0 means the payload
    /// is a full state rather than a diff.
    pub from_state: u64,
    /// The highest peer state we have applied.
    pub ack_state: u64,
    pub flags: u8,
    /// 0-based fragment index within this target state.
    pub frag_index: u16,
    /// Total fragments for this target state. 1 means unfragmented.
    pub frag_count: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn encode(&self) -> Result<Vec<u8>, ProtoError> {
        postcard::to_stdvec(self).map_err(|e| ProtoError::Malformed(format!("encoding frame: {e}")))
    }

    /// Decode one datagram.
    ///
    /// Trailing bytes are an error rather than being ignored: a datagram is a
    /// message boundary, so anything left over means this is not the frame it
    /// claims to be.
    pub fn decode(bytes: &[u8]) -> Result<Frame, ProtoError> {
        let (frame, rest) = postcard::take_from_bytes::<Frame>(bytes)
            .map_err(|e| ProtoError::Malformed(format!("decoding frame: {e}")))?;
        if !rest.is_empty() {
            return Err(ProtoError::Malformed(format!(
                "{} trailing bytes after the frame",
                rest.len()
            )));
        }
        Ok(frame)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ProtoError;

    /// Every field a different value, so ANY reorder changes these bytes.
    fn pinned() -> Frame {
        Frame {
            my_state: 1,
            from_state: 2,
            ack_state: 3,
            flags: 4,
            frag_index: 5,
            frag_count: 6,
            payload: vec![7, 8],
        }
    }

    /// Field order is wire-significant: postcard serialises in declaration
    /// order, so a tidy-up that reorders the struct would silently break
    /// interoperability with no useful error. This pins the encoding to exact
    /// bytes so that reorder fails here instead of in the field.
    #[test]
    fn the_encoding_is_pinned_to_exact_bytes() {
        let bytes = pinned().encode().expect("encode");
        assert_eq!(
            bytes,
            vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x02, 0x07, 0x08],
            "Frame's field order changed, or postcard's encoding did"
        );
    }

    #[test]
    fn large_values_are_varint_encoded() {
        let f = Frame {
            my_state: 300,
            from_state: 0,
            ack_state: 0,
            flags: 0,
            frag_index: 0,
            frag_count: 1,
            payload: Vec::new(),
        };
        let bytes = f.encode().expect("encode");
        assert_eq!(
            bytes,
            vec![0xac, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00],
            "300 must be two varint bytes, not eight fixed ones"
        );
    }

    #[test]
    fn every_field_survives_the_round_trip_in_its_own_slot() {
        let back = Frame::decode(&pinned().encode().expect("encode")).expect("decode");
        assert_eq!(back.my_state, 1);
        assert_eq!(back.from_state, 2);
        assert_eq!(back.ack_state, 3);
        assert_eq!(back.flags, 4);
        assert_eq!(back.frag_index, 5);
        assert_eq!(back.frag_count, 6);
        assert_eq!(back.payload, vec![7, 8]);
    }

    #[test]
    fn the_zstd_flag_is_bit_zero() {
        assert_eq!(FLAG_ZSTD, 0x01);
        let f = Frame {
            my_state: 9,
            from_state: 0,
            ack_state: 0,
            flags: FLAG_ZSTD,
            frag_index: 0,
            frag_count: 1,
            payload: vec![0xff],
        };
        let back = Frame::decode(&f.encode().unwrap()).expect("decode");
        assert_eq!(back.flags & FLAG_ZSTD, FLAG_ZSTD);
    }

    #[test]
    fn a_full_state_is_from_state_zero() {
        // 0 is reserved: seq starts at 1, so from_state == 0 means "this is a
        // full state, not a diff" (spec §8.5).
        let f = Frame {
            my_state: 1,
            from_state: 0,
            ack_state: 0,
            flags: 0,
            frag_index: 0,
            frag_count: 1,
            payload: vec![1, 2, 3],
        };
        let back = Frame::decode(&f.encode().unwrap()).expect("decode");
        assert_eq!(back.from_state, 0);
        assert_eq!(back.frag_count, 1, "1 means unfragmented");
    }

    #[test]
    fn a_large_payload_survives() {
        let f = Frame {
            my_state: u64::MAX,
            from_state: u64::MAX - 1,
            ack_state: 0,
            flags: 0,
            frag_index: u16::MAX,
            frag_count: u16::MAX,
            payload: vec![0x5a; 60_000],
        };
        let back = Frame::decode(&f.encode().unwrap()).expect("decode");
        assert_eq!(back.my_state, u64::MAX);
        assert_eq!(back.frag_index, u16::MAX);
        assert_eq!(back.payload.len(), 60_000);
    }

    #[test]
    fn truncated_bytes_are_malformed_not_a_panic() {
        let bytes = pinned().encode().expect("encode");
        for cut in 0..bytes.len() {
            match Frame::decode(&bytes[..cut]) {
                Err(ProtoError::Malformed(_)) => {}
                Ok(_) => {
                    // A prefix that happens to be a valid shorter Frame is
                    // acceptable only if it did not consume trailing garbage.
                    // With this fixture no prefix is valid, so this is a bug.
                    panic!("a {cut}-byte prefix must not decode");
                }
                Err(other) => panic!("expected Malformed, got {other:?}"),
            }
        }
    }

    #[test]
    fn trailing_bytes_are_rejected() {
        let mut bytes = pinned().encode().expect("encode");
        bytes.push(0xff);
        assert!(
            matches!(Frame::decode(&bytes), Err(ProtoError::Malformed(_))),
            "a datagram with trailing rubbish is not a Frame"
        );
    }
}

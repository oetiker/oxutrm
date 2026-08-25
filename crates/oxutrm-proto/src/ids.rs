//! The session identifier.

use crate::ProtoError;

/// 128-bit session identifier.
///
/// `Display` and `FromStr` are 32 lowercase hex characters. That form is what
/// travels in `Signal::HostHello.session_id`, what names the registry
/// directory, and what a user types after `--attach`, so it is deliberately
/// one representation and not three.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct SessionId(pub [u8; 16]);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl std::str::FromStr for SessionId {
    type Err = ProtoError;

    /// Accepts upper or lower case; `Display` always produces lower.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.len() != 32 {
            return Err(ProtoError::Malformed(format!(
                "session id must be 32 hex characters, got {}",
                s.len()
            )));
        }
        let mut bytes = [0u8; 16];
        for (i, byte) in bytes.iter_mut().enumerate() {
            let pair = &s[i * 2..i * 2 + 2];
            *byte = u8::from_str_radix(pair, 16).map_err(|_| {
                ProtoError::Malformed(format!(
                    "session id must be 32 hex characters, {pair:?} is not hex"
                ))
            })?;
        }
        Ok(SessionId(bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn display_is_thirty_two_lowercase_hex_characters() {
        let id = SessionId([
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ]);
        let shown = id.to_string();
        assert_eq!(shown, "00112233445566778899aabbccddeeff");
        assert_eq!(shown.len(), 32, "128 bits as hex");
        assert!(
            shown
                .chars()
                .all(|c| c.is_ascii_digit() || c.is_ascii_lowercase()),
            "lowercase hex only: {shown}"
        );
    }

    #[test]
    fn from_str_round_trips_display() {
        let id = SessionId([
            0xde, 0xad, 0xbe, 0xef, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09,
            0x0a, 0x0b,
        ]);
        let back = SessionId::from_str(&id.to_string()).expect("parse");
        assert_eq!(back, id);
    }

    #[test]
    fn from_str_accepts_uppercase_but_display_stays_lowercase() {
        let id = SessionId::from_str("AABBCCDDEEFF00112233445566778899").expect("parse");
        assert_eq!(id.to_string(), "aabbccddeeff00112233445566778899");
    }

    #[test]
    fn from_str_rejects_anything_that_is_not_sixteen_bytes_of_hex() {
        for bad in [
            "",
            "00112233445566778899aabbccddeef",    // 31 characters
            "00112233445566778899aabbccddeeff0",  // 33 characters
            "00112233445566778899aabbccddeefg",   // 'g' is not hex
            "00112233-4455-6677-8899-aabbccddee", // a uuid, not our format
        ] {
            assert!(SessionId::from_str(bad).is_err(), "must reject {bad:?}");
        }
    }

    #[test]
    fn a_parse_failure_says_what_it_wanted() {
        let err = SessionId::from_str("nonsense").expect_err("must fail");
        let shown = err.to_string();
        assert!(
            shown.contains("32"),
            "the message must name the length: {shown}"
        );
    }

    #[test]
    fn ids_are_usable_as_map_keys() {
        // Hash + Eq are contract-required, so this must compile as well as pass.
        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(SessionId([1; 16])));
        assert!(!seen.insert(SessionId([1; 16])));
        assert!(seen.insert(SessionId([2; 16])));
    }
}

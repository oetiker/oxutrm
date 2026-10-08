//! A session's name: what `--new --name`, `--attach <name>` and the
//! selector's `r` deal in.

use serde::{Deserialize, Serialize};

/// The longest name, in characters.
pub const MAX_NAME: usize = 24;

/// A session's name.
///
/// 1 to [`MAX_NAME`] characters, none of them a control character, no
/// whitespace at either end, and **at least one character outside
/// `[0-9a-f]`**. That last rule is what lets `--attach x` take a name or an
/// id prefix with no precedence between them: an id is lowercase hex, so a
/// name can never be read as one (spec §2.3).
///
/// The field is private: [`Name::parse`] is the only way to make one, and
/// the wire goes through it too (`serde(try_from)`), so a name that broke a
/// rule fails the whole message rather than reaching a session.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    /// `text` as a name, or why it cannot be one. The reason is for the
    /// user: the selector shows it under the list, `connect` prints it.
    pub fn parse(text: &str) -> Result<Name, String> {
        let n = text.chars().count();
        if n == 0 {
            return Err("a name cannot be empty".to_string());
        }
        if n > MAX_NAME {
            return Err(format!("a name has at most {MAX_NAME} characters"));
        }
        if text.chars().any(char::is_control) {
            return Err("a name cannot contain control characters".to_string());
        }
        if text.trim() != text {
            return Err("a name cannot start or end with a space".to_string());
        }
        if text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')) {
            return Err(
                "a name needs a character outside 0-9 and a-f, or it reads as a session id"
                    .to_string(),
            );
        }
        Ok(Name(text.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Name {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Name {
    type Error = String;
    fn try_from(s: String) -> Result<Self, Self::Error> {
        Name::parse(&s)
    }
}

impl From<Name> for String {
    fn from(n: Name) -> String {
        n.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_names_are_accepted() {
        for ok in ["build", "logs", "web-1", "Ölberg", "a b", "x", "deadbeeX"] {
            assert_eq!(Name::parse(ok).expect(ok).as_str(), ok);
        }
        // Exactly the limit.
        assert!(Name::parse(&"x".repeat(MAX_NAME)).is_ok());
    }

    #[test]
    fn a_name_that_could_be_an_id_prefix_is_refused() {
        for hex in ["a3f9", "deadbeef", "0", "cafe", "3ff1218f5e0c"] {
            let why = Name::parse(hex).expect_err(hex);
            assert!(why.contains("session id"), "{hex}: {why}");
        }
        // Uppercase hex is not what an id looks like, so it is a name.
        assert!(Name::parse("CAFE").is_ok());
    }

    #[test]
    fn empty_long_padded_and_control_names_are_refused() {
        assert!(Name::parse("").is_err());
        assert!(Name::parse(&"x".repeat(MAX_NAME + 1)).is_err());
        assert!(Name::parse(" build").is_err());
        assert!(Name::parse("build ").is_err());
        assert!(Name::parse("bu\tild").is_err());
        assert!(Name::parse("bu\u{1b}[2Jild").is_err());
    }

    #[test]
    fn the_limit_is_in_characters_not_bytes() {
        let wide = "\u{e9}".repeat(MAX_NAME);
        assert!(wide.len() > MAX_NAME);
        assert!(Name::parse(&wide).is_ok());
    }

    #[test]
    fn the_wire_goes_through_parse() {
        let n = Name::parse("build").unwrap();
        let json = serde_json::to_string(&n).unwrap();
        assert_eq!(json, "\"build\"");
        assert_eq!(serde_json::from_str::<Name>(&json).unwrap(), n);
        assert!(serde_json::from_str::<Name>("\"cafe\"").is_err());
    }
}

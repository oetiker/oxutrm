//! A remote program's hyperlinks (OSC 8), as part of the screen state.
//!
//! # Why a table and an id per cell
//!
//! A link usually covers a word or a file name, and its URI can be two
//! kilobytes long. Each cell therefore carries a small id
//! ([`Cell::link`](crate::Cell::link), 0 for none) and the state carries one
//! table from id to URI ([`ScreenState::links`](crate::ScreenState::links)).
//! An unlinked cell costs one byte on the wire and a URI travels once per
//! state, not once per cell.
//!
//! # Safety (**I10**)
//!
//! The client writes each URI into an OSC 8 sequence on the user's real
//! terminal, between `;` and BEL. A URI is therefore held to the bytes the
//! OSC 8 specification allows, `0x20..=0x7E`: that set holds no ESC, no BEL
//! and no C1, so a URI cannot end the sequence early. Its length is bounded
//! by [`MAX_LINK_URI`] and tested before the bytes are scanned, as I8 does.
//! A link is rejected, never repaired, for the reasons in `text.rs`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{ApplyError, MAX_LINK_URI, MAX_LINKS, TextField};

/// Is `uri` a URI the client may write into an OSC 8?
///
/// 1..=[`MAX_LINK_URI`] bytes, each in `0x20..=0x7E`. `id` only names the
/// link in the error.
pub fn check_link_uri(id: u16, uri: &str) -> Result<(), ApplyError> {
    // Length FIRST, as in I8: it is O(1) and it is the half that bounds work.
    if uri.len() > MAX_LINK_URI {
        return Err(ApplyError::TextTooLong {
            field: TextField::LinkUri,
            len: uri.len(),
            max: MAX_LINK_URI,
        });
    }
    if uri.is_empty() {
        return Err(ApplyError::BadLinkUri { id, byte: None });
    }
    if let Some(b) = uri.bytes().find(|b| !(0x20..=0x7e).contains(b)) {
        return Err(ApplyError::BadLinkUri { id, byte: Some(b) });
    }
    Ok(())
}

/// **I10** for a link table on its own: at most [`MAX_LINKS`] entries, no key
/// 0, every URI acceptable to [`check_link_uri`].
///
/// That every cell's link is a key of the table is checked by
/// [`ScreenState::validate`](crate::ScreenState::validate), which sees both.
pub fn check_links(links: &BTreeMap<u16, Arc<str>>) -> Result<(), ApplyError> {
    if links.len() > MAX_LINKS {
        return Err(ApplyError::TooManyLinks {
            len: links.len(),
            max: MAX_LINKS,
        });
    }
    for (&id, uri) in links {
        if id == 0 {
            return Err(ApplyError::LinkIdZero);
        }
        check_link_uri(id, uri)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(u16, &str)]) -> BTreeMap<u16, Arc<str>> {
        entries.iter().map(|&(k, v)| (k, Arc::from(v))).collect()
    }

    #[test]
    fn an_ordinary_uri_is_accepted() {
        for uri in [
            "https://example.org/",
            "file://thinlinc/home/oetiker/notes.md",
            "http://a/b?c=d&e=f#g~h",
            " ",
        ] {
            assert_eq!(check_link_uri(1, uri), Ok(()), "{uri:?}");
        }
    }

    #[test]
    fn a_uri_of_exactly_the_cap_is_accepted_and_one_byte_more_is_not() {
        let at = format!("https://{}", "a".repeat(MAX_LINK_URI - 8));
        assert_eq!(check_link_uri(1, &at), Ok(()));
        assert_eq!(
            check_link_uri(1, &format!("{at}a")),
            Err(ApplyError::TextTooLong {
                field: TextField::LinkUri,
                len: MAX_LINK_URI + 1,
                max: MAX_LINK_URI,
            })
        );
    }

    /// The length is tested before the bytes: an over-long URI full of ESC is
    /// reported as too long, which is the check that bounds the work.
    #[test]
    fn the_length_is_tested_before_the_bytes() {
        let bomb = "\x1b".repeat(MAX_LINK_URI + 1);
        assert!(matches!(
            check_link_uri(1, &bomb),
            Err(ApplyError::TextTooLong { .. })
        ));
    }

    #[test]
    fn a_uri_that_could_end_the_sequence_is_refused() {
        for (uri, byte) in [
            ("http://a/\x1b]52;c;AAAA", 0x1b),
            ("http://a/\x07", 0x07),
            ("http://a/\n", b'\n'),
            ("http://a/\x7f", 0x7f),
            // U+009C, the C1 string terminator, in its UTF-8 form.
            ("http://a/\u{9c}", 0xc2),
            ("http://ä/", 0xc3),
        ] {
            assert_eq!(
                check_link_uri(7, uri),
                Err(ApplyError::BadLinkUri {
                    id: 7,
                    byte: Some(byte)
                }),
                "{uri:?}"
            );
        }
        assert_eq!(
            check_link_uri(7, ""),
            Err(ApplyError::BadLinkUri { id: 7, byte: None })
        );
    }

    #[test]
    fn a_table_with_key_zero_is_refused() {
        assert_eq!(
            check_links(&table(&[(0, "http://a")])),
            Err(ApplyError::LinkIdZero)
        );
    }

    #[test]
    fn a_table_of_exactly_the_cap_is_accepted_and_one_more_is_not() {
        let mut t: BTreeMap<u16, Arc<str>> = (1..=MAX_LINKS as u16)
            .map(|k| (k, Arc::from("http://a")))
            .collect();
        assert_eq!(check_links(&t), Ok(()));
        t.insert(MAX_LINKS as u16 + 1, Arc::from("http://a"));
        assert_eq!(
            check_links(&t),
            Err(ApplyError::TooManyLinks {
                len: MAX_LINKS + 1,
                max: MAX_LINKS
            })
        );
    }

    #[test]
    fn a_table_with_a_hostile_uri_is_refused() {
        assert!(matches!(
            check_links(&table(&[(1, "http://a"), (2, "http://b\x07")])),
            Err(ApplyError::BadLinkUri { id: 2, .. })
        ));
    }
}

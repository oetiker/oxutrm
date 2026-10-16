//! A remote program's clipboard write (OSC 52), as part of the screen state.
//!
//! # Why a field and not a message
//!
//! A copy is an event, but it rides on the replicated state the same way the
//! bell does: a monotonic counter plus the latest value. A lost datagram then
//! costs nothing, because the next diff from the same acknowledged base still
//! carries it, and a reattach or a ring miss needs no rules of its own. A
//! separate message would need its own reliability, its own demux and its own
//! answer to "replay this after a reconnect?".
//!
//! # Why `serial` is separate from `copy`
//!
//! The host drops the text once the client has acknowledged a state that
//! carries it, so later full states stay small and a later client never sees
//! stale clipboard content. The serial stays: it is what tells a client
//! whether a copy is one it has already written.
//!
//! # Safety
//!
//! The text is never painted. The client base64-encodes it into one OSC 52,
//! so control characters in it cannot reach the terminal as control, and only
//! its length is bounded (**I8**). The OSC 52 rule in `text.rs` that keeps
//! clipboard writes out of *cells* is untouched by this.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{ApplyError, MAX_CLIPBOARD, TextField};

/// Which clipboard a copy is for.
///
/// The emulator distinguishes these two: OSC 52's `c` is `Clipboard`, `p` and
/// `s` are `Selection`. Any other target letter never reaches the state.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum ClipboardTarget {
    Clipboard,
    Selection,
}

/// One copy: where it goes and what it says.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct ClipboardCopy {
    pub target: ClipboardTarget,
    /// `Arc` because the sender keeps a ring of recent states and each clone
    /// would otherwise copy up to [`MAX_CLIPBOARD`] bytes.
    pub text: Arc<str>,
}

/// The clipboard part of a [`ScreenState`](crate::ScreenState).
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct Clipboard {
    /// How many copies this session has made. Monotonic, like `bell` (**I9**):
    /// the client writes a copy when this rises past the last value it saw.
    pub serial: u32,
    /// The latest copy, until the client has acknowledged a state that
    /// carries it. `None` before the first copy and after delivery.
    pub copy: Option<ClipboardCopy>,
}

impl Clipboard {
    /// The same clipboard, minus its text: what a state shows once the
    /// client has acknowledged the copy. Tests use it to build that state.
    pub fn without_text(&self) -> Clipboard {
        Clipboard {
            serial: self.serial,
            copy: None,
        }
    }
}

/// **I8** for the clipboard: at most [`MAX_CLIPBOARD`] bytes of text.
///
/// Controls are allowed on purpose -- see the module comment.
pub fn check_clipboard(c: &Clipboard) -> Result<(), ApplyError> {
    if let Some(copy) = &c.copy
        && copy.text.len() > MAX_CLIPBOARD
    {
        return Err(ApplyError::TextTooLong {
            field: TextField::Clipboard,
            len: copy.text.len(),
            max: MAX_CLIPBOARD,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(text: &str) -> Clipboard {
        Clipboard {
            serial: 1,
            copy: Some(ClipboardCopy {
                target: ClipboardTarget::Clipboard,
                text: text.into(),
            }),
        }
    }

    #[test]
    fn a_copy_of_exactly_the_cap_is_accepted_and_one_byte_more_is_not() {
        assert_eq!(check_clipboard(&with(&"x".repeat(MAX_CLIPBOARD))), Ok(()));
        assert_eq!(
            check_clipboard(&with(&"x".repeat(MAX_CLIPBOARD + 1))),
            Err(ApplyError::TextTooLong {
                field: TextField::Clipboard,
                len: MAX_CLIPBOARD + 1,
                max: MAX_CLIPBOARD,
            })
        );
    }

    #[test]
    fn controls_in_a_copy_are_allowed_because_it_is_never_painted() {
        assert_eq!(check_clipboard(&with("a\x1b]52;c;?\x07\nb")), Ok(()));
    }

    #[test]
    fn without_text_keeps_the_serial() {
        let c = with("secret").without_text();
        assert_eq!(c.serial, 1);
        assert_eq!(c.copy, None);
    }

    #[test]
    fn the_text_round_trips_through_postcard() {
        let c = with("héllo");
        let back: Clipboard = postcard::from_bytes(&postcard::to_stdvec(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }
}

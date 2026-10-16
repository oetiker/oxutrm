//! Writing a remote program's clipboard copy (OSC 52) to the local terminal.
//!
//! Kept out of [`crate::Renderer`] on purpose: the renderer also paints
//! things that must never write the clipboard -- the popup, and a preview of
//! another session -- and a relay that only sees the states it is handed
//! cannot be reached by those.
//!
//! # When a copy is written
//!
//! The state carries a serial that counts every copy the session has made.
//! A copy is written when that serial rises past the last one this client
//! process saw **for that session**. The first state of a session sets the
//! baseline and writes nothing: a copy made before this client arrived is
//! stale, and overwriting the user's local clipboard with it would be a
//! surprise. A client that reconnects in the same process keeps its baseline,
//! so a copy made during an outage still arrives, once.

use std::collections::HashMap;

use base64::Engine as _;
use oxutrm_proto::{Clipboard, ClipboardTarget, SessionId};

/// Decides which copies reach the terminal, and how they are spelled.
pub struct ClipboardRelay {
    /// The terminal understands OSC 52 at all (`TerminalCaps::osc52`).
    osc52: bool,
    /// The last serial seen per session. `None` is a session with no id --
    /// a test fixture, or `oxutrm loopback`.
    seen: HashMap<Option<SessionId>, u32>,
}

impl ClipboardRelay {
    pub fn new(osc52: bool) -> ClipboardRelay {
        ClipboardRelay {
            osc52,
            seen: HashMap::new(),
        }
    }

    /// The bytes to write for a newly applied state of `session`, if any.
    ///
    /// `enabled` is the `terminal.clipboard` setting. Off still advances the
    /// baseline, so turning it on later does not replay an old copy.
    pub fn on_state(
        &mut self,
        session: Option<SessionId>,
        c: &Clipboard,
        enabled: bool,
    ) -> Option<Vec<u8>> {
        let last = self.seen.insert(session, c.serial)?;
        if c.serial <= last {
            // Never moves the baseline down: a lower serial is a state the
            // receiver should have refused (I9), not a new session.
            self.seen.insert(session, last);
            return None;
        }
        let copy = c.copy.as_ref()?;
        if !(enabled && self.osc52) {
            return None;
        }
        let target = match copy.target {
            ClipboardTarget::Clipboard => "c",
            ClipboardTarget::Selection => "p",
        };
        let encoded = base64::engine::general_purpose::STANDARD.encode(copy.text.as_bytes());
        let mut out = Vec::with_capacity(encoded.len() + 8);
        out.extend_from_slice(b"\x1b]52;");
        out.extend_from_slice(target.as_bytes());
        out.push(b';');
        out.extend_from_slice(encoded.as_bytes());
        out.push(0x07);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxutrm_proto::ClipboardCopy;

    fn clip(serial: u32, text: Option<&str>) -> Clipboard {
        Clipboard {
            serial,
            copy: text.map(|t| ClipboardCopy {
                target: ClipboardTarget::Clipboard,
                text: t.into(),
            }),
        }
    }

    fn id(n: u8) -> Option<SessionId> {
        Some(format!("{n:02x}").repeat(16).parse().expect("a session id"))
    }

    #[test]
    fn the_first_state_of_a_session_writes_nothing_however_fresh_it_looks() {
        let mut r = ClipboardRelay::new(true);
        assert_eq!(r.on_state(id(1), &clip(3, Some("stale")), true), None);
    }

    #[test]
    fn a_higher_serial_writes_exactly_one_osc_52() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(id(1), &clip(0, None), true);
        assert_eq!(
            r.on_state(id(1), &clip(1, Some("hello")), true),
            Some(b"\x1b]52;c;aGVsbG8=\x07".to_vec())
        );
        assert_eq!(
            r.on_state(id(1), &clip(1, Some("hello")), true),
            None,
            "the same serial again is the same copy"
        );
        assert_eq!(
            r.on_state(id(1), &clip(1, None), true),
            None,
            "nor is its delivery a new copy"
        );
    }

    #[test]
    fn an_empty_copy_is_passed_on_because_it_clears_the_clipboard() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(None, &clip(0, None), true);
        assert_eq!(
            r.on_state(None, &clip(1, Some("")), true),
            Some(b"\x1b]52;c;\x07".to_vec())
        );
    }

    #[test]
    fn the_selection_target_is_written_as_p() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(id(1), &clip(0, None), true);
        let c = Clipboard {
            serial: 1,
            copy: Some(ClipboardCopy {
                target: ClipboardTarget::Selection,
                text: "x".into(),
            }),
        };
        assert_eq!(
            r.on_state(id(1), &c, true),
            Some(b"\x1b]52;p;eA==\x07".to_vec())
        );
    }

    #[test]
    fn controls_in_the_text_travel_inside_the_base64() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(None, &clip(0, None), true);
        let out = r
            .on_state(None, &clip(1, Some("\x1b]0;evil\x07")), true)
            .unwrap();
        assert_eq!(
            out.iter().filter(|b| **b == 0x1b).count(),
            1,
            "the only ESC is the one that opens the OSC"
        );
        assert_eq!(out.iter().filter(|b| **b == 0x07).count(), 1);
    }

    #[test]
    fn switched_off_writes_nothing_and_switching_on_does_not_replay() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(id(1), &clip(0, None), true);
        assert_eq!(r.on_state(id(1), &clip(1, Some("a")), false), None);
        assert_eq!(r.on_state(id(1), &clip(1, Some("a")), true), None);
        assert!(r.on_state(id(1), &clip(2, Some("b")), true).is_some());
    }

    #[test]
    fn a_terminal_without_osc_52_gets_nothing() {
        let mut r = ClipboardRelay::new(false);
        r.on_state(id(1), &clip(0, None), true);
        assert_eq!(r.on_state(id(1), &clip(1, Some("a")), true), None);
    }

    #[test]
    fn sessions_keep_separate_serials_across_a_switch() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(id(1), &clip(5, None), true);
        r.on_state(id(2), &clip(9, Some("other")), true);
        // Back to session 1: its own counter decides, not session 2's.
        assert!(r.on_state(id(1), &clip(6, Some("one")), true).is_some());
        // Back to session 2 at the same serial: no replay.
        assert_eq!(r.on_state(id(2), &clip(9, Some("other")), true), None);
    }

    #[test]
    fn a_reconnect_in_the_same_process_delivers_what_it_missed_once() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(id(1), &clip(1, None), true);
        // The link was down while copy 2 was made; the first state after the
        // reconnect is a full state that still carries it.
        assert!(r.on_state(id(1), &clip(2, Some("missed")), true).is_some());
        assert_eq!(r.on_state(id(1), &clip(2, Some("missed")), true), None);
    }

    #[test]
    fn a_serial_going_backwards_never_lowers_the_baseline() {
        let mut r = ClipboardRelay::new(true);
        r.on_state(id(1), &clip(5, None), true);
        assert_eq!(r.on_state(id(1), &clip(2, Some("old")), true), None);
        assert_eq!(
            r.on_state(id(1), &clip(3, Some("old")), true),
            None,
            "3 is still not past 5"
        );
    }
}

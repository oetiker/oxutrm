//! Catching the things that arrive as events rather than as grid contents.
//!
//! Title, bell and OSC 52 do not live in the grid: `Term` reports them by
//! calling [`EventListener::send_event`]. That method takes **`&self`**, not
//! `&mut self`, so a listener that wants to remember anything needs interior
//! mutability. A `Mutex` is used rather than a channel because the reader is
//! the same thread a moment later and a channel would add a queue nobody
//! drains.

use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ClipboardType;

/// What the emulator has told us since the last drain.
#[derive(Default, Debug)]
pub struct Signals {
    /// The most recent title. `None` when it never changed.
    pub title: Option<String>,
    /// How many times the bell rang. Accumulated, never a flag: the client
    /// rings once per increment, so a count that reset would lose bells.
    pub bells: u32,
    /// OSC 52 copy requests, **already base64-decoded** by `vte`.
    pub clipboard: Vec<(ClipboardType, String)>,
    /// The child's exit code, once the emulator reports one. Signals are
    /// folded in as 128+n, exactly as a shell reports them.
    pub child_exit: Option<i32>,
    /// Something changed that a renderer would care about.
    pub wakeup: bool,
}

/// The listener handed to `Term::new`.
#[derive(Clone, Default)]
pub struct EventSink {
    inner: Arc<Mutex<Signals>>,
}

impl EventSink {
    pub fn new() -> EventSink {
        EventSink::default()
    }

    /// Take everything accumulated so far, leaving the sink empty.
    ///
    /// `bells` is the exception: it is returned and reset here, and
    /// [`crate::HostTerm`] accumulates it into a running total, because
    /// `ScreenState::bell` is monotonic for the life of the session.
    pub fn drain(&self) -> Signals {
        match self.inner.lock() {
            Ok(mut guard) => std::mem::take(&mut *guard),
            // A poisoned lock means a panic elsewhere while holding it. The
            // terminal is still usable and losing one batch of titles beats
            // taking the whole session down.
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        }
    }
}

impl EventListener for EventSink {
    fn send_event(&self, event: Event) {
        let Ok(mut s) = self.inner.lock() else { return };
        match event {
            // Already decoded by vte: no percent-decoding or base64 here.
            Event::Title(t) => s.title = Some(t),
            Event::ResetTitle => s.title = Some(String::new()),
            Event::Bell => s.bells = s.bells.saturating_add(1),
            // Already base64-decoded by vte.
            Event::ClipboardStore(kind, text) => s.clipboard.push((kind, text)),
            Event::ChildExit(status) => s.child_exit = Some(crate::pty::exit_code(status)),
            Event::Wakeup => s.wakeup = true,
            // The rest are for an interactive front end - mouse shape, colour
            // queries, PTY write-backs - and oxutrm answers none of them here.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_title_is_remembered_through_a_shared_reference() {
        // send_event takes &self. Without interior mutability this could not
        // record anything at all.
        let sink = EventSink::new();
        sink.send_event(Event::Title("vim".to_owned()));
        assert_eq!(sink.drain().title.as_deref(), Some("vim"));
    }

    #[test]
    fn the_last_title_wins() {
        let sink = EventSink::new();
        sink.send_event(Event::Title("first".to_owned()));
        sink.send_event(Event::Title("second".to_owned()));
        assert_eq!(sink.drain().title.as_deref(), Some("second"));
    }

    #[test]
    fn a_title_reset_is_an_empty_title_not_a_missing_one() {
        let sink = EventSink::new();
        sink.send_event(Event::Title("something".to_owned()));
        sink.send_event(Event::ResetTitle);
        assert_eq!(
            sink.drain().title.as_deref(),
            Some(""),
            "None would mean 'unchanged', which is a different thing"
        );
    }

    #[test]
    fn bells_are_counted_not_flagged() {
        let sink = EventSink::new();
        for _ in 0..3 {
            sink.send_event(Event::Bell);
        }
        assert_eq!(sink.drain().bells, 3);
    }

    #[test]
    fn draining_empties_the_sink() {
        let sink = EventSink::new();
        sink.send_event(Event::Bell);
        assert_eq!(sink.drain().bells, 1);
        assert_eq!(sink.drain().bells, 0, "a second drain sees nothing");
    }

    #[test]
    fn a_clipboard_store_arrives_decoded() {
        let sink = EventSink::new();
        sink.send_event(Event::ClipboardStore(
            ClipboardType::Clipboard,
            "plain".to_owned(),
        ));
        let got = sink.drain().clipboard;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, "plain", "vte decodes the base64 before we see it");
    }

    #[test]
    fn a_child_exit_is_recorded() {
        let sink = EventSink::new();
        assert_eq!(sink.drain().child_exit, None);
        use std::os::unix::process::ExitStatusExt as _;
        sink.send_event(Event::ChildExit(std::process::ExitStatus::from_raw(7 << 8)));
        assert_eq!(sink.drain().child_exit, Some(7));
    }

    #[test]
    fn events_we_do_not_answer_are_ignored_without_dying() {
        let sink = EventSink::new();
        sink.send_event(Event::MouseCursorDirty);
        sink.send_event(Event::CursorBlinkingChange);
        sink.send_event(Event::PtyWrite("x".to_owned()));
        sink.send_event(Event::Exit);
        let s = sink.drain();
        assert!(s.title.is_none());
        assert_eq!(s.bells, 0);
    }

    #[test]
    fn a_clone_shares_one_sink() {
        // Term takes the listener by value, so HostTerm keeps a clone. Both
        // must see the same events or the copy HostTerm holds is useless.
        let sink = EventSink::new();
        let copy = sink.clone();
        copy.send_event(Event::Bell);
        assert_eq!(sink.drain().bells, 1);
    }
}

//! The selector's preview of the selected session (preview spec §3.4,
//! §3.5): which session's screen to ask for, the shots that came back, and
//! what the pane shows.
//!
//! Pure, like `selector`: no I/O and no clock of its own. The session calls
//! [`Previews::arrive`] and [`Previews::ask_next`] each lap, sends the request
//! [`Previews::ask_next`] hands out, and brings its answer back to
//! [`Previews::answered`]. At most one request is out at a time; the
//! latest selection is the one asked next.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::collections::HashMap;
use std::time::{Duration, Instant};

use oxutrm_client::{PreviewPane, summarised};
use oxutrm_proto::{Attached, PREVIEW_MAX, ScreenState, SessionEntry, SessionId};

use crate::selector::label;

/// How old a shot may be and still not be asked again when the selection
/// returns to its row.
pub(crate) const PREVIEW_FRESH: Duration = Duration::from_secs(2);

/// What `+ new session` would start.
pub(crate) const NEW_SESSION: &str = "a new login shell in your home directory";
/// A session of another protocol version cannot parse the request.
pub(crate) const OTHER_VERSION: &str = "another version of oxutrm: no preview";
/// A session that is not detachable has no socket to ask through.
pub(crate) const NOT_DETACHABLE: &str = "no preview: it dies with its ssh";
/// No shot, and none can be asked for.
pub(crate) const LINK_DOWN: &str = "the link is down";

/// What came back for one session.
#[derive(Debug)]
enum Got {
    /// Checked: it validates and fits [`PREVIEW_MAX`].
    Screen(ScreenState),
    /// Refused or failed, in words for the user.
    Failed(String),
    /// A screen that failed the check: never shown, never repaired.
    Invalid,
}

#[derive(Debug, Default)]
pub(crate) struct Previews {
    /// The answers so far, with when they came.
    shots: HashMap<SessionId, (Got, Instant)>,
    /// The row the selection was on at the last [`Previews::arrive`]:
    /// `Some(None)` is `+ new session`, `None` that it has not arrived yet.
    selected: Option<Option<SessionId>>,
    /// The session to ask next.
    wanted: Option<SessionId>,
    /// The session whose request is out.
    out: Option<SessionId>,
}

/// Whether `e` can be asked for its screen at all.
fn previewable(e: &SessionEntry) -> bool {
    !e.this && e.detachable && e.attached != Attached::OtherVersion
}

impl Previews {
    /// The selection is on `row` (`None`: `+ new session`). Acts only when
    /// that is another row than at the last call: a previewable row with no
    /// shot under [`PREVIEW_FRESH`] becomes the one to ask next, and any
    /// row replaces what was wanted before.
    pub(crate) fn arrive(&mut self, row: Option<&SessionEntry>, now: Instant) {
        let id = row.map(|e| e.id);
        if self.selected == Some(id) {
            return;
        }
        self.selected = Some(id);
        self.wanted = row.filter(|e| previewable(e)).map(|e| e.id).filter(|id| {
            self.shots
                .get(id)
                .is_none_or(|(_, at)| now.saturating_duration_since(*at) >= PREVIEW_FRESH)
        });
    }

    /// The session to ask now, if any: on a live link, with no request
    /// out. It is out from here until [`Previews::answered`] hears of it.
    pub(crate) fn ask_next(&mut self, live: bool) -> Option<SessionId> {
        if !live || self.out.is_some() {
            return None;
        }
        self.out = self.wanted;
        self.out
    }

    /// The answer to the request for `id`. Dropped unless that request is
    /// the one out. A screen is kept only if it validates and fits
    /// [`PREVIEW_MAX`]; anything else is kept as not valid, never repaired.
    pub(crate) fn answered(
        &mut self,
        id: SessionId,
        answer: Result<ScreenState, String>,
        now: Instant,
    ) {
        if self.out != Some(id) {
            return;
        }
        self.out = None;
        if self.wanted == Some(id) {
            self.wanted = None;
        }
        let got = match answer {
            Ok(s)
                if s.validate().is_ok()
                    && s.rows <= PREVIEW_MAX.rows
                    && s.cols <= PREVIEW_MAX.cols =>
            {
                Got::Screen(s)
            }
            Ok(_) => Got::Invalid,
            Err(why) => Got::Failed(why),
        };
        self.shots.insert(id, (got, now));
    }

    /// A fresh list: shots of sessions it no longer lists are dropped, and
    /// such a session is no longer wanted.
    pub(crate) fn keep(&mut self, list: &[SessionEntry]) {
        let listed = |id: &SessionId| list.iter().any(|e| e.id == *id);
        self.shots.retain(|id, _| listed(id));
        if self.wanted.is_some_and(|id| !listed(&id)) {
            self.wanted = None;
        }
    }

    /// The selector closed, or the client is in another session now:
    /// nothing cached, wanted or out. The session aborts the request's task.
    pub(crate) fn clear(&mut self) {
        *self = Previews::default();
    }

    /// What the pane shows for `row` (`None`: `+ new session`). `own` is
    /// the client's own screen, cut for the session it is in; `live`
    /// whether the link is up.
    pub(crate) fn pane(
        &self,
        row: Option<&SessionEntry>,
        own: &ScreenState,
        live: bool,
    ) -> PreviewPane {
        let note = |s: &str| PreviewPane::Note(s.to_string());
        let Some(e) = row else {
            return note(NEW_SESSION);
        };
        if e.this {
            return PreviewPane::Screen(own.window(PREVIEW_MAX));
        }
        if e.attached == Attached::OtherVersion {
            return note(OTHER_VERSION);
        }
        if !e.detachable {
            return note(NOT_DETACHABLE);
        }
        match self.shots.get(&e.id) {
            Some((Got::Screen(s), _)) => PreviewPane::Screen(s.clone()),
            Some((Got::Failed(why), _)) => PreviewPane::Note(summarised(why)),
            Some((Got::Invalid, _)) => {
                PreviewPane::Note(format!("{} sent a screen that is not valid", label(e)))
            }
            None if !live => note(LINK_DOWN),
            None => PreviewPane::Note(format!("asking {} for its screen\u{2026}", label(e))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selector::fixtures::{BUILD, FISH, LOGS, entry, three};
    use oxutrm_proto::{CellText, TermSize};

    fn id(s: &str) -> SessionId {
        s.parse().unwrap()
    }

    fn row<'a>(list: &'a [SessionEntry], s: &str) -> &'a SessionEntry {
        list.iter().find(|e| e.id == id(s)).unwrap()
    }

    /// A shell's screen with a prompt and a line of output above it.
    fn screen(rows: u16, cols: u16) -> ScreenState {
        let mut s = ScreenState::blank(rows, cols).unwrap();
        for (r, text) in ["cargo build --release", "oetiker@thinlinc:~/src$ "]
            .iter()
            .enumerate()
        {
            for (c, ch) in text.chars().take(usize::from(cols)).enumerate() {
                s.cells[r * usize::from(cols) + c].text = CellText::new(ch.to_string());
            }
        }
        s.cursor.row = 1.min(rows - 1);
        s
    }

    fn note(p: &PreviewPane) -> &str {
        match p {
            PreviewPane::Note(n) => n,
            PreviewPane::Screen(_) => panic!("a screen, not a note"),
        }
    }

    #[test]
    fn arriving_on_a_session_asks_for_it_once() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, LOGS)), t);
        assert_eq!(p.ask_next(true), Some(id(LOGS)));
        assert_eq!(p.ask_next(true), None, "one request out at a time");
        p.arrive(Some(row(&list, LOGS)), t);
        assert_eq!(p.ask_next(true), None, "resting on the row asks nothing");
        p.answered(id(LOGS), Ok(screen(40, 120).window(PREVIEW_MAX)), t);
        assert_eq!(p.ask_next(true), None, "the answer is in: nothing to ask");
    }

    #[test]
    fn a_fresh_shot_is_not_asked_again_and_a_stale_one_is_shown_and_asked() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        let shot = screen(18, 68);
        p.arrive(Some(row(&list, LOGS)), t);
        p.ask_next(true);
        p.answered(id(LOGS), Ok(shot.clone()), t);

        p.arrive(None, t);
        p.arrive(Some(row(&list, LOGS)), t + Duration::from_millis(1500));
        assert_eq!(p.ask_next(true), None, "under 2 s old");

        p.arrive(None, t);
        p.arrive(Some(row(&list, LOGS)), t + PREVIEW_FRESH);
        assert_eq!(
            p.pane(Some(row(&list, LOGS)), &screen(40, 120), true),
            PreviewPane::Screen(shot),
            "the old shot shows while the new one is asked for"
        );
        assert_eq!(p.ask_next(true), Some(id(LOGS)));
    }

    #[test]
    fn the_latest_selection_is_asked_next() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, FISH)), t);
        assert_eq!(p.ask_next(true), Some(id(FISH)));
        p.arrive(Some(row(&list, LOGS)), t);
        p.arrive(None, t);
        p.arrive(Some(row(&list, LOGS)), t);
        assert_eq!(p.ask_next(true), None, "fish is still out");
        p.answered(id(FISH), Ok(screen(18, 68)), t);
        assert_eq!(p.ask_next(true), Some(id(LOGS)));
    }

    #[test]
    fn leaving_a_row_before_it_is_asked_drops_it() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, FISH)), t);
        p.ask_next(true);
        p.arrive(Some(row(&list, LOGS)), t);
        p.arrive(None, t);
        p.answered(id(FISH), Ok(screen(18, 68)), t);
        assert_eq!(p.ask_next(true), None, "the selection is on + new session");
    }

    #[test]
    fn an_answer_for_a_request_not_out_is_dropped() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.answered(id(LOGS), Ok(screen(18, 68)), t);
        assert_eq!(
            note(&p.pane(Some(row(&list, LOGS)), &screen(40, 120), true)),
            "asking logs for its screen\u{2026}"
        );
        p.arrive(Some(row(&list, LOGS)), t);
        assert_eq!(p.ask_next(true), Some(id(LOGS)), "nothing was kept");
        p.answered(id(FISH), Ok(screen(18, 68)), t);
        assert_eq!(p.ask_next(true), None, "logs is still out");
    }

    #[test]
    fn nothing_is_asked_while_the_link_is_down() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, LOGS)), t);
        assert_eq!(p.ask_next(false), None);
        assert_eq!(
            note(&p.pane(Some(row(&list, LOGS)), &screen(40, 120), false)),
            LINK_DOWN
        );
        assert_eq!(p.ask_next(true), Some(id(LOGS)), "asked once it is back");
    }

    #[test]
    fn a_cached_shot_shows_while_the_link_is_down() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, LOGS)), t);
        p.ask_next(true);
        p.answered(id(LOGS), Ok(screen(18, 68)), t);
        assert_eq!(
            p.pane(Some(row(&list, LOGS)), &screen(40, 120), false),
            PreviewPane::Screen(screen(18, 68))
        );
    }

    #[test]
    fn rows_with_no_screen_ask_next_for_are_never_asked() {
        let mut list = three();
        list.push(entry(
            "0123456789abcdef0123456789abcdef",
            Some("old"),
            "/bin/bash",
            1_791_000_000,
            Attached::OtherVersion,
        ));
        let mut tied = entry(
            "fedcba9876543210fedcba9876543210",
            Some("tied"),
            "/bin/bash",
            1_791_000_001,
            Attached::No,
        );
        tied.detachable = false;
        list.push(tied);
        let own = screen(40, 120);
        let mut p = Previews::default();
        let t = Instant::now();
        let mut seen = Vec::new();
        for r in [
            Some(row(&list, BUILD)),
            None,
            Some(&list[3]),
            Some(&list[4]),
        ] {
            p.arrive(r, t);
            assert_eq!(p.ask_next(true), None);
            seen.push(p.pane(r, &own, true));
        }
        assert_eq!(seen[0], PreviewPane::Screen(own.window(PREVIEW_MAX)));
        assert_eq!(note(&seen[1]), NEW_SESSION);
        assert_eq!(note(&seen[2]), OTHER_VERSION);
        assert_eq!(note(&seen[3]), NOT_DETACHABLE);
    }

    #[test]
    fn this_session_is_cut_from_the_clients_own_screen_every_time() {
        let list = three();
        let p = Previews::default();
        let mut own = screen(40, 120);
        let first = p.pane(Some(row(&list, BUILD)), &own, true);
        own.cells[0].text = CellText::new("$");
        let second = p.pane(Some(row(&list, BUILD)), &own, true);
        assert_ne!(first, second);
        assert_eq!(second, PreviewPane::Screen(own.window(PREVIEW_MAX)));
    }

    #[test]
    fn a_failure_says_why_in_the_pane() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, LOGS)), t);
        p.ask_next(true);
        p.answered(
            id(LOGS),
            Err("session 5d2e8a7c did not answer in time\nsecond line".to_string()),
            t,
        );
        assert_eq!(
            note(&p.pane(Some(row(&list, LOGS)), &screen(40, 120), true)),
            "session 5d2e8a7c did not answer in time"
        );
    }

    #[test]
    fn a_screen_with_a_control_character_is_not_valid() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        let mut hostile = screen(18, 68);
        hostile.cells[3].text = CellText::new("\u{9b}31m");
        assert!(hostile.validate().is_err());
        p.arrive(Some(row(&list, LOGS)), t);
        p.ask_next(true);
        p.answered(id(LOGS), Ok(hostile), t);
        assert_eq!(
            note(&p.pane(Some(row(&list, LOGS)), &screen(40, 120), true)),
            "logs sent a screen that is not valid"
        );
    }

    #[test]
    fn a_screen_larger_than_a_preview_is_not_valid() {
        let list = three();
        let t = Instant::now();
        for size in [
            TermSize { cols: 69, rows: 18 },
            TermSize { cols: 68, rows: 19 },
        ] {
            let mut p = Previews::default();
            p.arrive(Some(row(&list, FISH)), t);
            p.ask_next(true);
            p.answered(id(FISH), Ok(screen(size.rows, size.cols)), t);
            assert_eq!(
                note(&p.pane(Some(row(&list, FISH)), &screen(40, 120), true)),
                "a3f9c01e sent a screen that is not valid"
            );
        }
    }

    #[test]
    fn keep_drops_the_shots_of_sessions_no_longer_listed() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, LOGS)), t);
        p.ask_next(true);
        p.answered(id(LOGS), Ok(screen(18, 68)), t);
        p.arrive(Some(row(&list, FISH)), t);
        let without_fish: Vec<_> = list.iter().filter(|e| e.id != id(FISH)).cloned().collect();
        p.keep(&without_fish);
        assert_eq!(p.ask_next(true), None, "fish is gone: not wanted");
        p.keep(&list[..1]);
        assert_eq!(
            note(&p.pane(Some(row(&list, LOGS)), &screen(40, 120), true)),
            "asking logs for its screen\u{2026}",
            "logs' shot is gone with it"
        );
    }

    #[test]
    fn clear_forgets_everything() {
        let list = three();
        let mut p = Previews::default();
        let t = Instant::now();
        p.arrive(Some(row(&list, LOGS)), t);
        p.ask_next(true);
        p.clear();
        p.answered(id(LOGS), Ok(screen(18, 68)), t);
        assert_eq!(
            note(&p.pane(Some(row(&list, LOGS)), &screen(40, 120), true)),
            "asking logs for its screen\u{2026}",
            "the answer to a request from before the clear is dropped"
        );
        p.arrive(Some(row(&list, LOGS)), t);
        assert_eq!(
            p.ask_next(true),
            Some(id(LOGS)),
            "arriving again asks again"
        );
    }
}

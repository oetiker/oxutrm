//! The session selector's state and keys (switcher spec §3.2).
//!
//! Pure, like `ui`: every method takes what it needs -- the keys, whether
//! the link is live, whether the client is in a lobby, the time -- and the
//! session does everything that touches the host. The list is the host's,
//! set by [`Selector::set_rows`] whenever a fetch comes back; the keys come
//! back as an [`Out`]: a request to send, or where the popup goes.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::Instant;

use oxutrm_proto::{Attached, Name, SessionEntry, SessionId};

use crate::switcher::Ask;
use crate::ui::{ANSWER_GUARD, ESC, Field, Key};

/// What a key on the selector asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Out {
    /// Send this to the host.
    Ask(Ask),
    /// Back to the popup's status view.
    Back,
    /// Close the popup: `⏎` on the session the client is in.
    Close,
    /// End the client: `q` in a lobby.
    Quit,
}

/// A question the selector asks before it acts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Question {
    /// Switching to a session another client is attached to.
    TakeOver { id: SessionId, label: String },
    Kill {
        id: SessionId,
        label: String,
        in_use: bool,
    },
}

impl Question {
    /// The line the question is put in.
    pub(crate) fn text(&self) -> String {
        match self {
            Question::TakeOver { label, .. } => {
                format!("take over {label} from its other client? y/n")
            }
            Question::Kill {
                label,
                in_use: true,
                ..
            } => format!("kill {label} (in use elsewhere)? y/n"),
            Question::Kill { label, .. } => format!("kill {label}? y/n"),
        }
    }
}

/// What the selector needs to know about the session, read by read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ctx {
    /// The link is `Live`: requests can be sent.
    pub(crate) live: bool,
    /// In a lobby: `q` ends the client, and there is no popup to go back to.
    pub(crate) lobby: bool,
    /// A request is in flight: no second one until it is answered.
    pub(crate) busy: bool,
}

/// Why an action is not taken while the link is down.
pub(crate) const NOT_LIVE: &str = "the link is down; try again once it is back";
/// Why an action is not taken while another is under way.
pub(crate) const BUSY: &str = "waiting for the host to answer";

#[derive(Debug, Default)]
pub(crate) struct Selector {
    /// The host's sessions, oldest first.
    rows: Vec<SessionEntry>,
    /// A row of `rows`, or `rows.len()`: `+ new session`.
    cursor: usize,
    /// No list has come back since the selector opened.
    loading: bool,
    /// The question up, and when it was asked, for [`ANSWER_GUARD`].
    question: Option<(Question, Instant)>,
    /// The session being renamed and the name being typed for it, while
    /// `r` is open. The id, not the cursor: a refetch can move the rows.
    rename: Option<(SessionId, Field)>,
    /// The line under the list: why something was refused or failed.
    note: Option<String>,
    /// Why the last list fetch failed: no list is on its way. Shown under
    /// the list, where no note is, until a list arrives.
    list_failed: Option<String>,
    /// Inside a bracketed paste whose end has not arrived yet, and how many
    /// bytes of that end the last read finished on (see
    /// [`crate::ui::paste_through`]). A paste is never a command: its text
    /// goes into the rename field, or nowhere.
    pasting: Option<usize>,
}

impl Selector {
    /// Opened afresh: whatever the last list said is gone until the next
    /// one arrives, and the selection will start on this session.
    pub(crate) fn open(&mut self) {
        *self = Selector {
            loading: true,
            ..Selector::default()
        };
    }

    /// A fetched list. Ordered by start time, `+ new session` last. The
    /// first after opening puts the selection on `this`, else the first
    /// row; a later one keeps it on the same session if it is still there.
    /// A rename whose session is no longer listed ends, and the line under
    /// the list says so.
    pub(crate) fn set_rows(&mut self, mut rows: Vec<SessionEntry>) {
        rows.sort_by_key(|e| e.created_unix);
        if let Some((id, _)) = &self.rename
            && !rows.iter().any(|e| e.id == *id)
        {
            let gone = self
                .rows
                .iter()
                .find(|e| e.id == *id)
                .map_or_else(|| id.short(), label);
            self.note = Some(format!("{gone} has ended; it was not renamed"));
            self.rename = None;
        }
        let at = match (self.loading, self.selected()) {
            (true, _) => rows.iter().position(|e| e.this).unwrap_or(0),
            (false, Some(id)) => rows
                .iter()
                .position(|e| e.id == id)
                .unwrap_or(self.cursor.min(rows.len())),
            (false, None) => rows.len(),
        };
        self.rows = rows;
        self.cursor = at;
        self.loading = false;
        self.list_failed = None;
    }

    /// The list could not be fetched: say why under the list until one
    /// arrives. Nothing is on its way any more, so the selector stops
    /// saying it is asking; with no list, `+ new session` is all it offers.
    pub(crate) fn fail_list(&mut self, why: String) {
        self.question = None;
        self.loading = false;
        self.list_failed = Some(why);
    }

    /// Why the last list fetch failed, until a list arrives.
    pub(crate) fn list_failed(&self) -> Option<&str> {
        self.list_failed.as_deref()
    }

    /// Something failed: say why under the list. A question still up is
    /// dropped with it.
    pub(crate) fn fail(&mut self, why: String) {
        self.question = None;
        self.note = Some(why);
    }

    pub(crate) fn rows(&self) -> &[SessionEntry] {
        &self.rows
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn loading(&self) -> bool {
        self.loading
    }

    pub(crate) fn question(&self) -> Option<&Question> {
        self.question.as_ref().map(|(q, _)| q)
    }

    /// The name being typed, while `r` is open.
    pub(crate) fn rename(&self) -> Option<&str> {
        self.rename.as_ref().map(|(_, f)| f.as_str())
    }

    /// The session being renamed, while `r` is open.
    pub(crate) fn renaming(&self) -> Option<SessionId> {
        self.rename.as_ref().map(|(id, _)| *id)
    }

    pub(crate) fn note(&self) -> Option<&str> {
        self.note.as_deref()
    }

    /// The session the cursor is on; `None` on `+ new session`.
    fn selected(&self) -> Option<SessionId> {
        self.rows.get(self.cursor).map(|e| e.id)
    }

    /// Inside a paste whose end has not arrived yet: the popup key in it
    /// is text, not a close.
    pub(crate) fn pasting(&self) -> bool {
        self.pasting.is_some()
    }

    /// One read's bytes. Stops at the first key that produces an [`Out`]:
    /// the rest of the read is dropped, as on the popup.
    pub(crate) fn keys(&mut self, bytes: &[u8], ctx: Ctx, now: Instant) -> Option<Out> {
        // A note lasts until the next read.
        self.note = None;
        let lone_esc = bytes == [ESC];
        // The rest of a paste that began in an earlier read is the paste's.
        let mut rest = match self.pasting {
            Some(_) => self.paste(bytes),
            None => bytes,
        };
        while let Some((&b, tail)) = rest.split_first() {
            let key = if b == ESC && !lone_esc {
                let (key, used) = crate::ui::escape(tail);
                rest = &tail[used..];
                match key {
                    Some(k) => k,
                    None if tail.starts_with(b"[200~") => {
                        self.pasting = Some(0);
                        rest = self.paste(rest);
                        continue;
                    }
                    // Any other sequence -- a function key -- means
                    // nothing here.
                    None => continue,
                }
            } else {
                rest = tail;
                Key::Byte(b)
            };
            if let Some(out) = self.key(key, ctx, now) {
                return Some(out);
            }
        }
        None
    }

    /// A paste's text up to its end: typed into the rename field, control
    /// characters dropped so a pasted newline never saves; anywhere else,
    /// dropped. Returns what follows the end.
    fn paste<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let field = &mut self.rename;
        crate::ui::paste_through(&mut self.pasting, bytes, |b| {
            if let Some((_, f)) = field.as_mut() {
                f.type_byte(b);
            }
        })
    }

    /// One key.
    fn key(&mut self, key: Key, ctx: Ctx, now: Instant) -> Option<Out> {
        if self.question.is_some() {
            return self.answer(key, ctx, now);
        }
        if self.rename.is_some() {
            return self.rename_key(key, ctx);
        }
        let last = self.rows.len();
        match key {
            Key::Up | Key::Byte(b'k') => self.cursor = self.cursor.saturating_sub(1),
            Key::Down | Key::Byte(b'j') => self.cursor = (self.cursor + 1).min(last),
            Key::Byte(b'\r' | b'\n') => return self.enter(ctx, now),
            Key::Byte(b'n') => return self.act(ctx, Ask::New { name: None }),
            Key::Byte(b'r') => {
                let row = self.rows.get(self.cursor)?;
                if row.attached == Attached::OtherVersion {
                    self.note = Some(other_version(row));
                } else {
                    let mut field = Field::default();
                    field.set(row.name.as_ref().map(Name::to_string).unwrap_or_default());
                    self.rename = Some((row.id, field));
                }
            }
            Key::Byte(b'x') => {
                let row = self.rows.get(self.cursor)?;
                if row.attached == Attached::OtherVersion {
                    self.note = Some(other_version(row));
                } else {
                    let question = Question::Kill {
                        id: row.id,
                        label: label(row),
                        in_use: row.attached == Attached::Elsewhere,
                    };
                    self.question = Some((question, now));
                }
            }
            Key::Byte(b'q') | Key::Byte(ESC) => {
                return Some(if ctx.lobby { Out::Quit } else { Out::Back });
            }
            _ => {}
        }
        None
    }

    /// `⏎` on the cursor's row.
    fn enter(&mut self, ctx: Ctx, now: Instant) -> Option<Out> {
        let Some(row) = self.rows.get(self.cursor) else {
            return self.act(ctx, Ask::New { name: None });
        };
        if row.this {
            return Some(Out::Close);
        }
        if !row.detachable {
            self.note = Some(format!(
                "{} is not detachable: it tunnels its data through the ssh that \
                 created it, so it cannot be reattached",
                label(row)
            ));
            return None;
        }
        if row.attached == Attached::OtherVersion {
            self.note = Some(other_version(row));
            return None;
        }
        if row.attached == Attached::Elsewhere {
            let question = Question::TakeOver {
                id: row.id,
                label: label(row),
            };
            self.question = Some((question, now));
            return None;
        }
        let to = row.id;
        self.act(ctx, Ask::Switch { to })
    }

    /// A key while a question is up: `y` acts, `n` and `Esc` drop it, and
    /// anything else -- or anything before [`ANSWER_GUARD`] has passed,
    /// which is typing still in flight -- does nothing.
    fn answer(&mut self, key: Key, ctx: Ctx, now: Instant) -> Option<Out> {
        let (question, asked) = self.question.as_ref()?;
        if now.saturating_duration_since(*asked) < ANSWER_GUARD {
            return None;
        }
        match key {
            Key::Byte(b'y') => {
                let ask = match question {
                    Question::TakeOver { id, .. } => Ask::Switch { to: *id },
                    Question::Kill { id, .. } => Ask::Kill { id: *id },
                };
                self.question = None;
                self.act(ctx, ask)
            }
            Key::Byte(b'n') | Key::Byte(ESC) => {
                self.question = None;
                None
            }
            _ => None,
        }
    }

    /// A key while a name is typed in place: the config screen's line
    /// editing. `⏎` saves -- nothing typed clears the name -- and `Esc`
    /// cancels.
    fn rename_key(&mut self, key: Key, ctx: Ctx) -> Option<Out> {
        let (id, field) = self.rename.as_mut()?;
        let id = *id;
        match key {
            Key::Byte(ESC) => self.rename = None,
            Key::Byte(b'\r' | b'\n') => {
                let text = field.as_str().trim().to_string();
                let name = if text.is_empty() {
                    None
                } else {
                    match Name::parse(&text) {
                        Ok(n) => Some(n),
                        Err(why) => {
                            self.note = Some(why);
                            return None;
                        }
                    }
                };
                let out = self.act(ctx, Ask::Rename { id, name });
                if out.is_some() {
                    self.rename = None;
                }
                return out;
            }
            Key::Byte(0x7f | 0x08) => field.erase(),
            Key::Byte(b) if b >= 0x20 => field.type_byte(b),
            _ => {}
        }
        None
    }

    /// `ask`, unless the link is down or a request is under way: then the
    /// note says why not, and nothing is asked.
    fn act(&mut self, ctx: Ctx, ask: Ask) -> Option<Out> {
        if !ctx.live {
            self.note = Some(NOT_LIVE.to_string());
            return None;
        }
        if ctx.busy {
            self.note = Some(BUSY.to_string());
            return None;
        }
        Some(Out::Ask(ask))
    }
}

/// A session as the selector names it: its name, else the start of its id.
pub(crate) fn label(e: &SessionEntry) -> String {
    match &e.name {
        Some(n) => oxutrm_client::legible(n.as_str()),
        None => e.id.short(),
    }
}

fn other_version(e: &SessionEntry) -> String {
    format!(
        "{} runs another version of oxutrm; it can be ended only by ending its shell",
        label(e)
    )
}

#[cfg(test)]
pub(crate) mod fixtures {
    use oxutrm_proto::{Attached, Name, SessionEntry, TermSize};

    /// A session as a host lists it: real-looking ids, names, shells and
    /// sizes, because tiny dummy values once hid a cut-off column.
    pub(crate) fn entry(
        id: &str,
        name: Option<&str>,
        shell: &str,
        created_unix: u64,
        attached: Attached,
    ) -> SessionEntry {
        SessionEntry {
            id: id.parse().unwrap(),
            name: name.map(|n| Name::parse(n).unwrap()),
            shell: shell.to_string(),
            created_unix,
            size: TermSize {
                cols: 120,
                rows: 40,
            },
            detachable: true,
            attached,
            this: attached == Attached::Here,
        }
    }

    pub(crate) const BUILD: &str = "3ff1218f5e0c4b7d9a1c2e3f40516273";
    pub(crate) const FISH: &str = "a3f9c01e5b7d4c2e8f6a1b0c9d8e7f60";
    pub(crate) const LOGS: &str = "5d2e8a7c1b9f40e3a6c8d1f2b3e4a5c6";

    /// The spec's picture: `build` (this), an unnamed fish session in use
    /// elsewhere, `logs` detached.
    pub(crate) fn three() -> Vec<SessionEntry> {
        vec![
            entry(
                BUILD,
                Some("build"),
                "/bin/bash",
                1_791_450_840,
                Attached::Here,
            ),
            entry(
                FISH,
                None,
                "/usr/bin/fish",
                1_791_190_000,
                Attached::Elsewhere,
            ),
            entry(LOGS, Some("logs"), "/bin/zsh", 1_791_458_520, Attached::No),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;
    use std::time::Duration;

    const LIVE: Ctx = Ctx {
        live: true,
        lobby: false,
        busy: false,
    };

    fn opened(rows: Vec<SessionEntry>) -> Selector {
        let mut s = Selector::default();
        s.open();
        s.set_rows(rows);
        s
    }

    fn id(s: &str) -> SessionId {
        s.parse().unwrap()
    }

    /// Past the question's guard.
    fn later(t: Instant) -> Instant {
        t + ANSWER_GUARD + Duration::from_millis(1)
    }

    #[test]
    fn rows_are_ordered_by_start_and_the_selection_starts_on_this() {
        let s = opened(three());
        let names: Vec<String> = s.rows().iter().map(label).collect();
        assert_eq!(names, ["a3f9c01e", "build", "logs"]);
        assert_eq!(s.cursor(), 1, "on build, which is this");
        assert!(!s.loading());
    }

    /// A failed list ends the wait; its line lasts through the reads that
    /// follow, until a list arrives.
    #[test]
    fn a_failed_list_ends_loading_and_lasts_until_a_list_arrives() {
        let mut s = Selector::default();
        s.open();
        s.fail_list("listing the sessions failed: refused".to_string());
        assert!(!s.loading());
        s.keys(b"j", LIVE, Instant::now());
        assert_eq!(
            s.list_failed(),
            Some("listing the sessions failed: refused")
        );
        s.set_rows(three());
        assert_eq!(s.list_failed(), None);
    }

    #[test]
    fn without_this_the_selection_starts_on_the_first_row() {
        let mut rows = three();
        rows.retain(|e| !e.this);
        let s = opened(rows);
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn arrows_and_j_k_move_within_the_rows_and_the_new_row() {
        let mut s = opened(three());
        let t = Instant::now();
        s.keys(b"\x1b[B", LIVE, t);
        s.keys(b"j", LIVE, t);
        assert_eq!(s.cursor(), 3, "the + new session row");
        s.keys(b"j", LIVE, t);
        assert_eq!(s.cursor(), 3, "and not past it");
        s.keys(b"\x1bOA", LIVE, t);
        s.keys(b"kkkk", LIVE, t);
        assert_eq!(s.cursor(), 0);
    }

    #[test]
    fn enter_switches_creates_on_new_and_closes_on_this() {
        let t = Instant::now();
        let mut s = opened(three());
        assert_eq!(s.keys(b"\r", LIVE, t), Some(Out::Close), "on this");
        s.keys(b"j", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Switch { to: id(LOGS) }))
        );
        s.keys(b"j", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::New { name: None }))
        );
        assert_eq!(
            s.keys(b"n", LIVE, t),
            Some(Out::Ask(Ask::New { name: None }))
        );
    }

    #[test]
    fn a_session_in_use_asks_before_it_is_taken_over_and_the_guard_holds() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"k", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert_eq!(
            s.question().unwrap().text(),
            "take over a3f9c01e from its other client? y/n"
        );
        // Typing still in flight does not answer it.
        assert_eq!(s.keys(b"y", LIVE, t), None);
        assert!(s.question().is_some());
        assert_eq!(
            s.keys(b"y", LIVE, later(t)),
            Some(Out::Ask(Ask::Switch { to: id(FISH) }))
        );
        assert!(s.question().is_none());
    }

    #[test]
    fn x_asks_before_it_kills_and_n_or_esc_drop_the_question() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"jx", LIVE, t);
        assert_eq!(s.question().unwrap().text(), "kill logs? y/n");
        assert_eq!(s.keys(b"n", LIVE, later(t)), None);
        assert!(s.question().is_none());
        s.keys(b"kkx", LIVE, t);
        assert_eq!(
            s.question().unwrap().text(),
            "kill a3f9c01e (in use elsewhere)? y/n"
        );
        assert_eq!(s.keys(b"\x1b", LIVE, later(t)), None);
        assert!(s.question().is_none());
        s.keys(b"x", LIVE, t);
        assert_eq!(
            s.keys(b"y", LIVE, later(t)),
            Some(Out::Ask(Ask::Kill { id: id(FISH) }))
        );
    }

    #[test]
    fn r_edits_the_name_in_place() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"r", LIVE, t);
        assert_eq!(s.rename(), Some("build"), "starts from the name it has");
        s.keys(b"\x7f\x7f\x7f\x7f\x7fci", LIVE, t);
        assert_eq!(s.rename(), Some("ci"));
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Rename {
                id: id(BUILD),
                name: Some(Name::parse("ci").unwrap())
            }))
        );
        assert_eq!(s.rename(), None);

        // Nothing typed clears the name; Esc cancels.
        s.keys(b"r\x7f\x7f\x7f\x7f\x7f", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Rename {
                id: id(BUILD),
                name: None
            }))
        );
        s.keys(b"rxyz", LIVE, t);
        assert_eq!(s.keys(b"\x1b", LIVE, t), None);
        assert_eq!(s.rename(), None);
    }

    #[test]
    fn a_name_the_rules_refuse_keeps_the_field_and_says_why() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"jr", LIVE, t);
        s.keys(b"\x7f\x7f\x7f\x7fcafe", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert!(s.note().unwrap().contains("session id"), "{:?}", s.note());
        assert_eq!(s.rename(), Some("cafe"), "the field stays for a fix");
    }

    #[test]
    fn q_and_esc_go_back_to_the_popup_or_in_a_lobby_quit() {
        let t = Instant::now();
        let mut s = opened(three());
        assert_eq!(s.keys(b"q", LIVE, t), Some(Out::Back));
        assert_eq!(s.keys(b"\x1b", LIVE, t), Some(Out::Back));
        let lobby = Ctx {
            lobby: true,
            ..LIVE
        };
        assert_eq!(s.keys(b"q", lobby, t), Some(Out::Quit));
        assert_eq!(s.keys(b"\x1b", lobby, t), Some(Out::Quit));
    }

    #[test]
    fn a_lobby_with_no_sessions_offers_only_a_new_one() {
        let t = Instant::now();
        let lobby = Ctx {
            lobby: true,
            ..LIVE
        };
        let mut s = opened(vec![]);
        assert_eq!(s.cursor(), 0);
        assert_eq!(
            s.keys(b"\r", lobby, t),
            Some(Out::Ask(Ask::New { name: None }))
        );
        // `r` and `x` have no session to act on.
        assert_eq!(s.keys(b"rx", lobby, t), None);
        assert!(s.question().is_none() && s.rename().is_none());
    }

    #[test]
    fn actions_are_refused_while_the_link_is_down_or_a_request_is_out() {
        let t = Instant::now();
        let mut s = opened(three());
        let down = Ctx {
            live: false,
            ..LIVE
        };
        s.keys(b"j", down, t);
        assert_eq!(s.cursor(), 2, "moving still works");
        assert_eq!(s.keys(b"\r", down, t), None);
        assert_eq!(s.note(), Some(NOT_LIVE));
        let busy = Ctx { busy: true, ..LIVE };
        assert_eq!(s.keys(b"n", busy, t), None);
        assert_eq!(s.note(), Some(BUSY));
        // The note lasts one read.
        s.keys(b"k", LIVE, t);
        assert_eq!(s.note(), None);
    }

    #[test]
    fn a_session_that_cannot_be_switched_to_says_why() {
        let t = Instant::now();
        let mut rows = three();
        rows[2].detachable = false;
        rows.push(entry(
            "77c0ffee77c0ffee77c0ffee77c0ffee",
            Some("old"),
            "/bin/bash",
            1_791_460_000,
            Attached::OtherVersion,
        ));
        let mut s = opened(rows);
        s.keys(b"j", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert!(s.note().unwrap().contains("not detachable"));
        s.keys(b"j", LIVE, t);
        assert_eq!(s.keys(b"\r", LIVE, t), None);
        assert!(s.note().unwrap().contains("another version"));
        assert_eq!(s.keys(b"x", LIVE, t), None);
        assert!(
            s.question().is_none(),
            "another version cannot be killed from here"
        );
    }

    #[test]
    fn a_refetch_keeps_the_selection_on_the_same_session() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"j", LIVE, t);
        assert_eq!(s.cursor(), 2);
        // `a3f9c01e` was killed: logs moved up one row, and stays selected.
        let mut rows = three();
        rows.remove(1);
        s.set_rows(rows);
        assert_eq!(s.rows()[s.cursor()].id, id(LOGS));
        // The selected session itself gone: the same place, clamped.
        let mut rows = three();
        rows.retain(|e| e.id != id(LOGS) && e.id != id(FISH));
        s.set_rows(rows);
        assert_eq!(s.cursor(), 1, "the + new session row");
    }

    #[test]
    fn a_paste_is_never_a_command() {
        let t = Instant::now();
        let mut s = opened(three());
        assert_eq!(s.keys(b"\x1b[200~nj\r\x1b[201~", LIVE, t), None);
        assert_eq!(s.cursor(), 1, "j inside the paste moved nothing");
        // A paste cut across reads, its end marker too.
        assert_eq!(s.keys(b"\x1b[200~n", LIVE, t), None);
        assert_eq!(s.keys(b"jx\r\x1b[2", LIVE, t), None);
        assert!(s.question().is_none());
        assert_eq!(s.keys(b"01~j", LIVE, t), None);
        assert_eq!(s.cursor(), 2, "after the paste, j moves again");
    }

    #[test]
    fn a_paste_while_renaming_is_typed_and_its_newline_does_not_save() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"r", LIVE, t);
        assert_eq!(s.keys(b"\x1b[200~-c\ri\x1b[201~", LIVE, t), None);
        assert_eq!(s.rename(), Some("build-ci"), "control characters dropped");
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Rename {
                id: id(BUILD),
                name: Some(Name::parse("build-ci").unwrap())
            }))
        );
    }

    #[test]
    fn a_rename_stays_on_its_session_through_a_refetch_or_ends_with_it() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"jr", LIVE, t);
        assert_eq!(s.rename(), Some("logs"));
        // `a3f9c01e` was killed elsewhere: the rows shift, the rename does not.
        let mut rows = three();
        rows.remove(1);
        s.set_rows(rows);
        s.keys(b"\x7f\x7f\x7f\x7ftail", LIVE, t);
        assert_eq!(
            s.keys(b"\r", LIVE, t),
            Some(Out::Ask(Ask::Rename {
                id: id(LOGS),
                name: Some(Name::parse("tail").unwrap())
            }))
        );
        // The session being renamed is gone: so is its rename.
        s.keys(b"r", LIVE, t);
        let mut rows = three();
        rows.retain(|e| e.id != id(LOGS));
        s.set_rows(rows);
        assert_eq!(s.rename(), None);
        assert!(s.note().unwrap().contains("logs"), "{:?}", s.note());
    }

    #[test]
    fn a_failure_is_the_note_and_drops_a_question() {
        let t = Instant::now();
        let mut s = opened(three());
        s.keys(b"x", LIVE, t);
        s.fail("session a3f9c01e did not answer in time".to_string());
        assert!(s.question().is_none());
        assert_eq!(s.note(), Some("session a3f9c01e did not answer in time"));
    }
}

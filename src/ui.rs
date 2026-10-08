//! The status popup's modal state, and where every keystroke goes.
//!
//! Pure, like `linkstate`: every method takes the current `Instant`, so the
//! whole machine is tested without sleeping, and nothing here holds a timer.
//! The loop calls [`Ui::tick`] on laps it already takes.
//!
//! The rule that decides the keys: **while the popup is shown, every key
//! is its own.** Nothing typed into it is held or sent, whatever opened it
//! and whatever the link is doing. While it is closed, the popup key
//! (`popup.key`, `Ctrl-\` by default) opens it and every other byte goes to
//! the host -- or, during an outage, is held for the question asked when the
//! link answers again.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant};

use crate::linkstate::Phase;

/// `Ctrl-\`. Opens the popup while it is closed, in every phase; closes it
/// while it is shown. The default of `popup.key`: a session's own key is
/// `Ui::key`.
pub(crate) const PREFIX: u8 = 0x1c;

/// A read that is exactly this byte is the Esc key. Anywhere else in a read
/// it begins an escape sequence -- an arrow key, a function key -- which
/// runs to the end of the read.
pub(crate) const ESC: u8 = 0x1b;

/// How soon a second press of the popup key must follow the one that opened
/// the popup for the pair to mean that key once, literally, for the host. Measured between the
/// two reads' timestamps; nothing is armed.
pub(crate) const DOUBLE_PRESS: Duration = Duration::from_millis(500);

/// How long the question about held input must have been on the screen
/// before `s`, `d` or `q` answer it. Typing that was already in flight when
/// the link came back would otherwise send, drop or quit unseen. Measured
/// from the first lap or read that saw `Confirming` to the read's own
/// timestamp; nothing is armed.
pub(crate) const ANSWER_GUARD: Duration = Duration::from_millis(500);

/// How long the popup stays up showing the outcome after the link it opened
/// for comes back. The default of `popup.linger`.
pub(crate) const LINGER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Closed,
    /// Opened by hand, or touched by a key; stays until closed.
    Open {
        /// When the popup key that opened it arrived, for the double press.
        pressed: Option<Instant>,
    },
    /// Opened because the link went quiet.
    Auto,
    /// The link answers again after an `Auto` popup; closes `LINGER` after
    /// `since`.
    Lingering {
        since: Instant,
        outage: Duration,
    },
    /// The config screen. `cursor` is the row of [`crate::config::SETTINGS`]
    /// it is on; the field's text lives in `Ui`, because `Mode` is `Copy`.
    Config {
        cursor: usize,
        editing: Editing,
    },
    /// The session selector (switcher spec §3.2). Its state lives in `Ui`
    /// beside the config screen's field, because `Mode` is `Copy`.
    Sessions,
}

/// What the config screen is doing with the keys.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Editing {
    /// Browsing the list.
    None,
    /// A text field on the cursor's row.
    Text,
    /// Waiting for the new popup key to be pressed.
    Capture,
    /// The `stun_servers` sub-list on its own cursor, with a text field
    /// open on its entry at `cursor` -- or on a new one past the end.
    Servers { cursor: usize, field: bool },
    /// "save for all hosts or this host only?"
    Save,
}

impl Editing {
    /// A text field or a key capture: every byte is its own.
    fn takes_every_byte(self) -> bool {
        matches!(
            self,
            Editing::Text | Editing::Capture | Editing::Servers { field: true, .. }
        )
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Command {
    Quit,
    SendHeld,
    DropHeld,
    /// Something the config screen needs the session to check or do. The
    /// session answers with [`Ui::accepted`] or [`Ui::say`].
    Config(ConfigCmd),
    /// Something the selector asks of the host.
    Ask(crate::switcher::Ask),
}

/// What the config screen asks of the session. Rows are rows of
/// [`crate::config::SETTINGS`].
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum ConfigCmd {
    /// Enter on a row with nothing open: flip it, or open what edits it.
    Edit(usize),
    /// Enter in a text field.
    Accept { row: usize, text: String },
    /// The key pressed in a key capture; `None` for Backspace, which is
    /// `off`.
    Capture { row: usize, key: Option<u8> },
    /// The `stun_servers` sub-list as it would be after an entry was
    /// accepted, added or removed.
    Servers { row: usize, list: Vec<String> },
    /// `x`.
    Reset(usize),
    /// `w`, then `a` or `h`.
    Save(crate::config::Level),
}

/// The config screen's state, for the view.
pub(crate) struct ConfigScreen<'a> {
    pub(crate) cursor: usize,
    pub(crate) editing: Editing,
    pub(crate) field: &'a str,
    pub(crate) servers: &'a [String],
    /// Why the last change was refused, or what a save did.
    pub(crate) note: Option<&'a str>,
}

/// One line of text being typed: the config screen's fields and the
/// selector's rename. Input is UTF-8, so a character is added once all of
/// its bytes have arrived; control characters never are; at most
/// [`FIELD_MAX`] characters, so a pasted file stops there.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Field {
    text: String,
    /// The bytes of a character that has not arrived whole yet.
    partial: Vec<u8>,
}

impl Field {
    pub(crate) fn as_str(&self) -> &str {
        &self.text
    }

    /// Start over holding `text`.
    pub(crate) fn set(&mut self, text: String) {
        self.text = text;
        self.partial.clear();
    }

    pub(crate) fn clear(&mut self) {
        self.set(String::new());
    }

    /// Backspace: the last character.
    pub(crate) fn erase(&mut self) {
        self.partial.clear();
        self.text.pop();
    }

    /// A byte typed into the field.
    pub(crate) fn type_byte(&mut self, b: u8) {
        self.partial.push(b);
        match std::str::from_utf8(&self.partial) {
            Ok(s) => {
                for c in s.chars().filter(|c| !c.is_control()) {
                    if self.text.chars().count() < FIELD_MAX {
                        self.text.push(c);
                    }
                }
                self.partial.clear();
            }
            // The rest of the character is still to come.
            Err(e) if e.error_len().is_none() => {}
            Err(_) => self.partial.clear(),
        }
    }
}

/// A key the config screen and the selector read out of an escape sequence
/// or a byte.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Key {
    Up,
    Down,
    Byte(u8),
}

/// The end of a bracketed paste.
const PASTE_END: &[u8] = b"\x1b[201~";

/// The most a text field holds: a pasted file stops here.
const FIELD_MAX: usize = 256;

/// Where one read's bytes go. A command ends the read: bytes after it in the
/// same read are dropped -- neither sent, held nor seen by the popup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Routed {
    pub(crate) to_host: Vec<u8>,
    pub(crate) to_hold: Vec<u8>,
    pub(crate) command: Option<Command>,
}

/// What a lap's phase changed, for the activity log.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LinkChange {
    WentSilent,
    Back { outage: Duration },
}

pub(crate) struct Ui {
    mode: Mode,
    /// The popup was closed by hand during the current outage, so it does
    /// not open itself again until a new outage begins.
    dismissed: bool,
    /// When the current outage began: the phase's own `Silent { since }`.
    outage_since: Option<Instant>,
    /// How long the last outage lasted, for the lingering popup.
    last_outage: Duration,
    /// When the current `Confirming` question was first seen, by a lap or a
    /// read, for [`ANSWER_GUARD`]. `None` outside `Confirming`.
    asked_at: Option<Instant>,
    /// The byte that opens and closes the popup: `popup.key`. `None` is
    /// `off`, and then that byte is ordinary input.
    key: Option<u8>,
    /// How long the popup stays up after the link it opened for answers.
    linger: Duration,
    /// How long the host must have been silent before the popup opens by
    /// itself: the effective `popup.auto_open_after`. `None` never opens it.
    auto_open_after: Option<Duration>,
    /// Whether this outage's moment for opening by itself has come while
    /// the config screen was up: it is decided once per outage, as
    /// `dismissed` is (spec §4.2).
    config_decided: bool,
    /// The config screen's text field.
    field: Field,
    /// The `stun_servers` sub-list as the screen shows it.
    servers: Vec<String>,
    /// The sub-list as the last command proposed it, until the session
    /// accepts it.
    proposed: Option<Vec<String>>,
    /// What the help line says instead of the help: why a change was
    /// refused, or what a save did. Gone with the next read.
    note: Option<String>,
    /// The session selector's state, while it is up and between openings.
    selector: crate::selector::Selector,
    /// The client is in a lobby: the selector's `q` ends the client, and
    /// the popup key does not close it (there is nothing behind it).
    lobby: bool,
    /// A switcher request is in flight.
    busy: bool,
    /// Inside a bracketed paste on the config screen whose end has not
    /// arrived yet, and how many bytes of that end the last read finished
    /// on: a large paste is cut into reads wherever the buffer ends, its end
    /// marker too.
    pasting: Option<usize>,
}

impl Ui {
    pub(crate) fn new() -> Ui {
        Ui {
            mode: Mode::Closed,
            dismissed: false,
            outage_since: None,
            last_outage: Duration::ZERO,
            asked_at: None,
            key: Some(PREFIX),
            linger: LINGER,
            // At the first lap of an outage, as before the setting existed.
            // A session's `apply` sets the configured value, which is never
            // below `silent_after` and so changes nothing a lap could see.
            auto_open_after: Some(Duration::ZERO),
            config_decided: false,
            field: Field::default(),
            servers: Vec::new(),
            proposed: None,
            note: None,
            selector: crate::selector::Selector::default(),
            lobby: false,
            busy: false,
            pasting: None,
        }
    }

    /// What the selector needs to know about the session: whether it is in
    /// a lobby, and whether a request is in flight. Set before every read.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the session uses it from Task 12 on")
    )]
    pub(crate) fn set_switcher(&mut self, lobby: bool, busy: bool) {
        self.lobby = lobby;
        self.busy = busy;
    }

    /// Open the selector, as `s` does: from a connect-time lobby, or after
    /// the session it was showing ended. The session fetches the list.
    pub(crate) fn open_sessions(&mut self) {
        self.leave_config();
        self.selector.open();
        self.mode = Mode::Sessions;
    }

    /// The selector, for the view and for the session to fill in.
    pub(crate) fn selector(&self) -> &crate::selector::Selector {
        &self.selector
    }

    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the session uses it from Task 12 on")
    )]
    pub(crate) fn selector_mut(&mut self) -> &mut crate::selector::Selector {
        &mut self.selector
    }

    /// Close the popup, wherever it was: a switch landed, or a lobby
    /// became a session.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "the session uses it from Task 12 on")
    )]
    pub(crate) fn close_popup(&mut self) {
        self.leave_config();
        self.mode = Mode::Closed;
    }

    /// The popup's three settings, from `apply`. They act from the next
    /// read or lap: a popup already open stays open.
    pub(crate) fn retune(
        &mut self,
        key: Option<u8>,
        linger: Duration,
        auto_open_after: Option<Duration>,
    ) {
        self.key = key;
        self.linger = linger;
        self.auto_open_after = auto_open_after;
    }

    /// Whether an outage that began at `since` has lasted long enough at
    /// `now` for the popup to open by itself.
    fn auto_due(&self, since: Instant, now: Instant) -> bool {
        self.auto_open_after
            .is_some_and(|after| now.saturating_duration_since(since) >= after)
    }

    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Whether the popup is on the screen. `Confirming` forces it: the held
    /// input is waiting on an answer, and the question is in the popup.
    pub(crate) fn visible(&self, phase: Phase) -> bool {
        self.mode != Mode::Closed || phase == Phase::Confirming
    }

    /// One lap's phase. Opens the popup when an outage begins -- unless it
    /// was closed by hand during this one -- starts the linger when the link
    /// answers again, and ends it.
    pub(crate) fn tick(&mut self, phase: Phase, now: Instant) -> Option<LinkChange> {
        self.note_question(phase, now);
        if phase == Phase::Confirming {
            // The question takes over the config screen, field and all, so
            // its `s` and `d` can never land in a field -- and the
            // selector, whose `s`-less keys would leave it unanswerable.
            self.leave_config();
            self.leave_sessions();
        }
        if phase.is_outage() {
            let change = if self.outage_since.is_none() {
                // `Recovering` is only ever entered from `Silent`, so `now`
                // is never actually used -- but a lap that somehow saw
                // `Recovering` first still gets an outage that starts
                // somewhere.
                self.outage_since = Some(match phase {
                    Phase::Silent { since } => since,
                    _ => now,
                });
                self.dismissed = false;
                self.config_decided = false;
                Some(LinkChange::WentSilent)
            } else {
                None
            };
            let due = self
                .outage_since
                .is_some_and(|since| self.auto_due(since, now));
            match self.mode {
                Mode::Closed if !self.dismissed && due => self.mode = Mode::Auto,
                Mode::Lingering { .. } if due => self.mode = Mode::Auto,
                // The config screen gives way to the status view at the
                // moment the popup would have opened -- unless a field is
                // open, and then it stays for the rest of the outage.
                Mode::Config { editing, .. } if due && !self.config_decided => {
                    self.config_decided = true;
                    if !editing.takes_every_byte() {
                        self.leave_config();
                    }
                }
                _ => {}
            }
            return change;
        }

        let change = self.outage_since.take().map(|since| {
            self.last_outage = now.saturating_duration_since(since);
            LinkChange::Back {
                outage: self.last_outage,
            }
        });
        match self.mode {
            Mode::Auto if phase == Phase::Live => {
                self.mode = Mode::Lingering {
                    since: now,
                    outage: self.last_outage,
                };
            }
            Mode::Lingering { since, .. }
                if now.saturating_duration_since(since) >= self.linger =>
            {
                self.mode = Mode::Closed;
            }
            _ => {}
        }
        change
    }

    /// One read from the keyboard: what goes to the host, what is held, and
    /// the command, if one was typed.
    pub(crate) fn keys(&mut self, bytes: &[u8], phase: Phase, now: Instant) -> Routed {
        self.note_question(phase, now);
        if phase == Phase::Confirming {
            self.leave_config();
            self.leave_sessions();
        }
        if self.mode == Mode::Sessions {
            self.pasting = None;
            return self.sessions_keys(bytes, phase, now);
        }
        if matches!(self.mode, Mode::Config { .. }) {
            // The rest of a paste that began in an earlier read is the
            // paste's.
            let bytes = match self.pasting {
                Some(_) => self.paste(bytes),
                None => bytes,
            };
            return self.config_keys(bytes, phase);
        }
        // Pastes are only ever tracked on the config screen: anywhere else a
        // paste's bytes go where they always went, the host's included.
        self.pasting = None;
        let mut routed = Routed::default();
        let lone_esc = bytes == [ESC];
        let mut rest = bytes;
        while let Some((&b, tail)) = rest.split_first() {
            rest = tail;
            if !self.visible(phase) {
                if Some(b) == self.key {
                    self.mode = Mode::Open { pressed: Some(now) };
                } else {
                    deliver(b, phase, &mut routed);
                }
                continue;
            }
            if b == ESC && !lone_esc {
                // An escape sequence: the rest of the read is one key the
                // popup has no use for.
                self.touch();
                break;
            }
            if self.shown_key(b, phase, now, &mut routed) {
                break;
            }
        }
        routed
    }

    /// Start the answer guard the first time `Confirming` is seen, and
    /// forget it once the phase is anything else, so the next question is
    /// guarded from its own beginning.
    fn note_question(&mut self, phase: Phase, now: Instant) {
        if phase == Phase::Confirming {
            self.asked_at.get_or_insert(now);
        } else {
            self.asked_at = None;
        }
    }

    /// Whether the question has been up long enough to be answered by a
    /// read at `now`.
    fn answerable(&self, now: Instant) -> bool {
        self.asked_at
            .is_some_and(|at| now.saturating_duration_since(at) >= ANSWER_GUARD)
    }

    /// A key while the popup is shown: every key is its own. Returns whether
    /// the read is over.
    fn shown_key(&mut self, b: u8, phase: Phase, now: Instant, r: &mut Routed) -> bool {
        let confirming = phase == Phase::Confirming;
        // Just after the question appears, its keys are anybody's typing
        // still in flight: they do nothing, like every other key.
        let command = match b {
            _ if confirming && !self.answerable(now) => None,
            b'q' => Some(Command::Quit),
            b's' if confirming => Some(Command::SendHeld),
            b'd' if confirming => Some(Command::DropHeld),
            _ => None,
        };
        if command.is_some() {
            r.command = command;
            return true;
        }
        if b == b's' && !confirming {
            // Only on a live link (switcher spec §3.2): the key bar shows it
            // dimmed otherwise, and it does nothing but touch the popup.
            if phase == Phase::Live {
                self.open_sessions();
                r.command = Some(Command::Ask(crate::switcher::Ask::Sessions));
                return true;
            }
            self.touch();
            return false;
        }
        if b == b'c' && !confirming {
            self.mode = Mode::Config {
                cursor: 0,
                editing: Editing::None,
            };
            // Opened after this outage's moment has passed: there is no
            // moment left to give way at.
            self.config_decided = self
                .outage_since
                .is_some_and(|since| self.auto_due(since, now));
            return true;
        }
        match b {
            // The question about held input has to be answered before the
            // popup can go.
            _ if (b == ESC || Some(b) == self.key) && !confirming => self.close(b, phase, now, r),
            // Every other key only counts as touching the popup.
            _ => self.touch(),
        }
        false
    }

    /// Close the popup with `b`, `Esc` or the popup key.
    fn close(&mut self, b: u8, phase: Phase, now: Instant, r: &mut Routed) {
        if Some(b) == self.key
            && let Mode::Open { pressed: Some(at) } = self.mode
            && now.saturating_duration_since(at) < DOUBLE_PRESS
        {
            deliver(b, phase, r);
        }
        self.mode = Mode::Closed;
        if phase.is_outage() {
            self.dismissed = true;
        }
    }

    /// The config screen, if it is up, for the view.
    pub(crate) fn config_screen(&self) -> Option<ConfigScreen<'_>> {
        let Mode::Config { cursor, editing } = self.mode else {
            return None;
        };
        Some(ConfigScreen {
            cursor,
            editing,
            field: self.field.as_str(),
            servers: &self.servers,
            note: self.note.as_deref(),
        })
    }

    /// Open a text field on the cursor's row, holding `text`.
    pub(crate) fn open_text(&mut self, text: String) {
        if let Mode::Config { cursor, .. } = self.mode {
            self.field.set(text);
            self.mode = Mode::Config {
                cursor,
                editing: Editing::Text,
            };
        }
    }

    /// Wait for the new popup key.
    pub(crate) fn open_capture(&mut self) {
        if let Mode::Config { cursor, .. } = self.mode {
            self.mode = Mode::Config {
                cursor,
                editing: Editing::Capture,
            };
        }
    }

    /// Open the `stun_servers` sub-list on `list`.
    pub(crate) fn open_servers(&mut self, list: Vec<String>) {
        if let Mode::Config { cursor, .. } = self.mode {
            self.servers = list;
            self.mode = Mode::Config {
                cursor,
                editing: Editing::Servers {
                    cursor: 0,
                    field: false,
                },
            };
        }
    }

    /// The session took the change: the field or capture closes, and the
    /// sub-list becomes what was proposed.
    pub(crate) fn accepted(&mut self) {
        let Mode::Config { cursor, editing } = self.mode else {
            return;
        };
        self.field.clear();
        let editing = match editing {
            Editing::Servers { cursor: at, .. } => {
                if let Some(list) = self.proposed.take() {
                    self.servers = list;
                }
                Editing::Servers {
                    cursor: at.min(self.servers.len().saturating_sub(1)),
                    field: false,
                }
            }
            _ => Editing::None,
        };
        self.mode = Mode::Config { cursor, editing };
    }

    /// Put `note` on the help line until the next read: why a change was
    /// refused -- the field, if one is open, stays open -- or what a save
    /// did.
    pub(crate) fn say(&mut self, note: String) {
        self.proposed = None;
        self.note = Some(note);
    }

    /// Back from the selector to the status view.
    fn leave_sessions(&mut self) {
        if self.mode == Mode::Sessions {
            self.mode = Mode::Open { pressed: None };
        }
    }

    /// One read on the selector. The popup key closes the popup as it does
    /// everywhere -- except in a lobby, where there is nothing behind it.
    fn sessions_keys(&mut self, bytes: &[u8], phase: Phase, now: Instant) -> Routed {
        use crate::selector::{Ctx, Out};
        let mut r = Routed::default();
        if let Some(key) = self.key
            && bytes.first() == Some(&key)
            && self.selector.rename().is_none()
            && !self.selector.pasting()
        {
            if !self.lobby {
                self.mode = Mode::Closed;
                if phase.is_outage() {
                    self.dismissed = true;
                }
            }
            return r;
        }
        let ctx = Ctx {
            live: phase == Phase::Live,
            lobby: self.lobby,
            busy: self.busy,
        };
        match self.selector.keys(bytes, ctx, now) {
            None => {}
            Some(Out::Ask(a)) => r.command = Some(Command::Ask(a)),
            Some(Out::Back) => self.mode = Mode::Open { pressed: None },
            Some(Out::Close) => self.mode = Mode::Closed,
            Some(Out::Quit) => r.command = Some(Command::Quit),
        }
        r
    }

    /// Back from the config screen to the status view, dropping whatever
    /// field was open. The pending edits are the session's and stay.
    fn leave_config(&mut self) {
        if matches!(self.mode, Mode::Config { .. }) {
            self.mode = Mode::Open { pressed: None };
            self.field.clear();
            self.proposed = None;
            self.note = None;
            self.pasting = None;
        }
    }

    /// One read on the config screen. A command ends the read, as it does
    /// on the status view.
    fn config_keys(&mut self, bytes: &[u8], phase: Phase) -> Routed {
        let mut r = Routed::default();
        self.note = None;
        let lone_esc = bytes == [ESC];
        let mut rest = bytes;
        while let Some((&b, tail)) = rest.split_first() {
            if !matches!(self.mode, Mode::Config { .. }) {
                break;
            }
            let key = if b == ESC && !lone_esc {
                let (key, used) = escape(tail);
                rest = &tail[used..];
                match key {
                    Some(key) => key,
                    None if tail.starts_with(b"[200~") => {
                        self.pasting = Some(0);
                        rest = self.paste(rest);
                        continue;
                    }
                    None => continue,
                }
            } else {
                rest = tail;
                Key::Byte(b)
            };
            if self.config_key(key, phase, &mut r) {
                break;
            }
        }
        r
    }

    /// A paste's text up to its end, if the end is in `bytes`: into an open
    /// field, or nowhere. Returns what follows the end.
    fn paste<'a>(&mut self, bytes: &'a [u8]) -> &'a [u8] {
        let mut pasting = self.pasting;
        let rest = paste_through(&mut pasting, bytes, |b| self.paste_byte(b));
        self.pasting = pasting;
        rest
    }

    /// One byte of a paste: into the field, if one is open. `type_byte`
    /// drops control characters, the paste's ESC and newlines among them.
    fn paste_byte(&mut self, b: u8) {
        if matches!(
            self.mode,
            Mode::Config {
                editing: Editing::Text | Editing::Servers { field: true, .. },
                ..
            }
        ) {
            self.field.type_byte(b);
        }
    }

    /// One key on the config screen. Returns whether the read is over.
    fn config_key(&mut self, key: Key, phase: Phase, r: &mut Routed) -> bool {
        use crate::config::{Level, SETTINGS};
        let Mode::Config { cursor, editing } = self.mode else {
            return true;
        };
        let to = |editing| Mode::Config { cursor, editing };
        let command = |c| Some(Command::Config(c));
        let enter = |k| matches!(k, Key::Byte(b'\r' | b'\n'));
        let erase = |k| matches!(k, Key::Byte(0x7f | 0x08));
        match editing {
            // A text field: every byte is its own; an arrow is nothing.
            Editing::Text | Editing::Servers { field: true, .. } => match key {
                Key::Byte(ESC) => {
                    self.field.clear();
                    self.mode = to(match editing {
                        Editing::Servers { cursor: at, .. } => Editing::Servers {
                            cursor: at.min(self.servers.len().saturating_sub(1)),
                            field: false,
                        },
                        _ => Editing::None,
                    });
                }
                k if enter(k) => {
                    let text = self.field.as_str().trim().to_string();
                    r.command = match editing {
                        Editing::Servers { cursor: at, .. } => {
                            let mut list = self.servers.clone();
                            match (list.get_mut(at), text.is_empty()) {
                                (Some(_), true) => {
                                    list.remove(at);
                                }
                                (Some(entry), false) => *entry = text,
                                (None, false) => list.push(text),
                                // Adding nothing: the field just closes.
                                (None, true) => {
                                    self.accepted();
                                    return true;
                                }
                            }
                            self.proposed = Some(list.clone());
                            command(ConfigCmd::Servers { row: cursor, list })
                        }
                        _ => command(ConfigCmd::Accept { row: cursor, text }),
                    };
                    // A newline ends the read: the rest of an unbracketed
                    // paste is dropped, never run as commands.
                    return true;
                }
                k if erase(k) => {
                    self.field.erase();
                }
                Key::Byte(b) if b >= 0x20 => self.field.type_byte(b),
                _ => {}
            },
            Editing::Capture => match key {
                Key::Byte(ESC) => self.mode = to(Editing::None),
                k if erase(k) => {
                    r.command = command(ConfigCmd::Capture {
                        row: cursor,
                        key: None,
                    });
                    return true;
                }
                Key::Byte(b) => {
                    r.command = command(ConfigCmd::Capture {
                        row: cursor,
                        key: Some(b),
                    });
                    return true;
                }
                _ => {}
            },
            Editing::Servers {
                cursor: at,
                field: false,
            } => {
                let last = self.servers.len().saturating_sub(1);
                match key {
                    Key::Up | Key::Byte(b'k') => {
                        self.mode = to(Editing::Servers {
                            cursor: at.saturating_sub(1),
                            field: false,
                        });
                    }
                    Key::Down | Key::Byte(b'j') => {
                        self.mode = to(Editing::Servers {
                            cursor: (at + 1).min(last),
                            field: false,
                        });
                    }
                    k if enter(k) && at < self.servers.len() => {
                        self.field.set(self.servers[at].clone());
                        self.mode = to(Editing::Servers {
                            cursor: at,
                            field: true,
                        });
                    }
                    Key::Byte(b'+') => {
                        self.field.clear();
                        self.mode = to(Editing::Servers {
                            cursor: self.servers.len(),
                            field: true,
                        });
                    }
                    Key::Byte(b'-') if at < self.servers.len() => {
                        let mut list = self.servers.clone();
                        list.remove(at);
                        self.proposed = Some(list.clone());
                        r.command = command(ConfigCmd::Servers { row: cursor, list });
                        return true;
                    }
                    Key::Byte(ESC) => self.mode = to(Editing::None),
                    Key::Byte(b'q') => {
                        r.command = Some(Command::Quit);
                        return true;
                    }
                    _ => {}
                }
            }
            Editing::Save => match key {
                Key::Byte(b'a') => {
                    r.command = command(ConfigCmd::Save(Level::Global));
                    return true;
                }
                Key::Byte(b'h') => {
                    r.command = command(ConfigCmd::Save(Level::Host));
                    return true;
                }
                Key::Byte(ESC) => self.mode = to(Editing::None),
                _ => {}
            },
            Editing::None => match key {
                Key::Up | Key::Byte(b'k') => {
                    self.mode = Mode::Config {
                        cursor: cursor.saturating_sub(1),
                        editing,
                    };
                }
                Key::Down | Key::Byte(b'j') => {
                    self.mode = Mode::Config {
                        cursor: (cursor + 1).min(SETTINGS.len() - 1),
                        editing,
                    };
                }
                k if enter(k) => {
                    r.command = command(ConfigCmd::Edit(cursor));
                    return true;
                }
                Key::Byte(b'x') => {
                    r.command = command(ConfigCmd::Reset(cursor));
                    return true;
                }
                Key::Byte(b'w') => self.mode = to(Editing::Save),
                Key::Byte(b'q') => {
                    r.command = Some(Command::Quit);
                    return true;
                }
                // Back to the status view; the edits stay applied.
                Key::Byte(ESC) => self.leave_config(),
                // The popup key closes the popup, as on the status view.
                Key::Byte(b) if Some(b) == self.key => {
                    self.leave_config();
                    self.mode = Mode::Closed;
                    if phase.is_outage() {
                        self.dismissed = true;
                    }
                    return true;
                }
                _ => {}
            },
        }
        false
    }

    /// A key that does nothing still tells an outage's popup that someone is
    /// looking at it: it stays until closed.
    fn touch(&mut self) {
        if matches!(self.mode, Mode::Auto | Mode::Lingering { .. }) {
            self.mode = Mode::Open { pressed: None };
        }
    }
}

/// A bracketed paste's text, read up to its end if the end is in `bytes`:
/// each byte goes to `sink`, and what follows the end is returned.
/// `pasting` is how many bytes of the end marker the last read finished on:
/// `Some` while a paste is open, `None` once its end has arrived. A read
/// that stops inside the end marker leaves it to the next read to say
/// whether it was one.
pub(crate) fn paste_through<'a>(
    pasting: &mut Option<usize>,
    bytes: &'a [u8],
    mut sink: impl FnMut(u8),
) -> &'a [u8] {
    for (i, &b) in bytes.iter().enumerate() {
        let matched = pasting.unwrap_or(0);
        if b == PASTE_END[matched] {
            if matched + 1 == PASTE_END.len() {
                *pasting = None;
                return &bytes[i + 1..];
            }
            *pasting = Some(matched + 1);
            continue;
        }
        // What looked like the end's beginning was text after all. Only its
        // ESC can begin the end again: the marker has no other overlap with
        // itself.
        for &h in &PASTE_END[..matched] {
            sink(h);
        }
        if b == PASTE_END[0] {
            *pasting = Some(1);
        } else {
            *pasting = Some(0);
            sink(b);
        }
    }
    &bytes[bytes.len()..]
}

/// The key an escape sequence stands for on the config screen and the
/// selector, and how many bytes after the ESC the sequence took. Only the
/// arrows mean anything; any other sequence is skipped whole. A bracketed
/// paste's start is `None` too, with its five bytes used: the caller tells
/// it from the rest by looking, and reads the paste with [`paste_through`].
pub(crate) fn escape(tail: &[u8]) -> (Option<Key>, usize) {
    match tail {
        [b'[' | b'O', b'A', ..] => (Some(Key::Up), 2),
        [b'[' | b'O', b'B', ..] => (Some(Key::Down), 2),
        [b'[', b'2', b'0', b'0', b'~', ..] => (None, 5),
        [b'[', rest @ ..] => {
            // CSI: parameter and intermediate bytes, then one final byte.
            let end = rest
                .iter()
                .position(|b| (0x40..=0x7e).contains(b))
                .map_or(rest.len(), |i| i + 1);
            (None, 1 + end)
        }
        [b'O', _, ..] => (None, 2),
        // Alt and a key.
        [_, ..] => (None, 1),
        [] => (None, 0),
    }
}

/// Where a byte typed with the popup closed goes: the host while the link
/// answers, the held buffer during an outage.
///
/// This is also the route of the literal a double press stands for. During an
/// outage that literal is held with the other typed bytes, and reaches the host
/// only through the `Confirming` answer. Under `Confirming` itself a quick
/// second press of the popup key sends nothing, so no literal runs ahead of the held input.
fn deliver(b: u8, phase: Phase, r: &mut Routed) {
    if phase.is_outage() {
        r.to_hold.push(b);
    } else {
        r.to_host.push(b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn silent(t: Instant) -> Phase {
        Phase::Silent { since: t }
    }

    fn recovering(t: Instant) -> Phase {
        Phase::Recovering {
            attempt: 0,
            next_try: t,
        }
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    fn open_at(t: Instant) -> Ui {
        let mut ui = Ui::new();
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: Some(t) });
        ui
    }

    fn auto_at(t: Instant) -> Ui {
        let mut ui = Ui::new();
        ui.tick(silent(t), t);
        assert_eq!(ui.mode(), Mode::Auto);
        ui
    }

    fn lingering_at(t: Instant) -> Ui {
        let mut ui = auto_at(t);
        ui.tick(Phase::Live, ms(t, 100));
        assert!(matches!(ui.mode(), Mode::Lingering { .. }));
        ui
    }

    /// An outage whose popup the user closed by hand.
    fn dismissed_at(t: Instant) -> Ui {
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(&[ESC], silent(t), t), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);
        ui
    }

    fn host(bytes: &[u8]) -> Routed {
        Routed {
            to_host: bytes.to_vec(),
            ..Routed::default()
        }
    }

    fn hold(bytes: &[u8]) -> Routed {
        Routed {
            to_hold: bytes.to_vec(),
            ..Routed::default()
        }
    }

    fn command(c: Command) -> Routed {
        Routed {
            command: Some(c),
            ..Routed::default()
        }
    }

    // ---- closed, healthy link ------------------------------------------

    #[test]
    fn typing_on_a_healthy_link_passes_through_untouched() {
        let mut ui = Ui::new();
        assert_eq!(
            ui.keys(b"ls -l\r", Phase::Live, Instant::now()),
            host(b"ls -l\r")
        );
        assert_eq!(ui.mode(), Mode::Closed);
        assert!(!ui.visible(Phase::Live));
    }

    #[test]
    fn ctrl_backslash_opens_the_popup_and_is_not_sent() {
        let ui = open_at(Instant::now());
        assert!(ui.visible(Phase::Live));
    }

    #[test]
    fn what_comes_before_ctrl_backslash_goes_to_the_host_and_what_follows_to_the_popup() {
        let mut ui = Ui::new();
        let r = ui.keys(b"ls\x1cq", Phase::Live, Instant::now());
        assert_eq!(
            r,
            Routed {
                to_host: b"ls".to_vec(),
                command: Some(Command::Quit),
                ..Routed::default()
            }
        );
    }

    #[test]
    fn a_double_press_within_the_window_sends_one_literal() {
        let t = Instant::now();
        let mut ui = open_at(t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, ms(t, 499)), host(&[PREFIX]));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_second_press_after_the_window_only_closes() {
        let t = Instant::now();
        let mut ui = open_at(t);
        assert_eq!(
            ui.keys(&[PREFIX], Phase::Live, ms(t, 500)),
            Routed::default()
        );
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_double_press_in_one_read_sends_one_literal() {
        let mut ui = Ui::new();
        assert_eq!(
            ui.keys(&[PREFIX, PREFIX], Phase::Live, Instant::now()),
            host(&[PREFIX])
        );
        assert_eq!(ui.mode(), Mode::Closed);
    }

    /// Only the press that OPENED the popup starts the window: one opened
    /// by an outage has none, so a single `Ctrl-\` only closes it.
    #[test]
    fn a_popup_not_opened_by_ctrl_backslash_has_no_double_press() {
        let t = Instant::now();
        let mut ui = lingering_at(t);
        assert_eq!(
            ui.keys(&[PREFIX], Phase::Live, ms(t, 150)),
            Routed::default()
        );
        assert_eq!(ui.mode(), Mode::Closed);
    }

    // ---- shown: every key is the popup's ---------------------------------

    /// Nothing typed into the popup goes anywhere, and it stays open.
    #[test]
    fn every_key_belongs_to_the_open_popup() {
        let t = Instant::now();
        let mut ui = open_at(t);
        // No `s` and no `c` in it: those open the selector and the config
        // screen.
        assert_eq!(
            ui.keys(b"pwd -P\r", Phase::Live, ms(t, 10)),
            Routed::default()
        );
        assert_eq!(
            ui.mode(),
            Mode::Open { pressed: Some(t) },
            "typing closed the popup"
        );
        assert!(ui.visible(Phase::Live));
    }

    /// Review focus 1. A lone `Esc` closes; an arrow key, or application
    /// keypad `1` (`ESC O q`), is one key the popup ignores -- its `q`
    /// must not quit and its bytes must not reach the host.
    #[test]
    fn a_lone_esc_closes_but_an_escape_sequence_does_nothing() {
        let t = Instant::now();
        let mut ui = open_at(t);
        assert_eq!(ui.keys(&[ESC], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);

        for sequence in [&b"\x1b[A"[..], b"\x1bOq"] {
            let mut ui = open_at(t);
            assert_eq!(
                ui.keys(sequence, Phase::Live, t),
                Routed::default(),
                "{sequence:?}"
            );
            assert_eq!(
                ui.mode(),
                Mode::Open { pressed: Some(t) },
                "{sequence:?} closed the popup"
            );

            let mut ui = auto_at(t);
            assert_eq!(
                ui.keys(sequence, silent(t), t),
                Routed::default(),
                "{sequence:?}"
            );
            assert_eq!(
                ui.mode(),
                Mode::Open { pressed: None },
                "{sequence:?} did not touch the popup"
            );
        }
    }

    #[test]
    fn q_quits_whatever_opened_the_popup() {
        let t = Instant::now();
        assert_eq!(
            open_at(t).keys(b"q", Phase::Live, t),
            command(Command::Quit)
        );
        assert_eq!(auto_at(t).keys(b"q", silent(t), t), command(Command::Quit));
        assert_eq!(
            auto_at(t).keys(b"q", recovering(t), t),
            command(Command::Quit)
        );
        assert_eq!(
            lingering_at(t).keys(b"q", Phase::Live, ms(t, 200)),
            command(Command::Quit)
        );
        assert_eq!(
            confirming_at(t).keys(b"q", Phase::Confirming, t + ANSWER_GUARD),
            command(Command::Quit)
        );
    }

    #[test]
    fn s_opens_the_selector_on_a_live_link_and_asks_for_the_list() {
        let t = Instant::now();
        let mut ui = open_at(t);
        assert_eq!(
            ui.keys(b"s", Phase::Live, t),
            command(Command::Ask(crate::switcher::Ask::Sessions))
        );
        assert_eq!(ui.mode(), Mode::Sessions);
        assert!(ui.selector().loading(), "a fresh list is on its way");
    }

    #[test]
    fn s_does_nothing_while_the_link_is_down() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"s", silent(t), t), Routed::default());
        assert!(
            matches!(ui.mode(), Mode::Open { .. }),
            "touched, not opened"
        );
    }
    #[test]
    fn send_and_drop_are_commands_only_under_confirming() {
        let t = Instant::now();
        for (key, want) in [(b's', Command::SendHeld), (b'd', Command::DropHeld)] {
            assert_eq!(
                confirming_at(t).keys(&[key], Phase::Confirming, t + ANSWER_GUARD),
                command(want)
            );
            assert_eq!(
                auto_at(t).keys(&[key], silent(t), t),
                Routed::default(),
                "{} was honoured under an outage",
                key as char
            );
        }
        // On a live link `s` opens the selector and `d` only touches the
        // popup: neither is ever an answer to a question that is not up.
        assert_eq!(
            open_at(t).keys(b"s", Phase::Live, t),
            command(Command::Ask(crate::switcher::Ask::Sessions))
        );
        assert_eq!(open_at(t).keys(b"d", Phase::Live, t), Routed::default());
    }

    #[test]
    fn confirming_forces_the_popup_visible() {
        let ui = Ui::new();
        assert!(!ui.visible(Phase::Live));
        assert!(ui.visible(Phase::Confirming));
    }

    /// The held input is waiting on an answer: neither `Esc` nor `Ctrl-\`
    /// closes the question, and a quick second `Ctrl-\` sends no literal
    /// ahead of the input it is about.
    #[test]
    fn confirming_cannot_be_closed_until_it_is_answered() {
        let t = Instant::now();
        let mut ui = Ui::new();
        assert_eq!(ui.keys(&[ESC], Phase::Confirming, t), Routed::default());
        assert_eq!(ui.keys(&[PREFIX], Phase::Confirming, t), Routed::default());
        assert!(
            ui.visible(Phase::Confirming),
            "the question was closed unanswered"
        );

        let mut ui = open_at(t);
        assert_eq!(
            ui.keys(&[PREFIX], Phase::Confirming, ms(t, 10)),
            Routed::default()
        );
        assert_eq!(ui.mode(), Mode::Open { pressed: Some(t) });
    }

    // ---- the question's answer guard ------------------------------------

    /// The question about held input, first on the screen at `t` (the lap
    /// that saw `Confirming`).
    fn confirming_at(t: Instant) -> Ui {
        let mut ui = Ui::new();
        ui.tick(Phase::Confirming, t);
        assert!(ui.visible(Phase::Confirming));
        ui
    }

    /// Final review, Important 1. Typing that was already in flight when
    /// the link came back must not answer a question nobody has read yet:
    /// within `ANSWER_GUARD` of the question appearing, `s`, `d` and `q`
    /// are keys like any other -- consumed, and nothing else happens.
    #[test]
    fn s_d_and_q_do_nothing_just_after_the_question_appears() {
        let t = Instant::now();
        for key in *b"sdq" {
            let mut ui = confirming_at(t);
            assert_eq!(
                ui.keys(&[key], Phase::Confirming, ms(t, 100)),
                Routed::default(),
                "{} answered a question shown 100 ms ago",
                key as char
            );
            assert!(ui.visible(Phase::Confirming), "{}", key as char);
            assert_eq!(
                ui.keys(&[key], Phase::Confirming, ms(t, 499)),
                Routed::default(),
                "{} answered inside the guard",
                key as char
            );
        }
    }

    /// From `ANSWER_GUARD` on, they answer as before. A before-assertion for
    /// the test above: the same `Ui`, the same key, only the time differs.
    #[test]
    fn s_d_and_q_answer_once_the_question_has_been_up_long_enough() {
        let t = Instant::now();
        for (key, want) in [
            (b's', Command::SendHeld),
            (b'd', Command::DropHeld),
            (b'q', Command::Quit),
        ] {
            let mut ui = confirming_at(t);
            assert_eq!(
                ui.keys(&[key], Phase::Confirming, ms(t, 100)),
                Routed::default()
            );
            assert_eq!(
                ui.keys(&[key], Phase::Confirming, ms(t, 600)),
                command(want.clone()),
                "{}",
                key as char
            );
            let mut ui = confirming_at(t);
            assert_eq!(
                ui.keys(&[key], Phase::Confirming, t + ANSWER_GUARD),
                command(want),
                "{} at exactly the guard",
                key as char
            );
        }
    }

    /// `keys` can see `Confirming` before any lap has: the guard then
    /// starts at that read.
    #[test]
    fn the_guard_starts_at_the_first_read_that_sees_the_question() {
        let t = Instant::now();
        let mut ui = Ui::new();
        assert_eq!(ui.keys(b"s", Phase::Confirming, t), Routed::default());
        // A later lap does not move the start.
        ui.tick(Phase::Confirming, ms(t, 300));
        assert_eq!(
            ui.keys(b"s", Phase::Confirming, ms(t, 500)),
            command(Command::SendHeld)
        );
    }

    /// A new question is a new question: leaving `Confirming` clears the
    /// start, and the next episode is guarded from its own beginning.
    #[test]
    fn the_guard_restarts_for_a_new_question() {
        let t = Instant::now();
        let mut ui = confirming_at(t);
        assert_eq!(
            ui.keys(b"x", Phase::Confirming, ms(t, 1_000)),
            Routed::default()
        );
        ui.tick(silent(ms(t, 1_100)), ms(t, 3_100));
        let again = ms(t, 5_000);
        ui.tick(Phase::Confirming, again);
        assert_eq!(
            ui.keys(b"s", Phase::Confirming, again + Duration::from_millis(100)),
            Routed::default(),
            "the second question inherited the first one's guard"
        );
        assert_eq!(
            ui.keys(b"s", Phase::Confirming, again + ANSWER_GUARD),
            command(Command::SendHeld)
        );

        // The same through `keys` alone: a read under `Silent` clears it.
        let mut ui = confirming_at(t);
        ui.keys(b"", silent(ms(t, 1_100)), ms(t, 3_100));
        assert_eq!(
            ui.keys(b"d", Phase::Confirming, ms(t, 5_000)),
            Routed::default(),
            "a read outside Confirming left the old start in place"
        );
    }

    // ---- outages -------------------------------------------------------

    #[test]
    fn an_outage_opens_the_popup_by_itself_once() {
        let t = Instant::now();
        let mut ui = Ui::new();
        assert_eq!(ui.tick(Phase::Live, t), None);
        assert_eq!(ui.mode(), Mode::Closed);
        assert_eq!(
            ui.tick(silent(t), ms(t, 2_000)),
            Some(LinkChange::WentSilent)
        );
        assert_eq!(ui.mode(), Mode::Auto);
        assert_eq!(
            ui.tick(silent(t), ms(t, 2_100)),
            None,
            "the outage started twice"
        );
    }

    /// Nothing jumps.
    #[test]
    fn an_open_popup_stays_open_when_an_outage_starts() {
        let t = Instant::now();
        let mut ui = open_at(t);
        ui.tick(silent(t), ms(t, 10));
        assert_eq!(ui.mode(), Mode::Open { pressed: Some(t) });
    }

    #[test]
    fn the_popup_lingers_after_the_link_returns_then_closes() {
        let t = Instant::now();
        let mut ui = Ui::new();
        ui.tick(silent(t), ms(t, 2_000));
        let back = ms(t, 4_200);
        assert_eq!(
            ui.tick(Phase::Live, back),
            Some(LinkChange::Back {
                outage: Duration::from_millis(4_200)
            }),
            "the outage was not measured from when the host went quiet"
        );
        assert_eq!(
            ui.mode(),
            Mode::Lingering {
                since: back,
                outage: Duration::from_millis(4_200)
            }
        );
        ui.tick(Phase::Live, back + LINGER - Duration::from_millis(1));
        assert!(ui.visible(Phase::Live), "closed early");
        ui.tick(Phase::Live, back + LINGER);
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn a_new_outage_while_lingering_reopens_it_as_an_outage() {
        let t = Instant::now();
        let mut ui = lingering_at(t);
        assert_eq!(
            ui.tick(silent(ms(t, 200)), ms(t, 2_200)),
            Some(LinkChange::WentSilent)
        );
        assert_eq!(ui.mode(), Mode::Auto);
    }

    /// Typing into the popup an outage opened goes nowhere: not held, not
    /// sent, and the popup stays up.
    #[test]
    fn typing_into_the_outage_popup_is_neither_held_nor_sent() {
        let t = Instant::now();
        for phase in [silent(t), recovering(t)] {
            let mut ui = auto_at(t);
            assert_eq!(
                ui.keys(b"make test\r", phase, t),
                Routed::default(),
                "{phase:?}"
            );
            assert!(ui.visible(phase), "the popup closed under {phase:?}");
        }
    }

    /// The popup a user touched does not close on its own afterwards.
    #[test]
    fn a_key_typed_during_the_outage_keeps_the_popup_open_after_it() {
        let t = Instant::now();
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"x", silent(t), t), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
        ui.tick(Phase::Live, ms(t, 100));
        ui.tick(Phase::Live, ms(t, 100) + LINGER);
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
    }

    #[test]
    fn a_lingering_popup_is_kept_by_any_key_and_closed_by_its_own() {
        let t = Instant::now();
        let mut ui = lingering_at(t);
        assert_eq!(
            ui.keys(b"pw", Phase::Live, ms(t, 200)),
            Routed::default(),
            "typing reached the host"
        );
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
        ui.tick(Phase::Live, ms(t, 100) + LINGER);
        assert_eq!(
            ui.mode(),
            Mode::Open { pressed: None },
            "a touched popup closed by itself"
        );

        let mut ui = lingering_at(t);
        assert_eq!(ui.keys(&[ESC], Phase::Live, ms(t, 200)), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);
    }

    // ---- an outage closed by hand --------------------------------------

    /// Once closed, what is typed is held exactly as before the popup
    /// existed -- `q`, `s`, `d`, `Esc` and arrow keys included: none of them
    /// is a command while the popup is closed.
    #[test]
    fn closing_the_outage_popup_holds_what_is_typed_after() {
        let t = Instant::now();
        for phase in [silent(t), recovering(t)] {
            let mut ui = dismissed_at(t);
            assert_eq!(
                ui.keys(b"squeue -d", phase, t),
                hold(b"squeue -d"),
                "{phase:?}"
            );
            assert_eq!(ui.keys(&[ESC], phase, t), hold(&[ESC]), "{phase:?}");
            assert_eq!(ui.keys(b"\x1b[A", phase, t), hold(b"\x1b[A"), "{phase:?}");
            assert!(!ui.visible(phase), "{phase:?}");
        }
    }

    /// Review focus 2. A popup closed during an outage stays closed for the
    /// rest of that outage, `Recovering` included, and the next outage opens
    /// it again.
    #[test]
    fn a_closed_outage_does_not_reopen_itself_until_a_new_one() {
        let t = Instant::now();
        let mut ui = dismissed_at(t);
        assert_eq!(ui.tick(silent(t), ms(t, 2_500)), None);
        assert_eq!(
            ui.mode(),
            Mode::Closed,
            "the popup re-opened for the outage it was closed in"
        );
        assert_eq!(ui.tick(recovering(ms(t, 9_000)), ms(t, 9_000)), None);
        assert_eq!(ui.mode(), Mode::Closed, "Recovering re-opened it");

        assert_eq!(
            ui.tick(Phase::Live, ms(t, 10_000)),
            Some(LinkChange::Back {
                outage: Duration::from_millis(10_000)
            })
        );
        assert_eq!(ui.mode(), Mode::Closed, "a closed popup lingered");

        assert_eq!(
            ui.tick(silent(ms(t, 10_500)), ms(t, 12_500)),
            Some(LinkChange::WentSilent)
        );
        assert_eq!(
            ui.mode(),
            Mode::Auto,
            "the next outage did not open the popup"
        );
    }

    #[test]
    fn ctrl_backslash_reopens_a_closed_outage_and_closing_again_keeps_it_closed() {
        let t = Instant::now();
        let mut ui = dismissed_at(t);
        assert_eq!(
            ui.keys(&[PREFIX], silent(t), ms(t, 1_000)),
            Routed::default()
        );
        assert_eq!(
            ui.mode(),
            Mode::Open {
                pressed: Some(ms(t, 1_000))
            }
        );
        assert_eq!(
            ui.keys(b"abc", silent(t), ms(t, 1_200)),
            Routed::default(),
            "typing into it was kept"
        );

        assert_eq!(
            ui.keys(&[PREFIX], silent(t), ms(t, 2_000)),
            Routed::default()
        );
        assert_eq!(ui.mode(), Mode::Closed);
        ui.tick(silent(t), ms(t, 2_100));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    /// The literal a double press stands for is typing, and typing during
    /// an outage is held.
    #[test]
    fn a_double_press_during_an_outage_holds_one_literal() {
        let t = Instant::now();
        let mut ui = dismissed_at(t);
        assert_eq!(ui.keys(&[PREFIX, PREFIX], silent(t), t), hold(&[PREFIX]));
        assert_eq!(ui.mode(), Mode::Closed);
        ui.tick(silent(t), ms(t, 100));
        assert_eq!(
            ui.mode(),
            Mode::Closed,
            "the double press un-dismissed the outage"
        );
    }

    /// Review focus 3. Typing held behind a hand-closed popup is asked
    /// about when the link answers, and the question takes `s` and `d`.
    #[test]
    fn held_input_after_a_closed_outage_is_asked_about() {
        let t = Instant::now();
        let mut ui = dismissed_at(t);
        assert_eq!(ui.keys(b"make test\r", silent(t), t), hold(b"make test\r"));

        assert!(ui.tick(Phase::Confirming, ms(t, 4_000)).is_some());
        assert!(
            ui.visible(Phase::Confirming),
            "the question did not open the popup"
        );
        assert_eq!(
            ui.keys(b"x", Phase::Confirming, ms(t, 4_100)),
            Routed::default(),
            "typing into the question was kept"
        );
        assert_eq!(
            ui.keys(b"s", Phase::Confirming, ms(t, 4_500)),
            command(Command::SendHeld)
        );

        ui.tick(Phase::Live, ms(t, 4_600));
        assert!(!ui.visible(Phase::Live), "the answered question stayed up");
    }

    // ---- the popup's settings -------------------------------------------

    const CTRL_BRACKET: u8 = 0x1d;

    fn tuned(key: Option<u8>, linger: Duration, auto: Option<Duration>) -> Ui {
        let mut ui = Ui::new();
        ui.retune(key, linger, auto);
        ui
    }

    /// `popup.key = ctrl-]`: 0x1d opens the popup, and `Ctrl-\` is typing.
    #[test]
    fn the_configured_key_opens_the_popup_and_ctrl_backslash_is_typing() {
        let t = Instant::now();
        let mut ui = tuned(Some(CTRL_BRACKET), LINGER, Some(Duration::from_secs(2)));
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), host(&[PREFIX]));
        assert_eq!(ui.mode(), Mode::Closed);
        assert_eq!(ui.keys(&[CTRL_BRACKET], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: Some(t) });
        assert_eq!(
            ui.keys(&[CTRL_BRACKET], Phase::Live, ms(t, 2_000)),
            Routed::default()
        );
        assert_eq!(
            ui.mode(),
            Mode::Closed,
            "the configured key did not close it"
        );
    }

    /// Review focus 3. The double press belongs to the configured key: two
    /// quick `Ctrl-]` send one literal `Ctrl-]`, not a `Ctrl-\`.
    #[test]
    fn a_double_press_of_the_configured_key_sends_one_literal_of_it() {
        let t = Instant::now();
        let mut ui = tuned(Some(CTRL_BRACKET), LINGER, Some(Duration::from_secs(2)));
        assert_eq!(
            ui.keys(&[CTRL_BRACKET, CTRL_BRACKET], Phase::Live, t),
            host(&[CTRL_BRACKET])
        );
        assert_eq!(ui.mode(), Mode::Closed);
        let mut ui = tuned(Some(CTRL_BRACKET), LINGER, Some(Duration::from_secs(2)));
        ui.keys(&[CTRL_BRACKET], Phase::Live, t);
        assert_eq!(
            ui.keys(&[PREFIX], Phase::Live, ms(t, 100)),
            Routed::default()
        );
        assert!(
            matches!(ui.mode(), Mode::Open { .. }),
            "Ctrl-\\ still closes it"
        );
    }

    /// `popup.key = "off"`: the old key is typing, and Esc still closes a
    /// popup the outage opened.
    #[test]
    fn with_no_key_the_old_prefix_is_typing_and_esc_still_closes() {
        let t = Instant::now();
        let mut ui = tuned(None, LINGER, Some(Duration::ZERO));
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), host(&[PREFIX]));
        assert!(!ui.visible(Phase::Live));
        ui.tick(silent(t), ms(t, 2_000));
        assert_eq!(ui.mode(), Mode::Auto);
        assert_eq!(
            ui.keys(&[PREFIX], silent(t), ms(t, 2_100)),
            Routed::default()
        );
        assert_eq!(
            ui.mode(),
            Mode::Open { pressed: None },
            "a key that is not one only touches"
        );
        assert_eq!(ui.keys(&[ESC], silent(t), ms(t, 2_200)), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);
    }

    /// `auto_open_after = 25s`: an outage two seconds old does not open the
    /// popup; one twenty-five seconds old does, counted from when the host
    /// went quiet.
    #[test]
    fn the_popup_opens_by_itself_once_the_silence_reaches_auto_open_after() {
        let t = Instant::now();
        let mut ui = tuned(Some(PREFIX), LINGER, Some(Duration::from_secs(25)));
        assert_eq!(
            ui.tick(silent(t), ms(t, 2_000)),
            Some(LinkChange::WentSilent)
        );
        assert_eq!(ui.mode(), Mode::Closed, "opened before auto_open_after");
        ui.tick(recovering(ms(t, 20_000)), ms(t, 24_999));
        assert_eq!(ui.mode(), Mode::Closed);
        ui.tick(recovering(ms(t, 20_000)), ms(t, 25_000));
        assert_eq!(ui.mode(), Mode::Auto);
    }

    /// `auto_open_after = "off"`: no outage opens it, and the key still does.
    #[test]
    fn with_auto_open_off_no_outage_opens_the_popup() {
        let t = Instant::now();
        let mut ui = tuned(Some(PREFIX), LINGER, None);
        ui.tick(silent(t), ms(t, 2_000));
        ui.tick(recovering(t), ms(t, 600_000));
        assert_eq!(ui.mode(), Mode::Closed);
        assert_eq!(
            ui.keys(&[PREFIX], recovering(t), ms(t, 600_001)),
            Routed::default()
        );
        assert!(matches!(ui.mode(), Mode::Open { .. }));
    }

    #[test]
    fn the_popup_lingers_for_the_configured_time() {
        let t = Instant::now();
        let mut ui = tuned(Some(PREFIX), Duration::from_secs(10), Some(Duration::ZERO));
        ui.tick(silent(t), ms(t, 2_000));
        let back = ms(t, 4_000);
        ui.tick(Phase::Live, back);
        ui.tick(Phase::Live, back + LINGER);
        assert!(
            matches!(ui.mode(), Mode::Lingering { .. }),
            "closed at the old LINGER"
        );
        ui.tick(Phase::Live, back + Duration::from_secs(10));
        assert_eq!(ui.mode(), Mode::Closed);
    }

    // ---- the config screen ----------------------------------------------

    use crate::config::{Level, SETTINGS};

    fn config_at(t: Instant) -> Ui {
        let mut ui = open_at(t);
        assert_eq!(ui.keys(b"c", Phase::Live, t), Routed::default());
        assert_eq!(
            ui.mode(),
            Mode::Config {
                cursor: 0,
                editing: Editing::None
            }
        );
        ui
    }

    fn cfg(c: ConfigCmd) -> Routed {
        command(Command::Config(c))
    }

    fn editing(ui: &Ui) -> Editing {
        ui.config_screen().expect("the config screen is up").editing
    }

    fn cursor(ui: &Ui) -> usize {
        ui.config_screen().expect("the config screen is up").cursor
    }

    #[test]
    fn c_opens_the_config_screen_from_the_status_view() {
        let t = Instant::now();
        config_at(t);
        let mut ui = auto_at(t);
        assert_eq!(ui.keys(b"c", silent(t), t), Routed::default());
        assert!(ui.config_screen().is_some(), "not from the outage's popup");
        assert!(ui.visible(silent(t)));
    }

    /// `c` pressed after this outage's moment to open by itself has
    /// passed: there is no moment left to give way at, so the next lap
    /// leaves the config screen up.
    #[test]
    fn a_config_screen_opened_late_in_an_outage_stays() {
        let t = Instant::now();
        let mut ui = tuned(Some(PREFIX), LINGER, Some(Duration::from_secs(5)));
        ui.tick(silent(t), ms(t, 5_000));
        assert_eq!(ui.mode(), Mode::Auto);
        assert_eq!(ui.keys(b"c", silent(t), ms(t, 6_000)), Routed::default());
        assert!(ui.config_screen().is_some(), "c did not open it");
        ui.tick(silent(t), ms(t, 7_000));
        assert!(
            ui.config_screen().is_some(),
            "the config screen gave way to a moment already past: {:?}",
            ui.mode()
        );
    }

    #[test]
    fn c_does_nothing_under_confirming() {
        let t = Instant::now();
        let mut ui = confirming_at(t);
        assert_eq!(
            ui.keys(b"c", Phase::Confirming, t + ANSWER_GUARD),
            Routed::default()
        );
        assert!(ui.config_screen().is_none());
    }

    #[test]
    fn arrows_in_both_encodings_and_k_and_j_move_the_cursor_within_the_list() {
        let t = Instant::now();
        let mut ui = config_at(t);
        for (keys, want) in [
            (&b"\x1b[B"[..], 1),
            (b"\x1bOB", 2),
            (b"\x1b[A", 1),
            (b"j", 2),
            (b"k", 1),
            (b"kkk", 0),
            (b"\x1b[B\x1b[B", 2),
        ] {
            assert_eq!(ui.keys(keys, Phase::Live, t), Routed::default(), "{keys:?}");
            assert_eq!(cursor(&ui), want, "{keys:?}");
        }
        for _ in 0..SETTINGS.len() + 3 {
            ui.keys(b"j", Phase::Live, t);
        }
        assert_eq!(cursor(&ui), SETTINGS.len() - 1);
    }

    /// Other escape sequences are skipped whole, and what follows them in
    /// the read is still read.
    #[test]
    fn other_escape_sequences_are_skipped_whole() {
        let t = Instant::now();
        let mut ui = config_at(t);
        assert_eq!(
            ui.keys(b"\x1b[1;5Cj\x1bOqj", Phase::Live, t),
            Routed::default()
        );
        assert_eq!(cursor(&ui), 2, "the q of ESC O q quit, or a j was lost");
    }

    #[test]
    fn enter_x_and_w_ask_the_session() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.keys(b"jj", Phase::Live, t);
        assert_eq!(ui.keys(b"\r", Phase::Live, t), cfg(ConfigCmd::Edit(2)));
        assert_eq!(ui.keys(b"x", Phase::Live, t), cfg(ConfigCmd::Reset(2)));
        assert_eq!(ui.keys(b"w", Phase::Live, t), Routed::default());
        assert_eq!(editing(&ui), Editing::Save);
        assert_eq!(
            ui.keys(b"q", Phase::Live, t),
            Routed::default(),
            "q under the save question"
        );
        assert_eq!(
            ui.keys(b"a", Phase::Live, t),
            cfg(ConfigCmd::Save(Level::Global))
        );
        ui.accepted();
        ui.keys(b"w", Phase::Live, t);
        assert_eq!(
            ui.keys(b"h", Phase::Live, t),
            cfg(ConfigCmd::Save(Level::Host))
        );
        ui.accepted();
        ui.keys(b"w", Phase::Live, t);
        assert_eq!(ui.keys(&[ESC], Phase::Live, t), Routed::default());
        assert_eq!(editing(&ui), Editing::None);
    }

    #[test]
    fn esc_goes_back_to_the_status_view_and_q_quits() {
        let t = Instant::now();
        let mut ui = config_at(t);
        assert_eq!(ui.keys(&[ESC], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
        let mut ui = config_at(t);
        assert_eq!(ui.keys(b"q", Phase::Live, t), command(Command::Quit));
        let mut ui = config_at(t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed, "the popup key closes the popup");
    }

    #[test]
    fn a_text_field_takes_every_byte_until_enter() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text("20s".to_string());
        assert_eq!(
            ui.keys(b"\x7f\x7f\x7f30sqxwkj", Phase::Live, t),
            Routed::default()
        );
        assert_eq!(ui.config_screen().unwrap().field, "30sqxwkj");
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(
            editing(&ui),
            Editing::Text,
            "the popup key closed the field"
        );
        assert_eq!(ui.keys(b"\x1b[A", Phase::Live, t), Routed::default());
        assert_eq!(cursor(&ui), 0, "an arrow moved the list under the field");
        for _ in 0..5 {
            ui.keys(&[0x08], Phase::Live, t);
        }
        assert_eq!(
            ui.keys(b"\r", Phase::Live, t),
            cfg(ConfigCmd::Accept {
                row: 0,
                text: "30s".to_string()
            })
        );
        assert_eq!(
            editing(&ui),
            Editing::Text,
            "the field closed before the session answered"
        );
        ui.accepted();
        assert_eq!(editing(&ui), Editing::None);
    }

    #[test]
    fn esc_cancels_a_field_and_backspace_takes_one_character() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text(String::new());
        ui.keys("1müx".as_bytes(), Phase::Live, t);
        ui.keys(b"\x7f\x7f", Phase::Live, t);
        assert_eq!(ui.config_screen().unwrap().field, "1m");
        // A character split across two reads arrives whole.
        let u = "ü".as_bytes();
        ui.keys(&u[..1], Phase::Live, t);
        ui.keys(&u[1..], Phase::Live, t);
        assert_eq!(ui.config_screen().unwrap().field, "1mü");
        assert_eq!(ui.keys(&[ESC], Phase::Live, t), Routed::default());
        assert_eq!(editing(&ui), Editing::None);
        assert_eq!(ui.config_screen().unwrap().field, "");
    }

    #[test]
    fn a_refused_value_keeps_the_field_open_with_the_reason() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text("1.5s".to_string());
        ui.keys(b"\r", Phase::Live, t);
        ui.say("1.5s is not a whole number of seconds".to_string());
        let screen = ui.config_screen().unwrap();
        assert_eq!(screen.editing, Editing::Text);
        assert_eq!(screen.field, "1.5s");
        assert_eq!(screen.note, Some("1.5s is not a whole number of seconds"));
        ui.keys(b"\x7f", Phase::Live, t);
        assert_eq!(
            ui.config_screen().unwrap().note,
            None,
            "the reason outlived the next key"
        );
    }

    /// A newline in an unbracketed paste accepts the field, and the rest of
    /// the read is dropped: its `q` does not quit.
    #[test]
    fn a_newline_in_an_unbracketed_paste_accepts_and_drops_the_rest() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text(String::new());
        assert_eq!(
            ui.keys(b"45s\rq", Phase::Live, t),
            cfg(ConfigCmd::Accept {
                row: 0,
                text: "45s".to_string()
            })
        );
    }

    #[test]
    fn a_bracketed_paste_goes_into_the_field_without_wrappers_or_controls() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text(String::new());
        assert_eq!(
            ui.keys(b"\x1b[200~4\x1b5\ts\r\x1b[201~", Phase::Live, t),
            Routed::default()
        );
        assert_eq!(ui.config_screen().unwrap().field, "45s");
        // Split across reads: the middle read is all paste, `q` included.
        ui.open_text(String::new());
        ui.keys(b"\x1b[200~1m", Phase::Live, t);
        assert_eq!(ui.keys(b" q", Phase::Live, t), Routed::default());
        assert_eq!(
            ui.keys(b"\x1b[201~\r", Phase::Live, t),
            cfg(ConfigCmd::Accept {
                row: 0,
                text: "1m q".to_string()
            })
        );
    }

    #[test]
    fn a_paste_with_no_field_open_is_ignored() {
        let t = Instant::now();
        let mut ui = config_at(t);
        assert_eq!(
            ui.keys(b"\x1b[200~q\rx\x1b[201~", Phase::Live, t),
            Routed::default()
        );
        assert_eq!(
            ui.mode(),
            Mode::Config {
                cursor: 0,
                editing: Editing::None
            }
        );
    }

    #[test]
    fn key_capture_takes_the_next_key_whatever_it_is() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_capture();
        assert_eq!(
            ui.keys(&[PREFIX], Phase::Live, t),
            cfg(ConfigCmd::Capture {
                row: 0,
                key: Some(PREFIX)
            }),
            "the popup key itself can be captured"
        );
        assert_eq!(
            ui.keys(b"\r", Phase::Live, t),
            cfg(ConfigCmd::Capture {
                row: 0,
                key: Some(b'\r')
            }),
            "Enter is captured, for the session to refuse"
        );
        assert_eq!(
            ui.keys(b"q", Phase::Live, t),
            cfg(ConfigCmd::Capture {
                row: 0,
                key: Some(b'q')
            })
        );
        assert_eq!(
            ui.keys(&[0x7f], Phase::Live, t),
            cfg(ConfigCmd::Capture { row: 0, key: None })
        );
        assert_eq!(ui.keys(&[ESC], Phase::Live, t), Routed::default());
        assert_eq!(
            editing(&ui),
            Editing::None,
            "Esc did not cancel the capture"
        );
    }

    #[test]
    fn the_servers_sub_list_edits_adds_and_removes() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_servers(vec!["a:1".to_string(), "b:2".to_string()]);
        ui.keys(b"j", Phase::Live, t);
        assert_eq!(
            ui.keys(b"-", Phase::Live, t),
            cfg(ConfigCmd::Servers {
                row: 0,
                list: vec!["a:1".to_string()]
            })
        );
        ui.accepted();
        assert_eq!(ui.config_screen().unwrap().servers, ["a:1"]);
        assert_eq!(
            editing(&ui),
            Editing::Servers {
                cursor: 0,
                field: false
            }
        );

        ui.keys(b"+", Phase::Live, t);
        assert_eq!(
            editing(&ui),
            Editing::Servers {
                cursor: 1,
                field: true
            }
        );
        assert_eq!(
            ui.keys(b"c:3\r", Phase::Live, t),
            cfg(ConfigCmd::Servers {
                row: 0,
                list: vec!["a:1".to_string(), "c:3".to_string()]
            })
        );
        ui.say("refused".to_string());
        assert_eq!(
            ui.config_screen().unwrap().servers,
            ["a:1"],
            "a refused list was kept"
        );
        assert_eq!(
            editing(&ui),
            Editing::Servers {
                cursor: 1,
                field: true
            }
        );
        ui.keys(&[ESC], Phase::Live, t);
        assert_eq!(
            editing(&ui),
            Editing::Servers {
                cursor: 0,
                field: false
            }
        );

        ui.keys(b"\r", Phase::Live, t);
        assert_eq!(ui.config_screen().unwrap().field, "a:1");
        assert_eq!(
            ui.keys(b"\x7f9\r", Phase::Live, t),
            cfg(ConfigCmd::Servers {
                row: 0,
                list: vec!["a:9".to_string()]
            })
        );
        ui.accepted();
        ui.keys(&[ESC], Phase::Live, t);
        assert_eq!(editing(&ui), Editing::None);
    }

    /// A large paste is cut into reads wherever the buffer ends, its end
    /// marker too: the end is still found, and what follows it is read.
    #[test]
    fn a_paste_end_split_across_reads_still_ends_the_paste() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text(String::new());
        assert_eq!(
            ui.keys(b"\x1b[200~ab\x1b[20", Phase::Live, t),
            Routed::default()
        );
        assert_eq!(
            ui.keys(b"1~\r", Phase::Live, t),
            cfg(ConfigCmd::Accept {
                row: 0,
                text: "ab".to_string()
            })
        );
        // What looked like the end's beginning was text after all.
        ui.open_text(String::new());
        ui.keys(b"\x1b[200~a\x1b[2", Phase::Live, t);
        ui.keys(b"x\x1b[201~", Phase::Live, t);
        assert_eq!(ui.config_screen().unwrap().field, "a[2x");
    }

    /// With no field open the paste is ignored, but its end is still found
    /// across reads, and the screen's keys work after it.
    #[test]
    fn a_split_paste_end_with_no_field_open_does_not_swallow_the_keys() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.keys(b"\x1b[200~ab\x1b[20", Phase::Live, t);
        assert_eq!(ui.keys(b"1~j", Phase::Live, t), Routed::default());
        assert_eq!(cursor(&ui), 1, "the j after the paste was swallowed");
        assert_eq!(ui.keys(&[ESC], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
    }

    /// Paste tracking is the config screen's: with the popup closed a
    /// bracketed paste is the host's, byte for byte, split end and all --
    /// or held, during an outage.
    #[test]
    fn a_paste_with_the_popup_closed_goes_where_typing_goes() {
        let t = Instant::now();
        let first = b"\x1b[200~ls -l\x1b[20";
        let second = b"1~\r";
        let mut ui = Ui::new();
        assert_eq!(ui.keys(first, Phase::Live, t).to_host, first);
        assert_eq!(ui.keys(second, Phase::Live, t).to_host, second);
        let mut ui = dismissed_at(t);
        assert_eq!(ui.keys(first, silent(t), t).to_hold, first);
        assert_eq!(ui.keys(second, silent(t), t).to_hold, second);
        // An unended paste on the config screen does not follow the user
        // out of it.
        let mut ui = config_at(t);
        ui.keys(b"\x1b[200~ab", Phase::Live, t);
        ui.mode = Mode::Closed;
        assert_eq!(ui.keys(b"ls\r", Phase::Live, t).to_host, b"ls\r");
    }

    // ---- the config screen and the link (spec §4.2) ----------------------

    /// Under default tuning the popup would open at the first outage lap;
    /// these use a threshold so the moment can be stepped over.
    fn config_tuned(t: Instant, auto: Option<Duration>) -> Ui {
        let mut ui = config_at(t);
        ui.retune(Some(PREFIX), LINGER, auto);
        ui
    }

    #[test]
    fn an_outage_with_no_field_open_switches_to_the_status_view_when_auto_open_fires() {
        let t = Instant::now();
        let mut ui = config_tuned(t, Some(Duration::from_secs(5)));
        ui.tick(silent(t), ms(t, 2_000));
        assert!(
            ui.config_screen().is_some(),
            "switched before auto_open_after"
        );
        ui.tick(silent(t), ms(t, 5_000));
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
        // `Open`, not `Auto`: it does not linger away when the link is back.
        ui.tick(Phase::Live, ms(t, 6_000));
        ui.tick(Phase::Live, ms(t, 60_000));
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
    }

    #[test]
    fn an_outage_with_a_field_open_keeps_the_screen_for_the_rest_of_it() {
        let t = Instant::now();
        let mut ui = config_tuned(t, Some(Duration::from_secs(5)));
        ui.open_text("20s".to_string());
        ui.tick(silent(t), ms(t, 5_000));
        assert_eq!(editing(&ui), Editing::Text);
        // Closing the field later in the same outage does not switch.
        ui.keys(&[ESC], silent(t), ms(t, 6_000));
        ui.tick(silent(t), ms(t, 7_000));
        assert!(
            ui.config_screen().is_some(),
            "switched after the moment had passed"
        );
        // The next outage decides again.
        ui.tick(Phase::Live, ms(t, 8_000));
        ui.tick(silent(ms(t, 9_000)), ms(t, 14_000));
        assert_eq!(ui.mode(), Mode::Open { pressed: None });
    }

    #[test]
    fn with_auto_open_off_an_outage_never_switches() {
        let t = Instant::now();
        let mut ui = config_tuned(t, None);
        ui.tick(silent(t), ms(t, 2_000));
        ui.tick(recovering(t), ms(t, 600_000));
        assert!(ui.config_screen().is_some());
    }

    /// The question takes over, from a lap or from a read, and an open
    /// field goes with it: its `s` and `d` never land in a field.
    #[test]
    fn confirming_always_switches_and_drops_the_field() {
        let t = Instant::now();
        let mut ui = config_at(t);
        ui.open_text("2".to_string());
        ui.tick(Phase::Confirming, ms(t, 1_000));
        assert!(ui.config_screen().is_none());
        assert!(ui.visible(Phase::Confirming));

        let mut ui = config_at(t);
        ui.open_text("2".to_string());
        assert_eq!(
            ui.keys(b"s", Phase::Confirming, t),
            Routed::default(),
            "inside the guard"
        );
        assert!(
            ui.config_screen().is_none(),
            "the read under Confirming typed into the field"
        );
        assert_eq!(
            ui.keys(b"s", Phase::Confirming, t + ANSWER_GUARD),
            command(Command::SendHeld)
        );
    }

    // ---- the session selector -------------------------------------------

    fn sessions_at(t: Instant) -> Ui {
        let mut ui = open_at(t);
        ui.keys(b"s", Phase::Live, t);
        ui.selector_mut()
            .set_rows(crate::selector::fixtures::three());
        ui
    }

    #[test]
    fn the_selectors_keys_are_its_own_and_q_goes_back_to_the_popup() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        assert_eq!(
            ui.keys(b"j\r", Phase::Live, t),
            command(Command::Ask(crate::switcher::Ask::Switch {
                to: crate::selector::fixtures::LOGS.parse().unwrap()
            }))
        );
        assert_eq!(ui.keys(b"q", Phase::Live, t), Routed::default());
        assert!(matches!(ui.mode(), Mode::Open { .. }));
    }

    #[test]
    fn in_a_lobby_q_quits_and_the_popup_key_does_not_close_it() {
        let t = Instant::now();
        let mut ui = Ui::new();
        ui.set_switcher(true, false);
        ui.open_sessions();
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(
            ui.mode(),
            Mode::Sessions,
            "nothing behind a lobby's selector"
        );
        assert_eq!(ui.keys(b"q", Phase::Live, t), command(Command::Quit));
    }

    #[test]
    fn the_popup_key_closes_the_selector_in_a_session() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed);
    }

    #[test]
    fn the_selector_stays_through_an_outage_and_refuses_its_actions() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        ui.tick(silent(t), t);
        ui.tick(silent(t), ms(t, 5_000));
        assert_eq!(ui.mode(), Mode::Sessions, "an outage does not take it down");
        assert_eq!(ui.keys(b"n", silent(t), ms(t, 5_000)), Routed::default());
        assert_eq!(ui.selector().note(), Some(crate::selector::NOT_LIVE));
    }

    #[test]
    fn a_request_in_flight_refuses_the_next() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        ui.set_switcher(false, true);
        assert_eq!(ui.keys(b"n", Phase::Live, t), Routed::default());
        assert_eq!(ui.selector().note(), Some(crate::selector::BUSY));
    }

    #[test]
    fn the_held_input_question_takes_the_selector_down() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        ui.tick(Phase::Confirming, t);
        assert!(matches!(ui.mode(), Mode::Open { .. }));
    }

    #[test]
    fn closing_the_popup_from_the_session_closes_whatever_it_shows() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        ui.close_popup();
        assert_eq!(ui.mode(), Mode::Closed);
    }

    /// The popup key while a name is being typed: it neither closes the
    /// selector under the field nor ends up in the name.
    #[test]
    fn the_popup_key_while_renaming_is_neither_a_close_nor_a_character() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        ui.keys(b"r", Phase::Live, t);
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Sessions);
        assert_eq!(ui.selector().rename(), Some("build"));
    }

    /// A paste on the selector is text, never keys: an `n` in it asks for
    /// nothing, and the popup key in it, at the start of a later read of
    /// the same paste, does not close the selector.
    #[test]
    fn a_paste_on_the_selector_is_neither_a_command_nor_a_close() {
        let t = Instant::now();
        let mut ui = sessions_at(t);
        assert_eq!(ui.keys(b"\x1b[200~n\r", Phase::Live, t), Routed::default());
        assert_eq!(ui.keys(&[PREFIX, b'j'], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Sessions);
        assert_eq!(ui.keys(b"\x1b[201~", Phase::Live, t), Routed::default());
        assert_eq!(ui.selector().cursor(), 1, "nothing in the paste moved it");
        assert_eq!(ui.keys(&[PREFIX], Phase::Live, t), Routed::default());
        assert_eq!(ui.mode(), Mode::Closed, "after the paste, the key closes");
    }
}

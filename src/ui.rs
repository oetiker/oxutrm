//! The status popup's modal state, and where every keystroke goes.
//!
//! Pure, like `linkstate`: every method takes the current `Instant`, so the
//! whole machine is tested without sleeping, and nothing here holds a timer.
//! The loop calls [`Ui::tick`] on laps it already takes.
//!
//! The rule that decides the keys: **while the popup is shown, every key
//! is its own.** Nothing typed into it is held or sent, whatever opened it
//! and whatever the link is doing. While it is closed, `Ctrl-\` opens it and
//! every other byte goes to the host -- or, during an outage, is held for the
//! question asked when the link answers again.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant};

use crate::linkstate::Phase;

/// `Ctrl-\`. Opens the popup while it is closed, in every phase; closes it
/// while it is shown.
pub(crate) const PREFIX: u8 = 0x1c;

/// A read that is exactly this byte is the Esc key. Anywhere else in a read
/// it begins an escape sequence -- an arrow key, a function key -- which
/// runs to the end of the read.
pub(crate) const ESC: u8 = 0x1b;

/// How soon a second `Ctrl-\` must follow the one that opened the popup for
/// the pair to mean one literal `Ctrl-\` for the host. Measured between the
/// two reads' timestamps; nothing is armed.
pub(crate) const DOUBLE_PRESS: Duration = Duration::from_millis(500);

/// How long the question about held input must have been on the screen
/// before `s`, `d` or `q` answer it. Typing that was already in flight when
/// the link came back would otherwise send, drop or quit unseen. Measured
/// from the first lap or read that saw `Confirming` to the read's own
/// timestamp; nothing is armed.
pub(crate) const ANSWER_GUARD: Duration = Duration::from_millis(500);

/// How long the popup stays up showing the outcome after the link it opened
/// for comes back.
pub(crate) const LINGER: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mode {
    Closed,
    /// Opened by hand, or touched by a key; stays until closed.
    Open {
        /// When the `Ctrl-\` that opened it arrived, for the double press.
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
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Command {
    Quit,
    SendHeld,
    DropHeld,
}

/// Where one read's bytes go. A command ends the read: anything after it
/// belongs to whatever the command leads to, not to the buffer before it.
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
}

impl Ui {
    pub(crate) fn new() -> Ui {
        Ui {
            mode: Mode::Closed,
            dismissed: false,
            outage_since: None,
            last_outage: Duration::ZERO,
            asked_at: None,
        }
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
                Some(LinkChange::WentSilent)
            } else {
                None
            };
            match self.mode {
                Mode::Closed if !self.dismissed => self.mode = Mode::Auto,
                Mode::Lingering { .. } => self.mode = Mode::Auto,
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
            Mode::Lingering { since, .. } if now.saturating_duration_since(since) >= LINGER => {
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
        let mut routed = Routed::default();
        let lone_esc = bytes == [ESC];
        let mut rest = bytes;
        while let Some((&b, tail)) = rest.split_first() {
            rest = tail;
            if !self.visible(phase) {
                if b == PREFIX {
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
        match b {
            // The question about held input has to be answered before the
            // popup can go.
            PREFIX | ESC if !confirming => self.close(b, phase, now, r),
            // `c` and `s` are offered by a later version; they, and every
            // other key, only count as touching the popup.
            _ => self.touch(),
        }
        false
    }

    /// Close the popup with `b`, `Esc` or `Ctrl-\`.
    fn close(&mut self, b: u8, phase: Phase, now: Instant, r: &mut Routed) {
        if b == PREFIX
            && let Mode::Open { pressed: Some(at) } = self.mode
            && now.saturating_duration_since(at) < DOUBLE_PRESS
        {
            deliver(PREFIX, phase, r);
        }
        self.mode = Mode::Closed;
        if phase.is_outage() {
            self.dismissed = true;
        }
    }

    /// A key that does nothing still tells an outage's popup that someone is
    /// looking at it: it stays until closed.
    fn touch(&mut self) {
        if matches!(self.mode, Mode::Auto | Mode::Lingering { .. }) {
            self.mode = Mode::Open { pressed: None };
        }
    }
}

/// Where a byte typed with the popup closed goes: the host while the link
/// answers, the held buffer during an outage.
///
/// This is also the route of the literal a double press stands for. During an
/// outage that literal is held with the other typed bytes, and reaches the host
/// only through the `Confirming` answer. Under `Confirming` itself a quick
/// second `Ctrl-\` sends nothing, so no literal runs ahead of the held input.
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
        assert_eq!(
            ui.keys(b"ls -l\r", Phase::Live, ms(t, 10)),
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
    fn c_and_s_are_offered_later_and_do_nothing_now() {
        let t = Instant::now();
        for key in *b"cs" {
            let mut ui = open_at(t);
            assert_eq!(ui.keys(&[key], Phase::Live, t), Routed::default());
            assert!(matches!(ui.mode(), Mode::Open { .. }), "{}", key as char);

            let mut ui = auto_at(t);
            assert_eq!(
                ui.keys(&[key], silent(t), t),
                Routed::default(),
                "{} under an outage",
                key as char
            );
        }
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
            assert_eq!(open_at(t).keys(&[key], Phase::Live, t), Routed::default());
        }
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
                command(want),
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
            ui.keys(b"ls", Phase::Live, ms(t, 200)),
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
}

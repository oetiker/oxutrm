//! Whether the host is still answering, and what the user is told about it.
//!
//! Pure: no I/O, no terminal, no runtime. Every method takes the current
//! `Instant` as a parameter rather than reading the clock, which is what lets
//! the whole state machine be tested without sleeping.

use std::time::{Duration, Instant};

/// How long a reply may be owed before the user is told. Below this a blip
/// resolves without ever painting: an indicator that fires on every hiccup is
/// the noise it was built to remove.
pub const SILENT_AFTER: Duration = Duration::from_secs(2);

/// How long a session may be completely quiet before the client says something
/// merely to see whether anyone is still there.
///
/// **0.2 Hz.** Set that against the 250 Hz poll removed in `19cc001`, and note
/// that it applies only to an ATTACHED client: a detached host session has no
/// client, so it has no heartbeat and its idle cost is unchanged.
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(5);

/// `Ctrl-\`. A prefix rather than a bare key, and live only while a notice is
/// showing: while the link is healthy every byte belongs to the host, which is
/// what keeps oxutrm out of the escape-character collisions Mosh must live
/// with.
const PREFIX: u8 = 0x1c;

/// How much blind typing is kept. Beyond this the buffer STOPS ACCEPTING; it
/// does not drop the oldest bytes, because the oldest are the command and the
/// newest are the newline, and discarding from the front is exactly how a
/// truncated command still runs.
pub const MAX_HELD: usize = 64 * 1024;

/// How much of the held input is shown before it is summarised.
const HELD_SHOWN: usize = 200;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Live,
    Silent { since: Instant },
    Confirming,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    Quit,
    SendHeld,
    DropHeld,
}

/// Where to cut `bytes` for display: at most `HELD_SHOWN` bytes, backed off
/// so the cut never lands inside a multi-byte UTF-8 sequence. A UTF-8
/// continuation byte matches `10xxxxxx`; stepping back while the byte right
/// after the cut is one lands the cut on a leading byte (or 0, or the end)
/// instead of splitting a character into a mangled fragment.
fn held_cut(bytes: &[u8]) -> usize {
    let mut cut = bytes.len().min(HELD_SHOWN);
    while cut > 0 && cut < bytes.len() && (bytes[cut] & 0xc0) == 0x80 {
        cut -= 1;
    }
    cut
}

/// Held input as something safe to put in a box.
///
/// Control bytes become readable rather than being emitted: the notice is
/// painted through the renderer, and a raw `\r` in a cell would be a control
/// scalar the receiver's validation rejects. This covers both C0
/// (0x00-0x1F, plus DEL) and C1 (U+0080-U+009F): U+009B is CSI, and
/// terminals in UTF-8 mode act on it, so a raw C1 scalar reaching the
/// renderer is untrusted input landing as a control sequence, the same class
/// of bug as an unescaped C0 byte.
///
/// The held buffer is raw terminal input, so it is decoded as UTF-8 rather
/// than mapped one byte to one scalar -- a character the user actually typed
/// is very often multi-byte, and byte-for-byte mapping turns it into
/// mojibake, which defeats the entire point of `Confirming`. Anything that
/// is not valid UTF-8 (the cap can cut a sequence short, and a user can type
/// any byte) becomes the replacement character rather than panicking or
/// vanishing.
pub fn render_held(bytes: &[u8]) -> String {
    let cut = held_cut(bytes);
    let shown = &bytes[..cut];
    let mut out = String::with_capacity(shown.len() + 16);

    for ch in String::from_utf8_lossy(shown).chars() {
        match ch {
            '\r' | '\n' => out.push('\u{21b5}'),
            '\u{0}'..='\u{1f}' => {
                out.push('^');
                out.push((ch as u8 + b'@') as char);
            }
            '\u{7f}' => out.push_str("^?"),
            '\u{80}'..='\u{9f}' => {
                out.push_str(&format!("<{:02X}>", ch as u32));
            }
            _ => out.push(ch),
        }
    }

    if bytes.len() > shown.len() {
        out.push_str(&format!(
            "  ...and {} more bytes",
            bytes.len() - shown.len()
        ));
    }
    out
}

/// What the client believes about the link, and why.
pub struct LinkState {
    phase: Phase,
    /// The last time anything at all arrived from the host.
    last_heard: Instant,
    /// When the currently outstanding reply started being owed, if one is.
    ///
    /// The grace period is measured from HERE and not from `last_heard`.
    /// `last_heard` only advances when something arrives, so on a quiet
    /// session it is arbitrarily old and every first lap with a reply owed
    /// looks like a two-second outage. See `evaluate`.
    owed_since: Option<Instant>,
    /// The last time we said anything, so a quiet link can be prodded.
    last_sent: Instant,
    /// Typed while not `Live`, and not delivered to anyone yet.
    held: Vec<u8>,
    /// The prefix arrived at the end of a read and its letter has not.
    prefix_pending: bool,
}

impl LinkState {
    pub fn new(now: Instant) -> LinkState {
        LinkState {
            phase: Phase::Live,
            last_heard: now,
            owed_since: None,
            last_sent: now,
            held: Vec::new(),
            prefix_pending: false,
        }
    }

    /// For this module's own tests, and only for them.
    ///
    /// The loop never asks: `evaluate` already returns the phase it decided,
    /// so a caller that asked separately would be reading a value one lap
    /// stale. `#[cfg(test)]` rather than `#[allow(dead_code)]` because that is
    /// the truth about it — a later phase that genuinely needs to read the
    /// state without advancing it can lift the attribute back off.
    #[cfg(test)]
    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// A frame arrived. Whatever we believed, the host is answering.
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
        // Whatever was owed has been answered. Anything owed from here starts
        // its own clock, or the grace period would be measured from a moment
        // the host has already replied to.
        self.owed_since = None;
        // A `Ctrl-\` whose letter never came belongs to the box that was up
        // when it was typed. Carrying it into the next one lets it eat the
        // first byte typed there -- and if that byte is `s`, the `Confirming`
        // box answers its own question and delivers typing nobody confirmed.
        self.prefix_pending = false;
        // Coming back with something typed blind is a question, not a
        // resumption. Delivering it silently would replay it against a screen
        // that moved while the user could not watch.
        self.phase = if self.held.is_empty() {
            Phase::Live
        } else {
            Phase::Confirming
        };
    }

    /// We sent something, so a reply is owed from here.
    pub fn sent(&mut self, now: Instant) {
        self.last_sent = now;
    }

    /// Nothing has been said in either direction for long enough that the
    /// caller should say something, purely so that an answer is owed.
    ///
    /// Without this an idle session cannot tell an outage from calm, and the
    /// user would find out by pressing a key into a screen that had been dead
    /// for ten minutes.
    pub fn heartbeat_due(&self, now: Instant) -> bool {
        let quiet_since = self.last_heard.max(self.last_sent);
        now.duration_since(quiet_since) >= HEARTBEAT_IDLE
    }

    /// One lap's worth of judgement.
    ///
    /// `reply_owed` is the caller's answer to "have we said something the host
    /// has not acknowledged". It is the whole signal: the sync engine sends an
    /// empty-diff frame purely to move an owed ack, so an unanswered input is a
    /// real round-trip failure rather than an inference. With nothing owed,
    /// silence is indistinguishable from calm and this reports `Live` --
    /// closing that gap is what the heartbeat is for.
    pub fn evaluate(&mut self, now: Instant, reply_owed: bool) -> Phase {
        if let Phase::Silent { .. } = self.phase {
            // Already told the user. The clock keeps running from `last_heard`;
            // recomputing it here would restart the counter every lap.
            return self.phase;
        }

        // When the owing STARTED, which is the thing the grace period is
        // about. `last_heard` cannot stand in for it: nothing arrives on a
        // quiet session, so `last_heard` is arbitrarily old and the first lap
        // after a keystroke would read as a two-second outage. Worse, the
        // heartbeat owes a reply every `HEARTBEAT_IDLE`, which is longer than
        // `SILENT_AFTER`, so every idle session would raise the notice every
        // five seconds for ever.
        match (reply_owed, self.owed_since) {
            (true, None) => self.owed_since = Some(now),
            // Answered. The next owing starts its own clock.
            (false, _) => self.owed_since = None,
            (true, Some(_)) => {}
        }

        if self
            .owed_since
            .is_some_and(|since| now.duration_since(since) >= SILENT_AFTER)
        {
            // `last_heard` and not `now`: the counter must report how long the
            // host has been quiet, not how long since we worked it out.
            //
            // It is deliberately NOT `owed_since` either, which would be a
            // different and smaller number -- the reply may have started being
            // owed long after the host went quiet. The consequence is that the
            // displayed figure can OVERSTATE the silence. Ordinarily by at
            // most `HEARTBEAT_IDLE`, because a heartbeat that is answered
            // refreshes `last_heard`.
            //
            // Ordinarily, and not always, and the exception is written down
            // because it is the kind of thing that gets rediscovered as a bug:
            // `ClientSession::take_frames` applies a frame the pacing tick
            // scavenged out of the channel without telling this module, so
            // `last_heard` does not move for it. That cannot raise a FALSE
            // notice -- the ack such a frame carries clears `owed_since` on
            // the same lap, which is the whole point of measuring the owing
            // instead of the silence -- but it can leave the counter in a real
            // outage reading higher than the truth.
            self.phase = Phase::Silent {
                since: self.last_heard,
            };
        }
        self.phase
    }

    pub fn held(&self) -> &[u8] {
        &self.held
    }

    pub fn held_is_full(&self) -> bool {
        self.held.len() >= MAX_HELD
    }

    /// Deliver the held input, emptying the buffer.
    ///
    /// A half-typed prefix goes with it: the box it was typed into is gone,
    /// and a `Ctrl-\` left pending would consume the first byte of whatever
    /// the user types at the shell next.
    pub fn take_held(&mut self) -> Vec<u8> {
        self.phase = Phase::Live;
        self.prefix_pending = false;
        std::mem::take(&mut self.held)
    }

    /// Discard the held input, half-typed prefix and all.
    pub fn drop_held(&mut self) {
        self.held.clear();
        self.phase = Phase::Live;
        self.prefix_pending = false;
    }

    /// Feed keystrokes typed while a notice is showing.
    ///
    /// Returns the command the user asked for, if any; everything else is
    /// added to the held buffer. The prefix may arrive at the end of one read
    /// and its letter at the start of the next, which is why the pending flag
    /// outlives the call: a parser that only looked within one buffer would
    /// swallow the command and hold two stray bytes.
    ///
    /// **Which keys are commands depends on the phase**, because it depends on
    /// which box the user is reading. The gate lives here rather than in the
    /// caller so that a letter that is not a command in this phase falls into
    /// the ordinary "the user meant to type both bytes" arm below -- keeping
    /// the two bytes, in order, along with whatever else was in the same read.
    /// A caller that filtered the returned `Command` instead would lose the
    /// rest of the read, because a command ends the loop.
    pub fn hold_keys(&mut self, bytes: &[u8]) -> Option<Command> {
        for &b in bytes.iter() {
            if self.prefix_pending {
                self.prefix_pending = false;
                let command = match b {
                    // Offered by every notice. Closing oxutrm is always
                    // available and never touches the host.
                    b'q' => Some(Command::Quit),
                    // Offered ONLY by the `Confirming` box, because only that
                    // box asks the question they answer. Under `Silent` an `s`
                    // would throw the buffer at a link the user has just been
                    // told is not answering -- and empty it, so the review
                    // that is the whole point of holding never happens -- and
                    // a `d` would discard it with no confirmation at all.
                    b's' if self.phase == Phase::Confirming => Some(Command::SendHeld),
                    b'd' if self.phase == Phase::Confirming => Some(Command::DropHeld),
                    // Not a command here, so the user meant to type both bytes.
                    _ => {
                        self.push_held(PREFIX);
                        self.push_held(b);
                        None
                    }
                };
                if command.is_some() {
                    // Anything after a command in the same read belongs to
                    // whatever the command leads to, not to the old buffer.
                    return command;
                }
                continue;
            }

            if b == PREFIX {
                self.prefix_pending = true;
                continue;
            }
            self.push_held(b);
        }
        None
    }

    fn push_held(&mut self, b: u8) {
        if self.held.len() < MAX_HELD {
            self.held.push(b);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_fresh_link_is_live() {
        assert_eq!(LinkState::new(t0()).phase(), Phase::Live);
    }

    #[test]
    fn silence_with_a_reply_owed_becomes_silent_after_the_grace_period() {
        let t = t0();
        let mut s = LinkState::new(t);

        // The lap the owing begins on. The grace period runs from here, so it
        // has to be on the clock for the two below to mean what they say.
        assert_eq!(s.evaluate(t, true), Phase::Live);
        assert_eq!(
            s.evaluate(t + Duration::from_millis(1900), true),
            Phase::Live
        );
        assert!(matches!(
            s.evaluate(t + Duration::from_millis(2100), true),
            Phase::Silent { .. }
        ));
    }

    /// The grace period measures the OWING, not the calm before it.
    ///
    /// This is the defect the whole clock is shaped around. `evaluate` used to
    /// compare `now` with `last_heard`, and on a quiet session `last_heard` is
    /// arbitrarily old -- nothing arrives when nothing is happening. So the
    /// first lap after a keystroke found `now - last_heard` already past
    /// `SILENT_AFTER` and painted "no reply from host" for a reply that had
    /// been owed for zero milliseconds, on a link that was about to answer it.
    ///
    /// Spec 2: `Silent` is entered on "`SILENT_AFTER` with a reply owed and
    /// none arriving", and "a blip that resolves in 400 ms must never paint
    /// anything, or the indicator becomes the noise it was built to remove".
    #[test]
    fn a_reply_owed_for_a_moment_after_a_long_calm_stays_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        // Ten seconds of calm. Nothing is owed, so nothing is knowable, and
        // `last_heard` is now ten seconds stale.
        assert_eq!(s.evaluate(t + Duration::from_secs(10), false), Phase::Live);

        // A key is pressed, and a hundred milliseconds later the answer has
        // not come back yet. That is a healthy link, not an outage.
        assert_eq!(
            s.evaluate(t + Duration::from_millis(10_100), true),
            Phase::Live,
            "the notice painted for a reply owed for 100 ms: the grace period \
             is measuring the calm before the owing instead of the owing"
        );
    }

    /// An answered reply ends the owing, so the next one starts its own
    /// clock. Carrying the old start forward would make a link that answers
    /// everything promptly go `Silent` after two seconds of ordinary traffic.
    #[test]
    fn an_answered_reply_restarts_the_grace_period() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.evaluate(t, true);
        s.evaluate(t + Duration::from_secs(1), false);

        assert_eq!(
            s.evaluate(t + Duration::from_millis(2500), true),
            Phase::Live,
            "the grace period carried over from an owing the host had already \
             answered"
        );
    }

    /// Nothing owed means nothing is knowable. Without the heartbeat of Task 6
    /// this is also why an idle session would never notice an outage.
    #[test]
    fn silence_with_nothing_owed_stays_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(s.evaluate(t + Duration::from_secs(60), false), Phase::Live);
    }

    #[test]
    fn hearing_from_the_host_returns_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.evaluate(t, true);
        assert!(matches!(
            s.evaluate(t + Duration::from_secs(3), true),
            Phase::Silent { .. }
        ));

        s.heard(t + Duration::from_secs(4));
        assert_eq!(s.phase(), Phase::Live);
    }

    /// The `since` is the moment the host went quiet, not the moment we
    /// noticed. A counter that started at the grace period would under-report
    /// every outage by two seconds.
    #[test]
    fn the_silence_started_when_the_host_went_quiet_not_when_we_noticed() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.heard(t);
        s.evaluate(t, true);

        let Phase::Silent { since } = s.evaluate(t + Duration::from_secs(5), true) else {
            panic!("expected Silent");
        };
        assert_eq!(
            since, t,
            "the counter must run from the last thing we heard"
        );
    }

    #[test]
    fn silence_persists_across_laps_without_restarting_the_clock() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.evaluate(t, true);

        let first = s.evaluate(t + Duration::from_secs(3), true);
        let later = s.evaluate(t + Duration::from_secs(9), true);

        assert_eq!(first, later, "the clock restarted mid-outage");
    }

    #[test]
    fn a_quiet_link_wants_a_heartbeat_after_the_idle_period() {
        let t = t0();
        let s = LinkState::new(t);

        assert!(!s.heartbeat_due(t + Duration::from_secs(4)));
        assert!(s.heartbeat_due(t + Duration::from_secs(6)));
    }

    #[test]
    fn sending_postpones_the_heartbeat() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.sent(t + Duration::from_secs(4));
        assert!(!s.heartbeat_due(t + Duration::from_secs(6)));
        assert!(s.heartbeat_due(t + Duration::from_secs(10)));
    }

    #[test]
    fn hearing_postpones_the_heartbeat() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.heard(t + Duration::from_secs(4));
        assert!(!s.heartbeat_due(t + Duration::from_secs(6)));
    }

    /// The heartbeat exists to make an idle session detectable. Without it,
    /// `evaluate` can never see a reply owed, so an outage on a session nobody
    /// is typing into would go unreported until the user pressed a key.
    #[test]
    fn a_heartbeat_makes_an_idle_outage_visible() {
        let t = t0();
        let mut s = LinkState::new(t);

        assert_eq!(s.evaluate(t + Duration::from_secs(6), false), Phase::Live);
        assert!(s.heartbeat_due(t + Duration::from_secs(6)));

        // The caller sends the heartbeat; from here a reply is owed, and the
        // grace period runs from here rather than from the six quiet seconds
        // that preceded it -- which is why the lap at six seconds is still
        // `Live` and only the one at nine is not.
        s.sent(t + Duration::from_secs(6));
        assert_eq!(s.evaluate(t + Duration::from_secs(6), true), Phase::Live);
        assert!(matches!(
            s.evaluate(t + Duration::from_secs(9), true),
            Phase::Silent { .. }
        ));
    }

    #[test]
    fn keys_typed_offline_are_held_not_delivered() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"make test"), None);
        assert_eq!(s.held(), b"make test");
    }

    #[test]
    fn the_prefix_and_a_letter_are_a_command_and_are_not_held() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"ab\x1cq"), Some(Command::Quit));
        assert_eq!(
            s.held(),
            b"ab",
            "the prefix or the command leaked into the buffer"
        );
    }

    #[test]
    fn every_command_key_is_recognised() {
        for (byte, want) in [
            (b'q', Command::Quit),
            (b's', Command::SendHeld),
            (b'd', Command::DropHeld),
        ] {
            let t = t0();
            let mut s = LinkState::new(t);
            // `s` and `d` are offered by the `Confirming` box alone, so that
            // is the phase they have to be asked in. Something held and the
            // host answering is exactly what raises it.
            s.hold_keys(b"x");
            s.heard(t);
            assert_eq!(s.phase(), Phase::Confirming);

            assert_eq!(
                s.hold_keys(&[0x1c, byte]),
                Some(want),
                "for {}",
                byte as char
            );
        }
    }

    /// A box the user is reading offers `Ctrl-\ q` and, only when it is
    /// asking about held input, `s` and `d`. Honouring `s` under `Silent`
    /// throws the buffer at a link the client has just told the user is not
    /// answering, and empties it, so the review that is the entire point of
    /// holding never happens. `d` discards it with no confirmation at all.
    #[test]
    fn send_and_drop_are_not_commands_under_the_silent_notice() {
        for &byte in b"sd" {
            let t = t0();
            let mut s = LinkState::new(t);
            s.hold_keys(b"make test");
            s.evaluate(t, true);
            assert!(matches!(
                s.evaluate(t + Duration::from_secs(3), true),
                Phase::Silent { .. }
            ));

            assert_eq!(
                s.hold_keys(&[0x1c, byte]),
                None,
                "`Ctrl-\\ {}` was honoured under a notice that does not offer it",
                byte as char
            );
            assert_eq!(
                s.held(),
                [b"make test".as_slice(), &[0x1c, byte]].concat(),
                "the keystroke was neither a command nor kept"
            );
        }
    }

    /// The one key every box offers. Closing oxutrm is always available, and
    /// it never touches the host either way.
    #[test]
    fn quit_is_offered_in_every_phase() {
        let t = t0();
        for phase in ["live", "silent", "confirming"] {
            let mut s = LinkState::new(t);
            match phase {
                "silent" => {
                    s.evaluate(t, true);
                    s.evaluate(t + Duration::from_secs(3), true);
                }
                "confirming" => {
                    s.hold_keys(b"x");
                    s.heard(t);
                }
                _ => {}
            }
            assert_eq!(
                s.hold_keys(&[0x1c, b'q']),
                Some(Command::Quit),
                "under {phase}"
            );
        }
    }

    /// The prefix can be the last byte of one read and the letter the first of
    /// the next. A parser that only looked within one buffer would drop the
    /// command and hold two stray bytes.
    #[test]
    fn a_prefix_split_across_two_reads_still_commands() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"x\x1c"), None);
        assert_eq!(s.hold_keys(b"q"), Some(Command::Quit));
        assert_eq!(s.held(), b"x");
    }

    /// An unknown letter after the prefix is ordinary typing, and both bytes
    /// are kept: the user meant to type them.
    #[test]
    fn an_unknown_key_after_the_prefix_is_held_with_the_prefix() {
        let mut s = LinkState::new(t0());

        assert_eq!(s.hold_keys(b"\x1cz"), None);
        assert_eq!(s.held(), b"\x1cz");
    }

    /// The cap stops accepting rather than dropping the oldest bytes: the
    /// oldest are the command and the newest are the newline, so discarding
    /// from the front is how a truncated command still runs.
    #[test]
    fn a_full_buffer_stops_accepting_rather_than_dropping_the_oldest() {
        let mut s = LinkState::new(t0());
        s.hold_keys(&vec![b'a'; MAX_HELD]);

        assert!(s.held_is_full());
        s.hold_keys(b"zzz");
        assert_eq!(s.held().len(), MAX_HELD);
        assert_eq!(s.held()[0], b'a', "the oldest bytes were dropped");
        assert!(!s.held().contains(&b'z'), "accepted past the cap");
    }

    /// A `Ctrl-\` whose letter never came belongs to the box that was up when
    /// it was typed. Left pending, it eats the first byte typed under the NEXT
    /// box -- and the `Confirming` box's first key is the answer to a question
    /// about somebody's shell.
    #[test]
    fn a_half_typed_prefix_does_not_survive_the_notice_it_was_typed_into() {
        let t = t0();
        let mut s = LinkState::new(t);
        // Typed blind at a silent link, ending on a prefix with no letter.
        s.hold_keys(b"make test\r\x1c");

        // The host answers, and the box asks whether to deliver that.
        s.heard(t);
        assert_eq!(s.phase(), Phase::Confirming);

        assert_eq!(
            s.hold_keys(b"send it"),
            None,
            "a prefix left over from the previous box answered the new box's \
             question, and delivered typing nobody confirmed"
        );
        assert!(
            s.held().ends_with(b"send it"),
            "the leading byte was eaten by a stale prefix: {:?}",
            s.held()
        );
    }

    /// The same, for the two ways the user ends a box by their own hand. The
    /// bytes that follow go to the shell, so a stale prefix eats a keystroke
    /// out of a command line.
    #[test]
    fn resolving_the_buffer_drops_a_half_typed_prefix_with_it() {
        for resolve in [
            (|s: &mut LinkState| {
                s.take_held();
            }) as fn(&mut LinkState),
            |s: &mut LinkState| s.drop_held(),
        ] {
            let mut s = LinkState::new(t0());
            s.hold_keys(b"x\x1c");

            resolve(&mut s);

            assert_eq!(s.hold_keys(b"day"), None, "a stale prefix commanded");
            assert_eq!(s.held(), b"day", "the leading byte was eaten");
        }
    }

    #[test]
    fn taking_the_held_input_empties_the_buffer() {
        let mut s = LinkState::new(t0());
        s.hold_keys(b"hello");

        assert_eq!(s.take_held(), b"hello");
        assert!(s.held().is_empty());
    }

    /// Hearing from the host with something held is what raises the question,
    /// and the question is the `Confirming` phase.
    #[test]
    fn coming_back_with_held_input_asks_instead_of_going_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.hold_keys(b"make test\r");

        s.heard(t + Duration::from_secs(9));
        assert_eq!(s.phase(), Phase::Confirming);
    }

    #[test]
    fn coming_back_with_nothing_held_goes_straight_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.heard(t + Duration::from_secs(9));
        assert_eq!(s.phase(), Phase::Live);
    }

    #[test]
    fn resolving_the_held_input_returns_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.hold_keys(b"x");
        s.heard(t);
        assert_eq!(s.phase(), Phase::Confirming);

        s.drop_held();
        assert_eq!(s.phase(), Phase::Live);
        assert!(s.held().is_empty());
    }

    #[test]
    fn control_bytes_render_readably_rather_than_as_themselves() {
        assert_eq!(render_held(b"make test\r"), "make test\u{21b5}");
        assert_eq!(render_held(b"a\x03b"), "a^Cb");
        assert_eq!(render_held(b"\t"), "^I");
    }

    /// A paste can be enormous, and a box cannot hold it. Summarising beats
    /// truncating silently, which would show a command that is not the command
    /// about to run.
    #[test]
    fn a_long_buffer_is_summarised_rather_than_dumped() {
        let long = vec![b'x'; 5000];
        let shown = render_held(&long);

        assert!(shown.len() < 500, "not summarised: {} chars", shown.len());
        assert!(
            shown.contains("more"),
            "no indication of what was elided: {shown}"
        );
    }

    /// The held buffer is raw terminal input, so a character the user
    /// actually typed is very often multi-byte UTF-8. Mapping byte-for-byte
    /// turns it into mojibake, which defeats the entire point of
    /// `Confirming`: showing people what they typed so they can decide
    /// whether to deliver it.
    #[test]
    fn a_non_ascii_character_reads_back_as_itself() {
        assert_eq!(render_held("héllo 世界 🎉".as_bytes()), "héllo 世界 🎉");
    }

    /// C1 controls (U+0080-U+009F) are a second range of control scalars
    /// beyond C0 and DEL. U+009B is CSI, and terminals in UTF-8 mode act on
    /// it, so one reaching the renderer raw is untrusted input landing as a
    /// control sequence -- the same bug the C0 handling exists to prevent.
    #[test]
    fn a_c1_control_byte_does_not_appear_raw() {
        // U+009B (CSI) encoded as UTF-8: 0xC2 0x9B.
        let shown = render_held(&[b'a', 0xc2, 0x9b, b'b']);
        assert!(
            !shown.contains('\u{9b}'),
            "a raw C1 control reached the output: {shown:?}"
        );
    }

    /// `HELD_SHOWN` is a byte count, and cutting at a fixed byte offset can
    /// land inside a multi-byte character. The cut must back off to a
    /// character boundary rather than let a split sequence render as a
    /// mangled fragment.
    #[test]
    fn a_multi_byte_character_straddling_the_shown_boundary_is_not_split() {
        let mut buf = vec![b'a'; 199];
        buf.extend_from_slice("世".as_bytes()); // 3 bytes, occupying indices
        // 199..202 -- straddling the HELD_SHOWN=200 cut.

        let shown = render_held(&buf);

        assert!(
            !shown.contains('\u{e4}'),
            "the character's leading byte (0xE4) was rendered as a raw \
             Latin-1 scalar instead of the cut being backed off: {shown:?}"
        );
    }
}

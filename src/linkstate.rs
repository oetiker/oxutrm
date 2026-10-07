//! Whether the host is still answering, and what the user is told about it.
//!
//! Pure: no I/O, no terminal, no runtime. Every method takes the current
//! `Instant` as a parameter rather than reading the clock, which is what lets
//! the whole state machine be tested without sleeping.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant};

/// How long a reply may be owed before the user is told. Below this a blip
/// resolves without ever painting: an indicator that fires on every hiccup is
/// the noise it was built to remove.
///
/// The default of `recovery.silent_after`; a session's own value is a
/// [`LinkState`] field, set by [`LinkState::retune`].
pub const SILENT_AFTER: Duration = Duration::from_secs(2);

/// How long silence lasts before the client starts rebuilding the link on its
/// own.
///
/// **Twenty seconds**, and it is a guess the spec is honest about: long
/// enough that a rebuild is not raced against an outage about to end by
/// itself, short enough not to feel abandoned. Revisit it against a real bad
/// network, not by reasoning about it.
///
/// The default of `recovery.rebuild_after`; a session's own value is a
/// [`LinkState`] field, set by [`LinkState::retune`].
pub const REBUILD_AFTER: Duration = Duration::from_secs(20);

/// The delay that belongs to attempt `attempt` (zero-based): 1, 2, 4, 8, and
/// 8 for ever after.
///
/// Saturating rather than shifting: an attempt counter that runs for a week
/// must not wrap into an instant retry.
///
/// **This is not the schedule a user sees, and the difference has already been
/// written down wrongly once.** The delay that actually separates two attempts
/// comes from [`LinkState::attempt_failed`], which schedules the NEXT
/// attempt's number -- `backoff(attempt + 1)` -- from the moment the current
/// one failed. And the first attempt of an outage waits for nothing at all,
/// because `evaluate` enters `Recovering` with `next_try: now`: twenty seconds
/// of silence have already elapsed, and adding a second to them helps nobody.
///
/// So the observed sequence is: an immediate first attempt, then 2 s, 4 s,
/// 8 s, 8 s ... measured from each failure. Deliberate, and correct; only the
/// prose describing it was wrong.
///
/// Two more callers schedule from this, and leaving them out of a doc whose
/// whole job is to keep the schedule written down correctly is the one defect
/// it cannot afford. [`LinkState::begin_attempt`] re-stamps the CURRENT
/// attempt as `now + backoff(attempt)` when it starts, which is what stops a
/// lap that ran late from firing the same attempt twice; it does not advance
/// the counter. [`LinkState::rebuilt`] starts the count over at
/// `landed + backoff(0)` -- one second in which the freshly swapped link may
/// produce a frame before the loop is allowed to consider building another.
#[must_use]
pub fn backoff(attempt: u32) -> Duration {
    Duration::from_secs(1u64 << attempt.min(3))
}

/// How long a session may be completely quiet before the client says something
/// merely to see whether anyone is still there.
///
/// **0.2 Hz.** Set that against the 250 Hz poll removed in `19cc001`, and note
/// that it applies only to an ATTACHED client: a detached host session has no
/// client, so it has no heartbeat and its idle cost is unchanged.
pub const HEARTBEAT_IDLE: Duration = Duration::from_secs(5);

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
    Silent {
        since: Instant,
    },
    /// Silent long enough that the client is rebuilding the link itself.
    ///
    /// `attempt` is zero-based and feeds [`backoff`]; `next_try` is when the
    /// next one is due. The client never leaves this state of its own accord
    /// -- only a frame arriving does that, or the user quitting.
    Recovering {
        attempt: u32,
        next_try: Instant,
    },
    Confirming,
}

impl Phase {
    /// Whether the primary is not currently answering.
    ///
    /// `Confirming` is not an outage: a frame already came back, and the
    /// phase is only waiting on the user to say what to do with what was
    /// typed blind. Neither is `Live`. This is the predicate the failover
    /// decision (`failover_due`, below) and the route-follow decision
    /// (`session.rs`'s `follow_route`) share; other code that happens to
    /// look similar, such as the popup's key routing, may have its
    /// own reasons to draw the same line and is not implied by this doc.
    pub fn is_outage(&self) -> bool {
        matches!(self, Phase::Silent { .. } | Phase::Recovering { .. })
    }
}

/// How long after sending an answered probe the client fails over.
///
/// The probe goes out on the first lap in `Silent`, which is `SILENT_AFTER`
/// after a reply became owed, so failover lands at `SILENT_AFTER` plus this:
/// three seconds, the spec's `FAILOVER_AFTER` (§3.5). The second is there so a
/// primary that was only blipping gets to answer first, since failing over
/// costs a full-state snapshot and a new standby search.
pub const FAILOVER_GRACE: Duration = Duration::from_secs(1);

/// How long a probe may take before the standby counts as not answering.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long after a failed probe the next one may go, within one outage.
pub const PROBE_RETRY: Duration = Duration::from_secs(5);

/// How long after a link is up before a standby is searched for, so the
/// search never competes with the first paint (spec §3.1).
pub const STANDBY_DELAY: Duration = Duration::from_secs(5);

/// The wait before standby search number `failures + 1`.
pub fn standby_backoff(failures: u32) -> Duration {
    Duration::from_secs(match failures {
        0 => 30,
        1 => 60,
        2 => 120,
        _ => 300,
    })
}

/// Where this outage's probe of the standby stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeState {
    Idle,
    Pending { sent: Instant },
    Answered { sent: Instant },
    Failed { at: Instant },
}

/// Whether to fail over onto the standby now.
///
/// Only while the primary is not answering, and only on an answer from the
/// standby, never on the absence of one: failing over onto a standby that
/// is also dead would trade a connection that might revive for one that has
/// already been seen not to.
///
/// Failing over while `Recovering` is allowed here: an answer that lands
/// after the 20 s rebuild escalation is still an answer. But `Recovering` can
/// also mean a rebuild attempt is in flight, and landing that attempt would
/// have the host adopt it as the primary and close the standby this just
/// promoted as `TAKEN_OVER` -- so the caller (`Standby::step`) is responsible
/// for not failing over while a rebuild attempt that has sent its `Attach` is
/// running (`RebuildStage::Committed`); this function does not know about
/// rebuilds and does not gate on them.
pub fn failover_due(phase: Phase, probe: ProbeState, now: Instant) -> bool {
    let outage = phase.is_outage();
    match probe {
        ProbeState::Answered { sent } => outage && now.duration_since(sent) >= FAILOVER_GRACE,
        _ => false,
    }
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
/// Control bytes become readable rather than being emitted: the popup is
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
    /// `recovery.silent_after`: how long a reply may be owed before it is an
    /// outage.
    silent_after: Duration,
    /// The effective `recovery.rebuild_after`: how long the host may be
    /// silent before the client rebuilds.
    rebuild_after: Duration,
}

impl LinkState {
    pub fn new(now: Instant) -> LinkState {
        LinkState {
            phase: Phase::Live,
            last_heard: now,
            owed_since: None,
            last_sent: now,
            held: Vec::new(),
            silent_after: SILENT_AFTER,
            rebuild_after: REBUILD_AFTER,
        }
    }

    /// The two recovery timings, from `apply`. Both act on the next
    /// `evaluate`: an outage already `Silent` escalates as soon as it has
    /// lasted the new `rebuild_after`.
    pub fn retune(&mut self, silent_after: Duration, rebuild_after: Duration) {
        self.silent_after = silent_after;
        self.rebuild_after = rebuild_after;
    }

    /// The silence after which a rebuild starts, for the popup's countdown.
    pub fn rebuild_after(&self) -> Duration {
        self.rebuild_after
    }

    /// The phase, without advancing anything.
    ///
    /// `evaluate` returns the phase it decided, so the loop reads it from
    /// there rather than asking twice. This exists for the callers that need
    /// to know what is already true without deciding anything: phase 2's route
    /// probe runs only while `Silent`, and asking `evaluate` would both
    /// require a `reply_owed` it has no business computing and advance a state
    /// machine it only wants to read.
    pub fn phase_now(&self) -> Phase {
        self.phase
    }

    /// The last time anything at all arrived from the host.
    ///
    /// `Phase::Recovering` does not carry its own `since`, unlike `Silent`:
    /// only a frame arriving leaves `Recovering`, and that same frame is what
    /// updates this, so the popup, reporting how long the host has been
    /// quiet in `Recovering`, reads the silence's start from here instead.
    pub fn last_heard(&self) -> Instant {
        self.last_heard
    }

    /// A frame arrived. Whatever we believed, the host is answering.
    pub fn heard(&mut self, now: Instant) {
        self.last_heard = now;
        // Whatever was owed has been answered. Anything owed from here starts
        // its own clock, or the grace period would be measured from a moment
        // the host has already replied to.
        self.owed_since = None;
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
        // Recovering is terminal until something arrives: `heard` is the only
        // way out, and re-deciding here would reset the attempt counter every
        // lap.
        if let Phase::Recovering { .. } = self.phase {
            return self.phase;
        }
        if let Phase::Silent { since } = self.phase {
            // The one escalation. The clock still runs from `last_heard`, so
            // the displayed silence stays continuous across the boundary.
            if now.duration_since(since) >= self.rebuild_after {
                self.phase = Phase::Recovering {
                    attempt: 0,
                    next_try: now,
                };
            }
            return self.phase;
        }

        // When the owing STARTED, which is the thing the grace period is
        // about. `last_heard` cannot stand in for it: nothing arrives on a
        // quiet session, so `last_heard` is arbitrarily old and the first lap
        // after a keystroke would read as a two-second outage. Worse, the
        // heartbeat owes a reply every `HEARTBEAT_IDLE`, which is longer than
        // `SILENT_AFTER`, so every idle session would raise the popup every
        // five seconds for ever.
        match (reply_owed, self.owed_since) {
            (true, None) => self.owed_since = Some(now),
            // Answered. The next owing starts its own clock.
            (false, _) => self.owed_since = None,
            (true, Some(_)) => {}
        }

        if self
            .owed_since
            .is_some_and(|since| now.duration_since(since) >= self.silent_after)
        {
            // `last_heard` and not `now`: the counter must report how long the
            // host has been quiet, not how long since we worked it out.
            //
            // It is deliberately NOT `owed_since` either, which would be a
            // different and smaller number -- the reply may have started being
            // owed long after the host went quiet. The consequence is that the
            // displayed figure can OVERSTATE the silence. By at most
            // `HEARTBEAT_IDLE`, because a heartbeat that is answered refreshes
            // `last_heard`, and nothing else lets it fall further behind:
            // `ClientSession::take_frames` calls `note_heard` for every frame
            // it applies, whichever lap -- pacing, keyboard, resize, or the
            // loop's own frame arm -- scavenges it out of the channel, so
            // `last_heard` moves the moment any frame lands.
            self.phase = Phase::Silent {
                since: self.last_heard,
            };
        }
        self.phase
    }

    /// The client is about to run an attempt.
    pub fn begin_attempt(&mut self, now: Instant) {
        if let Phase::Recovering { attempt, .. } = self.phase {
            self.phase = Phase::Recovering {
                attempt,
                next_try: now + backoff(attempt),
            };
        }
    }

    /// An attempt landed and its link is in place, but nothing has arrived on
    /// it yet.
    ///
    /// The phase stays `Recovering`, deliberately: a link being up is not the
    /// same as the host answering on it, and only a frame -- through
    /// [`LinkState::heard`] -- may say that. What this does is stop the
    /// rebuilt link from being torn down by the loop that built it.
    ///
    /// Without it the phase still carries the OLD `next_try`, which is in the
    /// past by the time any real attempt has finished, so the very next lap
    /// starts another attempt against a link that was just swapped in. That
    /// second attempt can land too, displacing the first, leaving the phase
    /// still `Recovering` and starting a third: a loop that displaces itself
    /// for as long as no frame gets through, which is precisely the condition
    /// it exists to survive.
    ///
    /// The counter starts over as well. The failures that ran it up belong to
    /// the link that is gone; counting on across a successful rebuild would
    /// have the popup telling the user about an eleventh attempt over a
    /// connection built by the tenth.
    pub fn rebuilt(&mut self, now: Instant) {
        if let Phase::Recovering { .. } = self.phase {
            self.phase = Phase::Recovering {
                attempt: 0,
                next_try: now + backoff(0),
            };
        }
    }

    /// An attempt failed for a reason worth retrying.
    pub fn attempt_failed(&mut self, now: Instant) {
        if let Phase::Recovering { attempt, .. } = self.phase {
            let next = attempt.saturating_add(1);
            self.phase = Phase::Recovering {
                attempt: next,
                next_try: now + backoff(next),
            };
        }
    }

    pub fn held(&self) -> &[u8] {
        &self.held
    }

    pub fn held_is_full(&self) -> bool {
        self.held.len() >= MAX_HELD
    }

    /// Deliver the held input, emptying the buffer.
    pub fn take_held(&mut self) -> Vec<u8> {
        self.phase = Phase::Live;
        std::mem::take(&mut self.held)
    }

    /// Discard the held input.
    pub fn drop_held(&mut self) {
        self.held.clear();
        self.phase = Phase::Live;
    }

    /// Keep keystrokes typed while the link is not answering.
    ///
    /// Only the holding: which keystrokes are commands is decided by the
    /// popup (`ui.rs`) before anything reaches here. Beyond [`MAX_HELD`] the
    /// buffer stops accepting -- see there for why it never drops the oldest.
    pub fn hold_keys(&mut self, bytes: &[u8]) {
        let room = MAX_HELD.saturating_sub(self.held.len());
        self.held.extend_from_slice(&bytes[..bytes.len().min(room)]);
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
        assert_eq!(LinkState::new(t0()).phase_now(), Phase::Live);
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
            "the popup opened for a reply owed for 100 ms: the grace period \
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
        assert_eq!(s.phase_now(), Phase::Live);
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

        s.hold_keys(b"make test");
        assert_eq!(s.held(), b"make test");
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
        assert_eq!(s.phase_now(), Phase::Confirming);
    }

    #[test]
    fn coming_back_with_nothing_held_goes_straight_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);

        s.heard(t + Duration::from_secs(9));
        assert_eq!(s.phase_now(), Phase::Live);
    }

    #[test]
    fn resolving_the_held_input_returns_to_live() {
        let t = t0();
        let mut s = LinkState::new(t);
        s.hold_keys(b"x");
        s.heard(t);
        assert_eq!(s.phase_now(), Phase::Confirming);

        s.drop_held();
        assert_eq!(s.phase_now(), Phase::Live);
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

    #[test]
    fn silence_becomes_recovering_only_after_rebuild_after() {
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        assert_eq!(state.evaluate(t0, true), Phase::Live);
        // Silent first, and it must STAY Silent across the whole grace period:
        // a client that starts spawning ssh after two seconds spawns one on
        // every blip.
        let silent = t0 + SILENT_AFTER;
        assert!(matches!(state.evaluate(silent, true), Phase::Silent { .. }));
        let nearly = t0 + REBUILD_AFTER - Duration::from_millis(1);
        assert!(
            matches!(state.evaluate(nearly, true), Phase::Silent { .. }),
            "one millisecond early is still Silent"
        );
        match state.evaluate(t0 + REBUILD_AFTER, true) {
            Phase::Recovering { attempt, .. } => {
                assert_eq!(attempt, 0, "the first attempt is numbered 0")
            }
            other => panic!("expected Recovering, got {other:?}"),
        }
    }

    #[test]
    fn a_frame_returns_to_live_from_recovering() {
        // The whole reason the old link is held: whichever path revives first
        // wins. A client that could not leave Recovering would keep rebuilding
        // over a link that had already come back.
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        // `evaluate` escalates one stage per call -- see
        // `silence_becomes_recovering_only_after_rebuild_after` -- so reaching
        // `Recovering` from a fresh `Live` state needs the same build-up
        // through `Silent` rather than a single jump to `REBUILD_AFTER`.
        let _ = state.evaluate(t0, true);
        let _ = state.evaluate(t0 + SILENT_AFTER, true);
        let _ = state.evaluate(t0 + REBUILD_AFTER, true);
        assert!(
            matches!(state.phase_now(), Phase::Recovering { .. }),
            "expected Recovering before hearing anything, got {:?}",
            state.phase_now()
        );
        state.heard(t0 + REBUILD_AFTER + Duration::from_millis(1));
        assert_eq!(
            state.evaluate(t0 + REBUILD_AFTER + Duration::from_millis(2), false),
            Phase::Live
        );
    }

    #[test]
    fn the_backoff_doubles_then_holds_at_eight_seconds() {
        // Asserted first, and deliberately: a shift-based implementation
        // (`1u64 << attempt` with no cap) does not merely disagree with the
        // spec here, it panics on overflow. Put after the ordinary cases,
        // that panic is unreachable -- `assert_eq!` on `backoff(4)` already
        // fails and stops the test before this line ever runs. Asserted
        // first, an injection that removes the cap is guaranteed to be seen
        // failing on the line whose comment claims to prove it.
        assert_eq!(backoff(u32::MAX), Duration::from_secs(8));
        // Spec: 1, 2, 4, 8, then every 8 s indefinitely. The cap is the point --
        // an unbounded doubling means a client that reconnects hours after the
        // network came back.
        assert_eq!(backoff(0), Duration::from_secs(1));
        assert_eq!(backoff(1), Duration::from_secs(2));
        assert_eq!(backoff(2), Duration::from_secs(4));
        assert_eq!(backoff(3), Duration::from_secs(8));
        assert_eq!(backoff(4), Duration::from_secs(8));
        assert_eq!(backoff(1000), Duration::from_secs(8));
    }

    #[test]
    fn beginning_an_attempt_schedules_next_try_by_the_backoff() {
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        // See the comment in `a_frame_returns_to_live_from_recovering`: this
        // build-up is what actually reaches `Recovering`.
        let _ = state.evaluate(t0, true);
        let _ = state.evaluate(t0 + SILENT_AFTER, true);
        let _ = state.evaluate(t0 + REBUILD_AFTER, true);
        state.begin_attempt(t0 + REBUILD_AFTER);
        match state.phase_now() {
            Phase::Recovering { attempt, next_try } => {
                assert_eq!(
                    attempt, 0,
                    "begin_attempt must not itself advance the attempt count"
                );
                assert_eq!(next_try, t0 + REBUILD_AFTER + backoff(0));
            }
            other => panic!("expected Recovering, got {other:?}"),
        }
    }

    /// A rebuilt link gets a full backoff to produce its first frame, and the
    /// failures of the link it replaced do not follow it.
    ///
    /// Without the reschedule the phase keeps the `next_try` set when the
    /// landed attempt STARTED, which any real attempt outlives, so the loop
    /// immediately builds a second link and displaces the one it just made.
    #[test]
    fn a_rebuilt_link_is_given_a_backoff_before_the_next_attempt() {
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        let _ = state.evaluate(t0, true);
        let _ = state.evaluate(t0 + SILENT_AFTER, true);
        let _ = state.evaluate(t0 + REBUILD_AFTER, true);
        state.begin_attempt(t0 + REBUILD_AFTER);
        state.attempt_failed(t0 + REBUILD_AFTER + Duration::from_secs(1));
        assert!(
            matches!(state.phase_now(), Phase::Recovering { attempt: 1, .. }),
            "the fixture did not run the counter up, so the reset below would \
             prove nothing: {:?}",
            state.phase_now()
        );

        let landed = t0 + REBUILD_AFTER + Duration::from_secs(9);
        state.rebuilt(landed);

        match state.phase_now() {
            Phase::Recovering { attempt, next_try } => {
                assert_eq!(
                    attempt, 0,
                    "the failures of the link that is gone were carried onto \
                     the one that replaced it"
                );
                // The exact instant, not merely "later than it was": a
                // reschedule that only advanced by a tick would still leave
                // the next lap starting an attempt over a fresh link.
                assert_eq!(next_try, landed + backoff(0));
            }
            other => {
                panic!("a rebuilt link is still Recovering until a frame arrives, got {other:?}")
            }
        }
    }

    /// Landing is not being answered. Only a frame may say the host is back,
    /// so the phase has to survive the swap.
    #[test]
    fn a_rebuilt_link_does_not_by_itself_leave_recovering() {
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        let _ = state.evaluate(t0, true);
        let _ = state.evaluate(t0 + SILENT_AFTER, true);
        let _ = state.evaluate(t0 + REBUILD_AFTER, true);

        state.rebuilt(t0 + REBUILD_AFTER);

        assert!(
            matches!(state.phase_now(), Phase::Recovering { .. }),
            "the swap declared the host to be answering, which nothing has \
             observed: {:?}",
            state.phase_now()
        );
    }

    #[test]
    fn a_failed_attempt_schedules_the_next_one_further_out() {
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        // See the comment in `a_frame_returns_to_live_from_recovering`: this
        // build-up is what actually reaches `Recovering`.
        let _ = state.evaluate(t0, true);
        let _ = state.evaluate(t0 + SILENT_AFTER, true);
        let _ = state.evaluate(t0 + REBUILD_AFTER, true);
        assert!(
            matches!(state.phase_now(), Phase::Recovering { .. }),
            "expected Recovering before the failure, got {:?}",
            state.phase_now()
        );
        state.attempt_failed(t0 + REBUILD_AFTER + Duration::from_secs(1));
        match state.phase_now() {
            Phase::Recovering { attempt, next_try } => {
                assert_eq!(attempt, 1);
                // The exact schedule, not merely "later than before": a
                // `next_try` that only advanced by one tick of `now` -- with
                // `+ backoff(next)` dropped from `attempt_failed` entirely --
                // would still satisfy a bare `>` comparison, so that is not
                // what "further out" means here.
                assert_eq!(
                    next_try,
                    t0 + REBUILD_AFTER + Duration::from_secs(1) + backoff(1)
                );
            }
            other => panic!("expected Recovering, got {other:?}"),
        }
    }

    #[test]
    fn failover_waits_for_the_grace_after_an_answered_probe() {
        let t0 = Instant::now();
        let silent = Phase::Silent { since: t0 };
        let answered = ProbeState::Answered { sent: t0 };
        assert!(!failover_due(
            silent,
            answered,
            t0 + FAILOVER_GRACE - Duration::from_millis(1)
        ));
        assert!(failover_due(silent, answered, t0 + FAILOVER_GRACE));
    }

    #[test]
    fn failover_needs_an_answer() {
        let t0 = Instant::now();
        let silent = Phase::Silent { since: t0 };
        let late = t0 + Duration::from_secs(60);
        for p in [
            ProbeState::Idle,
            ProbeState::Pending { sent: t0 },
            ProbeState::Failed { at: t0 },
        ] {
            assert!(!failover_due(silent, p, late), "{p:?}");
        }
    }

    #[test]
    fn a_frame_before_the_grace_ends_cancels_the_failover() {
        // `Live` is what a frame on the primary produces. However the probe
        // went, a primary that answered is not failed over from.
        let t0 = Instant::now();
        assert!(!failover_due(
            Phase::Live,
            ProbeState::Answered { sent: t0 },
            t0 + FAILOVER_GRACE
        ));
    }

    #[test]
    fn recovering_still_fails_over() {
        // An answer that lands after the 20 s escalation is still an answer.
        let t0 = Instant::now();
        let rec = Phase::Recovering {
            attempt: 0,
            next_try: t0,
        };
        assert!(failover_due(
            rec,
            ProbeState::Answered { sent: t0 },
            t0 + FAILOVER_GRACE
        ));
    }

    #[test]
    fn the_standby_backoff_climbs_and_then_holds() {
        let s: Vec<u64> = (0..6).map(|n| standby_backoff(n).as_secs()).collect();
        assert_eq!(s, vec![30, 60, 120, 300, 300, 300]);
    }

    /// `retune` acts on the next `evaluate`, both timings: an outage needs
    /// the new `silent_after` of owing to begin, and the new `rebuild_after`
    /// of silence to escalate.
    #[test]
    fn retuned_timings_act_on_the_next_evaluate() {
        let t0 = Instant::now();
        let mut state = LinkState::new(t0);
        state.retune(Duration::from_secs(5), Duration::from_secs(30));
        assert_eq!(state.rebuild_after(), Duration::from_secs(30));
        assert_eq!(state.evaluate(t0, true), Phase::Live);
        assert_eq!(
            state.evaluate(t0 + SILENT_AFTER, true),
            Phase::Live,
            "the old silent_after"
        );
        assert!(matches!(
            state.evaluate(t0 + Duration::from_secs(5), true),
            Phase::Silent { .. }
        ));
        assert!(
            matches!(
                state.evaluate(t0 + REBUILD_AFTER, true),
                Phase::Silent { .. }
            ),
            "escalated at the old rebuild_after"
        );
        // Lowered mid-outage: the silence already past it escalates at once.
        state.retune(Duration::from_secs(5), Duration::from_secs(10));
        assert!(matches!(
            state.evaluate(t0 + REBUILD_AFTER, true),
            Phase::Recovering { .. }
        ));
    }
}

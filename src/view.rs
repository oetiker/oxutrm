//! What the status popup says, assembled from what the session knows.
//!
//! Pure: [`build`] takes plain facts and returns a [`PopupView`], so every
//! word of the popup is tested without a network or a terminal. Every number
//! in it that moves by itself is in whole seconds, which is what lets the
//! loop compare a freshly built view with the one on the screen and repaint
//! only when a shown whole-second value, or another shown fact, changes.
//! Countdowns and clocks can turn over on different sub-second phases, so
//! that is not a promise of one repaint a second. The log's times are
//! wall-clock minutes in the zone [`Facts::zone`] names, so they move only
//! when an entry does.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant, SystemTime};

use jiff::tz::TimeZone;
use oxutrm_client::{KeyHint, Marker, PopupView, Row, legible, rung_label, summarised};
use oxutrm_proto::PathDescription;

use crate::activity::Activity;
use crate::linkstate::{PROBE_RETRY, Phase, ProbeState, render_held};
use crate::quality::Quality;

/// The most log lines a view carries: the tallest box has fewer rows than
/// this, so building more would only be work for the layout to throw away.
pub(crate) const MAX_LOG_LINES: usize = 22;

/// Which session this is: the ssh target typed, and the host's name for the
/// session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) target: String,
    pub(crate) session_id: String,
}

pub(crate) struct StandbyFacts<'a> {
    pub(crate) path: Option<&'a PathDescription>,
    pub(crate) rtt: Option<Duration>,
    pub(crate) probe: ProbeState,
    pub(crate) searching: bool,
    pub(crate) next_search: Instant,
    /// Whether the last search found nothing. Its reason is for the file:
    /// the popup says only [`NO_SECOND_PATH`].
    pub(crate) last_search_failed: bool,
}

/// The ssh rebuild, for a session that has one to fall back on.
pub(crate) struct RebuildFacts<'a> {
    /// When the attempt now running began; `None` while none is.
    pub(crate) running_since: Option<Instant>,
    /// Why the last attempt failed.
    pub(crate) last_failure: Option<&'a str>,
    /// Whether the standby was switched in and, so far, that ended the
    /// outage: an attempt still shown would be one begun before the switch.
    pub(crate) switched: bool,
}

pub(crate) struct Facts<'a> {
    pub(crate) identity: Option<&'a Identity>,
    pub(crate) phase: Phase,
    /// How long the outage was, while the popup lingers after it.
    pub(crate) lingering: Option<Duration>,
    pub(crate) last_heard: Instant,
    /// The silence after which a rebuild starts: the effective
    /// `recovery.rebuild_after`, for the countdown.
    pub(crate) rebuild_after: Duration,
    /// The primary's path, as announced or as last swapped in.
    pub(crate) path: Option<&'a PathDescription>,
    pub(crate) quality: &'a Quality,
    pub(crate) rejected: u64,
    /// `None` for a host that offered no standby.
    pub(crate) standby: Option<StandbyFacts<'a>>,
    /// `None` for a session with nothing to rebuild through.
    pub(crate) rebuild: Option<RebuildFacts<'a>>,
    pub(crate) held: &'a [u8],
    pub(crate) held_full: bool,
    pub(crate) activity: &'a Activity,
    pub(crate) now: Instant,
    /// The zone the log's times are shown in: the system's in a session,
    /// a fixed one in a test.
    pub(crate) zone: &'a TimeZone,
}

pub(crate) fn build(f: &Facts<'_>) -> PopupView {
    let (marker, marker_text) = marker(f);
    PopupView {
        line: line(f.phase, &marker_text),
        title: title(f.identity),
        marker,
        marker_text,
        header: header(f),
        attempts: attempts(f),
        held: held(f),
        rtt: rtt(f),
        spark: f.quality.sparkline(),
        quality: quality(f),
        standby: f
            .standby
            .as_ref()
            .map_or_else(Vec::new, |s| standby(s, f.now)),
        log: log(f.activity, f.zone),
        keys: keys(f.phase),
    }
}

/// `oxutrm · <target> · <session-id prefix>`.
fn title(identity: Option<&Identity>) -> String {
    let Some(id) = identity else {
        return "oxutrm".to_string();
    };
    let mut t = format!("oxutrm \u{b7} {}", legible(&id.target));
    let prefix: String = id.session_id.chars().take(8).collect();
    if !prefix.is_empty() {
        t.push_str(&format!(" \u{b7} {}", legible(&prefix)));
    }
    t
}

/// The one line a screen too small for the box shows, longest first. Under
/// `Confirming` every key is the question's until it is answered, so the
/// line carries the question and, at the least, the keys that answer it.
fn line(phase: Phase, marker_text: &str) -> Vec<String> {
    let marker = format!("oxutrm: {marker_text}");
    if phase != Phase::Confirming {
        return vec![marker];
    }
    let keys = "s send / d drop";
    vec![
        format!("{marker} \u{b7} send typed input? {keys}"),
        format!("oxutrm: send typed input? {keys}"),
        format!("send typed input? {keys}"),
        keys.to_string(),
    ]
}

/// How the primary is reached, in the words the connect line used.
pub(crate) fn path_label(path: Option<&PathDescription>) -> String {
    path.map_or_else(|| "this link".to_string(), rung_label)
}

/// A count of bytes, singular for one: `1 byte`, `10 bytes`.
pub(crate) fn byte_count(n: usize) -> String {
    if n == 1 {
        "1 byte".to_string()
    } else {
        format!("{n} bytes")
    }
}

/// What the popup says of a standby search that found nothing, whatever
/// the reason. The reason names rungs of the connection ladder and what
/// each one hit, which means something in client.log, where all of it goes,
/// and nothing to someone glancing at the popup.
pub(crate) const NO_SECOND_PATH: &str = "no second path found";

/// The front of a reason, up to its first `: `, made legible and short.
///
/// A lost standby's reason is a chain whose first link says what happened
/// and whose rest is for the file, where the whole of it still goes.
pub(crate) fn first_clause(reason: &str) -> String {
    summarised(reason.split(": ").next().unwrap_or(reason))
}

/// A duration in its largest whole unit: `12s`, `4m`, `3h`.
pub(crate) fn age(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// A clock or a countdown, to the whole second: `23 s`, `4 m 30 s`.
/// Truncated, the way every stopwatch reads.
pub(crate) fn clock(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else {
        format!("{} m {} s", s / 60, s % 60)
    }
}

fn marker(f: &Facts<'_>) -> (Marker, String) {
    match (f.phase, f.lingering) {
        (Phase::Live, Some(_)) => (Marker::LiveAgain, "\u{25cf} LIVE again".to_string()),
        (Phase::Silent { .. }, _) => (Marker::Silent, "\u{25cf} SILENT".to_string()),
        (Phase::Recovering { .. }, _) => (Marker::Recovering, "\u{25cf} RECOVERING".to_string()),
        _ => (Marker::Live, "\u{25cf} LIVE".to_string()),
    }
}

/// The rest of the header line: how long the host has been silent, during
/// an outage; how the link is reached, otherwise.
fn header(f: &Facts<'_>) -> String {
    let link = || {
        let q = f.quality;
        format!(
            "{} \u{b7} up {} \u{b7} link {}",
            path_label(f.path),
            age(f.now.saturating_duration_since(q.segment_since())),
            q.segment()
        )
    };
    match (f.phase, f.lingering) {
        (Phase::Silent { since }, _) => {
            format!("silent {}", clock(f.now.saturating_duration_since(since)))
        }
        // `Recovering` carries no `since`; the silence runs from the last
        // frame, as it did through `Silent`.
        (Phase::Recovering { .. }, _) => format!(
            "silent {}",
            clock(f.now.saturating_duration_since(f.last_heard))
        ),
        (Phase::Live, Some(outage)) => {
            format!("outage {:.1} s \u{b7} {}", outage.as_secs_f64(), link())
        }
        _ => link(),
    }
}

/// During an outage, one row per means of recovery: its state and clock.
fn attempts(f: &Facts<'_>) -> Vec<Row> {
    if !f.phase.is_outage() {
        return Vec::new();
    }
    let mut rows = Vec::new();
    if let Some(s) = &f.standby {
        let state = match (s.path, s.probe) {
            (None, _) => "no standby".to_string(),
            (Some(_), ProbeState::Idle) => "not probed".to_string(),
            (Some(_), ProbeState::Pending { .. }) => "probing\u{2026}".to_string(),
            (Some(_), ProbeState::Answered { .. }) => "answered".to_string(),
            (Some(_), ProbeState::Failed { at }) => format!(
                "no answer \u{b7} next probe in {}",
                clock(PROBE_RETRY.saturating_sub(f.now.saturating_duration_since(at)))
            ),
        };
        rows.push(Row::new("standby probe", state));
    }
    let Some(r) = &f.rebuild else {
        return rows;
    };
    let label = "ssh rebuild";
    if r.switched {
        // The switch abandoned any attempt still connecting, and one that
        // had committed would have held it off (ruling B1); either way the
        // outage no longer waits on ssh.
        rows.push(Row::new(label, "not needed \u{b7} switched to standby"));
        return rows;
    }
    match f.phase {
        Phase::Silent { since } => rows.push(Row::new(
            label,
            format!(
                "in {}",
                clock(
                    f.rebuild_after
                        .saturating_sub(f.now.saturating_duration_since(since))
                )
            ),
        )),
        // `attempt` is zero-based and counts the attempts begun: the one
        // running is `attempt + 1` to a person, and once it has failed the
        // counter has moved on, so the one that failed is `attempt`.
        Phase::Recovering { attempt, next_try } => {
            let next = clock(next_try.saturating_duration_since(f.now));
            if let Some(started) = r.running_since {
                rows.push(Row::new(
                    label,
                    format!(
                        "attempt {} \u{b7} running {}",
                        attempt.saturating_add(1),
                        clock(f.now.saturating_duration_since(started))
                    ),
                ));
            } else if attempt > 0 {
                rows.push(Row::new(
                    label,
                    format!("attempt {attempt} failed \u{b7} next in {next}"),
                ));
                if let Some(why) = r.last_failure {
                    rows.push(Row::new("", summarised(why)));
                }
            } else {
                rows.push(Row::new(label, format!("attempt 1 in {next}")));
            }
        }
        _ => {}
    }
    rows
}

fn held(f: &Facts<'_>) -> Vec<String> {
    let n = f.held.len();
    match f.phase {
        // What was observed is a frame arriving; nothing "reconnected" --
        // the connection may never have dropped.
        Phase::Confirming => {
            let mut rows = vec![
                "the host is answering again - deliver what you typed?".to_string(),
                format!("You typed {} while offline:", byte_count(n)),
                render_held(f.held),
            ];
            if f.held_full {
                rows.push("The buffer is full; later keys were not kept.".to_string());
            }
            rows
        }
        // Someone who closed the popup to type into a dead screen cannot
        // tell "kept" from "discarded" until the question comes; opening the
        // popup again shows them here while it still matters. Present tense
        // for the cap: one they hear about afterwards is one they could not
        // act on.
        phase if phase.is_outage() && n > 0 => {
            let mut rows = vec![format!("{} typed since - kept, not sent", byte_count(n))];
            if f.held_full {
                rows.push("The buffer is full; later keys are not being kept.".to_string());
            }
            rows
        }
        _ => Vec::new(),
    }
}

/// `rtt   97 ms  (70–171)`: now, then the range of the last minute. The
/// layout draws the sparkline after it.
fn rtt(f: &Facts<'_>) -> String {
    let q = f.quality;
    // A dash for the whole outage, not only once the newest sample says so:
    // the sampler takes its samples on laps, so the phase can turn first.
    let now_rtt = if f.phase.is_outage() {
        None
    } else {
        q.rtt_now()
    };
    let mut rtt = match now_rtt {
        Some(r) => format!("rtt   {} ms", r.as_millis()),
        None => "rtt   \u{2014}".to_string(),
    };
    if let Some(s) = q.rtt_stats() {
        rtt.push_str(&format!(
            "  ({}\u{2013}{})",
            s.min.as_millis(),
            s.max.as_millis()
        ));
    }
    rtt
}

/// Loss and throughput on one row, once there is something to say; frames
/// that could not be applied on another, once there are any.
fn quality(f: &Facts<'_>) -> Vec<String> {
    let q = f.quality;
    let mut parts = Vec::new();
    if let Some(l) = q.loss() {
        let pct = l
            .percent()
            .map_or_else(|| "\u{2014}".to_string(), |p| format!("{p:.1} %"));
        parts.push(format!("loss  {pct}"));
    }
    if let Some(t) = q.throughput() {
        parts.push(format!(
            "\u{2191} {}  \u{2193} {}",
            rate(t.up),
            rate(t.down)
        ));
    }
    let mut rows = Vec::new();
    if !parts.is_empty() {
        rows.push(parts.join("  "));
    }
    if f.rejected > 0 {
        rows.push(format!("screen frames rejected: {}", f.rejected));
    }
    rows
}

/// The standby section: which standby there is, or when the next search
/// for one is. Its probe is not here: a probe is part of an outage, and the
/// attempts block says how it is going.
fn standby(s: &StandbyFacts<'_>, now: Instant) -> Vec<String> {
    let mut rows = Vec::new();
    match s.path {
        Some(p) => {
            let rtt = s.rtt.map_or_else(
                || "\u{2014}".to_string(),
                |r| format!("{} ms", r.as_millis()),
            );
            rows.push(format!("{} \u{b7} {rtt}", rung_label(p)));
        }
        None if s.searching => rows.push("searching\u{2026}".to_string()),
        None => rows.push(format!(
            "none \u{b7} next search in {}",
            clock(s.next_search.saturating_duration_since(now))
        )),
    }
    if s.path.is_none() && s.last_search_failed {
        rows.push(format!("last search: {NO_SECOND_PATH}"));
    }
    rows
}

/// The log as the popup shows it: details left out, then a run of
/// identical lines -- now next to each other -- folded into one with its
/// count, at its latest time.
fn log(a: &Activity, zone: &TimeZone) -> Vec<Row> {
    let mut lines: Vec<(SystemTime, &str, u32)> = Vec::new();
    for e in a.entries().filter(|e| !e.detail) {
        let n = e.repeats.saturating_add(1);
        match lines.last_mut() {
            Some(last) if last.1 == e.shown => {
                last.0 = e.at;
                last.2 = last.2.saturating_add(n);
            }
            _ => lines.push((e.at, &e.shown, n)),
        }
    }
    let skip = lines.len().saturating_sub(MAX_LOG_LINES);
    lines
        .into_iter()
        .skip(skip)
        .map(|(at, text, n)| {
            let text = if n > 1 {
                format!("{text} \u{d7}{n}")
            } else {
                text.to_string()
            };
            Row::new(hh_mm(at, zone), text)
        })
        .collect()
}

/// `t` as `HH:MM` on the wall clock of `zone`.
fn hh_mm(t: SystemTime, zone: &TimeZone) -> String {
    match jiff::Timestamp::try_from(t) {
        Ok(ts) => {
            let dt = zone.to_datetime(ts);
            format!("{:02}:{:02}", dt.hour(), dt.minute())
        }
        Err(_) => "--:--".to_string(),
    }
}

fn keys(phase: Phase) -> Vec<KeyHint> {
    let hint = |key: &str, label: &str, enabled: bool| KeyHint {
        key: key.to_string(),
        label: label.to_string(),
        enabled,
    };
    match phase {
        // The question closes only when answered. `s` is its answer, so
        // the dimmed `s sessions` would offer one key for two things.
        Phase::Confirming => vec![
            hint("s", "send", true),
            hint("d", "drop", true),
            hint("q", "quit", true),
            hint("c", "config", false),
        ],
        _ => vec![
            hint("Esc", "close", true),
            hint("q", "quit", true),
            hint("c", "config", false),
            hint("s", "sessions", false),
        ],
    }
}

fn rate(bytes_per_second: u64) -> String {
    match bytes_per_second {
        b if b < 1_000 => format!("{b} B/s"),
        b if b < 1_000_000 => format!("{:.1} kB/s", b as f64 / 1e3),
        b => format!("{:.1} MB/s", b as f64 / 1e6),
    }
}

/// Every word of `v` except the log, which is history and carries words
/// from earlier phases legitimately.
#[cfg(test)]
pub(crate) fn words(v: &PopupView) -> String {
    std::iter::once(format!("{} {}", v.marker_text, v.header))
        .chain(v.attempts.iter().map(|r| format!("{} {}", r.label, r.text)))
        .chain(v.held.iter().cloned())
        .chain(std::iter::once(v.rtt.clone()))
        .chain([&v.quality, &v.standby].into_iter().flatten().cloned())
        .chain(v.keys.iter().map(|k| format!("{} {}", k.key, k.label)))
        .chain(v.line.iter().cloned())
        .collect::<Vec<_>>()
        .join(" | ")
}

/// What a view of an outage may not say. The one list: `session.rs`
/// checks its own painted words against it too.
#[cfg(test)]
pub(crate) fn assert_claims_nothing_it_cannot_see(shown: &str) {
    let lower = shown.to_lowercase();
    for claim in [
        "safe",
        "reconnect",
        "retry",
        "keeps running",
        "still running",
        "is running",
    ] {
        assert!(
            !lower.contains(claim),
            "claims {claim:?}, which the client cannot know: {shown}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity::Kind;
    use oxutrm_proto::{NatType, Rung};

    /// The log's zone in every test that does not name one.
    static UTC: TimeZone = TimeZone::UTC;

    fn path() -> PathDescription {
        PathDescription {
            rung: Rung::StunPunch,
            local: "127.0.0.1:1".parse().unwrap(),
            remote: "203.0.113.7:443".parse().unwrap(),
            probes_sent: 0,
            nat_type: NatType::Unknown,
            rtt_ms: 38,
            mtu: 1400,
        }
    }

    fn facts<'a>(q: &'a Quality, a: &'a Activity, phase: Phase, now: Instant) -> Facts<'a> {
        Facts {
            identity: None,
            phase,
            lingering: None,
            last_heard: now,
            rebuild_after: crate::linkstate::REBUILD_AFTER,
            path: None,
            quality: q,
            rejected: 0,
            standby: None,
            rebuild: None,
            held: &[],
            held_full: false,
            activity: a,
            now,
            zone: &UTC,
        }
    }

    fn reading(rtt_ms: u64, sent: u64, lost: u64, tx: u64, rx: u64) -> crate::quality::Reading {
        crate::quality::Reading {
            rtt: Duration::from_millis(rtt_ms),
            sent,
            lost,
            tx_bytes: tx,
            rx_bytes: rx,
        }
    }

    fn secs(t: Instant, s: u64) -> Instant {
        t + Duration::from_secs(s)
    }

    fn standby(path: Option<&PathDescription>, t: Instant) -> StandbyFacts<'_> {
        StandbyFacts {
            path,
            rtt: path.map(|_| Duration::from_millis(41)),
            probe: ProbeState::Idle,
            searching: false,
            next_search: secs(t, 270),
            last_search_failed: false,
        }
    }

    #[test]
    fn the_claims_checker_catches_each_claim() {
        for claim in [
            "safe",
            "reconnect",
            "retry",
            "keeps running",
            "still running",
            "is running",
        ] {
            let caught = std::panic::catch_unwind(|| {
                assert_claims_nothing_it_cannot_see(&format!("x {claim} y"))
            });
            assert!(caught.is_err(), "{claim:?} slipped through");
        }
        assert_claims_nothing_it_cannot_see("silent 3 s");
    }

    fn single_row(v: &PopupView, cols: u16) -> String {
        let o = oxutrm_client::layout_popup(v, oxutrm_proto::TermSize { cols, rows: 5 });
        assert_eq!(o.rows, 1, "not the single-line fallback at {cols}x5");
        o.cells
            .iter()
            .map(|c| c.text.to_string())
            .collect::<String>()
    }

    /// Final review, Minor 1. Below the minimum box the popup is one line,
    /// and under `Confirming` every key is swallowed until `s` or `d`: the
    /// line must say so, at the narrowest fallback size too.
    #[test]
    fn below_the_minimum_the_question_still_shows_its_answer_keys() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let live = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(single_row(&live, 19).trim_end(), "oxutrm: \u{25cf} LIVE");

        let ask = build(&Facts {
            held: b"make test\r",
            ..facts(&q, &a, Phase::Confirming, t)
        });
        assert_eq!(single_row(&ask, 19).trim_end(), "s send / d drop");
        assert_eq!(
            single_row(&ask, 79).trim_end(),
            "oxutrm: \u{25cf} LIVE \u{b7} send typed input? s send / d drop"
        );
    }

    #[test]
    fn the_marker_follows_the_phase() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        for (phase, marker, text) in [
            (Phase::Live, Marker::Live, "\u{25cf} LIVE"),
            (Phase::Confirming, Marker::Live, "\u{25cf} LIVE"),
            (
                Phase::Silent { since: t },
                Marker::Silent,
                "\u{25cf} SILENT",
            ),
            (
                Phase::Recovering {
                    attempt: 0,
                    next_try: t,
                },
                Marker::Recovering,
                "\u{25cf} RECOVERING",
            ),
        ] {
            let v = build(&facts(&q, &a, phase, t));
            assert_eq!(
                (v.marker, v.marker_text.as_str()),
                (marker, text),
                "{phase:?}"
            );
        }
    }

    #[test]
    fn held_typing_is_reported_during_the_outage_and_asked_about_after() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let typed = b"make test\r";
        let silent = Phase::Silent { since: t };
        assert!(
            build(&facts(&q, &a, silent, t)).held.is_empty(),
            "a held section with nothing held"
        );

        let v = build(&Facts {
            held: typed,
            ..facts(&q, &a, silent, t)
        });
        assert_eq!(v.held, ["10 bytes typed since - kept, not sent"]);
        let full = build(&Facts {
            held: typed,
            held_full: true,
            ..facts(&q, &a, silent, t)
        });
        assert_eq!(
            full.held.last().map(String::as_str),
            Some("The buffer is full; later keys are not being kept.")
        );

        let ask = build(&Facts {
            held: typed,
            ..facts(&q, &a, Phase::Confirming, t)
        });
        assert_eq!(
            ask.held,
            [
                "the host is answering again - deliver what you typed?",
                "You typed 10 bytes while offline:",
                "make test\u{21b5}",
            ]
        );
    }

    /// Final review, Minor 6: one byte is "1 byte", in both held texts.
    #[test]
    fn one_held_byte_is_one_byte() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let kept = build(&Facts {
            held: b"x",
            ..facts(&q, &a, Phase::Silent { since: t }, t)
        });
        assert_eq!(kept.held, ["1 byte typed since - kept, not sent"]);
        let ask = build(&Facts {
            held: b"x",
            ..facts(&q, &a, Phase::Confirming, t)
        });
        assert_eq!(
            ask.held.get(1).map(String::as_str),
            Some("You typed 1 byte while offline:")
        );
        let two = build(&Facts {
            held: b"xy",
            ..facts(&q, &a, Phase::Silent { since: t }, t)
        });
        assert_eq!(two.held, ["2 bytes typed since - kept, not sent"]);
    }

    /// Every bar carries the dimmed `c config`; every bar but the
    /// question's carries the dimmed `s sessions`, because there `s` sends.
    /// The question has no `Esc close`: it closes only when answered.
    #[test]
    fn the_key_bar_offers_what_works_in_each_phase() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let bar = |phase| -> Vec<(String, bool)> {
            build(&facts(&q, &a, phase, t))
                .keys
                .into_iter()
                .map(|k| (format!("{} {}", k.key, k.label), k.enabled))
                .collect()
        };
        let own = |s: &str, on: bool| (s.to_string(), on);
        let shown = [
            own("Esc close", true),
            own("q quit", true),
            own("c config", false),
            own("s sessions", false),
        ];
        for phase in [
            Phase::Live,
            Phase::Silent { since: t },
            Phase::Recovering {
                attempt: 0,
                next_try: t,
            },
        ] {
            assert_eq!(bar(phase), shown, "{phase:?}");
        }
        assert_eq!(
            bar(Phase::Confirming),
            [
                own("s send", true),
                own("d drop", true),
                own("q quit", true),
                own("c config", false)
            ]
        );
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(age(Duration::from_millis(59_999)), "59s");
        assert_eq!(age(Duration::from_secs(60)), "1m");
        assert_eq!(age(Duration::from_secs(3_599)), "59m");
        assert_eq!(age(Duration::from_secs(7_300)), "2h");
    }

    #[test]
    fn the_title_names_the_target_and_the_session() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let bare = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(bare.title, "oxutrm");

        let id = Identity {
            target: "bastion".to_string(),
            session_id: "f00dcafe0123".to_string(),
        };
        let v = build(&Facts {
            identity: Some(&id),
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.title, "oxutrm \u{b7} bastion \u{b7} f00dcafe");
        assert!(
            !words(&v).contains("f00dcafe") && !words(&v).contains("attach"),
            "the session is named in the body too: {}",
            words(&v)
        );
    }

    #[test]
    fn rtt_is_a_dash_for_the_whole_outage_even_if_the_newest_sample_is_not_flagged() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        q.push(t, reading(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), reading(40, 0, 0, 0, 0), false);
        assert!(q.rtt_now().is_some(), "the sample is not flagged");
        for phase in [
            Phase::Silent { since: t },
            Phase::Recovering {
                attempt: 0,
                next_try: t,
            },
        ] {
            let v = build(&facts(&q, &a, phase, secs(t, 2)));
            assert_eq!(v.rtt, "rtt   \u{2014}  (30\u{2013}40)", "{phase:?}");
        }
    }

    /// The loop repaints when the view differs, so a view built from
    /// numbers finer than whole seconds would repaint on every lap. Within
    /// one whole second of every shown clock and countdown -- the silence,
    /// the probe's retry, the rebuild's start, its run and its next try --
    /// it is the same. Their clocks are all pinned to one sub-second phase
    /// here; in a session they need not be.
    #[test]
    fn a_view_is_the_same_for_a_second_at_a_time() {
        let t = Instant::now();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let mut a = Activity::new();
        a.record_at(Kind::Standby, "not found: x", base);
        let mut q = Quality::new(t);
        q.push(t, reading(30, 10, 0, 100, 100), false);
        let p = path();
        let at = |phase: Phase, running: Option<Instant>, ms: u64| {
            build(&Facts {
                last_heard: t,
                standby: Some(StandbyFacts {
                    probe: ProbeState::Failed { at: t },
                    ..standby(Some(&p), t)
                }),
                rebuild: Some(RebuildFacts {
                    running_since: running,
                    last_failure: Some("ssh exited"),
                    switched: false,
                }),
                ..facts(&q, &a, phase, t + Duration::from_millis(ms))
            })
        };
        let silent = Phase::Silent { since: t };
        let recovering = Phase::Recovering {
            attempt: 1,
            next_try: t + Duration::from_secs(10),
        };
        for (phase, running) in [(silent, None), (recovering, None), (recovering, Some(t))] {
            let (before, same, after) = (
                at(phase, running, 2_100),
                at(phase, running, 2_900),
                at(phase, running, 3_100),
            );
            assert_eq!(before, same, "{phase:?} {running:?}");
            // And each clock does move at the whole second: row by row, so
            // the header's `silent N s` turning over cannot stand in for a
            // probe retry or an attempt clock that never moved. Only the
            // reason row under a failed attempt has no clock.
            assert_ne!(before.header, after.header, "{phase:?} {running:?}");
            let clocked: Vec<_> = before
                .attempts
                .iter()
                .zip(&after.attempts)
                .filter(|(b, _)| !b.label.is_empty())
                .collect();
            assert_eq!(clocked.len(), 2, "{phase:?} {running:?}: {before:?}");
            for (b, a) in clocked {
                assert_ne!(
                    b.text, a.text,
                    "{} did not move: {phase:?} {running:?}",
                    b.label
                );
            }
        }
    }

    #[test]
    fn a_lingering_popup_says_how_long_the_link_was_gone_and_how_it_is_reached() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        let v = build(&Facts {
            lingering: Some(Duration::from_millis(4_240)),
            path: Some(&p),
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.marker, Marker::LiveAgain);
        assert_eq!(v.marker_text, "\u{25cf} LIVE again");
        assert_eq!(
            v.header,
            "outage 4.2 s \u{b7} IPv4 punched \u{b7} up 0s \u{b7} link 1"
        );

        // A new outage while lingering is an outage, whatever came before.
        let v = build(&Facts {
            lingering: Some(Duration::from_secs(4)),
            ..facts(&q, &a, Phase::Silent { since: t }, t)
        });
        assert_eq!(v.marker, Marker::Silent);
    }

    /// Truncated: "at least this long", the way every stopwatch reads. A
    /// rounded counter would overstate an outage the user may act on.
    /// `Silent` counts from its own `since`, `Recovering` from the last
    /// frame heard.
    #[test]
    fn the_silence_is_counted_in_whole_seconds_from_when_the_host_went_quiet() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let v = build(&Facts {
            // Not the silence's start: `Silent` must not read this.
            last_heard: secs(t, 3),
            ..facts(
                &q,
                &a,
                Phase::Silent { since: t },
                t + Duration::from_millis(6_900),
            )
        });
        assert_eq!(v.header, "silent 6 s");

        let v = build(&Facts {
            last_heard: t,
            ..facts(
                &q,
                &a,
                Phase::Recovering {
                    attempt: 0,
                    next_try: t,
                },
                t + Duration::from_millis(94_900),
            )
        });
        assert_eq!(v.header, "silent 1 m 34 s");
    }

    #[test]
    fn rtt_is_a_dash_while_the_link_is_down() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        let empty = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(empty.rtt, "rtt   \u{2014}", "a range from no samples");
        q.push(t, reading(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), reading(40, 0, 0, 0, 0), false);
        let up = build(&facts(&q, &a, Phase::Live, secs(t, 1)));
        assert_eq!(up.rtt, "rtt   40 ms  (30\u{2013}40)");

        q.push(secs(t, 2), reading(40, 0, 0, 0, 0), true);
        let down = build(&facts(&q, &a, Phase::Silent { since: t }, secs(t, 2)));
        assert_eq!(down.rtt, "rtt   \u{2014}  (30\u{2013}40)");
    }

    #[test]
    fn loss_and_throughput_share_a_row_once_there_is_something_to_say() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        let empty = build(&facts(&q, &a, Phase::Live, t));
        assert!(empty.quality.is_empty(), "{:?}", empty.quality);

        for i in 0..=5 {
            q.push(
                secs(t, i),
                reading(
                    30,
                    100 + i * 20,
                    i / 5,
                    1_000 + i * 1_000,
                    5_000 + i * 10_000,
                ),
                false,
            );
        }
        let v = build(&facts(&q, &a, Phase::Live, secs(t, 5)));
        assert_eq!(
            v.quality,
            ["loss  1.0 %  \u{2191} 1.0 kB/s  \u{2193} 10.0 kB/s"]
        );
    }

    #[test]
    fn the_header_names_the_path_its_age_and_which_link_it_is() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        q.new_segment(t);
        let p = path();
        let v = build(&Facts {
            path: Some(&p),
            ..facts(&q, &a, Phase::Live, secs(t, 125))
        });
        assert_eq!(v.header, "IPv4 punched \u{b7} up 2m \u{b7} link 2");
        assert_eq!(v.marker_text, "\u{25cf} LIVE");
    }

    #[test]
    fn rejected_frames_are_reported_only_when_there_are_any() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        assert!(!words(&build(&facts(&q, &a, Phase::Live, t))).contains("rejected"));
        let v = build(&Facts {
            rejected: 3,
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.quality, ["screen frames rejected: 3"]);
    }

    #[test]
    fn the_standby_section_says_what_the_standby_is_doing() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        let row = |s: StandbyFacts<'_>| {
            build(&Facts {
                standby: Some(s),
                ..facts(&q, &a, Phase::Live, t)
            })
            .standby
        };

        assert_eq!(row(standby(Some(&p), t)), ["IPv4 punched \u{b7} 41 ms"]);
        for probe in [
            ProbeState::Pending { sent: t },
            ProbeState::Answered { sent: t },
            ProbeState::Failed { at: t },
        ] {
            assert_eq!(
                row(StandbyFacts {
                    probe,
                    ..standby(Some(&p), t)
                }),
                ["IPv4 punched \u{b7} 41 ms"],
                "the probe is said twice: {probe:?}"
            );
        }
        assert_eq!(
            row(StandbyFacts {
                searching: true,
                ..standby(None, t)
            }),
            ["searching\u{2026}"]
        );
        assert_eq!(
            row(standby(None, t)),
            ["none \u{b7} next search in 4 m 30 s"]
        );
        // Under a minute, no `0 m`.
        assert_eq!(
            row(StandbyFacts {
                next_search: secs(t, 23),
                ..standby(None, t)
            }),
            ["none \u{b7} next search in 23 s"]
        );
        assert!(
            build(&facts(&q, &a, Phase::Live, t)).standby.is_empty(),
            "a host with no standby got a section"
        );
    }

    /// A search that found nothing says so in the user's words. Its reason
    /// -- which rung hit what, partly the far end's own words -- is for
    /// client.log and never reaches a cell.
    #[test]
    fn a_failed_search_says_no_second_path_was_found() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let rows = |failed: bool| {
            build(&Facts {
                standby: Some(StandbyFacts {
                    last_search_failed: failed,
                    ..standby(None, t)
                }),
                ..facts(&q, &a, Phase::Live, t)
            })
            .standby
        };
        assert_eq!(
            rows(true),
            [
                "none \u{b7} next search in 4 m 30 s",
                "last search: no second path found"
            ]
        );
        assert_eq!(rows(false), ["none \u{b7} next search in 4 m 30 s"]);
    }

    /// A lost standby's reason is the far end's own words: only its first
    /// clause reaches the popup, escaped; the file has the rest.
    #[test]
    fn a_first_clause_is_cut_at_its_colon_and_escaped() {
        assert_eq!(first_clause("closed by peer: 0: gone"), "closed by peer");
        assert_eq!(first_clause("timed out"), "timed out");
        let hostile = first_clause("\u{1b}[2Jgone\u{9b}1m\nsecond line");
        assert!(hostile.starts_with("^[[2Jgone"), "{hostile:?}");
        assert!(!hostile.chars().any(char::is_control), "{hostile:?}");
        assert!(!hostile.contains("second line"), "{hostile:?}");
    }

    fn rebuild_facts(running_since: Option<Instant>) -> RebuildFacts<'static> {
        RebuildFacts {
            running_since,
            last_failure: None,
            switched: false,
        }
    }

    /// The attempts block of an outage: a row per means of recovery, each
    /// with its own state and clock.
    fn attempts_at(f: Facts<'_>) -> Vec<(String, String)> {
        build(&f)
            .attempts
            .into_iter()
            .map(|r| (r.label, r.text))
            .collect()
    }

    fn pair(label: &str, text: &str) -> (String, String) {
        (label.to_string(), text.to_string())
    }

    #[test]
    fn a_silent_outage_shows_the_probe_and_when_the_rebuild_starts() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        let silent = Phase::Silent { since: t };
        let now = t + Duration::from_millis(6_200);
        let with = |probe: ProbeState| {
            attempts_at(Facts {
                standby: Some(StandbyFacts {
                    probe,
                    ..standby(Some(&p), t)
                }),
                rebuild: Some(rebuild_facts(None)),
                ..facts(&q, &a, silent, now)
            })
        };
        // REBUILD_AFTER is 20 s; 6.2 s of it have gone, 13.8 s remain.
        assert_eq!(
            with(ProbeState::Pending { sent: t }),
            [
                pair("standby probe", "probing\u{2026}"),
                pair("ssh rebuild", "in 13 s")
            ]
        );
        assert_eq!(with(ProbeState::Answered { sent: t })[0].1, "answered");
        assert_eq!(with(ProbeState::Idle)[0].1, "not probed");
        // PROBE_RETRY is 5 s from the failure.
        assert_eq!(
            with(ProbeState::Failed {
                at: t + Duration::from_secs(4)
            })[0]
                .1,
            "no answer \u{b7} next probe in 2 s"
        );
        let gone = attempts_at(Facts {
            standby: Some(standby(None, t)),
            ..facts(&q, &a, silent, now)
        });
        assert_eq!(gone, [pair("standby probe", "no standby")]);
        assert!(
            attempts_at(facts(&q, &a, silent, now)).is_empty(),
            "rows for a standby the host never offered and a rebuild there is none of"
        );
    }

    #[test]
    fn a_recovering_outage_shows_the_attempt_running_failed_or_due() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let now = secs(t, 40);
        let rebuild = |attempt, running: Option<Instant>, failure| {
            attempts_at(Facts {
                last_heard: t,
                rebuild: Some(RebuildFacts {
                    running_since: running,
                    last_failure: failure,
                    switched: false,
                }),
                ..facts(
                    &q,
                    &a,
                    Phase::Recovering {
                        attempt,
                        next_try: now + Duration::from_millis(4_500),
                    },
                    now,
                )
            })
        };
        // Zero-based and counting attempts begun: the one running is the
        // third, to a person.
        assert_eq!(
            rebuild(2, Some(now - Duration::from_millis(14_300)), None),
            [pair("ssh rebuild", "attempt 3 \u{b7} running 14 s")]
        );
        // Once the third has failed the counter has moved on to 3; the one
        // that failed is still the third.
        assert_eq!(
            rebuild(3, None, Some("ssh exited with status 255\nnoise")),
            [
                pair("ssh rebuild", "attempt 3 failed \u{b7} next in 4 s"),
                pair("", "ssh exited with status 255"),
            ]
        );
        assert_eq!(
            rebuild(0, None, None),
            [pair("ssh rebuild", "attempt 1 in 4 s")]
        );
    }

    /// Review M2. Once the standby is switched in, an ssh attempt begun
    /// before the switch may still be running, but it is not what the
    /// outage is waiting on: its row says so instead of showing it as the
    /// current attempt -- with a number the switch has reset.
    #[test]
    fn after_a_switch_the_rebuild_row_shows_no_attempt_as_current() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let now = secs(t, 40);
        for (phase, running) in [
            (
                Phase::Recovering {
                    attempt: 0,
                    next_try: secs(t, 48),
                },
                Some(secs(t, 30)),
            ),
            (Phase::Silent { since: t }, None),
        ] {
            let rows = attempts_at(Facts {
                rebuild: Some(RebuildFacts {
                    running_since: running,
                    last_failure: Some("ssh exited"),
                    switched: true,
                }),
                ..facts(&q, &a, phase, now)
            });
            assert_eq!(
                rows,
                [pair("ssh rebuild", "not needed \u{b7} switched to standby")],
                "{phase:?}"
            );
        }
    }

    /// Outside an outage there is nothing being recovered.
    #[test]
    fn there_are_no_attempts_outside_an_outage() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        for phase in [Phase::Live, Phase::Confirming] {
            let rows = attempts_at(Facts {
                standby: Some(standby(Some(&p), t)),
                rebuild: Some(rebuild_facts(Some(t))),
                ..facts(&q, &a, phase, t)
            });
            assert!(rows.is_empty(), "{phase:?}: {rows:?}");
        }
    }

    #[test]
    fn clocks_read_in_whole_seconds_and_minutes() {
        assert_eq!(clock(Duration::from_millis(59_999)), "59 s");
        assert_eq!(clock(Duration::from_secs(60)), "1 m 0 s");
        assert_eq!(clock(Duration::from_secs(270)), "4 m 30 s");
    }

    /// 2026-09-21T14:13:20Z.
    fn base() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000)
    }

    #[test]
    fn the_log_shows_the_newest_entries_with_their_time_and_count() {
        let t = Instant::now();
        let q = Quality::new(t);
        let mut a = Activity::new();
        for i in 0..30u64 {
            a.record_at(
                Kind::Link,
                &format!("e-{i:02}"),
                base() + Duration::from_secs(i * 60),
            );
        }
        a.record_at(Kind::Link, "e-29", base() + Duration::from_secs(29 * 60));
        let v = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(v.log.len(), MAX_LOG_LINES);
        assert_eq!(v.log.first(), Some(&Row::new("14:21", "e-08")));
        assert_eq!(v.log.last(), Some(&Row::new("14:42", "e-29 \u{d7}2")));
    }

    /// The times are the wall clock of the zone the facts name, which in a
    /// session is the system's; client.log stays UTC.
    #[test]
    fn the_log_shows_wall_clock_time_in_the_given_zone() {
        let t = Instant::now();
        let q = Quality::new(t);
        let mut a = Activity::new();
        a.record_at(Kind::Link, "e", base());
        let zone = TimeZone::fixed(jiff::tz::offset(2));
        let v = build(&Facts {
            zone: &zone,
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.log, [Row::new("16:13", "e")]);
    }

    /// The steps of an outage are in the file; the popup shows the line
    /// that sums the outage up, in its own wording.
    #[test]
    fn the_log_leaves_details_out_and_uses_the_popups_wording() {
        let t = Instant::now();
        let q = Quality::new(t);
        let mut a = Activity::new();
        a.record_entry(Kind::Link, "silent", None, true, base());
        a.record_entry(Kind::Outage, "3.0 s", Some("outage 3.0 s"), false, base());
        let v = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(v.log, [Row::new("14:13", "outage 3.0 s")]);
    }

    /// Two search failures with a detail between them are neighbours once
    /// the detail is gone, and fold; the same line with a shown one between
    /// them does not.
    #[test]
    fn identical_lines_left_next_to_each_other_fold_with_their_count() {
        let t = Instant::now();
        let q = Quality::new(t);
        let mut a = Activity::new();
        let at = |s: u64| base() + Duration::from_secs(s);
        let not_found = |a: &mut Activity, s| {
            a.record_entry(
                Kind::Standby,
                "not found: x",
                Some("standby not found: x"),
                false,
                at(s),
            );
        };
        not_found(&mut a, 0);
        not_found(&mut a, 30);
        a.record_entry(Kind::Standby, "search started", None, true, at(60));
        not_found(&mut a, 90);
        a.record_at(Kind::Input, "held input sent (1 byte)", at(100));
        not_found(&mut a, 400);
        let v = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(
            v.log,
            [
                Row::new("14:14", "standby not found: x \u{d7}3"),
                Row::new("14:15", "held input sent (1 byte)"),
                Row::new("14:20", "standby not found: x"),
            ]
        );
    }

    /// In `Silent` and `Confirming` nothing is reconnecting -- the rebuild
    /// starts only in `Recovering` -- and from here a dead network and a
    /// crashed host look the same, so those views may not vouch for the far
    /// end, even hedged. Checked with every row they can carry: a standby
    /// whose probe failed, a rebuild waiting to start, held input.
    #[test]
    fn the_views_of_an_outage_claim_nothing_the_client_cannot_see() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        for phase in [Phase::Silent { since: t }, Phase::Confirming] {
            let v = build(&Facts {
                held: b"x",
                standby: Some(StandbyFacts {
                    probe: ProbeState::Failed { at: t },
                    ..standby(Some(&p), t)
                }),
                rebuild: Some(RebuildFacts {
                    running_since: None,
                    last_failure: Some("ssh exited"),
                    switched: false,
                }),
                ..facts(&q, &a, phase, secs(t, 1))
            });
            if matches!(phase, Phase::Silent { .. }) {
                assert_eq!(v.attempts.len(), 2, "the rows are not there to check");
            }
            assert_claims_nothing_it_cannot_see(&words(&v));
        }
    }

    /// The whole popup at 80x24, from facts to cells: the LIVE view after a
    /// failover, and the view of an outage with an ssh attempt running.
    fn screen(f: &Facts<'_>) -> String {
        let o =
            oxutrm_client::layout_popup(&build(f), oxutrm_proto::TermSize { cols: 80, rows: 24 });
        (0..o.rows)
            .map(|r| {
                (0..o.cols)
                    .map(|c| {
                        o.cells[usize::from(r) * usize::from(o.cols) + usize::from(c)]
                            .text
                            .to_string()
                    })
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A session at 22:59 UTC that failed over twice, with what a real one
    /// records: the details of each outage, its summary, the searches.
    fn history() -> Activity {
        let at = |h: u64, m: u64| {
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_035_200 + h * 3600 + m * 60)
        };
        let mut a = Activity::new();
        a.record_entry(Kind::Link, "silent", None, true, at(22, 51));
        a.record_entry(Kind::Rebuild, "attempt 1 started", None, true, at(22, 51));
        a.record_entry(
            Kind::Rebuild,
            "attempt 1 failed: ssh exited",
            None,
            true,
            at(22, 52),
        );
        a.record_entry(
            Kind::Failover,
            "switched to standby (IPv4 punched)",
            None,
            true,
            at(22, 53),
        );
        a.record_entry(
            Kind::Outage,
            "96.3 s \u{2192} switched to standby (IPv4 punched) after 1 failed ssh attempt",
            Some("outage 96.3 s \u{2192} switched to standby (IPv4 punched) after 1 failed ssh attempt"),
            false,
            at(22, 53),
        );
        a.record_entry(Kind::Link, "silent", None, true, at(22, 59));
        a.record_entry(Kind::Failover, "probing standby", None, true, at(22, 59));
        a.record_entry(
            Kind::Outage,
            "8.1 s \u{2192} switched to standby (IPv4 punched)",
            Some("outage 8.1 s \u{2192} switched to standby (IPv4 punched)"),
            false,
            at(22, 59),
        );
        a.record_entry(Kind::Standby, "search started", None, true, at(22, 59));
        a.record_entry(
            Kind::Standby,
            "not found: the host gave up: no usable path to the host.",
            Some(NO_SECOND_PATH),
            false,
            at(22, 59),
        );
        a
    }

    fn a_minute_of_rtt(q: &mut Quality, t: Instant) {
        q.new_segment(t);
        for i in 0..60u64 {
            let rtt = [70, 90, 111, 140, 171, 120, 97][(i % 7) as usize];
            q.push(secs(t, i), reading(rtt, 100 + i * 3, 0, 0, 0), false);
        }
    }

    #[test]
    fn snapshot_live_80x24() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        a_minute_of_rtt(&mut q, t);
        let a = history();
        let p = path();
        let id = Identity {
            target: "thinlinc".to_string(),
            session_id: "3ff1218f5e0c".to_string(),
        };
        let now = secs(t, 66);
        insta::assert_snapshot!(screen(&Facts {
            identity: Some(&id),
            path: Some(&p),
            standby: Some(StandbyFacts {
                next_search: now + Duration::from_secs(23),
                last_search_failed: true,
                ..standby(None, t)
            }),
            ..facts(&q, &a, Phase::Live, now)
        }));
    }

    #[test]
    fn snapshot_outage_80x24() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        a_minute_of_rtt(&mut q, t);
        let a = history();
        let p = path();
        let id = Identity {
            target: "thinlinc".to_string(),
            session_id: "3ff1218f5e0c".to_string(),
        };
        let now = secs(t, 94);
        insta::assert_snapshot!(screen(&Facts {
            identity: Some(&id),
            path: Some(&p),
            last_heard: secs(t, 60),
            standby: Some(StandbyFacts {
                probe: ProbeState::Failed { at: secs(t, 92) },
                ..standby(Some(&p), t)
            }),
            rebuild: Some(rebuild_facts(Some(secs(t, 80)))),
            held: b"make test\r\n",
            ..facts(
                &q,
                &a,
                Phase::Recovering {
                    attempt: 0,
                    next_try: secs(t, 81),
                },
                now,
            )
        }));
    }
}

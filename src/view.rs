//! What the status popup says, assembled from what the session knows.
//!
//! Pure: [`build`] takes plain facts and returns a [`PopupView`], so every
//! word of the popup is tested without a network or a terminal. Every number
//! in it that moves by itself is in whole seconds, which is what lets the
//! loop compare a freshly built view with the one on the screen and repaint
//! at most once a second while nothing else changes.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::time::{Duration, Instant, SystemTime};

use oxutrm_client::{KeyHint, Marker, PopupView, legible, rung_label, summarised};
use oxutrm_proto::PathDescription;

use crate::activity::Activity;
use crate::linkstate::{Phase, ProbeState, render_held};
use crate::quality::Quality;

/// The most log lines a view carries: the tallest box has fewer rows than
/// this, so building more would only be work for the layout to throw away.
pub(crate) const MAX_LOG_LINES: usize = 22;

/// Which session this is: the ssh target typed, and the host's names for the
/// session and for this attach.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    pub(crate) target: String,
    pub(crate) session_id: String,
    pub(crate) attach_id: u64,
}

pub(crate) struct StandbyFacts<'a> {
    pub(crate) path: Option<&'a PathDescription>,
    pub(crate) rtt: Option<Duration>,
    pub(crate) probe: ProbeState,
    pub(crate) searching: bool,
    pub(crate) next_search: Instant,
    pub(crate) last_failure: Option<&'a str>,
}

pub(crate) struct Facts<'a> {
    pub(crate) identity: Option<&'a Identity>,
    pub(crate) phase: Phase,
    /// How long the outage was, while the popup lingers after it.
    pub(crate) lingering: Option<Duration>,
    pub(crate) last_heard: Instant,
    /// The primary's path, as announced or as last swapped in.
    pub(crate) path: Option<&'a PathDescription>,
    pub(crate) quality: &'a Quality,
    pub(crate) rejected: u64,
    /// `None` for a host that offered no standby.
    pub(crate) standby: Option<StandbyFacts<'a>>,
    /// Why the last rebuild attempt failed.
    pub(crate) rebuild_failure: Option<&'a str>,
    pub(crate) held: &'a [u8],
    pub(crate) held_full: bool,
    pub(crate) activity: &'a Activity,
    pub(crate) now: Instant,
    /// The wall clock, for the log's ages.
    pub(crate) wall: SystemTime,
}

pub(crate) fn build(f: &Facts<'_>) -> PopupView {
    let (marker, marker_text) = marker(f);
    PopupView {
        title: f.identity.map_or_else(
            || "oxutrm".to_string(),
            |i| format!("oxutrm \u{b7} {}", legible(&i.target)),
        ),
        marker,
        marker_text,
        held: held(f),
        recovering: recovering(f),
        status: status(f),
        standby: f
            .standby
            .as_ref()
            .map_or_else(Vec::new, |s| standby(s, f.now)),
        spark: f.quality.sparkline(),
        log: log(f.activity, f.wall),
        keys: keys(f.phase),
    }
}

/// How the primary is reached, in the words the connect line used.
pub(crate) fn path_label(path: Option<&PathDescription>) -> String {
    path.map_or_else(|| "this link".to_string(), rung_label)
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

fn marker(f: &Facts<'_>) -> (Marker, String) {
    match (f.phase, f.lingering) {
        (Phase::Live, Some(outage)) => (
            Marker::LiveAgain,
            format!(
                "\u{25cf} LIVE again via {} \u{b7} outage {:.1} s",
                path_label(f.path),
                outage.as_secs_f64()
            ),
        ),
        (Phase::Silent { .. }, _) => (Marker::Silent, "\u{25cf} SILENT".to_string()),
        (Phase::Recovering { .. }, _) => (Marker::Recovering, "\u{25cf} RECOVERING".to_string()),
        _ => (Marker::Live, "\u{25cf} LIVE".to_string()),
    }
}

fn held(f: &Facts<'_>) -> Vec<String> {
    let n = f.held.len();
    match f.phase {
        // What was observed is a frame arriving; nothing "reconnected" --
        // the connection may never have dropped.
        Phase::Confirming => {
            let mut rows = vec![
                "the host is answering again - deliver what you typed?".to_string(),
                format!("You typed {n} bytes while offline:"),
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
            let mut rows = vec![format!("{n} bytes typed since - kept, not sent")];
            if f.held_full {
                rows.push("The buffer is full; later keys are not being kept.".to_string());
            }
            rows
        }
        _ => Vec::new(),
    }
}

fn recovering(f: &Facts<'_>) -> Vec<String> {
    let Phase::Recovering { attempt, next_try } = f.phase else {
        return Vec::new();
    };
    // `attempt` is zero-based, which is right where it indexes the backoff;
    // a person reading "reconnect attempt 0" would read it as a bug.
    let mut rows = vec![
        format!(
            "host quiet for {}s",
            f.now.saturating_duration_since(f.last_heard).as_secs()
        ),
        format!("reconnect attempt {}", attempt.saturating_add(1)),
        format!(
            "next try in {}s",
            next_try.saturating_duration_since(f.now).as_secs()
        ),
    ];
    if let Some(why) = f.rebuild_failure {
        rows.push(format!("last attempt: {}", summarised(why)));
    }
    rows
}

fn status(f: &Facts<'_>) -> Vec<String> {
    let q = f.quality;
    let mut rows = Vec::new();
    if let Phase::Silent { since } = f.phase {
        // Truncated: "at least this long", as every stopwatch reads.
        rows.push(format!(
            "silent for {}s",
            f.now.saturating_duration_since(since).as_secs()
        ));
    }
    // A dash for the whole outage, not only once the newest sample says so:
    // the sampler takes its samples on laps, so the phase can turn first.
    let now_rtt = if f.phase.is_outage() {
        None
    } else {
        q.rtt_now()
    };
    let mut rtt = match now_rtt {
        Some(r) => format!("rtt {} ms", r.as_millis()),
        None => "rtt \u{2014}".to_string(),
    };
    if let Some(s) = q.rtt_stats() {
        rtt.push_str(&format!(
            " \u{b7} min {} / avg {} / max {} ms",
            s.min.as_millis(),
            s.avg.as_millis(),
            s.max.as_millis()
        ));
    }
    rows.push(rtt);
    if let Some(l) = q.loss() {
        let pct = l
            .percent()
            .map_or_else(|| "\u{2014}".to_string(), |p| format!("{p:.1}%"));
        rows.push(format!(
            "loss {pct} in the last minute \u{b7} sent {} lost {}",
            l.total_sent, l.total_lost
        ));
    }
    if let Some(t) = q.throughput() {
        rows.push(format!("\u{2191} {} \u{2193} {}", rate(t.up), rate(t.down)));
    }
    rows.push(format!(
        "{} \u{b7} link {} of this session, up {}",
        path_label(f.path),
        q.segment(),
        age(f.now.saturating_duration_since(q.segment_since()))
    ));
    if let Some(id) = f.identity {
        let prefix: String = id.session_id.chars().take(8).collect();
        rows.push(format!(
            "session {} \u{b7} attach {}",
            legible(&prefix),
            id.attach_id
        ));
    }
    if f.rejected > 0 {
        rows.push(format!("screen frames rejected: {}", f.rejected));
    }
    rows
}

fn standby(s: &StandbyFacts<'_>, now: Instant) -> Vec<String> {
    let mut rows = Vec::new();
    match s.path {
        Some(p) => {
            let rtt = s.rtt.map_or_else(
                || "\u{2014}".to_string(),
                |r| format!("{} ms", r.as_millis()),
            );
            let probe = match s.probe {
                ProbeState::Idle => "",
                ProbeState::Pending { .. } => " \u{b7} probing",
                ProbeState::Answered { .. } => " \u{b7} probe answered",
                ProbeState::Failed { .. } => " \u{b7} probe failed",
            };
            rows.push(format!("standby {} \u{b7} {rtt}{probe}", rung_label(p)));
        }
        None if s.searching => rows.push("standby: searching\u{2026}".to_string()),
        None => {
            let wait = s.next_search.saturating_duration_since(now).as_secs();
            rows.push(format!(
                "standby: none \u{b7} next search in {} m {} s",
                wait / 60,
                wait % 60
            ));
        }
    }
    if s.path.is_none()
        && let Some(why) = s.last_failure
    {
        rows.push(format!("last search: {}", summarised(why)));
    }
    rows
}

fn log(a: &Activity, wall: SystemTime) -> Vec<String> {
    let skip = a.entries().len().saturating_sub(MAX_LOG_LINES);
    a.entries()
        .skip(skip)
        .map(|e| {
            let age = age(wall.duration_since(e.at).unwrap_or_default());
            let count = if e.repeats > 0 {
                format!(" (\u{d7}{})", e.repeats.saturating_add(1))
            } else {
                String::new()
            };
            format!("{age:>4} {} {}{count}", e.kind.name(), e.text)
        })
        .collect()
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
    std::iter::once(v.marker_text.clone())
        .chain(
            [&v.held, &v.recovering, &v.status, &v.standby]
                .into_iter()
                .flatten()
                .cloned(),
        )
        .chain(v.keys.iter().map(|k| format!("{} {}", k.key, k.label)))
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
            path: None,
            quality: q,
            rejected: 0,
            standby: None,
            rebuild_failure: None,
            held: &[],
            held_full: false,
            activity: a,
            now,
            wall: SystemTime::now(),
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

    #[test]
    fn the_title_names_the_target_and_a_row_names_the_session() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let bare = build(&facts(&q, &a, Phase::Live, t));
        assert_eq!(bare.title, "oxutrm");
        assert!(!words(&bare).contains("session "), "{}", words(&bare));

        let id = Identity {
            target: "bastion".to_string(),
            session_id: "f00dcafe0123".to_string(),
            attach_id: 3,
        };
        let v = build(&Facts {
            identity: Some(&id),
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.title, "oxutrm \u{b7} bastion");
        assert!(
            v.status
                .contains(&"session f00dcafe \u{b7} attach 3".to_string()),
            "{:?}",
            v.status
        );
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
        assert_claims_nothing_it_cannot_see("host quiet for 3s");
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
            assert!(
                v.status
                    .contains(&"rtt \u{2014} \u{b7} min 30 / avg 35 / max 40 ms".to_string()),
                "{phase:?}: {:?}",
                v.status
            );
        }
    }

    /// The loop repaints when the view differs, so a view that changes
    /// faster than once a second repaints faster than once a second.
    #[test]
    fn a_view_is_the_same_for_a_second_at_a_time() {
        let t = Instant::now();
        let a_base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        let mut a = Activity::new();
        a.record_at(Kind::Link, "went quiet", a_base);
        let mut q = Quality::new(t);
        q.push(t, reading(30, 10, 0, 100, 100), false);
        let at = |phase: Phase, ms: u64, wall_ms: u64| {
            build(&Facts {
                last_heard: t,
                wall: a_base + Duration::from_millis(wall_ms),
                ..facts(&q, &a, phase, t + Duration::from_millis(ms))
            })
        };
        let silent = Phase::Silent { since: t };
        let recovering = Phase::Recovering {
            attempt: 1,
            next_try: t + Duration::from_secs(10),
        };
        for phase in [silent, recovering] {
            assert_eq!(
                at(phase, 2_100, 5_100),
                at(phase, 2_900, 5_900),
                "{phase:?}"
            );
            assert_ne!(
                at(phase, 2_100, 5_100),
                at(phase, 3_100, 6_100),
                "{phase:?}"
            );
        }
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
    fn a_lingering_popup_says_how_the_link_came_back_and_how_long_it_was_gone() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let p = path();
        let v = build(&Facts {
            lingering: Some(Duration::from_millis(4_240)),
            path: Some(&p),
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.marker, Marker::LiveAgain);
        assert_eq!(
            v.marker_text,
            "\u{25cf} LIVE again via IPv4 punched \u{b7} outage 4.2 s"
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
    #[test]
    fn the_silence_is_counted_in_whole_seconds_from_when_the_host_went_quiet() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let v = build(&facts(
            &q,
            &a,
            Phase::Silent { since: t },
            t + Duration::from_millis(6_900),
        ));
        assert_eq!(v.status.first().map(String::as_str), Some("silent for 6s"));
    }

    #[test]
    fn rtt_is_a_dash_while_the_link_is_down() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        q.push(t, reading(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), reading(40, 0, 0, 0, 0), false);
        let up = build(&facts(&q, &a, Phase::Live, secs(t, 1)));
        assert!(
            up.status
                .contains(&"rtt 40 ms \u{b7} min 30 / avg 35 / max 40 ms".to_string()),
            "{:?}",
            up.status
        );

        q.push(secs(t, 2), reading(40, 0, 0, 0, 0), true);
        let down = build(&facts(&q, &a, Phase::Silent { since: t }, secs(t, 2)));
        assert!(
            down.status
                .contains(&"rtt \u{2014} \u{b7} min 30 / avg 35 / max 40 ms".to_string()),
            "{:?}",
            down.status
        );
    }

    #[test]
    fn loss_and_throughput_have_a_row_each_once_there_is_something_to_say() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        let empty = build(&facts(&q, &a, Phase::Live, t));
        assert!(!words(&empty).contains("loss"), "{}", words(&empty));

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
        assert!(
            v.status
                .contains(&"loss 1.0% in the last minute \u{b7} sent 200 lost 1".to_string()),
            "{:?}",
            v.status
        );
        assert!(
            v.status
                .contains(&"\u{2191} 1.0 kB/s \u{2193} 10.0 kB/s".to_string()),
            "{:?}",
            v.status
        );
    }

    #[test]
    fn the_path_row_counts_links_and_their_age() {
        let t = Instant::now();
        let a = Activity::new();
        let mut q = Quality::new(t);
        q.new_segment(t);
        let p = path();
        let v = build(&Facts {
            path: Some(&p),
            ..facts(&q, &a, Phase::Live, secs(t, 125))
        });
        assert!(
            v.status
                .contains(&"IPv4 punched \u{b7} link 2 of this session, up 2m".to_string()),
            "{:?}",
            v.status
        );
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
        assert!(v.status.contains(&"screen frames rejected: 3".to_string()));
    }

    fn standby(path: Option<&PathDescription>, t: Instant) -> StandbyFacts<'_> {
        StandbyFacts {
            path,
            rtt: path.map(|_| Duration::from_millis(41)),
            probe: ProbeState::Idle,
            searching: false,
            next_search: secs(t, 270),
            last_failure: None,
        }
    }

    #[test]
    fn the_standby_block_says_what_the_standby_is_doing() {
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

        assert_eq!(
            row(standby(Some(&p), t)),
            ["standby IPv4 punched \u{b7} 41 ms"]
        );
        assert_eq!(
            row(StandbyFacts {
                probe: ProbeState::Pending { sent: t },
                ..standby(Some(&p), t)
            }),
            ["standby IPv4 punched \u{b7} 41 ms \u{b7} probing"]
        );
        assert_eq!(
            row(StandbyFacts {
                probe: ProbeState::Answered { sent: t },
                ..standby(Some(&p), t)
            }),
            ["standby IPv4 punched \u{b7} 41 ms \u{b7} probe answered"]
        );
        assert_eq!(
            row(StandbyFacts {
                probe: ProbeState::Failed { at: t },
                ..standby(Some(&p), t)
            }),
            ["standby IPv4 punched \u{b7} 41 ms \u{b7} probe failed"]
        );
        assert_eq!(
            row(StandbyFacts {
                searching: true,
                ..standby(None, t)
            }),
            ["standby: searching\u{2026}"]
        );
        assert_eq!(
            row(standby(None, t)),
            ["standby: none \u{b7} next search in 4 m 30 s"]
        );
        assert!(
            build(&facts(&q, &a, Phase::Live, t)).standby.is_empty(),
            "a host with no standby got a block"
        );
    }

    /// Review focus 6: the reason is the far end's own words.
    #[test]
    fn a_search_failure_from_the_far_end_is_shown_escaped_and_short() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let s = StandbyFacts {
            last_failure: Some("\u{1b}[2Jgone\u{9b}1m\nsecond line"),
            ..standby(None, t)
        };
        let v = build(&Facts {
            standby: Some(s),
            ..facts(&q, &a, Phase::Live, t)
        });
        let last = v.standby.last().unwrap();
        assert!(last.starts_with("last search: ^[[2Jgone"), "{last:?}");
        assert!(!last.chars().any(char::is_control), "{last:?}");
        assert!(!last.contains("second line"), "{last:?}");
    }

    #[test]
    fn the_recovering_section_reports_quiet_time_attempt_countdown_and_reason() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        let now = secs(t, 23);
        let phase = Phase::Recovering {
            attempt: 2,
            next_try: now + Duration::from_secs(5),
        };
        let bare = build(&Facts {
            last_heard: t,
            ..facts(&q, &a, phase, now)
        });
        // `attempt` is zero-based; the user reads the third attempt as 3.
        assert_eq!(
            bare.recovering,
            [
                "host quiet for 23s",
                "reconnect attempt 3",
                "next try in 5s"
            ]
        );

        let why = build(&Facts {
            last_heard: t,
            rebuild_failure: Some("ssh exited with status 255: Permission denied (publickey)"),
            ..facts(&q, &a, phase, now)
        });
        assert_eq!(
            why.recovering.last().map(String::as_str),
            Some("last attempt: ssh exited with status 255: Permission denied (publickey)")
        );
        assert!(
            build(&facts(&q, &a, Phase::Silent { since: t }, now))
                .recovering
                .is_empty()
        );
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
    fn the_log_shows_the_newest_entries_with_their_age_and_count() {
        let t = Instant::now();
        let q = Quality::new(t);
        let mut a = Activity::new();
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_000);
        for i in 0..30u64 {
            a.record_at(
                Kind::Link,
                &format!("e-{i:02}"),
                base + Duration::from_secs(i),
            );
        }
        a.record_at(Kind::Link, "e-29", base + Duration::from_secs(29));
        let v = build(&Facts {
            wall: base + Duration::from_secs(29),
            ..facts(&q, &a, Phase::Live, t)
        });
        assert_eq!(v.log.len(), MAX_LOG_LINES);
        assert_eq!(v.log.first().map(String::as_str), Some(" 21s link e-08"));
        assert_eq!(
            v.log.last().map(String::as_str),
            Some("  0s link e-29 (\u{d7}2)")
        );
    }

    #[test]
    fn ages_read_in_the_largest_whole_unit() {
        assert_eq!(age(Duration::from_millis(59_999)), "59s");
        assert_eq!(age(Duration::from_secs(60)), "1m");
        assert_eq!(age(Duration::from_secs(3_599)), "59m");
        assert_eq!(age(Duration::from_secs(7_300)), "2h");
    }

    /// In `Silent` and `Confirming` nothing is reconnecting -- the rebuild
    /// starts only in `Recovering` -- and from here a dead network and a
    /// crashed host look the same, so those views may not vouch for the far
    /// end, even hedged.
    #[test]
    fn the_views_of_an_outage_claim_nothing_the_client_cannot_see() {
        let t = Instant::now();
        let (q, a) = (Quality::new(t), Activity::new());
        for phase in [Phase::Silent { since: t }, Phase::Confirming] {
            let shown = words(&build(&Facts {
                held: b"x",
                ..facts(&q, &a, phase, t)
            }))
            .to_lowercase();
            assert_claims_nothing_it_cannot_see(&shown);
        }
    }
}

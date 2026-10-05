//! Link quality: a minute of measurements of the primary link, and the
//! numbers the status popup derives from them.
//!
//! Fed plain values -- the session reads them off quinn -- so nothing here
//! needs a network to test. At most one sample a second, taken on a lap the
//! client loop already runs.

// This runs while a client session owns the screen: nothing here may print,
// or it lands raw on the painted raw-mode terminal.
#![cfg_attr(not(test), deny(clippy::print_stderr, clippy::print_stdout))]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Samples kept: one minute at one a second.
pub(crate) const WINDOW: usize = 60;

/// The least time between two samples.
pub(crate) const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// What throughput is averaged over.
pub(crate) const THROUGHPUT_SPAN: Duration = Duration::from_secs(5);

/// What one connection reports at one moment. Counters are cumulative for
/// that connection and start at zero with it.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct Reading {
    /// quinn's smoothed estimate.
    pub(crate) rtt: Duration,
    pub(crate) sent: u64,
    pub(crate) lost: u64,
    pub(crate) tx_bytes: u64,
    pub(crate) rx_bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Sample {
    at: Instant,
    reading: Reading,
    /// Which link of the session it came from.
    segment: u32,
    /// Taken while the phase was an outage.
    outage: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct RttStats {
    pub(crate) min: Duration,
    pub(crate) max: Duration,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Loss {
    /// Sent and lost across the window, on the current link.
    pub(crate) window_sent: u64,
    pub(crate) window_lost: u64,
}

impl Loss {
    /// Lost as a share of sent across the window; `None` when nothing was
    /// sent, which is not the same as nothing lost.
    pub(crate) fn percent(&self) -> Option<f64> {
        (self.window_sent > 0).then(|| self.window_lost as f64 * 100.0 / self.window_sent as f64)
    }
}

/// Bytes per second, up and down.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Throughput {
    pub(crate) up: u64,
    pub(crate) down: u64,
}

pub(crate) struct Quality {
    ring: VecDeque<Sample>,
    segment: u32,
    segment_since: Instant,
}

impl Quality {
    pub(crate) fn new(now: Instant) -> Quality {
        Quality {
            ring: VecDeque::with_capacity(WINDOW + 1),
            segment: 1,
            segment_since: now,
        }
    }

    pub(crate) fn due(&self, now: Instant) -> bool {
        self.ring
            .back()
            .is_none_or(|s| now.saturating_duration_since(s.at) >= SAMPLE_EVERY)
    }

    pub(crate) fn push(&mut self, at: Instant, reading: Reading, outage: bool) {
        self.ring.push_back(Sample {
            at,
            reading,
            segment: self.segment,
            outage,
        });
        while self.ring.len() > WINDOW {
            self.ring.pop_front();
        }
    }

    /// A different connection carries the session from `now` on: a failover
    /// or a landed rebuild. Its counters start at zero, so nothing is ever
    /// subtracted across the boundary.
    pub(crate) fn new_segment(&mut self, now: Instant) {
        self.segment = self.segment.saturating_add(1);
        self.segment_since = now;
    }

    /// Which link of this session the current one is, counting from 1.
    pub(crate) fn segment(&self) -> u32 {
        self.segment
    }

    pub(crate) fn segment_since(&self) -> Instant {
        self.segment_since
    }

    fn current(&self) -> impl DoubleEndedIterator<Item = &Sample> + '_ {
        let segment = self.segment;
        self.ring.iter().filter(move |s| s.segment == segment)
    }

    /// The newest sample's RTT, or `None` while it was taken in an outage.
    pub(crate) fn rtt_now(&self) -> Option<Duration> {
        self.ring
            .back()
            .filter(|s| s.segment == self.segment && !s.outage)
            .map(|s| s.reading.rtt)
    }

    pub(crate) fn rtt_stats(&self) -> Option<RttStats> {
        let rtts: Vec<Duration> = self
            .current()
            .filter(|s| !s.outage)
            .map(|s| s.reading.rtt)
            .collect();
        let min = *rtts.iter().min()?;
        let max = *rtts.iter().max()?;
        Some(RttStats { min, max })
    }

    pub(crate) fn loss(&self) -> Option<Loss> {
        let first = self.current().next()?;
        let last = self.current().next_back()?;
        Some(Loss {
            window_sent: last.reading.sent.saturating_sub(first.reading.sent),
            window_lost: last.reading.lost.saturating_sub(first.reading.lost),
        })
    }

    pub(crate) fn throughput(&self) -> Option<Throughput> {
        let last = self.current().next_back()?;
        let first = self
            .current()
            .find(|s| last.at.saturating_duration_since(s.at) <= THROUGHPUT_SPAN)?;
        let span = last.at.saturating_duration_since(first.at);
        if span.is_zero() {
            return None;
        }
        let per_second =
            |from: u64, to: u64| (to.saturating_sub(from) as f64 / span.as_secs_f64()) as u64;
        Some(Throughput {
            up: per_second(first.reading.tx_bytes, last.reading.tx_bytes),
            down: per_second(first.reading.rx_bytes, last.reading.rx_bytes),
        })
    }

    /// One value a sample, in milliseconds, oldest first; an outage second
    /// is `None`, drawn as a gap.
    pub(crate) fn sparkline(&self) -> Vec<Option<u64>> {
        self.ring
            .iter()
            .map(|s| (!s.outage).then_some(s.reading.rtt.as_millis() as u64))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(rtt_ms: u64, sent: u64, lost: u64, tx: u64, rx: u64) -> Reading {
        Reading {
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
    fn a_sample_is_due_at_most_once_a_second() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        assert!(q.due(t), "the first sample waited");
        q.push(t, r(30, 0, 0, 0, 0), false);
        assert!(!q.due(t + Duration::from_millis(999)));
        assert!(q.due(secs(t, 1)));
    }

    #[test]
    fn the_ring_holds_one_window() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        for i in 0..100 {
            q.push(secs(t, i), r(i, 0, 0, 0, 0), false);
        }
        let spark = q.sparkline();
        assert_eq!(spark.len(), WINDOW);
        assert_eq!(
            spark.first(),
            Some(&Some(40)),
            "the oldest kept is not the 41st"
        );
    }

    #[test]
    fn rtt_statistics_cover_the_answered_seconds_of_this_link() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), r(40, 0, 0, 0, 0), false);
        q.push(secs(t, 2), r(900, 0, 0, 0, 0), true);
        q.push(secs(t, 3), r(50, 0, 0, 0, 0), false);

        let s = q.rtt_stats().expect("no statistics");
        assert_eq!(s.min, Duration::from_millis(30));
        assert_eq!(
            s.max,
            Duration::from_millis(50),
            "an outage second's RTT counted"
        );
        assert_eq!(q.rtt_now(), Some(Duration::from_millis(50)));
    }

    /// While the link is down quinn's estimate is a stale number, and
    /// showing it as "now" would claim a link that is not there.
    #[test]
    fn there_is_no_rtt_now_during_an_outage_and_its_seconds_are_gaps() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 0, 0, 0, 0), false);
        assert_eq!(q.rtt_now(), Some(Duration::from_millis(30)));
        q.push(secs(t, 1), r(30, 0, 0, 0, 0), true);
        assert_eq!(q.rtt_now(), None);
        assert_eq!(q.sparkline(), vec![Some(30), None]);
    }

    #[test]
    fn loss_is_measured_across_the_window() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 1_000, 10, 0, 0), false);
        q.push(secs(t, 1), r(30, 1_100, 12, 0, 0), false);
        q.push(secs(t, 2), r(30, 1_200, 14, 0, 0), false);
        let l = q.loss().unwrap();
        assert_eq!((l.window_sent, l.window_lost), (200, 4));
        assert_eq!(l.percent(), Some(2.0));
    }

    /// A swapped-in connection starts its counters at zero. A delta taken
    /// across the swap would be a huge negative -- saturated to nonsense --
    /// or the old link's loss charged to the new one.
    #[test]
    fn no_delta_is_ever_taken_across_two_links() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(30, 5_000, 900, 9_000_000, 9_000_000), false);
        q.push(secs(t, 1), r(30, 5_100, 950, 9_100_000, 9_100_000), false);
        assert_eq!(q.segment(), 1);
        q.new_segment(secs(t, 2));
        assert_eq!(q.segment(), 2);
        assert_eq!(q.segment_since(), secs(t, 2));
        assert!(
            q.loss().is_none() && q.rtt_stats().is_none(),
            "the new link inherited samples"
        );

        q.push(secs(t, 2), r(20, 10, 0, 1_000, 2_000), false);
        q.push(secs(t, 3), r(20, 20, 1, 2_000, 4_000), false);
        let l = q.loss().unwrap();
        assert_eq!((l.window_sent, l.window_lost), (10, 1));
        assert_eq!(
            q.throughput(),
            Some(Throughput {
                up: 1_000,
                down: 2_000
            })
        );
        assert_eq!(q.rtt_stats().unwrap().max, Duration::from_millis(20));
    }

    #[test]
    fn throughput_is_averaged_over_the_last_five_seconds() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        // A burst ten seconds ago that must not count.
        q.push(t, r(30, 0, 0, 0, 0), false);
        q.push(secs(t, 1), r(30, 0, 0, 50_000_000, 0), false);
        for i in 2..=11 {
            let base = 50_000_000 + (i - 6).max(0) as u64 * 1_000;
            q.push(
                secs(t, i as u64),
                r(30, 0, 0, base, (i as u64) * 10_000),
                false,
            );
        }
        // tx grows 1000 B/s from second 6 on; rx 10000 B/s throughout.
        assert_eq!(
            q.throughput(),
            Some(Throughput {
                up: 1_000,
                down: 10_000
            })
        );
    }

    #[test]
    fn one_sample_is_not_a_rate() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        assert_eq!(q.throughput(), None);
        q.push(t, r(30, 0, 0, 10, 10), false);
        assert_eq!(q.throughput(), None);
        assert_eq!(q.loss().map(|l| l.percent()), Some(None));
    }

    #[test]
    fn the_sparkline_is_in_milliseconds() {
        let t = Instant::now();
        let mut q = Quality::new(t);
        q.push(t, r(38, 0, 0, 0, 0), false);
        q.push(secs(t, 1), r(1_250, 0, 0, 0, 0), false);
        assert_eq!(q.sparkline(), vec![Some(38), Some(1_250)]);
    }
}

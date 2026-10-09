// SPDX-License-Identifier: Apache-2.0
//! Shared latency instruments, so the Starfire client and the Comet host report
//! the **same quantities in the same units** and their numbers can be summed
//! into one pipeline budget.
//!
//! Three pieces:
//! * [`LatencySeries`] — a fixed-capacity sample window with percentile
//!   summaries. Latency work is decided by the tail, so every stage reports
//!   p50 / p99 / max rather than a mean. Recording never allocates.
//! * [`TransitTracker`] — one-way network delay **above the path's floor**,
//!   derived from the sender's media timestamps. It needs no clock sync between
//!   the two machines: the unknown clock offset is a constant, so it cancels
//!   against the sliding minimum. What is left is queueing + retry delay, which
//!   is exactly the part Wi-Fi adds.
//! * [`media_clock`] — the 90 kHz media timestamp both ends agree on (the RTP
//!   timestamp field; observed on the wire from a Sunshine capture, see
//!   `tests/fixtures/video/stream-hevc.fix`).

use std::fmt;
use std::time::{Duration, Instant};

/// Percentile summary of a [`LatencySeries`], in microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Summary {
    /// Samples recorded over the series' lifetime (not just the window).
    pub count: u64,
    pub min_us: u32,
    pub mean_us: u32,
    pub p50_us: u32,
    pub p95_us: u32,
    pub p99_us: u32,
    pub max_us: u32,
}

impl Summary {
    /// Median in milliseconds.
    pub fn p50_ms(&self) -> f64 {
        self.p50_us as f64 / 1000.0
    }
    /// 99th percentile in milliseconds.
    pub fn p99_ms(&self) -> f64 {
        self.p99_us as f64 / 1000.0
    }
    /// Largest sample in milliseconds.
    pub fn max_ms(&self) -> f64 {
        self.max_us as f64 / 1000.0
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.count == 0 {
            return write!(f, "no samples");
        }
        write!(
            f,
            "p50 {:.2}  p95 {:.2}  p99 {:.2}  max {:.2} ms  (min {:.2}, mean {:.2}, n={})",
            self.p50_us as f64 / 1000.0,
            self.p95_us as f64 / 1000.0,
            self.p99_us as f64 / 1000.0,
            self.max_us as f64 / 1000.0,
            self.min_us as f64 / 1000.0,
            self.mean_us as f64 / 1000.0,
            self.count,
        )
    }
}

/// A window of the most recent latency samples (microseconds). Percentiles are
/// computed over the window; `count` / `min` / `max` / `mean` cover everything
/// recorded since the last [`clear`](LatencySeries::clear).
#[derive(Debug, Clone)]
pub struct LatencySeries {
    window: Vec<u32>,
    cap: usize,
    next: usize,
    count: u64,
    sum_us: u64,
    min_us: u32,
    max_us: u32,
}

impl LatencySeries {
    /// Default window: at 120 fps this is ~34 s of per-frame samples.
    pub const DEFAULT_WINDOW: usize = 4096;

    pub fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW)
    }

    /// A series whose percentiles cover the last `cap` samples (min 1).
    pub fn with_window(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            window: Vec::with_capacity(cap),
            cap,
            next: 0,
            count: 0,
            sum_us: 0,
            min_us: u32::MAX,
            max_us: 0,
        }
    }

    /// Record one sample. Saturates at `u32::MAX` microseconds (~71 minutes).
    pub fn record(&mut self, d: Duration) {
        // Whole microseconds in u64 arithmetic (`as_micros` works in u128).
        // Anything past u32::MAX us saturates in record_us anyway, so clamping
        // the seconds first keeps the multiply from overflowing.
        let secs = d.as_secs().min(u32::MAX as u64);
        self.record_us(secs * 1_000_000 + d.subsec_micros() as u64);
    }

    /// Record one sample given in microseconds.
    pub fn record_us(&mut self, us: u64) {
        let us = us.min(u32::MAX as u64) as u32;
        if self.window.len() < self.cap {
            self.window.push(us);
        } else {
            self.window[self.next] = us;
        }
        // Wrap the ring index with a compare, not a division (`next < cap`).
        self.next += 1;
        if self.next == self.cap {
            self.next = 0;
        }
        self.count += 1;
        self.sum_us += us as u64;
        self.min_us = self.min_us.min(us);
        self.max_us = self.max_us.max(us);
    }

    /// Record the time from `start` to `end`; a negative span records zero.
    pub fn record_span(&mut self, start: Instant, end: Instant) {
        self.record(end.saturating_duration_since(start));
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Forget every sample (start a new measurement window).
    pub fn clear(&mut self) {
        self.window.clear();
        self.next = 0;
        self.count = 0;
        self.sum_us = 0;
        self.min_us = u32::MAX;
        self.max_us = 0;
    }

    /// Percentile summary. Works on a copy of the window, so call it at report
    /// time (once a second), not per frame.
    ///
    /// Three order statistics need no full sort: select the p99 rank, then
    /// select p95 within everything below it, then p50 within everything below
    /// that -- each `select_nth_unstable` is linear, against a full sort's
    /// `n log n` (a 4096-sample window sorted per report).
    pub fn summary(&self) -> Summary {
        if self.count == 0 {
            return Summary::default();
        }
        let mut v = self.window.clone();
        let rank =
            |p: f64| -> usize { (((v.len() - 1) as f64 * p).round() as usize).min(v.len() - 1) };
        let (i50, i95, i99) = (rank(0.50), rank(0.95), rank(0.99));
        let p99 = *v.select_nth_unstable(i99).1;
        // Everything left of i99 is now <= p99 and holds the i99 smallest, so
        // lower ranks can be selected inside that prefix alone.
        let p95 = if i95 == i99 {
            p99
        } else {
            *v[..i99].select_nth_unstable(i95).1
        };
        let p50 = if i50 == i95 {
            p95
        } else {
            *v[..i95].select_nth_unstable(i50).1
        };
        Summary {
            count: self.count,
            min_us: self.min_us,
            mean_us: (self.sum_us / self.count) as u32,
            p50_us: p50,
            p95_us: p95,
            p99_us: p99,
            max_us: self.max_us,
        }
    }
}

impl Default for LatencySeries {
    fn default() -> Self {
        Self::new()
    }
}

/// The 90 kHz media clock carried in the RTP timestamp field of every video
/// packet. The host stamps the frame's capture time; the client only ever looks
/// at *differences*, so the epoch is arbitrary.
pub mod media_clock {
    use std::time::Duration;

    /// Ticks per second of the media clock.
    pub const HZ: u64 = 90_000;

    /// Media-clock ticks for an elapsed duration (wraps at 2^32, ~13.25 h).
    pub fn ticks(elapsed: Duration) -> u32 {
        ((elapsed.as_micros() as u64).wrapping_mul(HZ) / 1_000_000) as u32
    }

    /// Signed microseconds from `earlier` to `later`, correct across the 2^32
    /// wrap as long as the two stamps are within ~6.6 hours of each other.
    pub fn delta_us(earlier: u32, later: u32) -> i64 {
        let ticks = later.wrapping_sub(earlier) as i32 as i64;
        ticks * 1_000_000 / HZ as i64
    }
}

/// One-way delay above the path's best case, from sender media timestamps.
///
/// For each frame the *transit* is `local arrival time − sender timestamp`.
/// The two clocks have an unknown offset, so transit is only meaningful
/// relative to its own minimum: `transit − min(transit over the window)` is the
/// extra delay this frame saw compared with the fastest recent frame — queueing
/// at the access point, Wi-Fi retries, a busy channel. A sliding window (rather
/// than an all-time minimum) keeps crystal drift between the two machines
/// (tens of ppm, i.e. about a millisecond a minute) from leaking into the
/// result.
#[derive(Debug, Clone)]
pub struct TransitTracker {
    /// The floor window in nanoseconds (comparisons run on an integer timeline).
    window_ns: i64,
    epoch: Option<(Instant, u32)>,
    /// Monotonic deque of `(arrival_ns, transit_us)`, arrival in nanoseconds
    /// since the epoch: transit values strictly increase front to back, so the
    /// front is the window minimum. Integer times keep each eviction test a
    /// subtraction instead of an `Instant` difference.
    ///
    /// Held in a `Vec` read from `head`: entries leave the front by advancing
    /// `head` and the dead prefix is dropped once it is half the vector, so
    /// every operation is a plain index (no ring-buffer wrap arithmetic).
    mins: Vec<(i64, i64)>,
    head: usize,
}

impl TransitTracker {
    /// Default floor window. Long enough to always contain an unqueued frame,
    /// short enough that clock drift across it stays well under a millisecond.
    pub const DEFAULT_WINDOW: Duration = Duration::from_secs(10);

    pub fn new() -> Self {
        Self::with_window(Self::DEFAULT_WINDOW)
    }

    pub fn with_window(window: Duration) -> Self {
        Self {
            window_ns: window.as_nanos().min(i64::MAX as u128) as i64,
            epoch: None,
            mins: Vec::new(),
            head: 0,
        }
    }

    /// Feed one frame's sender timestamp and local arrival time; returns that
    /// frame's delay above the window floor (zero for the fastest frame).
    pub fn observe(&mut self, sender_ts: u32, arrival: Instant) -> Duration {
        let (t0, ts0) = *self.epoch.get_or_insert((arrival, sender_ts));
        // Nanoseconds since the epoch in i64 arithmetic (`as_nanos` is u128);
        // the seconds clamp keeps it in range for ~292 years of uptime.
        let since = arrival.saturating_duration_since(t0);
        let local_ns =
            since.as_secs().min(9_000_000_000) as i64 * 1_000_000_000 + since.subsec_nanos() as i64;
        let local_us = local_ns / 1_000;
        let sender_us = media_clock::delta_us(ts0, sender_ts);
        let transit = local_us - sender_us;

        // Same test as `arrival - at > window` on Instants (an arrival before
        // `at` is a negative difference, never past the window).
        while let Some(&(at, _)) = self.mins.get(self.head) {
            if local_ns - at > self.window_ns {
                self.head += 1;
            } else {
                break;
            }
        }
        while self.mins.len() > self.head {
            match self.mins.last() {
                Some(&(_, t)) if t >= transit => {
                    self.mins.pop();
                }
                _ => break,
            }
        }
        if self.head >= 64 && self.head * 2 >= self.mins.len() {
            self.mins.drain(..self.head);
            self.head = 0;
        }
        self.mins.push((local_ns, transit));
        let floor = self.mins.get(self.head).map_or(transit, |&(_, t)| t);
        Duration::from_micros((transit - floor).max(0) as u64)
    }
}

impl Default for TransitTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The tracker's floor equals a brute-force minimum over every frame still
    /// inside the window, across long runs (well past the queue's compaction
    /// threshold) with jittery arrivals and media timestamps.
    #[test]
    fn transit_floor_matches_brute_force_over_long_runs() {
        let t0 = Instant::now();
        let window = Duration::from_millis(500);
        let mut tracker = TransitTracker::with_window(window);
        let mut seen: Vec<(Duration, i64)> = Vec::new();
        let mut x: u64 = 7;
        let mut next = || {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (x >> 33) as u64
        };
        let (mut at, mut ts) = (Duration::ZERO, 0u32);
        for i in 0..5_000u32 {
            at += Duration::from_micros(4_000 + next() % 9_000);
            // A media clock running behind the arrivals on average, with
            // jitter either way: transit mostly rises, so the queue grows well
            // past its compaction threshold.
            ts = ts.wrapping_add(500 + (next() % 400) as u32);
            let got = tracker.observe(ts, t0 + at);
            let transit = at.as_micros() as i64 - media_clock::delta_us(0, ts);
            seen.push((at, transit));
            let floor = seen
                .iter()
                .filter(|&&(a, _)| at - a <= window)
                .map(|&(_, t)| t)
                .min()
                .unwrap_or(transit);
            assert_eq!(
                got,
                Duration::from_micros((transit - floor).max(0) as u64),
                "frame {i}"
            );
        }
    }

    /// `record` stores exactly what `as_micros` (saturated to u32) gives.
    #[test]
    fn record_matches_as_micros() {
        for d in [
            Duration::ZERO,
            Duration::from_nanos(999),
            Duration::from_nanos(1_000),
            Duration::from_micros(1_234_567),
            Duration::from_secs(4_294),
            Duration::from_secs(4_295),
            Duration::new(4_294, 967_295_999),
            Duration::new(4_294, 967_296_000),
            Duration::MAX,
        ] {
            let mut a = LatencySeries::with_window(1);
            a.record(d);
            let want = d.as_micros().min(u32::MAX as u128) as u32;
            assert_eq!(a.summary().max_us, want, "{d:?}");
        }
    }

    /// The selection-based percentiles equal a full sort's, for every window
    /// size from 1 up and for heavily duplicated samples.
    #[test]
    fn summary_percentiles_match_a_full_sort() {
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        for n in (1..=300).chain([1000, 4096, 5000]) {
            for modulo in [7u64, 1_000_000] {
                let mut s = LatencySeries::with_window(4096);
                for _ in 0..n {
                    x = x
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    s.record_us((x >> 33) % modulo);
                }
                let mut sorted = s.window.clone();
                sorted.sort_unstable();
                let at = |p: f64| {
                    sorted[(((sorted.len() - 1) as f64 * p).round() as usize).min(sorted.len() - 1)]
                };
                let got = s.summary();
                assert_eq!(
                    (got.p50_us, got.p95_us, got.p99_us),
                    (at(0.50), at(0.95), at(0.99)),
                    "n {n} modulo {modulo}"
                );
            }
        }
    }

    #[test]
    fn empty_series_summarises_to_zero() {
        let s = LatencySeries::new();
        assert!(s.is_empty());
        assert_eq!(s.summary(), Summary::default());
        assert_eq!(s.summary().to_string(), "no samples");
    }

    #[test]
    fn percentiles_follow_the_distribution() {
        let mut s = LatencySeries::new();
        for us in 1..=1000u64 {
            s.record_us(us);
        }
        let sum = s.summary();
        assert_eq!(sum.count, 1000);
        assert_eq!(sum.min_us, 1);
        assert_eq!(sum.max_us, 1000);
        assert_eq!(sum.mean_us, 500);
        // Nearest-rank on 1..=1000: index round(999*p).
        assert_eq!(sum.p50_us, 501);
        assert_eq!(sum.p95_us, 950);
        assert_eq!(sum.p99_us, 990);
    }

    /// The tail is the point: one slow frame in a hundred must show in `max`
    /// (and p99) while leaving the median untouched.
    #[test]
    fn a_single_outlier_is_visible_in_the_tail_not_the_median() {
        let mut s = LatencySeries::new();
        for _ in 0..99 {
            s.record(Duration::from_micros(900));
        }
        s.record(Duration::from_millis(40));
        let sum = s.summary();
        assert_eq!(sum.p50_us, 900);
        assert_eq!(sum.max_us, 40_000);
        assert_eq!(sum.p99_us, 900, "99 of 100 samples are 900us");
        assert!(
            sum.mean_us > 900,
            "the mean moves, which is why we don't lead with it"
        );
    }

    #[test]
    fn window_keeps_only_the_newest_samples_for_percentiles() {
        let mut s = LatencySeries::with_window(4);
        for us in [1000u64, 1000, 1000, 1000, 10, 20, 30, 40] {
            s.record_us(us);
        }
        let sum = s.summary();
        assert_eq!(sum.count, 8, "count covers everything recorded");
        assert_eq!(sum.max_us, 1000, "lifetime max survives the window");
        assert_eq!(sum.p99_us, 40, "percentiles see only the last four samples");
        assert_eq!(sum.min_us, 10);
    }

    #[test]
    fn clear_starts_a_fresh_window() {
        let mut s = LatencySeries::new();
        s.record_us(5000);
        s.clear();
        assert!(s.is_empty());
        s.record_us(7);
        assert_eq!(s.summary().max_us, 7);
        assert_eq!(s.summary().min_us, 7);
    }

    #[test]
    fn media_clock_round_trips_and_survives_the_wrap() {
        assert_eq!(media_clock::ticks(Duration::from_millis(1000)), 90_000);
        assert_eq!(media_clock::delta_us(0, 1500), 16_666); // one 60 fps frame
        assert_eq!(media_clock::delta_us(1500, 0), -16_666);
        // Across the 2^32 wrap: 10 ticks before, 80 ticks after => 90 ticks = 1 ms.
        assert_eq!(media_clock::delta_us(u32::MAX - 9, 80), 1000);
    }

    /// The property the tracker exists for: a constant clock offset between the
    /// two machines contributes nothing; only delay beyond the best frame shows.
    #[test]
    fn transit_reports_only_delay_above_the_floor() {
        let t0 = Instant::now();
        let frame = |n: u32| 1_000_000u32.wrapping_add(n * 1500); // 60 fps, arbitrary sender epoch
        let at = |n: u32, extra_us: u64| t0 + Duration::from_micros(n as u64 * 16_667 + extra_us);
        let mut tr = TransitTracker::new();
        assert_eq!(tr.observe(frame(0), at(0, 0)), Duration::ZERO);
        // On-time frames stay at ~0 (sub-tick rounding only).
        assert!(tr.observe(frame(1), at(1, 0)) < Duration::from_micros(20));
        // A frame held 7 ms by the network reads 7 ms.
        let late = tr.observe(frame(2), at(2, 7_000));
        assert!(
            (late.as_micros() as i64 - 7_000).abs() < 20,
            "expected ~7 ms above floor, got {late:?}"
        );
        // The next on-time frame is back at the floor.
        assert!(tr.observe(frame(3), at(3, 0)) < Duration::from_micros(20));
    }

    /// A faster frame than any seen so far lowers the floor: it reads zero and
    /// later frames are measured against it.
    #[test]
    fn transit_floor_follows_the_fastest_frame() {
        let t0 = Instant::now();
        let mut tr = TransitTracker::new();
        // First frame arrived 5 ms "late" relative to what the path can do.
        tr.observe(0, t0 + Duration::from_micros(5_000));
        let fast = tr.observe(1500, t0 + Duration::from_micros(16_667));
        assert_eq!(fast, Duration::ZERO, "the faster frame becomes the floor");
        let again = tr.observe(3000, t0 + Duration::from_micros(2 * 16_667 + 5_000));
        assert!(
            (again.as_micros() as i64 - 5_000).abs() < 30,
            "got {again:?}"
        );
    }

    /// Old minima must age out, or clock drift would accumulate into the result.
    #[test]
    fn transit_floor_expires_with_the_window() {
        let t0 = Instant::now();
        let mut tr = TransitTracker::with_window(Duration::from_millis(100));
        tr.observe(0, t0); // floor = 0
                           // 200 ms later every frame carries a steady +3 ms (e.g. drift): once the
                           // old floor has aged out, the steady offset is the new floor and reads 0.
        let ts = media_clock::ticks(Duration::from_millis(200));
        let d1 = tr.observe(ts, t0 + Duration::from_micros(203_000));
        assert!(
            d1 < Duration::from_micros(30),
            "stale floor must not be used: {d1:?}"
        );
    }
}

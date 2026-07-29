//! Render timing: monitrs measuring its own frames (§16.1, §26).
//!
//! §26 requires a system monitor to measure and expose its own overhead, and
//! §16.1 sets the budget this type is measured against: *ordinary frame render
//! below 16 ms at 160×48*. The Inspect screen (§7.5) renders these figures beside
//! the collector's own, which is why the shape mirrors
//! [`monitrs_core::model::TierHealth`].
//!
//! The window is a fixed-size ring: §16.1 also forbids unbounded memory growth
//! over a twelve-hour run, so a percentile must be computed from a bounded sample
//! rather than from every frame ever drawn.

use core::time::Duration;
use std::time::Instant;

/// The §16.1 budget for one ordinary frame at 160×48.
pub const FRAME_BUDGET: Duration = Duration::from_millis(16);

/// How many recent frames the percentile is computed from.
///
/// At the default 100 ms tick this is a few seconds of frames: long enough to be
/// stable, short enough that the figure describes *now* rather than the start-up
/// frame that also had to build the first process table.
pub const TIMING_WINDOW: usize = 64;

/// Frame timing for the current run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderTiming {
    frames: u64,
    last: Duration,
    max: Duration,
    total: Duration,
    slow_frames: u64,
    window: [Duration; TIMING_WINDOW],
    filled: usize,
    next: usize,
    last_at: Option<Instant>,
    last_interval: Option<Duration>,
}

impl Default for RenderTiming {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderTiming {
    /// No frames drawn yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: 0,
            last: Duration::ZERO,
            max: Duration::ZERO,
            total: Duration::ZERO,
            slow_frames: 0,
            window: [Duration::ZERO; TIMING_WINDOW],
            filled: 0,
            next: 0,
            last_at: None,
            last_interval: None,
        }
    }

    /// Records a frame that finished at `at` and took `duration`.
    ///
    /// `at` is monotonic (§8.1): the frame *interval* is what proves there is no
    /// redraw busy loop (§16.1), and a wall clock that jumps would fake one.
    pub(in crate::app) fn record(&mut self, at: Instant, duration: Duration) {
        self.frames = self.frames.saturating_add(1);
        self.last = duration;
        self.max = self.max.max(duration);
        self.total = self.total.saturating_add(duration);
        if duration > FRAME_BUDGET {
            self.slow_frames = self.slow_frames.saturating_add(1);
        }
        if let Some(slot) = self.window.get_mut(self.next) {
            *slot = duration;
        }
        self.next = self.next.saturating_add(1) % TIMING_WINDOW;
        self.filled = self.filled.saturating_add(1).min(TIMING_WINDOW);
        self.last_interval = self
            .last_at
            .map(|previous| at.saturating_duration_since(previous));
        self.last_at = Some(at);
    }

    /// How many frames have been drawn.
    #[must_use]
    pub const fn frames(&self) -> u64 {
        self.frames
    }

    /// The most recent frame's duration.
    #[must_use]
    pub const fn last(&self) -> Duration {
        self.last
    }

    /// The slowest frame of this run.
    #[must_use]
    pub const fn max(&self) -> Duration {
        self.max
    }

    /// How many frames exceeded [`FRAME_BUDGET`].
    #[must_use]
    pub const fn slow_frames(&self) -> u64 {
        self.slow_frames
    }

    /// When the most recent frame finished.
    #[must_use]
    pub const fn last_at(&self) -> Option<Instant> {
        self.last_at
    }

    /// The gap between the two most recent frames.
    ///
    /// `None` until two frames have been drawn. A gap far below the tick interval,
    /// sustained, is the signature of the redraw busy loop §16.1 forbids.
    #[must_use]
    pub const fn last_interval(&self) -> Option<Duration> {
        self.last_interval
    }

    /// The mean frame duration over the whole run.
    #[must_use]
    pub fn mean(&self) -> Option<Duration> {
        let frames = u32::try_from(self.frames).ok().filter(|count| *count > 0)?;
        Some(self.total / frames)
    }

    /// The 95th percentile of the retained window.
    ///
    /// `None` before the first frame; exact for the frames retained rather than an
    /// estimate, because sorting sixty-four durations costs nothing and an
    /// approximation would be one more thing to distrust when a budget is missed.
    #[must_use]
    pub fn p95(&self) -> Option<Duration> {
        if self.filled == 0 {
            return None;
        }
        let mut sorted = [Duration::ZERO; TIMING_WINDOW];
        let slice = self.window.get(..self.filled)?;
        let target = sorted.get_mut(..self.filled)?;
        target.copy_from_slice(slice);
        target.sort_unstable();
        // ceil(0.95 * n) - 1, computed in integers: the highest sample at or below
        // the 95th percentile.
        let rank = (self.filled * 95).div_ceil(100).max(1).saturating_sub(1);
        target.get(rank).copied()
    }

    /// Whether the 95th percentile is inside the §16.1 budget.
    ///
    /// `true` before the first frame: an interface that has not drawn yet has not
    /// missed anything.
    #[must_use]
    pub fn is_within_budget(&self) -> bool {
        self.p95().is_none_or(|p95| p95 <= FRAME_BUDGET)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn nothing_is_reported_before_the_first_frame() {
        let timing = RenderTiming::new();
        assert_eq!(timing.frames(), 0);
        assert_eq!(timing.p95(), None);
        assert_eq!(timing.mean(), None);
        assert_eq!(timing.last_at(), None);
        assert_eq!(timing.last_interval(), None);
        assert!(
            timing.is_within_budget(),
            "an interface that has not drawn has not missed a budget"
        );
    }

    #[test]
    fn frame_intervals_are_measured_between_frames() {
        let start = t0();
        let mut timing = RenderTiming::new();

        timing.record(start, Duration::from_millis(4));
        assert_eq!(timing.last_interval(), None, "one frame has no interval");

        timing.record(start + Duration::from_millis(100), Duration::from_millis(5));
        assert_eq!(timing.last_interval(), Some(Duration::from_millis(100)));
        assert_eq!(timing.frames(), 2);
        assert_eq!(timing.last(), Duration::from_millis(5));
        assert_eq!(timing.max(), Duration::from_millis(5));
        assert_eq!(
            timing.mean(),
            Some(Duration::from_millis(4) + Duration::from_micros(500))
        );
    }

    #[test]
    fn frames_over_the_budget_are_counted() {
        let mut timing = RenderTiming::new();
        timing.record(t0(), Duration::from_millis(4));
        timing.record(t0(), FRAME_BUDGET);
        assert_eq!(
            timing.slow_frames(),
            0,
            "exactly at budget is within budget"
        );
        timing.record(t0(), FRAME_BUDGET + Duration::from_millis(1));
        assert_eq!(timing.slow_frames(), 1);
        assert_eq!(timing.max(), FRAME_BUDGET + Duration::from_millis(1));
    }

    #[test]
    fn the_percentile_comes_from_a_bounded_window() {
        let mut timing = RenderTiming::new();
        // 200 fast frames, then the window can only contain fast ones.
        for _ in 0..(TIMING_WINDOW * 3) {
            timing.record(t0(), Duration::from_millis(2));
        }
        assert_eq!(timing.p95(), Some(Duration::from_millis(2)));
        assert!(timing.is_within_budget());
        assert_eq!(timing.frames(), (TIMING_WINDOW * 3) as u64);

        // A slow frame enters the window; the all-time max keeps it forever.
        timing.record(t0(), Duration::from_millis(90));
        assert_eq!(timing.max(), Duration::from_millis(90));
        assert_eq!(
            timing.p95(),
            Some(Duration::from_millis(2)),
            "one outlier in sixty-four is below the 95th percentile"
        );
    }

    #[test]
    fn a_sustained_overrun_fails_the_budget() {
        let mut timing = RenderTiming::new();
        for _ in 0..TIMING_WINDOW {
            timing.record(t0(), Duration::from_millis(40));
        }
        assert_eq!(timing.p95(), Some(Duration::from_millis(40)));
        assert!(!timing.is_within_budget());
    }

    #[test]
    fn a_single_frame_is_its_own_percentile() {
        let mut timing = RenderTiming::new();
        timing.record(t0(), Duration::from_millis(7));
        assert_eq!(timing.p95(), Some(Duration::from_millis(7)));
        assert_eq!(timing.mean(), Some(Duration::from_millis(7)));
    }

    #[test]
    fn the_percentile_picks_the_upper_end_of_a_mixed_window() {
        let mut timing = RenderTiming::new();
        for index in 0..20u64 {
            // 1..=20 ms, so the 95th percentile is the 19th sample: 19 ms.
            timing.record(t0(), Duration::from_millis(index + 1));
        }
        assert_eq!(timing.p95(), Some(Duration::from_millis(19)));
    }
}

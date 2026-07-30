//! Independent scheduling of the sampling tiers (§8.6).
//!
//! The scheduler is deliberately pure: it takes a monotonic instant and answers
//! which tiers are due. That makes tier timing testable by advancing a fake
//! clock, with no sleeping and no real collector.

use core::time::Duration;
use std::time::Instant;

use monitrs_core::model::Tier;

/// How often each tier runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TierIntervals {
    /// CPU, memory, processes, network and disk counters.
    pub fast: Duration,
    /// Filesystem capacity, static device state, sensors.
    pub medium: Duration,
    /// Users, device lists, static metadata.
    pub slow: Duration,
}

impl Default for TierIntervals {
    /// The §8.6 defaults: 1 s, 5 s, 30 s.
    fn default() -> Self {
        Self {
            fast: Duration::from_secs(1),
            medium: Duration::from_secs(5),
            slow: Duration::from_secs(30),
        }
    }
}

impl TierIntervals {
    /// Derives medium and slow intervals from a configured fast interval.
    ///
    /// The multipliers (5x, 30x) preserve the default relationship, so a user who
    /// sets `interval = "250ms"` gets proportionally faster secondary tiers rather
    /// than a fast tier that has overtaken them. Medium and slow are clamped to
    /// never run *more* often than fast, which would be pointless work.
    #[must_use]
    pub fn derived_from(fast: Duration) -> Self {
        Self {
            fast,
            medium: fast.saturating_mul(5),
            slow: fast.saturating_mul(30),
        }
    }

    /// The interval for one tier. The on-demand tier has none: it runs when the
    /// selection changes, never on a timer.
    #[must_use]
    pub const fn for_tier(&self, tier: Tier) -> Option<Duration> {
        match tier {
            Tier::Fast => Some(self.fast),
            Tier::Medium => Some(self.medium),
            Tier::Slow => Some(self.slow),
            Tier::OnDemand => None,
        }
    }
}

/// Which tiers are due on a given tick.
///
/// A set rather than a single tier because the tiers align periodically: at 5 s
/// both fast and medium are due, and doing them in one pass produces one
/// internally consistent snapshot instead of two partial ones (§10.4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DueTiers {
    fast: bool,
    medium: bool,
    slow: bool,
}

impl DueTiers {
    /// Nothing due.
    pub const NONE: Self = Self {
        fast: false,
        medium: false,
        slow: false,
    };

    /// Every timed tier due, which is the state of the very first tick.
    pub const ALL: Self = Self {
        fast: true,
        medium: true,
        slow: true,
    };

    /// Only the fast tier, which is what four ticks in five actually are.
    ///
    /// Exists for measurement. §16.1 budgets "sample collection below 200 ms p95",
    /// and the tick that budget is about is this one — at the default intervals the
    /// medium tier joins every fifth tick and the slow tier every thirtieth. With
    /// only [`Self::NONE`] and [`Self::ALL`] constructible from outside the crate, a
    /// benchmark or an out-of-crate measurement had to use `ALL` and so measured the
    /// most expensive tick there is, which understates the collector by a wide margin
    /// and makes the budget look tighter than it is.
    ///
    /// [`TierScheduler::due_at`] remains the only thing that decides what is *really*
    /// due; this constructs a set, it does not schedule one.
    #[must_use]
    pub const fn fast_only() -> Self {
        Self {
            fast: true,
            medium: false,
            slow: false,
        }
    }

    /// The fast and medium tiers, which is every fifth tick at the defaults.
    #[must_use]
    pub const fn fast_and_medium() -> Self {
        Self {
            fast: true,
            medium: true,
            slow: false,
        }
    }

    /// Whether a specific tier is due. The on-demand tier is never "due".
    #[must_use]
    pub const fn contains(&self, tier: Tier) -> bool {
        match tier {
            Tier::Fast => self.fast,
            Tier::Medium => self.medium,
            Tier::Slow => self.slow,
            Tier::OnDemand => false,
        }
    }

    /// Whether anything at all is due.
    #[must_use]
    pub const fn any(&self) -> bool {
        self.fast || self.medium || self.slow
    }
}

/// Tracks when each timed tier last ran and decides what is due.
#[derive(Clone, Debug)]
pub struct TierScheduler {
    intervals: TierIntervals,
    last_fast: Option<Instant>,
    last_medium: Option<Instant>,
    last_slow: Option<Instant>,
}

impl TierScheduler {
    /// A scheduler that considers every tier due immediately.
    #[must_use]
    pub const fn new(intervals: TierIntervals) -> Self {
        Self {
            intervals,
            last_fast: None,
            last_medium: None,
            last_slow: None,
        }
    }

    /// The configured intervals.
    #[must_use]
    pub const fn intervals(&self) -> TierIntervals {
        self.intervals
    }

    /// Replaces the intervals, keeping the last-run times.
    ///
    /// Used by config reload. Keeping the timestamps means shortening an interval
    /// makes the tier due sooner rather than resetting its phase, which would
    /// briefly stall collection right after a reload.
    pub const fn set_intervals(&mut self, intervals: TierIntervals) {
        self.intervals = intervals;
    }

    /// Which tiers are due at `now`, without recording that they ran.
    #[must_use]
    pub fn due_at(&self, now: Instant) -> DueTiers {
        DueTiers {
            fast: Self::is_due(self.last_fast, self.intervals.fast, now),
            medium: Self::is_due(self.last_medium, self.intervals.medium, now),
            slow: Self::is_due(self.last_slow, self.intervals.slow, now),
        }
    }

    /// Records that `due` ran at `now`.
    ///
    /// Kept separate from [`Self::due_at`] so a caller that fails to collect does
    /// not have to pretend it succeeded, and so tests can query without mutating.
    pub const fn mark_completed(&mut self, due: DueTiers, now: Instant) {
        if due.fast {
            self.last_fast = Some(now);
        }
        if due.medium {
            self.last_medium = Some(now);
        }
        if due.slow {
            self.last_slow = Some(now);
        }
    }

    /// How long until the soonest tier is due, for the sampler's sleep.
    ///
    /// Returns [`Duration::ZERO`] when something is already due, so a caller that
    /// treats this as a sleep duration cannot accidentally skip a tick. A busy
    /// loop is avoided because the caller marks completion before asking again.
    #[must_use]
    pub fn time_until_next(&self, now: Instant) -> Duration {
        [
            Self::remaining(self.last_fast, self.intervals.fast, now),
            Self::remaining(self.last_medium, self.intervals.medium, now),
            Self::remaining(self.last_slow, self.intervals.slow, now),
        ]
        .into_iter()
        .min()
        .unwrap_or(Duration::ZERO)
    }

    /// The elapsed interval since the fast tier last ran.
    ///
    /// This is the value rate arithmetic must divide by (§8.1) — never the
    /// configured interval, which is a target rather than a measurement.
    #[must_use]
    pub fn elapsed_since_fast(&self, now: Instant) -> Duration {
        self.last_fast
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last))
    }

    fn is_due(last: Option<Instant>, interval: Duration, now: Instant) -> bool {
        match last {
            // Never run: due immediately, so the first frame has data.
            None => true,
            Some(last) => now.saturating_duration_since(last) >= interval,
        }
    }

    fn remaining(last: Option<Instant>, interval: Duration, now: Instant) -> Duration {
        match last {
            None => Duration::ZERO,
            Some(last) => interval.saturating_sub(now.saturating_duration_since(last)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two constructors agree with what the scheduler actually produces.
    ///
    /// They exist for measurement, and a measurement of the wrong tick shape would be
    /// worse than no measurement — so they are pinned against the scheduler rather
    /// than against their own definitions.
    #[test]
    fn the_named_tier_sets_match_what_the_scheduler_produces() {
        let intervals = TierIntervals::derived_from(Duration::from_millis(250));
        let mut scheduler = TierScheduler::new(intervals);
        let start = Instant::now();

        // The first tick is every tier, which `ALL` already names.
        assert_eq!(scheduler.due_at(start), DueTiers::ALL);
        scheduler.mark_completed(DueTiers::ALL, start);

        // One fast interval later, only the fast tier is due.
        let fast_at = start + intervals.for_tier(Tier::Fast).expect("a fast interval");
        assert_eq!(
            scheduler.due_at(fast_at),
            DueTiers::fast_only(),
            "the ordinary tick, and the one §16.1's collection budget is about"
        );

        // And at the medium interval, fast and medium together.
        let medium_at = start + intervals.for_tier(Tier::Medium).expect("a medium interval");
        let due = scheduler.due_at(medium_at);
        assert!(due.contains(Tier::Fast) && due.contains(Tier::Medium));
        assert_eq!(
            due.contains(Tier::Slow),
            DueTiers::fast_and_medium().contains(Tier::Slow),
            "the slow tier is not due at the medium interval"
        );
    }

    fn scheduler() -> TierScheduler {
        TierScheduler::new(TierIntervals::default())
    }

    #[test]
    fn the_first_tick_runs_every_tier_so_the_first_frame_has_data() {
        let now = Instant::now();
        assert_eq!(scheduler().due_at(now), DueTiers::ALL);
    }

    #[test]
    fn a_tier_is_not_due_again_until_its_interval_has_actually_elapsed() {
        let start = Instant::now();
        let mut scheduler = scheduler();
        scheduler.mark_completed(DueTiers::ALL, start);

        let almost = start + Duration::from_millis(999);
        assert!(!scheduler.due_at(almost).any());

        let exactly = start + Duration::from_secs(1);
        let due = scheduler.due_at(exactly);
        assert!(due.contains(Tier::Fast));
        assert!(!due.contains(Tier::Medium));
        assert!(!due.contains(Tier::Slow));
    }

    #[test]
    fn aligned_tiers_are_collected_in_one_pass_for_a_consistent_snapshot() {
        let start = Instant::now();
        let mut scheduler = scheduler();
        scheduler.mark_completed(DueTiers::ALL, start);

        let at_five = start + Duration::from_secs(5);
        let due = scheduler.due_at(at_five);
        assert!(due.contains(Tier::Fast) && due.contains(Tier::Medium));
        assert!(!due.contains(Tier::Slow));

        let at_thirty = start + Duration::from_secs(30);
        let due = scheduler.due_at(at_thirty);
        assert!(due.contains(Tier::Fast) && due.contains(Tier::Medium) && due.contains(Tier::Slow));
    }

    #[test]
    fn the_on_demand_tier_is_never_scheduled_on_a_timer() {
        let now = Instant::now();
        assert!(!scheduler().due_at(now).contains(Tier::OnDemand));
        assert!(DueTiers::ALL.contains(Tier::Fast));
        assert!(!DueTiers::ALL.contains(Tier::OnDemand));
        assert!(TierIntervals::default().for_tier(Tier::OnDemand).is_none());
    }

    #[test]
    fn a_failed_collection_does_not_advance_the_schedule() {
        let start = Instant::now();
        let mut scheduler = scheduler();
        // Observed as due but never marked completed: still due a tick later.
        let _ = scheduler.due_at(start);
        assert!(
            scheduler
                .due_at(start + Duration::from_millis(10))
                .contains(Tier::Fast)
        );
        scheduler.mark_completed(
            DueTiers {
                fast: true,
                medium: false,
                slow: false,
            },
            start,
        );
        assert!(
            !scheduler
                .due_at(start + Duration::from_millis(10))
                .contains(Tier::Fast)
        );
    }

    #[test]
    fn elapsed_comes_from_the_measured_gap_not_the_configured_interval() {
        let start = Instant::now();
        let mut scheduler = scheduler();
        scheduler.mark_completed(DueTiers::ALL, start);

        // The scheduler asked for 1s and the OS delivered 1.4s. Rate arithmetic
        // must see 1.4s (§8.1), which is the whole point of this method.
        let late = start + Duration::from_millis(1_400);
        assert_eq!(
            scheduler.elapsed_since_fast(late),
            Duration::from_millis(1_400)
        );
    }

    #[test]
    fn the_first_elapsed_is_zero_which_marks_the_snapshot_as_warming_up() {
        assert_eq!(
            scheduler().elapsed_since_fast(Instant::now()),
            Duration::ZERO
        );
    }

    #[test]
    fn time_until_next_is_zero_when_something_is_already_due() {
        let now = Instant::now();
        assert_eq!(scheduler().time_until_next(now), Duration::ZERO);
    }

    #[test]
    fn time_until_next_tracks_the_soonest_tier() {
        let start = Instant::now();
        let mut scheduler = scheduler();
        scheduler.mark_completed(DueTiers::ALL, start);
        let after = start + Duration::from_millis(600);
        assert_eq!(scheduler.time_until_next(after), Duration::from_millis(400));
    }

    #[test]
    fn derived_intervals_preserve_the_default_tier_relationship() {
        let intervals = TierIntervals::derived_from(Duration::from_millis(250));
        assert_eq!(intervals.fast, Duration::from_millis(250));
        assert_eq!(intervals.medium, Duration::from_millis(1_250));
        assert_eq!(intervals.slow, Duration::from_millis(7_500));

        let default = TierIntervals::derived_from(Duration::from_secs(1));
        assert_eq!(default, TierIntervals::default());
    }

    #[test]
    fn derived_intervals_do_not_overflow_at_the_configured_maximum() {
        // §8.5 caps the interval at 60s; nothing here may panic on multiplication.
        let intervals = TierIntervals::derived_from(Duration::from_secs(60));
        assert_eq!(intervals.slow, Duration::from_secs(1_800));
        let extreme = TierIntervals::derived_from(Duration::MAX);
        assert_eq!(extreme.slow, Duration::MAX);
    }

    #[test]
    fn shortening_an_interval_on_reload_makes_the_tier_due_sooner_not_later() {
        let start = Instant::now();
        let mut scheduler = TierScheduler::new(TierIntervals::default());
        scheduler.mark_completed(DueTiers::ALL, start);

        let at_400ms = start + Duration::from_millis(400);
        assert!(!scheduler.due_at(at_400ms).contains(Tier::Fast));

        scheduler.set_intervals(TierIntervals::derived_from(Duration::from_millis(250)));
        assert!(
            scheduler.due_at(at_400ms).contains(Tier::Fast),
            "a shorter interval must take effect against the existing timestamp"
        );
    }
}

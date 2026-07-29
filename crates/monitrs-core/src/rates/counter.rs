//! One cumulative OS counter turned into a validated rate.
//!
//! Everything in this file exists to keep a single promise from §8.2: an
//! invalid delta yields a typed state, never a huge or negative number.

use core::time::Duration;
use std::time::Instant;

use crate::model::{MetricState, UnavailableReason};
use crate::rates::keyed::DeltaTracker;
use crate::units::Rate;

/// The bit width of a cumulative OS counter (§8.2).
///
/// The width is what separates a *wraparound* from a *reset*. A 32-bit
/// interface counter read on a 64-bit host jumps back to a small value roughly
/// every 34 seconds on a saturated gigabit link; reporting each of those as a
/// reset would blank the interface row almost continuously. When the width is
/// genuinely unknown there is no evidence to tell wrap from reset apart, so the
/// conservative answer applies: report a reset and re-baseline.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum CounterWidth {
    /// The width is not known, so every backwards move is treated as a reset.
    ///
    /// The default, because guessing a width and guessing wrong fabricates
    /// traffic that never happened.
    #[default]
    Unknown,
    /// A 32-bit counter, such as a legacy `ifTable`-style interface counter or a
    /// 32-bit device register widened to `u64` by the caller.
    Bits32,
    /// A 64-bit counter, such as Linux `/proc/net/dev` on a 64-bit kernel.
    ///
    /// Included for completeness: at 100 Gbit/s a 64-bit byte counter takes
    /// over forty years to wrap, so in practice this behaves like
    /// [`CounterWidth::Unknown`] apart from rejecting absurd backwards jumps.
    Bits64,
}

impl CounterWidth {
    /// The number of significant bits, or `None` when the width is unknown.
    #[must_use]
    pub const fn bits(self) -> Option<u32> {
        match self {
            Self::Unknown => None,
            Self::Bits32 => Some(32),
            Self::Bits64 => Some(64),
        }
    }

    /// The largest value the counter can hold, or `None` when unknown.
    #[must_use]
    pub const fn max_value(self) -> Option<u64> {
        match self {
            Self::Unknown => None,
            // Widening casts: a 32-bit counter's ceiling always fits a `u64`.
            Self::Bits32 => Some(u32::MAX as u64),
            Self::Bits64 => Some(u64::MAX),
        }
    }

    /// The counter's modulus, `2^bits`.
    ///
    /// `u128` because a 64-bit counter's modulus does not fit in a `u64`.
    const fn modulus(self) -> Option<u128> {
        match self {
            Self::Unknown => None,
            Self::Bits32 => Some(1u128 << 32),
            Self::Bits64 => Some(1u128 << 64),
        }
    }
}

/// The forward distance the counter travelled, or `None` when the movement
/// cannot be explained without inventing data.
///
/// A backwards move is only read as a wrap when the width is known *and* going
/// forward through the ceiling is a shorter journey than the counter having
/// fallen back — that is, when the apparent drop exceeds half the counter
/// range. Choosing the shorter modular arc has two useful consequences: it is
/// the interpretation that assumes the least, and it bounds a wrapped delta to
/// below half the range, so even a misjudged reset cannot produce an unbounded
/// rate (§8.2). An exactly-half drop is treated as a reset, because it is not
/// evidence of anything.
fn forward_delta(previous: u64, current: u64, width: CounterWidth) -> Option<u64> {
    if current >= previous {
        return Some(current - previous);
    }
    let modulus = width.modulus()?;
    // A reading outside the declared width means the declaration is wrong, and
    // a wrap computed from the wrong modulus is pure fiction.
    if u128::from(previous) >= modulus || u128::from(current) >= modulus {
        return None;
    }
    let backwards = u128::from(previous) - u128::from(current);
    let wrapped = modulus - backwards;
    if wrapped < backwards {
        u64::try_from(wrapped).ok()
    } else {
        None
    }
}

/// What one counter reading means relative to the previous one.
///
/// Returned by [`CounterTracker::observe`] for callers that need the raw
/// movement — running totals such as `NetworkSnapshot::since_launch` accumulate
/// deltas rather than rates. Callers that only want a rate use
/// [`CounterTracker::rate`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CounterDelta {
    /// There was no previous reading, so no delta exists yet.
    ///
    /// §8.2 and §26: the first sample of delta-based data is warming up, and it
    /// is emphatically not zero.
    FirstSample,
    /// A validated, non-negative movement over a measured monotonic interval.
    Advanced {
        /// How far the counter moved forward.
        delta: u64,
        /// The monotonic interval the movement happened in.
        ///
        /// May be [`Duration::ZERO`] when two readings share a timestamp. The
        /// `delta` is still valid as a total in that case, but no rate can be
        /// derived from it (§8.1).
        elapsed: Duration,
        /// Whether the movement was reconstructed from a known-width wrap.
        ///
        /// Exposed so collectors can record a wrap as a health issue instead of
        /// silently trusting a reconstructed delta.
        wrapped: bool,
    },
    /// The counter moved backwards in a way no known width explains.
    ///
    /// The tracker has already re-baselined on the offending reading, so the
    /// *next* reading produces a valid delta rather than a second reset (§8.2).
    Reset,
}

impl CounterDelta {
    /// The rate this delta represents, as a publishable metric state.
    #[must_use]
    pub fn rate(self) -> MetricState<Rate> {
        match self {
            Self::FirstSample => MetricState::WarmingUp,
            Self::Advanced { delta, elapsed, .. } => match Rate::from_delta(delta, elapsed) {
                Some(rate) => MetricState::Available(rate),
                // A zero-length interval carries no rate information at all.
                // §8.2 prefers warming up over a fabricated number, and it is
                // also the honest forecast: the next sample has a real
                // interval.
                None => MetricState::WarmingUp,
            },
            Self::Reset => MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
        }
    }

    /// How far the counter moved, if this reading produced a usable movement.
    ///
    /// Returns `None` for the first sample and for a reset, so a caller
    /// accumulating a running total cannot add an invalid delta to it.
    #[must_use]
    pub const fn advanced_by(self) -> Option<u64> {
        match self {
            Self::Advanced { delta, .. } => Some(delta),
            Self::FirstSample | Self::Reset => None,
        }
    }

    /// Whether this reading was reconstructed from a known-width wraparound.
    #[must_use]
    pub const fn wrapped(self) -> bool {
        matches!(self, Self::Advanced { wrapped: true, .. })
    }
}

/// One cumulative counter's baseline and the rules for reading the next value.
///
/// # Monotonic time
///
/// The tracker stores the [`Instant`] of every reading and derives the interval
/// with [`Instant::saturating_duration_since`]. A wall-clock jump therefore
/// cannot shorten, lengthen, or negate an interval, which is exactly what §8.1
/// requires. Callers **must** pass the snapshot's monotonic `captured_at` and
/// never an `Instant` reconstructed from a `SystemTime`.
///
/// # Example
///
/// ```
/// use core::time::Duration;
/// use std::time::Instant;
///
/// use monitrs_core::rates::{CounterTracker, CounterWidth};
///
/// let mut rx = CounterTracker::new(CounterWidth::Bits64);
/// let start = Instant::now();
///
/// // The first reading establishes a baseline; there is nothing to divide yet.
/// assert!(rx.rate(1_000, start).is_warming_up());
///
/// // 2 000 bytes over half a second is 4 000 B/s, not 2 000 B/s.
/// let state = rx.rate(3_000, start + Duration::from_millis(500));
/// let rate = state.fresh().copied().expect("second sample is measurable");
/// assert_eq!(rate.per_second(), 4_000.0);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct CounterTracker {
    width: CounterWidth,
    last: Option<Reading>,
}

/// One retained counter reading.
#[derive(Clone, Copy, Debug)]
struct Reading {
    value: u64,
    at: Instant,
}

impl CounterTracker {
    /// Builds a tracker with no baseline, for a counter of the given width.
    #[must_use]
    pub const fn new(width: CounterWidth) -> Self {
        Self { width, last: None }
    }

    /// The width this tracker was told to assume.
    #[must_use]
    pub const fn width(&self) -> CounterWidth {
        self.width
    }

    /// Whether the next reading will be the first, and so warming up (§8.2).
    #[must_use]
    pub const fn is_warming_up(&self) -> bool {
        self.last.is_none()
    }

    /// The last accepted reading, or `None` while warming up.
    #[must_use]
    pub const fn last_value(&self) -> Option<u64> {
        match self.last {
            Some(reading) => Some(reading.value),
            None => None,
        }
    }

    /// When the last reading was accepted, or `None` while warming up.
    #[must_use]
    pub const fn last_observed_at(&self) -> Option<Instant> {
        match self.last {
            Some(reading) => Some(reading.at),
            None => None,
        }
    }

    /// Drops the baseline so the next reading warms up again.
    ///
    /// Collectors call this when the thing behind the counter changed identity —
    /// a renamed interface, a re-created device node, a reused PID — because a
    /// delta across such a change describes two different counters (§8.2).
    pub fn forget_baseline(&mut self) {
        self.last = None;
    }

    /// Folds one cumulative reading in and classifies the movement.
    ///
    /// `at` must be monotonic; see the type-level note on time.
    pub fn observe(&mut self, value: u64, at: Instant) -> CounterDelta {
        let Some(previous) = self.last.replace(Reading { value, at }) else {
            return CounterDelta::FirstSample;
        };
        // Saturating rather than checked: `Instant` is monotonic, so a reversed
        // pair means the caller broke the contract. Yielding a zero-length
        // interval degrades to `WarmingUp`, which is safe; panicking in a
        // sampling loop is not (§14.3).
        let elapsed = at.saturating_duration_since(previous.at);
        match forward_delta(previous.value, value, self.width) {
            Some(delta) => CounterDelta::Advanced {
                delta,
                elapsed,
                // The wrap path is the only way a backwards reading survives
                // `forward_delta`.
                wrapped: value < previous.value,
            },
            // The baseline was already replaced above, which is what makes the
            // sample *after* a reset valid (§8.2).
            None => CounterDelta::Reset,
        }
    }

    /// Folds one cumulative reading in and publishes the resulting rate.
    pub fn rate(&mut self, value: u64, at: Instant) -> MetricState<Rate> {
        self.observe(value, at).rate()
    }
}

impl DeltaTracker for CounterTracker {
    type Config = CounterWidth;
    type Reading = u64;
    type Value = Rate;

    fn with_config(config: Self::Config) -> Self {
        Self::new(config)
    }

    fn observe_reading(&mut self, reading: Self::Reading, at: Instant) -> MetricState<Self::Value> {
        self.rate(reading, at)
    }

    // The bodies are written out rather than delegating to the identically named
    // inherent methods, which would be an ambiguous path.
    fn last_observed_at(&self) -> Option<Instant> {
        self.last.map(|reading| reading.at)
    }

    fn forget_baseline(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed origin so every test can express times relative to it.
    fn origin() -> Instant {
        Instant::now()
    }

    /// Rates are calculated in `f64`, so compare within a tolerance: an exact
    /// comparison is both denied by `clippy::float_cmp` and wrong in general,
    /// because an interval such as 1.4 s has no exact binary representation.
    fn assert_rate(state: &MetricState<Rate>, expected: f64) {
        let actual = state
            .fresh()
            .expect("expected a measured rate")
            .per_second();
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}/s, got {actual}/s"
        );
    }

    #[test]
    fn a_first_sample_is_warming_up_and_not_zero() {
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        let state = tracker.rate(4_096, origin());
        assert!(state.is_warming_up());
        assert_eq!(state.fresh(), None);
        assert_ne!(state, MetricState::Available(Rate::ZERO));
    }

    #[test]
    fn a_second_sample_divides_by_the_real_interval() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        assert_eq!(tracker.observe(1_000, t0), CounterDelta::FirstSample);
        let state = tracker.rate(3_000, t0 + Duration::from_secs(2));
        assert_rate(&state, 1_000.0);
    }

    #[test]
    fn the_same_delta_over_different_intervals_gives_different_rates() {
        let t0 = origin();
        let mut fast = CounterTracker::new(CounterWidth::Bits64);
        let mut slow = CounterTracker::new(CounterWidth::Bits64);
        fast.rate(0, t0);
        slow.rate(0, t0);

        let half = fast.rate(1_000, t0 + Duration::from_millis(500));
        let double = slow.rate(1_000, t0 + Duration::from_secs(2));

        assert_rate(&half, 2_000.0);
        assert_rate(&double, 500.0);
    }

    #[test]
    fn a_counter_that_does_not_move_is_a_real_zero_rate() {
        // Distinct from unavailable: the counter was read and genuinely did not
        // advance, which is information.
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        tracker.rate(7_777, t0);
        let state = tracker.rate(7_777, t0 + Duration::from_secs(1));
        assert_eq!(state, MetricState::Available(Rate::ZERO));
    }

    #[test]
    fn zero_elapsed_is_warming_up_rather_than_a_division_by_zero() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        tracker.rate(100, t0);
        let state = tracker.rate(900, t0);
        assert!(state.is_warming_up());
        // The movement itself is still recoverable for running totals.
        let mut totals = CounterTracker::new(CounterWidth::Bits64);
        totals.observe(100, t0);
        assert_eq!(totals.observe(900, t0).advanced_by(), Some(800));
    }

    #[test]
    fn a_reversed_instant_cannot_produce_a_negative_or_huge_rate() {
        // The contract is monotonic time, but a broken caller must degrade
        // safely rather than panic or overflow (§8.1).
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        tracker.rate(0, t0 + Duration::from_secs(10));
        let state = tracker.rate(1_000_000, t0);
        assert!(state.is_warming_up());
    }

    #[test]
    fn a_backwards_counter_of_unknown_width_is_a_typed_reset() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Unknown);
        tracker.rate(9_000_000, t0);
        let state = tracker.rate(12, t0 + Duration::from_secs(1));
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
        assert_eq!(state.fresh(), None);
    }

    #[test]
    fn the_sample_after_a_reset_is_valid_again() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Unknown);
        tracker.rate(9_000_000, t0);
        let reset = tracker.rate(12, t0 + Duration::from_secs(1));
        assert!(!reset.is_available());

        // Re-baselined on the offending reading, so this is a normal delta.
        let recovered = tracker.rate(1_012, t0 + Duration::from_secs(2));
        assert_rate(&recovered, 1_000.0);
    }

    #[test]
    fn a_reset_never_reports_a_rate_derived_from_the_new_value() {
        // The failure this pins down: treating `current - 0` as the delta, which
        // would announce nine megabytes of traffic that never happened.
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Unknown);
        tracker.rate(9_000_000, t0);
        let delta = tracker.observe(12, t0 + Duration::from_secs(1));
        assert_eq!(delta, CounterDelta::Reset);
        assert_eq!(delta.advanced_by(), None);
    }

    #[test]
    fn a_known_width_counter_wraps_instead_of_resetting() {
        // A 32-bit byte counter 300 bytes below its ceiling, plus 1 000 bytes.
        let t0 = origin();
        let ceiling = u64::from(u32::MAX) + 1;
        let previous = ceiling - 300;
        let mut tracker = CounterTracker::new(CounterWidth::Bits32);
        tracker.observe(previous, t0);

        let delta = tracker.observe(700, t0 + Duration::from_secs(1));
        assert_eq!(
            delta,
            CounterDelta::Advanced {
                delta: 1_000,
                elapsed: Duration::from_secs(1),
                wrapped: true,
            }
        );
        assert_rate(&delta.rate(), 1_000.0);
        assert!(delta.wrapped());
    }

    #[test]
    fn the_same_movement_is_a_reset_when_the_width_is_unknown() {
        let t0 = origin();
        let previous = u64::from(u32::MAX) + 1 - 300;
        let mut tracker = CounterTracker::new(CounterWidth::Unknown);
        tracker.observe(previous, t0);
        assert_eq!(
            tracker.observe(700, t0 + Duration::from_secs(1)),
            CounterDelta::Reset
        );
    }

    #[test]
    fn a_wrap_at_the_exact_boundary_is_reconstructed_exactly() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits32);
        tracker.observe(u64::from(u32::MAX), t0);
        assert_eq!(
            tracker
                .observe(0, t0 + Duration::from_secs(1))
                .advanced_by(),
            Some(1),
            "u32::MAX -> 0 is a single step forward"
        );
    }

    #[test]
    fn a_small_backwards_move_is_a_reset_even_at_a_known_width() {
        // Only a drop past half the counter range is evidence of a wrap; a
        // device that re-initialised its counter to a mid-range value is not.
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits32);
        tracker.observe(3_000_000_000, t0);
        assert_eq!(
            tracker.observe(2_999_000_000, t0 + Duration::from_secs(1)),
            CounterDelta::Reset
        );
    }

    #[test]
    fn a_reading_outside_the_declared_width_is_a_reset_not_a_wrap() {
        // The declaration is wrong, so its modulus would fabricate the delta.
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits32);
        tracker.observe(u64::from(u32::MAX) + 5_000, t0);
        assert_eq!(
            tracker.observe(10, t0 + Duration::from_secs(1)),
            CounterDelta::Reset
        );
    }

    #[test]
    fn a_wrapped_delta_can_never_exceed_half_the_counter_range() {
        // The bound that makes a misjudged wrap harmless (§8.2).
        let half = 1u64 << 31;
        let t0 = origin();
        for previous in [u64::from(u32::MAX), 3_000_000_000, half + 1] {
            for current in [0, 1, 1_000, half - 1] {
                let mut tracker = CounterTracker::new(CounterWidth::Bits32);
                tracker.observe(previous, t0);
                if let Some(delta) = tracker
                    .observe(current, t0 + Duration::from_secs(1))
                    .advanced_by()
                    && current < previous
                {
                    assert!(delta < half, "{previous} -> {current} produced {delta}");
                }
            }
        }
    }

    #[test]
    fn a_sixty_four_bit_counter_still_rejects_an_absurd_backwards_jump() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        tracker.observe(1_000_000, t0);
        assert_eq!(
            tracker.observe(9, t0 + Duration::from_secs(1)),
            CounterDelta::Reset
        );
    }

    #[test]
    fn forgetting_the_baseline_makes_the_next_reading_warm_up() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits64);
        tracker.rate(1_000, t0);
        assert!(!tracker.is_warming_up());

        tracker.forget_baseline();
        assert!(tracker.is_warming_up());
        assert_eq!(tracker.last_value(), None);
        assert_eq!(tracker.last_observed_at(), None);
        assert!(
            tracker
                .rate(500_000, t0 + Duration::from_secs(1))
                .is_warming_up(),
            "a dropped baseline must not be reconstructed from the old value"
        );
    }

    #[test]
    fn the_baseline_tracks_the_most_recent_reading() {
        let t0 = origin();
        let at = t0 + Duration::from_secs(3);
        let mut tracker = CounterTracker::new(CounterWidth::Bits32);
        tracker.observe(42, t0);
        tracker.observe(84, at);
        assert_eq!(tracker.last_value(), Some(84));
        assert_eq!(tracker.last_observed_at(), Some(at));
        assert_eq!(tracker.width(), CounterWidth::Bits32);
    }

    #[test]
    fn widths_report_their_own_limits() {
        assert_eq!(CounterWidth::Unknown.bits(), None);
        assert_eq!(CounterWidth::Unknown.max_value(), None);
        assert_eq!(CounterWidth::Bits32.bits(), Some(32));
        assert_eq!(CounterWidth::Bits32.max_value(), Some(u64::from(u32::MAX)));
        assert_eq!(CounterWidth::Bits64.bits(), Some(64));
        assert_eq!(CounterWidth::Bits64.max_value(), Some(u64::MAX));
        assert_eq!(CounterWidth::default(), CounterWidth::Unknown);
    }

    #[test]
    fn a_forward_move_is_never_treated_as_a_wrap() {
        let t0 = origin();
        let mut tracker = CounterTracker::new(CounterWidth::Bits32);
        tracker.observe(10, t0);
        let delta = tracker.observe(4_000_000_000, t0 + Duration::from_secs(1));
        assert_eq!(delta.advanced_by(), Some(3_999_999_990));
        assert!(!delta.wrapped());
    }
}

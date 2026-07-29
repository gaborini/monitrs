//! The bounded in-memory history behind Time Lens (§2.1) and spike attribution
//! (§2.2).
//!
//! # Why this is not "the last N snapshots"
//!
//! §8.5 and §26 both forbid cloning the full process table into every retained
//! sample: at 200 processes and 300 samples that would be 60 000
//! [`ProcessSnapshot`](crate::model::ProcessSnapshot)s kept alive to render a
//! panel that shows ten rows. A [`HistoricalSample`] therefore stores a
//! fixed-size aggregate ([`HistoricalSystemMetrics`]) plus at most `K`
//! contributors for each of four metrics ([`ContributorSet`]). Its size is a
//! function of the configured `K`, never of how many processes the machine runs.
//!
//! # Ordering never depends on the wall clock
//!
//! Every sample carries a `monotonic_offset` measured from the ring's start
//! [`Instant`](std::time::Instant). §8.1 requires that a wall-clock change can
//! never make history move backwards, so eviction, seeking, and comparison are
//! all driven by that offset; `wall_time` exists only to label the selected
//! sample in the header (`sample 22:14:07`, §5.6).
//!
//! # Evidence, not proof
//!
//! Contributor lists and their coverage shares are *correlational*. §2.2 is
//! explicit that this is evidence rather than causal proof, which is why the
//! vocabulary here is "contributors" and "coverage" and why nothing in this
//! module is named after causation.
//!
//! # Unavailable stays unavailable
//!
//! Every aggregate is a [`MetricState`]. A counter
//! reset arrives as [`UnavailableReason::CounterReset`] and is retained as such,
//! so the timeline shows a gap instead of a spike (§21 M4).

mod contributors;
mod ring;
mod sample;
mod view;

pub use contributors::{
    Contributor, ContributorMetric, ContributorSet, ContributorTrend, MAX_RETAINED_COMMAND_WIDTH,
    MAX_RETAINED_NAME_WIDTH, MetricContributors,
};
pub use ring::{
    ClampReason, ClampedValue, DEFAULT_HISTORY_DURATION, DEFAULT_MEMORY_BUDGET_BYTES,
    DEFAULT_SAMPLE_INTERVAL, DEFAULT_TOP_CONTRIBUTORS_PER_METRIC, HistoryClamp, HistoryConfig,
    HistoryField, HistoryLimits, HistoryRing, MAX_HISTORY_DURATION, MAX_MEMORY_BUDGET_BYTES,
    MAX_SAMPLE_INTERVAL, MAX_TOP_CONTRIBUTORS_PER_METRIC, MIN_HISTORY_DURATION,
    MIN_MEMORY_BUDGET_BYTES, MIN_SAMPLE_INTERVAL, RecordOutcome,
};
pub use sample::{HistoricalSample, HistoricalSystemMetrics, HistoryMetric};
pub use view::{
    COMPARISON_LOOKBACK, ComparisonBaseline, HISTORY_STEP_MULTIPLIER, HistoryPosition, HistoryView,
    MetricComparison, MetricComparisons, SeekOutcome,
};

use crate::model::{MetricState, UnavailableReason};

/// How well an unavailable state describes a *group* of readings.
///
/// When several devices contribute to one aggregate and none of them measured
/// anything, the aggregate has to pick one explanation. The order prefers the
/// most actionable one: a permission problem is something the user can fix, a
/// typed transient reason names what happened, and "warming up" or "unsupported"
/// are the least informative. Higher wins.
const fn unavailable_rank<T>(state: &MetricState<T>) -> u8 {
    match state {
        MetricState::PermissionDenied => 4,
        MetricState::TemporarilyUnavailable(_) => 3,
        MetricState::Stale { .. } => 2,
        MetricState::WarmingUp => 1,
        // `Available` never reaches this function; ranking it last keeps the
        // match exhaustive without a panicking branch.
        MetricState::Unsupported | MetricState::Available(_) => 0,
    }
}

/// Keeps whichever of two unavailable states better describes the group.
fn most_representative<T>(
    current: Option<MetricState<T>>,
    candidate: MetricState<T>,
) -> MetricState<T> {
    match current {
        Some(current) if unavailable_rank(&current) >= unavailable_rank(&candidate) => current,
        _ => candidate,
    }
}

/// Re-expresses one metric's unavailability as the unavailability of a value
/// derived from it.
///
/// The type parameter changes because a coverage share is not the same quantity
/// as the reading it was derived from, but the *reason* must survive: §4 forbids
/// turning "permission denied" into 0%.
///
/// A `Stale` or `Available` input becomes
/// [`UnavailableReason::NeedsSecondSample`]: a derived aggregate cannot honestly
/// present one device's retained value as the whole group's total, and what it
/// actually needs is a fresh reading.
fn propagate_unavailable<T, U>(state: MetricState<T>) -> MetricState<U> {
    match state {
        MetricState::Available(_) | MetricState::Stale { .. } => {
            MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample)
        }
        MetricState::WarmingUp => MetricState::WarmingUp,
        MetricState::PermissionDenied => MetricState::PermissionDenied,
        MetricState::Unsupported => MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(reason) => MetricState::TemporarilyUnavailable(reason),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    #[test]
    fn a_permission_problem_outranks_every_other_explanation() {
        let group: MetricState<u64> =
            most_representative(Some(MetricState::WarmingUp), MetricState::PermissionDenied);
        assert_eq!(group, MetricState::PermissionDenied);

        let group: MetricState<u64> = most_representative(
            Some(MetricState::PermissionDenied),
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
        );
        assert_eq!(group, MetricState::PermissionDenied);
    }

    #[test]
    fn a_typed_transient_reason_beats_warming_up_and_unsupported() {
        let reset = MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        let group: MetricState<u64> = most_representative(Some(MetricState::Unsupported), reset);
        assert_eq!(group, reset);
        let group: MetricState<u64> = most_representative(Some(reset), MetricState::WarmingUp);
        assert_eq!(group, reset);
    }

    #[test]
    fn the_first_candidate_is_kept_when_nothing_is_ranked_yet() {
        let group: MetricState<u64> = most_representative(None, MetricState::Unsupported);
        assert_eq!(group, MetricState::Unsupported);
    }

    #[test]
    fn propagation_preserves_the_reason_a_metric_was_missing() {
        let denied: MetricState<u64> = MetricState::PermissionDenied;
        assert_eq!(
            propagate_unavailable::<u64, f32>(denied),
            MetricState::PermissionDenied
        );

        let reset: MetricState<u64> =
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        assert_eq!(
            propagate_unavailable::<u64, f32>(reset),
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );

        assert_eq!(
            propagate_unavailable::<u64, f32>(MetricState::Unsupported),
            MetricState::Unsupported
        );
        assert_eq!(
            propagate_unavailable::<u64, f32>(MetricState::WarmingUp),
            MetricState::WarmingUp
        );
    }

    #[test]
    fn a_retained_stale_reading_never_becomes_a_derived_total() {
        // Presenting one device's stale value as the whole group's total would be
        // a fabricated number; the group needs a fresh reading instead (§4).
        let stale = MetricState::Stale {
            value: 7u64,
            age: Duration::from_secs(4),
        };
        assert_eq!(
            propagate_unavailable::<u64, f32>(stale),
            MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample)
        );
    }
}

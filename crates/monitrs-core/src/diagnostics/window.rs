//! The history a rule is allowed to look at, and how it counts over it.
//!
//! # Why this is not just a [`HistoryView`]
//!
//! §11.1 sketches the rule interface as `evaluate(&SystemSnapshot, &HistoryView)`.
//! In this codebase a [`HistoryView`] is deliberately only a *cursor*: it holds no
//! reference to a ring so that it can be copied around application state (see
//! [`crate::history::view`]). A rule therefore needs the cursor **and** the ring
//! it indexes, and [`HistoryWindow`] is that pair. [`HistoryWindow::view`] exposes
//! the cursor unchanged.
//!
//! Evaluating against the cursor rather than always against live data is what lets
//! the Inspect screen explain a *selected* historical sample (§2.1, §7.5) with the
//! same rules that produced the live radar.
//!
//! # Counting rules
//!
//! Every count here reports three numbers: how many samples met the condition, how
//! many were readable at all, and how many were unavailable. A sample whose input
//! was withheld is **not** counted as failing the condition and **not** counted
//! towards the minimum sample requirement — §26's "unavailable is not zero" applies
//! to counting as much as to display, and §11.3 requires a counter reset not to be
//! read as an event.

use core::time::Duration;

use crate::history::{
    ContributorMetric, HistoricalSample, HistoryMetric, HistoryRing, HistoryView,
};
use crate::model::{MeasuredValue, ProcessIdentity};

use super::TimeWindow;

/// The outcome of counting a condition over a window of samples.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Counted {
    /// Samples whose reading met the condition.
    pub matched: usize,
    /// Samples that produced a fresh reading at all.
    pub considered: usize,
    /// Samples whose reading was unavailable, stale, or warming up.
    pub unavailable: usize,
    /// Monotonic span between the oldest and newest sample visited.
    pub span: Duration,
}

impl Counted {
    /// How many samples were visited, readable or not.
    #[must_use]
    pub const fn visited(&self) -> usize {
        self.considered.saturating_add(self.unavailable)
    }

    /// The evidence window these counts cover.
    #[must_use]
    pub const fn window(&self) -> TimeWindow {
        TimeWindow::new(self.span, self.considered)
    }

    /// Whether the condition held often enough, over enough readings, to support
    /// a sustained claim (§11.3).
    ///
    /// Both halves matter: `matched >= required` is the sustained condition, and
    /// `considered >= minimum` is the minimum-sample rule that stops a rule from
    /// firing on the second tick after launch.
    #[must_use]
    pub const fn sustained(&self, required: usize, minimum: usize) -> bool {
        self.considered >= minimum && self.matched >= required
    }
}

/// A ring plus the cursor into it that a rule evaluates against.
#[derive(Clone, Copy, Debug)]
pub struct HistoryWindow<'a> {
    ring: &'a HistoryRing,
    view: HistoryView,
}

impl<'a> HistoryWindow<'a> {
    /// Pairs a ring with an explicit cursor.
    #[must_use]
    pub const fn new(ring: &'a HistoryRing, view: HistoryView) -> Self {
        Self { ring, view }
    }

    /// Pairs a ring with a cursor following the newest sample.
    #[must_use]
    pub const fn live(ring: &'a HistoryRing) -> Self {
        Self::new(ring, HistoryView::live())
    }

    /// The cursor, which is the `HistoryView` §11.1's sketch passes.
    #[must_use]
    pub const fn view(&self) -> HistoryView {
        self.view
    }

    /// The ring being read.
    #[must_use]
    pub const fn ring(&self) -> &'a HistoryRing {
        self.ring
    }

    /// The interval history is configured to retain samples at (§8.5).
    ///
    /// Rules that need a reference interval use this rather than assuming one
    /// second (§8.1).
    #[must_use]
    pub fn expected_interval(&self) -> Duration {
        self.ring.limits().interval()
    }

    /// The sample the cursor selects, or the newest one when live.
    #[must_use]
    pub fn selected(&self) -> Option<&'a HistoricalSample> {
        self.view.selected(self.ring)
    }

    /// How many samples are retained in total.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// Whether no sample has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// The `count` samples ending at the cursor, oldest first.
    ///
    /// Resolved by absolute index, so it costs one deque lookup per sample and
    /// never scans the ring (§21 M4).
    pub fn recent(&self, count: usize) -> impl DoubleEndedIterator<Item = &'a HistoricalSample> {
        let ring = self.ring;
        self.bounds(count)
            .into_iter()
            .flat_map(move |(first, last)| {
                (first..=last).filter_map(move |at| ring.get_absolute(at))
            })
    }

    /// The newest retained sample strictly older than `sequence`.
    ///
    /// Rules that compare "now" against "the previous sample" use this so they
    /// work whether or not the snapshot under evaluation has already been recorded
    /// into the ring.
    #[must_use]
    pub fn previous_sample(&self, sequence: u64) -> Option<&'a HistoricalSample> {
        self.recent(2).rfind(|sample| sample.sequence < sequence)
    }

    /// Counts how many of the `count` most recent samples satisfy `predicate`.
    ///
    /// `predicate` sees only *freshly measured* values: a sample whose metric was
    /// unavailable is tallied in [`Counted::unavailable`] and never passed to the
    /// predicate, so no rule can accidentally treat a missing reading as a zero
    /// (§26).
    #[must_use]
    pub fn count_where(
        &self,
        metric: HistoryMetric,
        count: usize,
        predicate: impl Fn(f64) -> bool,
    ) -> Counted {
        let mut counted = Counted::default();
        let mut oldest: Option<Duration> = None;
        let mut newest = Duration::ZERO;

        for sample in self.recent(count) {
            if oldest.is_none() {
                oldest = Some(sample.monotonic_offset);
            }
            newest = sample.monotonic_offset;
            match sample.system.scalar(metric) {
                Some(value) => {
                    counted.considered = counted.considered.saturating_add(1);
                    if predicate(value) {
                        counted.matched = counted.matched.saturating_add(1);
                    }
                }
                None => counted.unavailable = counted.unavailable.saturating_add(1),
            }
        }

        counted.span = newest.saturating_sub(oldest.unwrap_or(newest));
        counted
    }

    /// Counts how many of the `count` most recent samples are at or above
    /// `threshold`.
    #[must_use]
    pub fn count_at_least(&self, metric: HistoryMetric, count: usize, threshold: f64) -> Counted {
        self.count_where(metric, count, |value| value >= threshold)
    }

    /// The oldest and newest freshly measured values of `metric` in the window.
    ///
    /// Returns `None` unless *both* ends were measured, because a trend computed
    /// against a missing endpoint is a fabrication (§26). The returned duration is
    /// the real span between the two samples, never an assumed one (§8.1).
    #[must_use]
    pub fn trend(&self, metric: HistoryMetric, count: usize) -> Option<(f64, f64, Duration)> {
        let mut first: Option<(f64, Duration)> = None;
        let mut last: Option<(f64, Duration)> = None;
        for sample in self.recent(count) {
            if let Some(value) = sample.system.scalar(metric) {
                if first.is_none() {
                    first = Some((value, sample.monotonic_offset));
                }
                last = Some((value, sample.monotonic_offset));
            }
        }
        let (start, start_at) = first?;
        let (end, end_at) = last?;
        Some((start, end, end_at.saturating_sub(start_at)))
    }

    /// The oldest and newest absolute indices of the `count` samples ending at the
    /// cursor.
    fn bounds(&self, count: usize) -> Option<(u64, u64)> {
        if count == 0 {
            return None;
        }
        let last = self.view.selected_absolute(self.ring)?;
        let span = u64::try_from(count).unwrap_or(u64::MAX).saturating_sub(1);
        let first = last.saturating_sub(span).max(self.ring.first_absolute());
        Some((first, last))
    }
}

/// The retained value of a contributor metric for one process in one sample.
///
/// Keyed on the full [`ProcessIdentity`], so a reused PID never resolves to the
/// series of the process that used to hold it (§26).
///
/// Contributor lists are bounded to the top `K` per metric (§8.5), so a process that
/// dropped out of the top `K` returns `None` — a gap, not a zero.
#[must_use]
pub fn contributor_value(
    sample: &HistoricalSample,
    metric: ContributorMetric,
    identity: ProcessIdentity,
) -> Option<f64> {
    sample
        .contributors
        .metric(metric)
        .entries()
        .iter()
        .find(|entry| entry.identity == identity)
        .map(|entry| measured_scalar(entry.value))
}

/// A measured value as a comparable number.
///
/// Byte counts and event counts are integral in the model (§10.4); widening them
/// here is only ever done to compare or difference them, never to store them.
pub(crate) fn measured_scalar(value: MeasuredValue) -> f64 {
    match value {
        MeasuredValue::Bytes(bytes) | MeasuredValue::Count(bytes) => bytes as f64,
        MeasuredValue::ByteRate(rate) | MeasuredValue::EventRate(rate) => rate.per_second(),
        MeasuredValue::Percent(percent) => f64::from(percent.value()),
        MeasuredValue::Duration(duration) => duration.as_secs_f64(),
        MeasuredValue::Load(load) => f64::from(load),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{Timeline, set_cpu, set_memory};
    use crate::history::HistoryMetric;
    use crate::model::{MetricState, UnavailableReason};

    #[test]
    fn counting_an_empty_ring_reports_nothing_considered() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let window = timeline.window();
        let counted = window.count_at_least(HistoryMetric::CpuBusy, 15, 80.0);

        assert!(window.is_empty());
        assert_eq!(counted, Counted::default());
        assert!(!counted.sustained(1, 1));
        assert_eq!(counted.window().samples, 0);
    }

    #[test]
    fn the_expected_interval_comes_from_history_rather_than_an_assumption() {
        // §8.1 forbids assuming one second; the rules that need a reference
        // interval read the configured one.
        for interval in [Duration::from_millis(500), Duration::from_secs(5)] {
            let timeline = Timeline::new(interval);
            assert_eq!(timeline.window().expected_interval(), timeline.interval());
            assert_eq!(timeline.window().expected_interval(), interval);
        }
    }

    #[test]
    fn counting_is_limited_to_the_requested_window() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..10 {
            timeline.push(|snapshot| set_cpu(snapshot, 10.0));
        }
        for _ in 0..5 {
            timeline.push(|snapshot| set_cpu(snapshot, 90.0));
        }

        let window = timeline.window();
        let counted = window.count_at_least(HistoryMetric::CpuBusy, 5, 80.0);
        assert_eq!(counted.matched, 5);
        assert_eq!(counted.considered, 5);
        assert_eq!(counted.span, Duration::from_secs(4));

        let wider = window.count_at_least(HistoryMetric::CpuBusy, 15, 80.0);
        assert_eq!(wider.matched, 5);
        assert_eq!(wider.considered, 15);
    }

    #[test]
    fn a_window_larger_than_the_ring_counts_only_what_exists() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..3 {
            timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        }
        let counted = timeline
            .window()
            .count_at_least(HistoryMetric::CpuBusy, 100, 80.0);
        assert_eq!(counted.visited(), 3);
        assert_eq!(counted.matched, 3);
    }

    #[test]
    fn an_unavailable_sample_is_neither_a_match_nor_a_considered_reading() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..5 {
            timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        }
        for _ in 0..5 {
            timeline.push(|snapshot| {
                snapshot.cpu.total =
                    MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
            });
        }

        let counted = timeline
            .window()
            .count_at_least(HistoryMetric::CpuBusy, 10, 80.0);
        assert_eq!(counted.matched, 5);
        assert_eq!(counted.considered, 5);
        assert_eq!(counted.unavailable, 5);
        assert_eq!(counted.visited(), 10);
        assert!(
            !counted.sustained(10, 10),
            "five readings cannot support a ten-sample claim"
        );
    }

    #[test]
    fn the_cursor_decides_which_window_is_counted() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..10 {
            timeline.push(|snapshot| set_cpu(snapshot, 95.0));
        }
        for _ in 0..10 {
            timeline.push(|snapshot| set_cpu(snapshot, 1.0));
        }

        let live = timeline.window();
        assert_eq!(
            live.count_at_least(HistoryMetric::CpuBusy, 10, 80.0)
                .matched,
            0
        );

        let mut view = HistoryView::live();
        view.step_back(timeline.ring(), 10);
        let historical = HistoryWindow::new(timeline.ring(), view);
        assert_eq!(
            historical
                .count_at_least(HistoryMetric::CpuBusy, 10, 80.0)
                .matched,
            10,
            "a rule evaluated over a selected sample must see that sample's past"
        );
        assert_eq!(historical.view(), view);
    }

    #[test]
    fn a_trend_needs_both_endpoints_measured() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        timeline.push(|snapshot| set_memory(snapshot, 1_000, 800));
        timeline.push(|snapshot| {
            snapshot.memory.usage = MetricState::PermissionDenied;
        });
        assert!(
            timeline
                .window()
                .trend(HistoryMetric::MemoryUsedShare, 2)
                .is_some(),
            "the older endpoint is still measured, so the trend spans one sample"
        );

        let mut only_unavailable = Timeline::new(Duration::from_secs(1));
        only_unavailable.push(|snapshot| {
            snapshot.memory.usage = MetricState::PermissionDenied;
        });
        assert!(
            only_unavailable
                .window()
                .trend(HistoryMetric::MemoryUsedShare, 2)
                .is_none()
        );
    }

    #[test]
    fn a_trend_reports_the_real_span_between_the_endpoints() {
        let mut timeline = Timeline::new(Duration::from_millis(500));
        for used_share in [10u64, 20, 30] {
            // History retains the *used* share, so the fixture sets availability to
            // its complement.
            timeline.push(move |snapshot| set_memory(snapshot, 1_000, 1_000 - used_share * 10));
        }
        let (start, end, span) = timeline
            .window()
            .trend(HistoryMetric::MemoryUsedShare, 3)
            .expect("three measured samples");
        assert!((start - 10.0).abs() < 0.01, "{start}");
        assert!((end - 30.0).abs() < 0.01, "{end}");
        assert_eq!(span, Duration::from_secs(1), "two 500ms intervals");
    }

    #[test]
    fn the_previous_sample_is_the_newest_one_older_than_the_snapshot() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        timeline.push(|snapshot| set_cpu(snapshot, 1.0));
        timeline.push(|snapshot| set_cpu(snapshot, 2.0));
        let current = timeline.push(|snapshot| set_cpu(snapshot, 3.0));

        let window = timeline.window();
        let previous = window
            .previous_sample(current.sequence)
            .expect("a previous sample exists");
        assert_eq!(previous.sequence, current.sequence - 1);
        assert!(
            window.previous_sample(0).is_none(),
            "nothing precedes the first sample"
        );
    }

    #[test]
    fn a_zero_length_window_reads_nothing_instead_of_panicking() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        timeline.push(|snapshot| set_cpu(snapshot, 50.0));
        assert_eq!(timeline.window().recent(0).count(), 0);
        assert_eq!(
            timeline
                .window()
                .count_at_least(HistoryMetric::CpuBusy, 0, 1.0)
                .visited(),
            0
        );
    }

    #[test]
    fn every_measured_value_kind_has_a_comparable_scalar() {
        use crate::units::{Percent, Rate};
        let rate = Rate::new(1_024.0).expect("valid rate");
        let cases = [
            (MeasuredValue::Bytes(4_096), 4_096.0),
            (MeasuredValue::Count(7), 7.0),
            (MeasuredValue::ByteRate(rate), 1_024.0),
            (MeasuredValue::EventRate(rate), 1_024.0),
            (
                MeasuredValue::Percent(Percent::new(37.5).expect("valid")),
                37.5,
            ),
            (MeasuredValue::Duration(Duration::from_secs(2)), 2.0),
            (MeasuredValue::Load(4.25), 4.25),
        ];
        for (value, expected) in cases {
            let scalar = measured_scalar(value);
            assert!((scalar - expected).abs() < 0.001, "{value:?} -> {scalar}");
        }
    }
}

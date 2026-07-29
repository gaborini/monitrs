//! Selection and comparison over a [`HistoryRing`]: the Time Lens cursor (§2.1)
//! and the comparison values of §2.5.

use core::time::Duration;

use crate::model::MeasuredValue;
use crate::units::{ByteUnits, Rate, format_byte_rate, format_bytes, format_history_offset};

use super::{HistoricalSample, HistoryMetric, HistoryRing};

/// How many samples `Shift+[` and `Shift+]` move (§5.6's `Shift+[/] x10`).
pub const HISTORY_STEP_MULTIPLIER: usize = 10;

/// The look-back §2.5 asks for: "30 seconds ago when history permits".
pub const COMPARISON_LOOKBACK: Duration = Duration::from_secs(30);

/// Where the Time Lens cursor is.
///
/// The selected sample is named by its *absolute* index rather than by its
/// distance from the newest sample, so a paused view keeps showing the same
/// sample as new ones arrive instead of drifting backwards under the cursor
/// (§2.1). Resolving an absolute index is index arithmetic, which is what makes
/// seeking constant time (§21 M4).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum HistoryPosition {
    /// Following the newest sample.
    #[default]
    Live,
    /// Pinned to one recorded sample.
    Selected {
        /// The sample's absolute index, as counted by
        /// [`HistoryRing::total_recorded`].
        absolute: u64,
    },
}

/// What a seek did.
///
/// Clamping is reported rather than silently absorbed so the UI can signal that
/// the end of history was reached instead of appearing to ignore the key.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SeekOutcome {
    /// The cursor moved the full requested distance.
    Moved,
    /// The cursor stopped at the oldest retained sample.
    ClampedAtOldest,
    /// The cursor stopped at the newest retained sample.
    ///
    /// The view stays in history rather than snapping back to live: §2.1 makes
    /// returning to live one explicit action, so stepping forward must not do it
    /// as a side effect.
    ClampedAtNewest,
    /// Nothing is recorded yet, so there is nothing to select.
    Empty,
}

impl SeekOutcome {
    /// Whether the requested distance was reduced.
    #[must_use]
    pub const fn was_clamped(self) -> bool {
        matches!(self, Self::ClampedAtOldest | Self::ClampedAtNewest)
    }
}

/// Which earlier sample a comparison is made against (§2.5).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ComparisonBaseline {
    /// The immediately preceding retained sample.
    PreviousSample,
    /// The newest sample at least this much older than the selected one.
    Elapsed(Duration),
}

impl ComparisonBaseline {
    /// The 30-second look-back §2.5 names.
    pub const THIRTY_SECONDS_AGO: Self = Self::Elapsed(COMPARISON_LOOKBACK);
}

/// One resolved comparison between the selected sample and an earlier one.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricComparison {
    /// Which metric was compared.
    pub metric: HistoryMetric,
    /// The selected sample's measurement.
    pub selected: MeasuredValue,
    /// The baseline sample's measurement.
    pub baseline: MeasuredValue,
    /// How much older the baseline sample is than the selected one.
    ///
    /// The *actual* distance, not the requested one: §8.1 forbids assuming a
    /// fixed interval, so "30 seconds ago" resolves to a real sample that may be
    /// 31 seconds back.
    pub baseline_age: Duration,
    /// `selected - baseline` in the metric's natural unit.
    ///
    /// Percentage metrics yield percentage *points* (§5.6's `+54 points vs now`).
    pub delta: f64,
}

impl MetricComparison {
    /// Renders the delta with an explicit sign in the metric's unit.
    #[must_use]
    pub fn render_delta(&self, units: ByteUnits) -> String {
        if self.metric.is_percentage() {
            return format!("{:+.0} points", self.delta);
        }
        match self.metric {
            HistoryMetric::LoadOne => format!("{:+.2}", self.delta),
            HistoryMetric::SwapUsed => {
                let magnitude = self.delta.abs();
                // A byte count is integral (§10.4); the delta is only floating
                // point because it is signed, so floor it back for display.
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let bytes = magnitude.min(u64::MAX as f64) as u64;
                format!("{}{}", sign(self.delta), format_bytes(bytes, units))
            }
            _ => match Rate::new(self.delta.abs()) {
                Some(rate) => format!("{}{}", sign(self.delta), format_byte_rate(rate, units)),
                // Unreachable for a difference of two validated finite rates.
                None => "n/a".to_owned(),
            },
        }
    }
}

/// The sign prefix for a rendered delta.
const fn sign(delta: f64) -> char {
    if delta < 0.0 { '-' } else { '+' }
}

/// Both comparisons §2.5 asks for, resolved together.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetricComparisons {
    /// Against the immediately preceding sample.
    pub previous_sample: Option<MetricComparison>,
    /// Against roughly 30 seconds earlier.
    ///
    /// `None` when history does not reach back that far, or when either sample's
    /// input was unavailable. §26 forbids reporting that as a zero change.
    pub thirty_seconds_ago: Option<MetricComparison>,
}

/// The Time Lens cursor over a ring (§2.1).
///
/// Holds no reference to the ring, so it can live in application state next to
/// one and be copied freely. Every method takes the ring it should be resolved
/// against.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub struct HistoryView {
    position: HistoryPosition,
}

impl HistoryView {
    /// A view following the newest sample.
    #[must_use]
    pub const fn live() -> Self {
        Self {
            position: HistoryPosition::Live,
        }
    }

    /// Where the cursor is.
    #[must_use]
    pub const fn position(self) -> HistoryPosition {
        self.position
    }

    /// Whether the view is following live data.
    #[must_use]
    pub const fn is_live(self) -> bool {
        matches!(self.position, HistoryPosition::Live)
    }

    /// Whether process-control actions may be offered.
    ///
    /// §2.1 and §15.1 both require process actions to be disabled while
    /// inspecting history: the process shown may not exist any more, and its PID
    /// may since have been reused. Encoding the rule here keeps every call site
    /// from having to remember it.
    #[must_use]
    pub const fn allows_process_actions(self) -> bool {
        self.is_live()
    }

    /// Returns to live, the single explicit action §2.1 specifies for `L`.
    pub const fn return_live(&mut self) {
        self.position = HistoryPosition::Live;
    }

    /// The absolute index the cursor resolves to, clamped into what is retained.
    ///
    /// Constant time. A selection whose sample has since been evicted resolves to
    /// the oldest retained sample rather than to nothing, so the panel does not
    /// blank out while the user is reading it.
    #[must_use]
    pub fn selected_absolute(self, ring: &HistoryRing) -> Option<u64> {
        let newest = ring.newest_absolute()?;
        Some(match self.position {
            HistoryPosition::Live => newest,
            HistoryPosition::Selected { absolute } => {
                absolute.max(ring.first_absolute()).min(newest)
            }
        })
    }

    /// The selected sample, or the newest one when live.
    #[must_use]
    pub fn selected(self, ring: &HistoryRing) -> Option<&HistoricalSample> {
        ring.get_absolute(self.selected_absolute(ring)?)
    }

    /// Moves `steps` samples towards the past (`[`, or `Shift+[` with
    /// [`HISTORY_STEP_MULTIPLIER`]).
    pub fn step_back(&mut self, ring: &HistoryRing, steps: usize) -> SeekOutcome {
        let Some(current) = self.selected_absolute(ring) else {
            return SeekOutcome::Empty;
        };
        let floor = ring.first_absolute();
        let available = current.saturating_sub(floor);
        let wanted = u64::try_from(steps).unwrap_or(u64::MAX);
        let target = current.saturating_sub(wanted.min(available));
        self.position = HistoryPosition::Selected { absolute: target };
        if wanted > available {
            SeekOutcome::ClampedAtOldest
        } else {
            SeekOutcome::Moved
        }
    }

    /// Moves `steps` samples towards the present (`]`, or `Shift+]`).
    pub fn step_forward(&mut self, ring: &HistoryRing, steps: usize) -> SeekOutcome {
        let Some(current) = self.selected_absolute(ring) else {
            return SeekOutcome::Empty;
        };
        let Some(ceiling) = ring.newest_absolute() else {
            return SeekOutcome::Empty;
        };
        let requested = current.saturating_add(u64::try_from(steps).unwrap_or(u64::MAX));
        let target = requested.min(ceiling);
        self.position = HistoryPosition::Selected { absolute: target };
        if requested > ceiling {
            SeekOutcome::ClampedAtNewest
        } else {
            SeekOutcome::Moved
        }
    }

    /// Selects the newest sample at least `offset` behind the newest one.
    ///
    /// Effectively constant time: sample offsets increase monotonically, so this
    /// is a binary search rather than a scan (§21 M4). An `offset` reaching past
    /// the oldest retained sample clamps there and says so.
    pub fn seek_to_offset(&mut self, ring: &HistoryRing, offset: Duration) -> SeekOutcome {
        let (Some(newest), Some(oldest)) = (ring.newest(), ring.oldest()) else {
            return SeekOutcome::Empty;
        };
        // An offset reaching past the oldest retained sample is a clamp even
        // though a sample is still selected, so the UI can say "that is as far
        // back as history goes" instead of appearing to ignore the request.
        let (index, clamped) = match newest.monotonic_offset.checked_sub(offset) {
            Some(target) if target >= oldest.monotonic_offset => {
                match ring.index_at_or_before_offset(target) {
                    Some(index) => (index, false),
                    None => (0, true),
                }
            }
            _ => (0, true),
        };
        let absolute = ring
            .first_absolute()
            .saturating_add(u64::try_from(index).unwrap_or(u64::MAX));
        self.position = HistoryPosition::Selected { absolute };
        if clamped {
            SeekOutcome::ClampedAtOldest
        } else {
            SeekOutcome::Moved
        }
    }

    /// How far behind live the selected sample is.
    ///
    /// Computed from monotonic offsets, so a wall-clock change cannot make the
    /// header count backwards (§8.1). Zero when live.
    #[must_use]
    pub fn offset_from_live(self, ring: &HistoryRing) -> Duration {
        match (self.selected(ring), ring.newest()) {
            (Some(selected), Some(newest)) => newest
                .monotonic_offset
                .saturating_sub(selected.monotonic_offset),
            _ => Duration::ZERO,
        }
    }

    /// The header offset text: `LIVE`, or `-00:37` (§2.1, §5.6).
    ///
    /// Reports the *offset* only. §2.1's third header state, `PAUSED`, is a UI
    /// state — a paused view that has not been scrubbed sits at offset zero — and
    /// is distinguished with [`Self::is_live`].
    #[must_use]
    pub fn format_offset(self, ring: &HistoryRing) -> String {
        format_history_offset(self.offset_from_live(ring))
    }

    /// Compares the selected sample's `metric` against an earlier sample (§2.5).
    ///
    /// Returns `None`, never a zero delta, when the baseline does not exist or
    /// when either sample's input was unavailable. That is what stops a counter
    /// reset from being rendered as a spike (§21 M4) and what honours §26's
    /// "unavailable is not zero".
    #[must_use]
    pub fn compare(
        self,
        ring: &HistoryRing,
        metric: HistoryMetric,
        baseline: ComparisonBaseline,
    ) -> Option<MetricComparison> {
        let selected_absolute = self.selected_absolute(ring)?;
        let selected = ring.get_absolute(selected_absolute)?;
        let earlier = match baseline {
            ComparisonBaseline::PreviousSample => {
                ring.get_absolute(selected_absolute.checked_sub(1)?)?
            }
            ComparisonBaseline::Elapsed(lookback) => {
                // `checked_sub` is the "when history permits" test: a selected
                // sample younger than the look-back has nothing to compare to.
                let target = selected.monotonic_offset.checked_sub(lookback)?;
                let sample = ring.get(ring.index_at_or_before_offset(target)?)?;
                if sample.sequence >= selected.sequence {
                    return None;
                }
                sample
            }
        };

        let selected_scalar = selected.system.scalar(metric)?;
        let earlier_scalar = earlier.system.scalar(metric)?;
        let selected_state = selected.system.measurement(metric);
        let earlier_state = earlier.system.measurement(metric);

        Some(MetricComparison {
            metric,
            selected: *selected_state.fresh()?,
            baseline: *earlier_state.fresh()?,
            baseline_age: selected
                .monotonic_offset
                .saturating_sub(earlier.monotonic_offset),
            delta: selected_scalar - earlier_scalar,
        })
    }

    /// Both comparisons §2.5 requires, in one call.
    #[must_use]
    pub fn comparisons(self, ring: &HistoryRing, metric: HistoryMetric) -> MetricComparisons {
        MetricComparisons {
            previous_sample: self.compare(ring, metric, ComparisonBaseline::PreviousSample),
            thirty_seconds_ago: self.compare(ring, metric, ComparisonBaseline::THIRTY_SECONDS_AGO),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::{HistoryConfig, HistoryLimits};
    use crate::model::{CpuUsage, MetricState, SystemSnapshot, UnavailableReason};
    use crate::units::Percent;
    use std::time::{Instant, SystemTime};

    /// A ring of one-second samples whose CPU busy percentage is `cpu[i]`.
    ///
    /// The memory budget is set to its maximum so that capacity is exactly
    /// `capacity_seconds`: these tests are about seeking, and the budget clamp has
    /// its own tests in [`super::super::ring`].
    fn ring_with(capacity_seconds: u64, cpu: &[Option<f32>]) -> HistoryRing {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(
            HistoryLimits::resolve(HistoryConfig {
                interval,
                duration: Duration::from_secs(capacity_seconds),
                memory_budget_bytes: crate::history::MAX_MEMORY_BUDGET_BYTES,
                ..HistoryConfig::default()
            }),
            start,
        );
        for (index, busy) in cpu.iter().enumerate() {
            let sequence = u64::try_from(index).unwrap_or(0);
            let captured_at = start + interval.saturating_mul(u32::try_from(index).unwrap_or(0));
            let mut snapshot = SystemSnapshot::warming_up(
                captured_at,
                SystemTime::UNIX_EPOCH + Duration::from_secs(sequence),
                8,
            );
            snapshot.sequence = sequence;
            snapshot.elapsed = interval;
            snapshot.cpu.total = match busy {
                Some(value) => MetricState::Available(CpuUsage::plain(
                    Percent::new(*value).expect("valid percent"),
                )),
                None => MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
            };
            ring.record(&snapshot);
        }
        ring
    }

    fn steady(count: usize) -> Vec<Option<f32>> {
        (0..count)
            .map(|index| Some(f32::from(u8::try_from(index % 100).unwrap_or(0))))
            .collect()
    }

    #[test]
    fn a_new_view_is_live_and_shows_the_newest_sample() {
        let ring = ring_with(60, &steady(10));
        let view = HistoryView::live();

        assert!(view.is_live());
        assert_eq!(view.position(), HistoryPosition::Live);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(9));
        assert_eq!(view.offset_from_live(&ring), Duration::ZERO);
        assert_eq!(view.format_offset(&ring), "LIVE");
    }

    #[test]
    fn the_default_view_is_live() {
        assert_eq!(HistoryView::default(), HistoryView::live());
    }

    #[test]
    fn process_actions_are_only_allowed_at_live() {
        let ring = ring_with(60, &steady(10));
        let mut view = HistoryView::live();
        assert!(view.allows_process_actions());

        view.step_back(&ring, 1);
        assert!(
            !view.allows_process_actions(),
            "§15.1 disables actions in history"
        );

        view.return_live();
        assert!(view.allows_process_actions());
    }

    #[test]
    fn stepping_back_reports_the_offset_from_live() {
        let ring = ring_with(600, &steady(120));
        let mut view = HistoryView::live();

        assert_eq!(view.step_back(&ring, 37), SeekOutcome::Moved);
        assert_eq!(view.offset_from_live(&ring), Duration::from_secs(37));
        assert_eq!(
            view.format_offset(&ring),
            "-00:37",
            "the §2.1 header reads HISTORY -00:37"
        );
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(119 - 37));
    }

    #[test]
    fn the_shift_multiplier_moves_ten_samples() {
        let ring = ring_with(600, &steady(120));
        let mut single = HistoryView::live();
        let mut multiple = HistoryView::live();

        for _ in 0..HISTORY_STEP_MULTIPLIER {
            single.step_back(&ring, 1);
        }
        multiple.step_back(&ring, HISTORY_STEP_MULTIPLIER);
        assert_eq!(single, multiple);
        assert_eq!(single.offset_from_live(&ring), Duration::from_secs(10));
    }

    #[test]
    fn seeking_clamps_at_the_oldest_sample() {
        let ring = ring_with(60, &steady(10));
        let mut view = HistoryView::live();

        assert_eq!(view.step_back(&ring, 100), SeekOutcome::ClampedAtOldest);
        assert!(view.step_back(&ring, 100).was_clamped());
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(0));
        assert_eq!(view.offset_from_live(&ring), Duration::from_secs(9));
    }

    #[test]
    fn seeking_clamps_at_the_newest_sample_without_returning_to_live() {
        let ring = ring_with(60, &steady(10));
        let mut view = HistoryView::live();
        view.step_back(&ring, 5);

        assert_eq!(view.step_forward(&ring, 100), SeekOutcome::ClampedAtNewest);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(9));
        assert!(
            !view.is_live(),
            "§2.1 makes returning to live an explicit action, not a side effect"
        );
        assert_eq!(view.offset_from_live(&ring), Duration::ZERO);
    }

    #[test]
    fn stepping_forward_from_the_middle_moves_the_full_distance() {
        let ring = ring_with(60, &steady(20));
        let mut view = HistoryView::live();
        view.step_back(&ring, 15);
        assert_eq!(view.step_forward(&ring, 5), SeekOutcome::Moved);
        assert_eq!(view.offset_from_live(&ring), Duration::from_secs(10));
    }

    #[test]
    fn seeking_an_empty_ring_reports_that_there_is_nothing_to_select() {
        let ring = ring_with(60, &[]);
        let mut view = HistoryView::live();

        assert_eq!(view.step_back(&ring, 1), SeekOutcome::Empty);
        assert_eq!(view.step_forward(&ring, 1), SeekOutcome::Empty);
        assert_eq!(
            view.seek_to_offset(&ring, Duration::from_secs(1)),
            SeekOutcome::Empty
        );
        assert!(view.is_live(), "an empty ring leaves the view at live");
        assert!(view.selected(&ring).is_none());
        assert_eq!(view.format_offset(&ring), "LIVE");
    }

    #[test]
    fn seeking_to_an_offset_selects_the_sample_at_or_before_it() {
        let ring = ring_with(600, &steady(120));
        let mut view = HistoryView::live();

        assert_eq!(
            view.seek_to_offset(&ring, Duration::from_secs(37)),
            SeekOutcome::Moved
        );
        assert_eq!(view.offset_from_live(&ring), Duration::from_secs(37));

        assert_eq!(
            view.seek_to_offset(&ring, Duration::from_millis(37_500)),
            SeekOutcome::Moved
        );
        assert_eq!(
            view.offset_from_live(&ring),
            Duration::from_secs(38),
            "an offset between samples resolves to the older one"
        );
    }

    #[test]
    fn seeking_to_an_offset_beyond_history_clamps_at_the_oldest_sample() {
        let ring = ring_with(60, &steady(10));
        let mut view = HistoryView::live();
        assert_eq!(
            view.seek_to_offset(&ring, Duration::from_secs(600)),
            SeekOutcome::ClampedAtOldest
        );
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(0));
    }

    #[test]
    fn seeking_to_zero_selects_the_newest_sample_but_stays_paused() {
        let ring = ring_with(60, &steady(10));
        let mut view = HistoryView::live();
        assert_eq!(
            view.seek_to_offset(&ring, Duration::ZERO),
            SeekOutcome::Moved
        );
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(9));
        assert!(!view.is_live());
    }

    #[test]
    fn a_selection_that_was_evicted_resolves_to_the_oldest_retained_sample() {
        let mut ring = ring_with(30, &steady(30));
        let mut view = HistoryView::live();
        view.step_back(&ring, 29);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(0));

        // Push the selected sample out of the ring.
        let refilled = steady(45);
        ring = ring_with(30, &refilled);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(15));
        assert!(!view.is_live());
    }

    #[test]
    fn selection_is_stable_as_new_samples_arrive() {
        // §2.1: a paused view must keep showing the sample the user selected.
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);
        let push = |ring: &mut HistoryRing, sequence: u64| {
            let mut snapshot = SystemSnapshot::warming_up(
                start + interval.saturating_mul(u32::try_from(sequence).unwrap_or(0)),
                SystemTime::UNIX_EPOCH,
                8,
            );
            snapshot.sequence = sequence;
            ring.record(&snapshot);
        };
        for sequence in 0..10 {
            push(&mut ring, sequence);
        }

        let mut view = HistoryView::live();
        view.step_back(&ring, 3);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(6));
        assert_eq!(view.offset_from_live(&ring), Duration::from_secs(3));

        for sequence in 10..15 {
            push(&mut ring, sequence);
        }
        assert_eq!(
            view.selected(&ring).map(|s| s.sequence),
            Some(6),
            "the cursor must stay on the same sample"
        );
        assert_eq!(view.offset_from_live(&ring), Duration::from_secs(8));
    }

    #[test]
    fn seeking_a_large_ring_lands_on_the_expected_sample() {
        // Correctness at both ends of a ring far larger than any UI will show;
        // resolution is index arithmetic, so cost does not grow with length.
        let ring = ring_with(3_600, &steady(3_000));
        let mut view = HistoryView::live();

        assert_eq!(view.step_back(&ring, 2_999), SeekOutcome::Moved);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(0));
        assert_eq!(view.step_back(&ring, 1), SeekOutcome::ClampedAtOldest);
        assert_eq!(view.step_forward(&ring, 2_999), SeekOutcome::Moved);
        assert_eq!(view.selected(&ring).map(|s| s.sequence), Some(2_999));
    }

    #[test]
    fn a_comparison_against_the_previous_sample_is_a_signed_delta() {
        let ring = ring_with(60, &[Some(20.0), Some(74.0)]);
        let view = HistoryView::live();

        let comparison = view
            .compare(
                &ring,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::PreviousSample,
            )
            .expect("one sample ago exists");
        assert!((comparison.delta - 54.0).abs() < 0.01, "{comparison:?}");
        assert_eq!(comparison.baseline_age, Duration::from_secs(1));
        assert_eq!(comparison.render_delta(ByteUnits::Iec), "+54 points");
    }

    #[test]
    fn the_oldest_sample_has_nothing_to_compare_against() {
        let ring = ring_with(60, &[Some(20.0), Some(74.0)]);
        let mut view = HistoryView::live();
        view.step_back(&ring, 1);
        assert!(
            view.compare(
                &ring,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::PreviousSample
            )
            .is_none()
        );
    }

    #[test]
    fn the_thirty_second_comparison_is_none_until_history_reaches_back() {
        let short = ring_with(60, &steady(10));
        let view = HistoryView::live();
        assert!(
            view.compare(
                &short,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::THIRTY_SECONDS_AGO
            )
            .is_none(),
            "§2.5 permits the comparison only when history permits; zero would be a lie"
        );

        let long = ring_with(300, &steady(60));
        let comparison = view
            .compare(
                &long,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::THIRTY_SECONDS_AGO,
            )
            .expect("history reaches back 30s");
        assert_eq!(comparison.baseline_age, Duration::from_secs(30));
    }

    #[test]
    fn the_thirty_second_comparison_is_none_when_the_ring_evicted_that_far_back() {
        // A ring only 10s deep can never satisfy a 30s look-back, even once it
        // has been running for minutes.
        let ring = ring_with(10, &steady(200));
        let view = HistoryView::live();
        assert!(
            view.compare(
                &ring,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::THIRTY_SECONDS_AGO
            )
            .is_none()
        );
    }

    #[test]
    fn both_comparisons_resolve_together() {
        let ring = ring_with(300, &steady(90));
        let view = HistoryView::live();
        let comparisons = view.comparisons(&ring, HistoryMetric::CpuBusy);
        assert!(comparisons.previous_sample.is_some());
        assert!(comparisons.thirty_seconds_ago.is_some());
    }

    #[test]
    fn an_unavailable_input_yields_no_comparison_rather_than_a_spike() {
        // §21 M4: counter resets do not create false spikes. The newest sample's
        // CPU reading is a typed reset, so no delta can be computed from it.
        let ring = ring_with(60, &[Some(20.0), Some(21.0), None]);
        let view = HistoryView::live();

        assert!(
            view.compare(
                &ring,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::PreviousSample
            )
            .is_none(),
            "an unavailable selected value must not become a delta"
        );

        // And the reverse: an unavailable *baseline* is equally unusable.
        let ring = ring_with(60, &[None, Some(90.0)]);
        assert!(
            view.compare(
                &ring,
                HistoryMetric::CpuBusy,
                ComparisonBaseline::PreviousSample
            )
            .is_none()
        );
    }

    #[test]
    fn comparing_an_empty_ring_returns_nothing() {
        let ring = ring_with(60, &[]);
        let view = HistoryView::live();
        for baseline in [
            ComparisonBaseline::PreviousSample,
            ComparisonBaseline::THIRTY_SECONDS_AGO,
        ] {
            assert!(
                view.compare(&ring, HistoryMetric::CpuBusy, baseline)
                    .is_none()
            );
        }
    }

    #[test]
    fn a_comparison_of_an_unsupported_metric_is_absent_not_zero() {
        let ring = ring_with(60, &steady(60));
        let view = HistoryView::live();
        // No disks were recorded, so disk throughput is unsupported throughout.
        assert!(
            view.compare(
                &ring,
                HistoryMetric::DiskRead,
                ComparisonBaseline::PreviousSample
            )
            .is_none()
        );
    }

    #[test]
    fn deltas_render_in_the_metrics_own_unit() {
        let comparison = MetricComparison {
            metric: HistoryMetric::SwapUsed,
            selected: MeasuredValue::Bytes(0),
            baseline: MeasuredValue::Bytes(4 * 1024 * 1024),
            baseline_age: Duration::from_secs(1),
            delta: -4.0 * 1024.0 * 1024.0,
        };
        assert_eq!(comparison.render_delta(ByteUnits::Iec), "-4.0 MiB");

        let comparison = MetricComparison {
            metric: HistoryMetric::LoadOne,
            selected: MeasuredValue::Load(4.5),
            baseline: MeasuredValue::Load(1.5),
            baseline_age: Duration::from_secs(1),
            delta: 3.0,
        };
        assert_eq!(comparison.render_delta(ByteUnits::Iec), "+3.00");

        let comparison = MetricComparison {
            metric: HistoryMetric::NetworkRx,
            selected: MeasuredValue::ByteRate(Rate::new(0.0).expect("valid")),
            baseline: MeasuredValue::ByteRate(Rate::new(1_048_576.0).expect("valid")),
            baseline_age: Duration::from_secs(1),
            delta: -1_048_576.0,
        };
        assert_eq!(comparison.render_delta(ByteUnits::Iec), "-1.0M/s");
    }

    #[test]
    fn a_non_finite_delta_renders_as_unavailable_rather_than_panicking() {
        let comparison = MetricComparison {
            metric: HistoryMetric::DiskRead,
            selected: MeasuredValue::ByteRate(Rate::ZERO),
            baseline: MeasuredValue::ByteRate(Rate::ZERO),
            baseline_age: Duration::from_secs(1),
            delta: f64::NAN,
        };
        assert_eq!(comparison.render_delta(ByteUnits::Iec), "n/a");
    }
}

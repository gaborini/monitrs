//! CPU-time deltas turned into percentages, with the two conventions §8.3 fixes.
//!
//! The two trackers here differ in what they divide by, and that difference *is*
//! the semantics:
//!
//! * [`SystemCpuTracker`] divides busy CPU time by *total* CPU time, which is
//!   already a share of every logical CPU. The result is aggregate machine usage
//!   in `0..=100` and is immune to a variable sample interval.
//! * [`ProcessCpuTracker`] divides process CPU time by the *elapsed monotonic
//!   interval*, giving "one core = 100%". A process on four cores reads 400%,
//!   matching `top` and `htop`.

use core::time::Duration;
use std::time::Instant;

use crate::model::{CpuNormalization, MetricState, UnavailableReason};
use crate::rates::keyed::DeltaTracker;
use crate::units::Percent;

/// `part / whole` as a percentage.
///
/// Returns `None` when `whole` is zero — there is no defined utilization over a
/// zero-length interval, and §4 forbids answering that with a number — or when a
/// duration exceeds 584 years in nanoseconds, which is not a sampling delta and
/// so is better reported as unavailable than silently truncated.
fn percent_of_duration(part: Duration, whole: Duration) -> Option<Percent> {
    let part = u64::try_from(part.as_nanos()).ok()?;
    let whole = u64::try_from(whole.as_nanos()).ok()?;
    Percent::ratio(part, whole)
}

/// Cumulative CPU time split into the part that counts as busy and the part that
/// does not.
///
/// Both fields are [`Duration`] rather than raw ticks. `/proc/stat` reports
/// `USER_HZ` jiffies and macOS reports Mach ticks, so converting at the collector
/// boundary keeps this engine free of platform constants while staying integral
/// as §10.4 requires.
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
pub struct CpuTimeTotals {
    /// Cumulative non-idle time, summed over every logical CPU.
    pub busy: Duration,
    /// Cumulative idle time, summed over every logical CPU.
    ///
    /// On Linux this should be `idle + iowait`: a CPU blocked on I/O is not
    /// doing work, and counting `iowait` as busy is what makes some monitors
    /// report a pegged CPU during a disk stall (§8.3).
    pub idle: Duration,
}

impl CpuTimeTotals {
    /// Builds totals from cumulative busy and idle CPU time.
    #[must_use]
    pub const fn new(busy: Duration, idle: Duration) -> Self {
        Self { busy, idle }
    }

    /// All CPU time accounted for, busy and idle together.
    ///
    /// Saturating: a sum that overflows [`Duration`] is not real CPU time, and a
    /// panic inside a sampling loop is never acceptable (§14.3).
    #[must_use]
    pub const fn total(self) -> Duration {
        self.busy.saturating_add(self.idle)
    }
}

/// Aggregate machine CPU utilization from cumulative CPU-time totals (§8.3).
///
/// The percentage is `busy_delta / (busy_delta + idle_delta)`. That divisor is
/// CPU time rather than wall time, which makes the result self-normalizing: it
/// is already a share of *all* logical CPUs, so it lands in `0..=100` without
/// ever being told the CPU count, and a sample interval that drifts from 1 s to
/// 4 s cannot distort it.
///
/// The reading [`Instant`] is retained even though the percentage does not use
/// it, so that a keyed set of per-core trackers can prune cores that stopped
/// reporting after a CPU hotplug event.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCpuTracker {
    last: Option<(CpuTimeTotals, Instant)>,
}

impl SystemCpuTracker {
    /// Builds a tracker with no baseline.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Whether the next reading will be the first, and so warming up (§8.2).
    #[must_use]
    pub const fn is_warming_up(&self) -> bool {
        self.last.is_none()
    }

    /// When the last reading was accepted, or `None` while warming up.
    #[must_use]
    pub const fn last_observed_at(&self) -> Option<Instant> {
        match self.last {
            Some((_, at)) => Some(at),
            None => None,
        }
    }

    /// Drops the baseline so the next reading warms up again.
    pub fn forget_baseline(&mut self) {
        self.last = None;
    }

    /// Folds one cumulative CPU-time reading in and publishes machine usage.
    pub fn observe(&mut self, totals: CpuTimeTotals, at: Instant) -> MetricState<Percent> {
        // `replace` re-baselines on every path, including the reset path, which
        // is what makes the sample after a reset valid (§8.2).
        let Some((previous, _)) = self.last.replace((totals, at)) else {
            return MetricState::WarmingUp;
        };
        let (Some(busy), Some(idle)) = (
            totals.busy.checked_sub(previous.busy),
            totals.idle.checked_sub(previous.idle),
        ) else {
            // Cumulative CPU time only falls if the counter was reset — a live
            // VM migration, or a re-read of a re-initialised source. §8.2
            // forbids turning that into a number.
            return MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        };
        match percent_of_duration(busy, busy.saturating_add(idle)) {
            // Jiffy-granularity rounding can put the ratio a hair above 100;
            // §8.3 fixes the aggregate range at 0..=100.
            Some(percent) => MetricState::Available(percent.clamped_to_100()),
            // No CPU time passed at all. That is not "0% busy" — it is an
            // interval too short (or a source too stalled) to measure (§8.2).
            None => MetricState::WarmingUp,
        }
    }
}

impl DeltaTracker for SystemCpuTracker {
    type Config = ();
    type Reading = CpuTimeTotals;
    type Value = Percent;

    fn with_config(_config: Self::Config) -> Self {
        Self::new()
    }

    fn observe_reading(&mut self, reading: Self::Reading, at: Instant) -> MetricState<Self::Value> {
        self.observe(reading, at)
    }

    // Written out rather than delegating to the identically named inherent
    // methods, which would be an ambiguous path.
    fn last_observed_at(&self) -> Option<Instant> {
        self.last.map(|(_, at)| at)
    }

    fn forget_baseline(&mut self) {
        self.last = None;
    }
}

/// Per-process CPU utilization from cumulative per-process CPU time (§8.3).
///
/// [`ProcessCpuTracker::observe`] returns the *core*-normalized value: one fully
/// used core is 100%, so a process saturating four cores reads 400% and a
/// single-threaded process on a 64-CPU machine still reads 100%. The other
/// convention is applied on top by [`ProcessCpuTracker::observe_normalized`],
/// which delegates to the frozen [`CpuNormalization::apply`] rather than
/// re-deriving the arithmetic.
///
/// # Monotonic time
///
/// Unlike [`SystemCpuTracker`], this divides by the elapsed interval, so the
/// interval's accuracy is the percentage's accuracy. Callers must pass the
/// snapshot's monotonic `captured_at`; a wall-clock jump would otherwise scale
/// every process on screen (§8.1). The interval is derived with
/// [`Instant::saturating_duration_since`], so it can never be negative.
///
/// # Identity
///
/// One tracker follows one process. Callers must key trackers on
/// [`crate::model::ProcessIdentity`] and not on a bare PID: a reused PID must
/// start a fresh baseline rather than inherit the dead process's CPU time (§26).
/// [`super::KeyedProcessCpuTrackers`] encodes that.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCpuTracker {
    last: Option<(Duration, Instant)>,
}

impl ProcessCpuTracker {
    /// Builds a tracker with no baseline.
    #[must_use]
    pub const fn new() -> Self {
        Self { last: None }
    }

    /// Whether the next reading will be the first, and so warming up (§8.3).
    #[must_use]
    pub const fn is_warming_up(&self) -> bool {
        self.last.is_none()
    }

    /// When the last reading was accepted, or `None` while warming up.
    #[must_use]
    pub const fn last_observed_at(&self) -> Option<Instant> {
        match self.last {
            Some((_, at)) => Some(at),
            None => None,
        }
    }

    /// Drops the baseline so the next reading warms up again.
    pub fn forget_baseline(&mut self) {
        self.last = None;
    }

    /// Folds one cumulative process CPU time in and publishes core-normalized
    /// usage: one core = 100%, so the result may exceed 100% (§8.3).
    pub fn observe(&mut self, cpu_time: Duration, at: Instant) -> MetricState<Percent> {
        let Some((previous_cpu, previous_at)) = self.last.replace((cpu_time, at)) else {
            return MetricState::WarmingUp;
        };
        let Some(cpu_delta) = cpu_time.checked_sub(previous_cpu) else {
            // A single process's CPU time is monotonic, so a fall means the
            // baseline belongs to a different process — the PID was reused
            // behind the caller's back. Keying on identity prevents it; refusing
            // to invent a percentage is the backstop (§26).
            return MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        };
        match percent_of_duration(cpu_delta, at.saturating_duration_since(previous_at)) {
            // Deliberately not clamped: exceeding 100% is the correct answer for
            // a multi-threaded process under core normalization (§8.3).
            Some(percent) => MetricState::Available(percent),
            None => MetricState::WarmingUp,
        }
    }

    /// Folds one reading in and publishes usage in the requested convention.
    ///
    /// `logical_cpus` is only consulted for [`CpuNormalization::Machine`]. A zero
    /// CPU count leaves machine normalization undefined, and §4 forbids
    /// substituting a number for an undefined value, so that case reports
    /// [`UnavailableReason::ReadFailed`] — the CPU count is what failed to read.
    pub fn observe_normalized(
        &mut self,
        cpu_time: Duration,
        at: Instant,
        normalization: CpuNormalization,
        logical_cpus: u16,
    ) -> MetricState<Percent> {
        let core_normalized = self.observe(cpu_time, at);
        let Some(percent) = core_normalized.fresh().copied() else {
            return core_normalized;
        };
        match normalization.apply(percent, logical_cpus) {
            Some(scaled) => MetricState::Available(scaled),
            None => MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
        }
    }
}

impl DeltaTracker for ProcessCpuTracker {
    type Config = ();
    type Reading = Duration;
    type Value = Percent;

    fn with_config(_config: Self::Config) -> Self {
        Self::new()
    }

    fn observe_reading(&mut self, reading: Self::Reading, at: Instant) -> MetricState<Self::Value> {
        self.observe(reading, at)
    }

    // Written out rather than delegating to the identically named inherent
    // methods, which would be an ambiguous path.
    fn last_observed_at(&self) -> Option<Instant> {
        self.last.map(|(_, at)| at)
    }

    fn forget_baseline(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Instant {
        Instant::now()
    }

    fn percent(state: &MetricState<Percent>) -> f32 {
        state
            .fresh()
            .expect("expected a measured percentage")
            .value()
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    #[test]
    fn totals_sum_busy_and_idle_without_overflowing() {
        let totals = CpuTimeTotals::new(secs(3), secs(5));
        assert_eq!(totals.total(), secs(8));
        let extreme = CpuTimeTotals::new(Duration::MAX, secs(1));
        assert_eq!(extreme.total(), Duration::MAX);
    }

    #[test]
    fn a_first_system_sample_is_warming_up_and_not_zero_percent() {
        let mut tracker = SystemCpuTracker::new();
        assert!(tracker.is_warming_up());
        let state = tracker.observe(CpuTimeTotals::new(secs(100), secs(900)), origin());
        assert!(state.is_warming_up());
        assert_eq!(state.fresh(), None);
        assert!(!tracker.is_warming_up());
    }

    #[test]
    fn system_cpu_is_the_busy_share_of_total_cpu_time() {
        // One of eight CPUs fully busy for a second: 1 s busy, 7 s idle.
        let t0 = origin();
        let mut tracker = SystemCpuTracker::new();
        tracker.observe(CpuTimeTotals::new(secs(0), secs(0)), t0);
        let state = tracker.observe(CpuTimeTotals::new(secs(1), secs(7)), t0 + secs(1));
        assert!((percent(&state) - 12.5).abs() < f32::EPSILON);
    }

    #[test]
    fn system_cpu_is_an_aggregate_capped_at_one_hundred_percent() {
        // Every CPU busy: §8.3 fixes the aggregate range at 0..=100, so an
        // eight-CPU machine reads 100%, never 800%.
        let t0 = origin();
        let mut tracker = SystemCpuTracker::new();
        tracker.observe(CpuTimeTotals::new(secs(0), secs(0)), t0);
        let state = tracker.observe(CpuTimeTotals::new(secs(8), secs(0)), t0 + secs(1));
        assert!((percent(&state) - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn system_cpu_does_not_depend_on_the_sample_interval_length() {
        // The divisor is CPU time, so the same deltas over a 250 ms and a 4 s
        // interval must agree. That is why §8.3 prefers delta CPU times.
        let t0 = origin();
        let before = CpuTimeTotals::new(secs(10), secs(70));
        let after = CpuTimeTotals::new(secs(11), secs(77));

        let mut quick = SystemCpuTracker::new();
        quick.observe(before, t0);
        let quick_state = quick.observe(after, t0 + Duration::from_millis(250));

        let mut slow = SystemCpuTracker::new();
        slow.observe(before, t0);
        let slow_state = slow.observe(after, t0 + secs(4));

        assert!((percent(&quick_state) - percent(&slow_state)).abs() < f32::EPSILON);
    }

    #[test]
    fn a_stalled_system_counter_is_warming_up_not_zero_percent() {
        let t0 = origin();
        let totals = CpuTimeTotals::new(secs(10), secs(70));
        let mut tracker = SystemCpuTracker::new();
        tracker.observe(totals, t0);
        let state = tracker.observe(totals, t0 + secs(1));
        assert!(state.is_warming_up());
        assert_ne!(state, MetricState::Available(Percent::ZERO));
    }

    #[test]
    fn system_cpu_time_going_backwards_is_a_reset_and_recovers_next_sample() {
        let t0 = origin();
        let mut tracker = SystemCpuTracker::new();
        tracker.observe(CpuTimeTotals::new(secs(100), secs(700)), t0);

        let reset = tracker.observe(CpuTimeTotals::new(secs(2), secs(6)), t0 + secs(1));
        assert_eq!(
            reset,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );

        let recovered = tracker.observe(CpuTimeTotals::new(secs(3), secs(13)), t0 + secs(2));
        assert!((percent(&recovered) - 12.5).abs() < f32::EPSILON);
    }

    #[test]
    fn an_idle_only_reset_is_detected_as_well_as_a_busy_only_one() {
        let t0 = origin();
        let mut tracker = SystemCpuTracker::new();
        tracker.observe(CpuTimeTotals::new(secs(100), secs(700)), t0);
        let reset = tracker.observe(CpuTimeTotals::new(secs(101), secs(1)), t0 + secs(1));
        assert_eq!(
            reset,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
    }

    #[test]
    fn a_first_process_sample_is_warming_up_and_not_zero_percent() {
        let mut tracker = ProcessCpuTracker::new();
        let state = tracker.observe(secs(42), origin());
        assert!(state.is_warming_up());
        assert_ne!(state, MetricState::Available(Percent::ZERO));
    }

    #[test]
    fn a_fully_busy_single_core_reads_one_hundred_percent_on_an_eight_cpu_machine() {
        // The regression this pins down: multiplying by the CPU count and
        // reporting 800% for one saturated core.
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(0), t0);
        let state = tracker.observe(secs(1), t0 + secs(1));
        assert!((percent(&state) - 100.0).abs() < f32::EPSILON);

        let machine =
            CpuNormalization::Machine.apply(Percent::new(percent(&state)).expect("valid"), 8);
        assert!((machine.expect("valid").value() - 12.5).abs() < f32::EPSILON);
    }

    #[test]
    fn a_process_on_four_cores_reads_four_hundred_percent_under_core_normalization() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(0), t0);
        let state = tracker.observe(secs(4), t0 + secs(1));
        assert!((percent(&state) - 400.0).abs() < f32::EPSILON);
    }

    #[test]
    fn both_normalizations_are_reachable_from_one_reading() {
        let t0 = origin();
        let mut core = ProcessCpuTracker::new();
        core.observe(secs(0), t0);
        let core_state = core.observe_normalized(secs(4), t0 + secs(1), CpuNormalization::Core, 8);
        assert!((percent(&core_state) - 400.0).abs() < f32::EPSILON);

        let mut machine = ProcessCpuTracker::new();
        machine.observe(secs(0), t0);
        let machine_state =
            machine.observe_normalized(secs(4), t0 + secs(1), CpuNormalization::Machine, 8);
        assert!((percent(&machine_state) - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn machine_normalization_without_a_cpu_count_is_unavailable_not_zero() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(0), t0);
        let state = tracker.observe_normalized(secs(1), t0 + secs(1), CpuNormalization::Machine, 0);
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
        );
        assert_eq!(state.fresh(), None);
    }

    #[test]
    fn normalization_preserves_an_unavailable_state_rather_than_scaling_it() {
        let mut tracker = ProcessCpuTracker::new();
        let first = tracker.observe_normalized(secs(5), origin(), CpuNormalization::Machine, 8);
        assert!(first.is_warming_up());
    }

    #[test]
    fn process_cpu_uses_the_actual_elapsed_interval() {
        // 500 ms of CPU over 500 ms of wall time is a saturated core; the same
        // CPU over 2 s is a quarter of one. Assuming a 1 s interval would report
        // 50% for both (§8.1).
        let t0 = origin();
        let cpu = Duration::from_millis(500);

        let mut quick = ProcessCpuTracker::new();
        quick.observe(Duration::ZERO, t0);
        let quick_state = quick.observe(cpu, t0 + Duration::from_millis(500));

        let mut slow = ProcessCpuTracker::new();
        slow.observe(Duration::ZERO, t0);
        let slow_state = slow.observe(cpu, t0 + secs(2));

        assert!((percent(&quick_state) - 100.0).abs() < f32::EPSILON);
        assert!((percent(&slow_state) - 25.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_process_that_used_no_cpu_reads_a_real_zero_percent() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(9), t0);
        let state = tracker.observe(secs(9), t0 + secs(1));
        assert_eq!(state, MetricState::Available(Percent::ZERO));
    }

    #[test]
    fn zero_elapsed_process_sample_is_warming_up_not_a_division_by_zero() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(1), t0);
        let state = tracker.observe(secs(2), t0);
        assert!(state.is_warming_up());
    }

    #[test]
    fn a_reversed_instant_cannot_make_a_process_percentage_negative_or_huge() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(0), t0 + secs(10));
        let state = tracker.observe(secs(5), t0);
        assert!(state.is_warming_up());
    }

    #[test]
    fn process_cpu_time_going_backwards_is_a_reset_and_recovers_next_sample() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(30), t0);

        let reset = tracker.observe(secs(1), t0 + secs(1));
        assert_eq!(
            reset,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );

        let recovered = tracker.observe(secs(2), t0 + secs(2));
        assert!((percent(&recovered) - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn forgetting_a_process_baseline_prevents_a_delta_across_the_gap() {
        let t0 = origin();
        let mut tracker = ProcessCpuTracker::new();
        tracker.observe(secs(0), t0);
        tracker.forget_baseline();
        assert!(tracker.is_warming_up());
        assert!(tracker.observe(secs(600), t0 + secs(1)).is_warming_up());
    }

    #[test]
    fn trackers_report_when_they_last_saw_a_reading() {
        let t0 = origin();
        let at = t0 + secs(5);
        let mut system = SystemCpuTracker::new();
        assert_eq!(system.last_observed_at(), None);
        system.observe(CpuTimeTotals::default(), at);
        assert_eq!(system.last_observed_at(), Some(at));

        let mut process = ProcessCpuTracker::new();
        assert_eq!(process.last_observed_at(), None);
        process.observe(Duration::ZERO, at);
        assert_eq!(process.last_observed_at(), Some(at));
    }
}

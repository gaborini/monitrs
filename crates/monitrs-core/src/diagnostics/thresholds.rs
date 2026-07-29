//! The tunable numbers every rule and every radar signal reads (§11.3, §12).
//!
//! §11.3 requires thresholds to be *encoded in configuration* rather than
//! scattered through the rules. This struct is that configuration: the five keys
//! §12 names under `[diagnostics]` plus documented defaults for everything else
//! the rules in §11.2 need. Nothing in this module reads a file — the config
//! layer deserializes into this type and hands it to the engine.

use core::time::Duration;

/// §12 default for `diagnostics.cpu_watch_percent`.
pub const DEFAULT_CPU_WATCH_PERCENT: f32 = 80.0;
/// §12 default for `diagnostics.cpu_critical_percent`.
pub const DEFAULT_CPU_CRITICAL_PERCENT: f32 = 95.0;
/// §12 default for `diagnostics.memory_watch_available_percent`.
pub const DEFAULT_MEMORY_WATCH_AVAILABLE_PERCENT: f32 = 15.0;
/// §12 default for `diagnostics.memory_critical_available_percent`.
pub const DEFAULT_MEMORY_CRITICAL_AVAILABLE_PERCENT: f32 = 5.0;
/// §12 default for `diagnostics.sustained_samples`.
pub const DEFAULT_SUSTAINED_SAMPLES: usize = 10;

/// Default number of recent samples a sustained condition is counted over.
///
/// §11.3's worked example reads "CPU > 90% for 12 of the last 15 samples", so the
/// window is a little larger than [`DEFAULT_SUSTAINED_SAMPLES`]: a condition may
/// miss a sample or two and still be sustained, which is exactly the tolerance
/// that stops one clean tick from clearing a real problem.
pub const DEFAULT_SUSTAINED_WINDOW: usize = 15;

/// Largest accepted `sustained_window`.
///
/// The window is a per-signal ring of observations, so it must be bounded for the
/// same reason history is (§10.3). Six hundred one-second observations is ten
/// minutes, far beyond any threshold a radar signal needs.
pub const MAX_SUSTAINED_WINDOW: usize = 600;

/// Smallest accepted value for any "how many intervals" multiplier.
pub const MIN_INTERVAL_MULTIPLE: f32 = 1.0;
/// Largest accepted value for any "how many intervals" multiplier.
///
/// Bounded so that a mistyped configuration cannot produce a threshold that
/// overflows duration arithmetic.
pub const MAX_INTERVAL_MULTIPLE: f32 = 1_000.0;

/// One mebibyte, used by several byte-valued defaults below.
const MIB: u64 = 1024 * 1024;

/// Every threshold the diagnostic engine reads.
///
/// Deliberately `Copy` plain data: rules hold a copy so that evaluating a rule is
/// a pure function of `(snapshot, history)` and needs no shared configuration
/// handle (§11.1).
///
/// Values arriving from configuration are not trusted. [`Thresholds::sanitized`]
/// repairs the combinations that would otherwise make a rule silently
/// undecidable — a critical threshold below its watch threshold, a window
/// narrower than the number of samples counted inside it, a non-finite percentage.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(default))]
pub struct Thresholds {
    /// §12's `diagnostics.enabled`.
    ///
    /// When false the engine derives nothing at all: [`super::RuleSet`] produces
    /// no findings and every radar signal reports [`crate::MetricState::Unsupported`].
    /// It deliberately does *not* report `normal`, because "we did not look" and
    /// "we looked and the system is fine" are different statements (§2.3).
    pub enabled: bool,

    /// Aggregate CPU busy percentage at which the CPU signal reaches `watch`.
    pub cpu_watch_percent: f32,
    /// Aggregate CPU busy percentage at which the CPU signal reaches `critical`.
    pub cpu_critical_percent: f32,

    /// Available-memory share at or below which the memory signal reaches `watch`.
    ///
    /// Expressed as *available* rather than used because that is the number that
    /// predicts reclaim pressure, and because §8.4 forbids treating all non-free
    /// memory as application use.
    pub memory_watch_available_percent: f32,
    /// Available-memory share at or below which the memory signal reaches
    /// `critical`.
    pub memory_critical_available_percent: f32,

    /// How many samples inside the window must meet a condition before a
    /// sustained finding is made, and before hysteresis escalates a signal.
    ///
    /// Also the minimum number of observations required before any sustained
    /// claim is possible at all: with fewer than this, the signal is
    /// [`crate::MetricState::WarmingUp`] (§11.3).
    pub sustained_samples: usize,
    /// How many recent samples `sustained_samples` is counted out of.
    pub sustained_window: usize,

    /// Combined swap-in plus swap-out throughput at which swap activity is
    /// `watch`, in bytes per second.
    ///
    /// Default 1 MiB/s. Below that, paging is ordinary lazy loading and a large
    /// but idle swap file is unremarkable; §11.2 cares about *activity*.
    pub swap_watch_bytes_per_second: f64,
    /// Combined swap throughput at which swap activity is `critical`, in bytes
    /// per second.
    ///
    /// Default 16 MiB/s: sustained tens of mebibytes per second means the working
    /// set does not fit and every fault is paid for in latency.
    pub swap_critical_bytes_per_second: f64,

    /// Linux PSI `some avg10` share at which a PSI signal reaches `watch`.
    ///
    /// Default 10%: a tenth of the last ten seconds with at least one task
    /// stalled is measurable rather than incidental.
    pub psi_watch_percent: f32,
    /// Linux PSI `some avg10` share at which a PSI signal reaches `critical`.
    ///
    /// Default 40%. Note that a busy machine legitimately shows non-zero CPU PSI,
    /// which is why this is generous rather than near zero.
    pub psi_critical_percent: f32,

    /// Block-device busy percentage at which the disk signal reaches `watch`.
    ///
    /// Only meaningful where the platform reports a semantically correct busy
    /// figure (§7.3); elsewhere the signal is unsupported rather than zero.
    pub disk_busy_watch_percent: f32,
    /// Block-device busy percentage at which the disk signal reaches `critical`.
    pub disk_busy_critical_percent: f32,

    /// Link utilization at which the network signal reaches `watch`.
    ///
    /// Only computed when the link speed is known (§7.4).
    pub network_watch_percent: f32,
    /// Link utilization at which the network signal reaches `critical`.
    pub network_critical_percent: f32,

    /// One-minute load per logical CPU at which the load signal reaches `watch`.
    ///
    /// Default 1.0: a run queue as long as the CPU count means everything
    /// runnable is waiting for a turn.
    pub load_watch_per_cpu: f32,
    /// One-minute load per logical CPU at which the load signal reaches
    /// `critical`. Default 2.0.
    pub load_critical_per_cpu: f32,

    /// Core-normalized CPU percentage a process must reach before a rise in its
    /// usage is reported at all. Default 100%, i.e. a full core.
    pub process_cpu_spike_percent: f32,
    /// Percentage points a process's CPU must rise between two samples to be
    /// called a spike. Default 50 points.
    pub process_cpu_spike_points: f32,

    /// Resident set size a process must exceed before its growth is reported.
    ///
    /// Default 128 MiB. Small processes double their footprint routinely and
    /// reporting them would bury the interesting cases.
    pub process_rss_minimum_bytes: u64,
    /// Resident-set growth rate that counts as rapidly increasing, in bytes per
    /// minute.
    ///
    /// Default 64 MiB/min. §11.3 forbids concluding anything about *why* memory
    /// is growing from this alone, so the rule reports the observation only.
    pub process_rss_growth_bytes_per_minute: u64,

    /// How many zombies must be present before the finding escalates from
    /// informational to `watch`.
    ///
    /// Default 8. One zombie is a normal instant in a process's teardown; a
    /// growing pile means a parent is not reaping (§11.2).
    pub zombie_watch_count: usize,

    /// Collector lag, in sample intervals, at which falling behind is `watch`.
    pub collector_lag_watch_intervals: f32,
    /// Collector lag, in sample intervals, at which falling behind is `critical`.
    pub collector_lag_critical_intervals: f32,

    /// Data age, in sample intervals, at which a snapshot counts as stale.
    pub stale_watch_intervals: f32,
    /// Data age, in sample intervals, at which staleness is `critical`.
    pub stale_critical_intervals: f32,

    /// Interval growth, in sample intervals, that is treated as a discontinuity
    /// rather than as a measurement.
    ///
    /// A suspended laptop produces one enormous interval. §11.3 requires the
    /// engine to reset cleanly across that rather than read the gap as an event,
    /// so anything beyond this multiple of the normal interval clears hysteresis
    /// state instead of feeding it (default 10 intervals).
    pub discontinuity_intervals: f32,

    /// Our own CPU budget, core-normalized (§16.1: p95 below 2%).
    pub self_cpu_budget_percent: f32,
    /// Our own resident memory budget (§16.1: below 50 MiB).
    pub self_rss_budget_bytes: u64,
    /// Our own fast-tier collection budget, in milliseconds (§16.1: p95 below
    /// 200 ms at 200 processes).
    ///
    /// Stored as milliseconds rather than a [`Duration`] so that it round-trips
    /// through a configuration file as a plain number; [`Self::self_sample_budget`]
    /// converts it.
    pub self_sample_budget_millis: u64,
}

impl Default for Thresholds {
    /// The §12 defaults, plus the documented defaults for the rest.
    fn default() -> Self {
        Self {
            enabled: true,
            cpu_watch_percent: DEFAULT_CPU_WATCH_PERCENT,
            cpu_critical_percent: DEFAULT_CPU_CRITICAL_PERCENT,
            memory_watch_available_percent: DEFAULT_MEMORY_WATCH_AVAILABLE_PERCENT,
            memory_critical_available_percent: DEFAULT_MEMORY_CRITICAL_AVAILABLE_PERCENT,
            sustained_samples: DEFAULT_SUSTAINED_SAMPLES,
            sustained_window: DEFAULT_SUSTAINED_WINDOW,
            swap_watch_bytes_per_second: MIB as f64,
            swap_critical_bytes_per_second: 16.0 * MIB as f64,
            psi_watch_percent: 10.0,
            psi_critical_percent: 40.0,
            disk_busy_watch_percent: 80.0,
            disk_busy_critical_percent: 95.0,
            network_watch_percent: 70.0,
            network_critical_percent: 90.0,
            load_watch_per_cpu: 1.0,
            load_critical_per_cpu: 2.0,
            process_cpu_spike_percent: 100.0,
            process_cpu_spike_points: 50.0,
            process_rss_minimum_bytes: 128 * MIB,
            process_rss_growth_bytes_per_minute: 64 * MIB,
            zombie_watch_count: 8,
            collector_lag_watch_intervals: 2.0,
            collector_lag_critical_intervals: 5.0,
            stale_watch_intervals: 3.0,
            stale_critical_intervals: 10.0,
            discontinuity_intervals: 10.0,
            self_cpu_budget_percent: 2.0,
            self_rss_budget_bytes: 50 * MIB,
            self_sample_budget_millis: 200,
        }
    }
}

impl Thresholds {
    /// Repairs values that would make a rule undecidable.
    ///
    /// Clamping rather than rejecting matches §8.5's treatment of history
    /// configuration: a mistyped number must not stop monitrs from starting, and
    /// it must not silently disable a documented signal either. Every constructor
    /// in this module sanitizes, so no rule has to defend itself against
    /// `sustained_window < sustained_samples` or a NaN percentage.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let defaults = Self::default();
        let mut out = Self {
            enabled: self.enabled,
            cpu_watch_percent: non_negative(self.cpu_watch_percent, defaults.cpu_watch_percent),
            cpu_critical_percent: non_negative(
                self.cpu_critical_percent,
                defaults.cpu_critical_percent,
            ),
            memory_watch_available_percent: non_negative(
                self.memory_watch_available_percent,
                defaults.memory_watch_available_percent,
            ),
            memory_critical_available_percent: non_negative(
                self.memory_critical_available_percent,
                defaults.memory_critical_available_percent,
            ),
            sustained_samples: self.sustained_samples.clamp(1, MAX_SUSTAINED_WINDOW),
            sustained_window: self.sustained_window.clamp(1, MAX_SUSTAINED_WINDOW),
            swap_watch_bytes_per_second: non_negative_f64(
                self.swap_watch_bytes_per_second,
                defaults.swap_watch_bytes_per_second,
            ),
            swap_critical_bytes_per_second: non_negative_f64(
                self.swap_critical_bytes_per_second,
                defaults.swap_critical_bytes_per_second,
            ),
            psi_watch_percent: non_negative(self.psi_watch_percent, defaults.psi_watch_percent),
            psi_critical_percent: non_negative(
                self.psi_critical_percent,
                defaults.psi_critical_percent,
            ),
            disk_busy_watch_percent: non_negative(
                self.disk_busy_watch_percent,
                defaults.disk_busy_watch_percent,
            ),
            disk_busy_critical_percent: non_negative(
                self.disk_busy_critical_percent,
                defaults.disk_busy_critical_percent,
            ),
            network_watch_percent: non_negative(
                self.network_watch_percent,
                defaults.network_watch_percent,
            ),
            network_critical_percent: non_negative(
                self.network_critical_percent,
                defaults.network_critical_percent,
            ),
            load_watch_per_cpu: non_negative(self.load_watch_per_cpu, defaults.load_watch_per_cpu),
            load_critical_per_cpu: non_negative(
                self.load_critical_per_cpu,
                defaults.load_critical_per_cpu,
            ),
            process_cpu_spike_percent: non_negative(
                self.process_cpu_spike_percent,
                defaults.process_cpu_spike_percent,
            ),
            process_cpu_spike_points: non_negative(
                self.process_cpu_spike_points,
                defaults.process_cpu_spike_points,
            ),
            process_rss_minimum_bytes: self.process_rss_minimum_bytes,
            process_rss_growth_bytes_per_minute: self.process_rss_growth_bytes_per_minute.max(1),
            zombie_watch_count: self.zombie_watch_count.max(1),
            collector_lag_watch_intervals: multiple(
                self.collector_lag_watch_intervals,
                defaults.collector_lag_watch_intervals,
            ),
            collector_lag_critical_intervals: multiple(
                self.collector_lag_critical_intervals,
                defaults.collector_lag_critical_intervals,
            ),
            stale_watch_intervals: multiple(
                self.stale_watch_intervals,
                defaults.stale_watch_intervals,
            ),
            stale_critical_intervals: multiple(
                self.stale_critical_intervals,
                defaults.stale_critical_intervals,
            ),
            discontinuity_intervals: multiple(
                self.discontinuity_intervals,
                defaults.discontinuity_intervals,
            ),
            self_cpu_budget_percent: non_negative(
                self.self_cpu_budget_percent,
                defaults.self_cpu_budget_percent,
            ),
            self_rss_budget_bytes: self.self_rss_budget_bytes.max(1),
            self_sample_budget_millis: self.self_sample_budget_millis.max(1),
        };

        // A critical threshold at or below its watch threshold would make the
        // watch state unreachable, so the more severe bound always wins.
        out.cpu_critical_percent = out.cpu_critical_percent.max(out.cpu_watch_percent);
        out.psi_critical_percent = out.psi_critical_percent.max(out.psi_watch_percent);
        out.disk_busy_critical_percent = out
            .disk_busy_critical_percent
            .max(out.disk_busy_watch_percent);
        out.network_critical_percent = out.network_critical_percent.max(out.network_watch_percent);
        out.load_critical_per_cpu = out.load_critical_per_cpu.max(out.load_watch_per_cpu);
        out.swap_critical_bytes_per_second = out
            .swap_critical_bytes_per_second
            .max(out.swap_watch_bytes_per_second);
        out.collector_lag_critical_intervals = out
            .collector_lag_critical_intervals
            .max(out.collector_lag_watch_intervals);
        out.stale_critical_intervals = out.stale_critical_intervals.max(out.stale_watch_intervals);
        // Memory thresholds are inverted: less available is worse, so the
        // critical bound is the *lower* number.
        out.memory_critical_available_percent = out
            .memory_critical_available_percent
            .min(out.memory_watch_available_percent);
        // Counting ten samples out of a window of five is not a condition anyone
        // can meet; widening the window keeps the requested severity.
        out.sustained_window = out.sustained_window.max(out.sustained_samples);
        out
    }

    /// How many observations must exist before any sustained claim is possible.
    ///
    /// Below this the signal is [`crate::MetricState::WarmingUp`] rather than
    /// `normal`: a rule that needs ten samples has no opinion after three, and
    /// §26 forbids dressing "no opinion" up as a measurement.
    #[must_use]
    pub const fn minimum_samples(&self) -> usize {
        self.sustained_samples
    }

    /// The fast-tier collection budget as a duration (§16.1).
    #[must_use]
    pub const fn self_sample_budget(&self) -> Duration {
        Duration::from_millis(self.self_sample_budget_millis)
    }

    /// The used-memory share equivalent to [`Self::memory_watch_available_percent`].
    ///
    /// History retains the *used* share (§8.5), so the available-share thresholds
    /// have to be expressed in the same terms to be counted over a window.
    #[must_use]
    pub fn memory_watch_used_percent(&self) -> f32 {
        (100.0 - self.memory_watch_available_percent).max(0.0)
    }

    /// The used-memory share equivalent to
    /// [`Self::memory_critical_available_percent`].
    #[must_use]
    pub fn memory_critical_used_percent(&self) -> f32 {
        (100.0 - self.memory_critical_available_percent).max(0.0)
    }

    /// Scales a sample interval by a multiplier without risking an overflow panic.
    ///
    /// [`Duration::mul_f32`] panics on overflow, which §14.3 forbids anywhere near
    /// the render path, so the arithmetic is done in seconds and compared as such.
    #[must_use]
    pub fn intervals_as_seconds(interval: Duration, multiple: f32) -> f64 {
        interval.as_secs_f64()
            * f64::from(multiple.clamp(MIN_INTERVAL_MULTIPLE, MAX_INTERVAL_MULTIPLE))
    }
}

/// Keeps a finite, non-negative percentage, falling back to the default.
fn non_negative(value: f32, fallback: f32) -> f32 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        fallback
    }
}

/// Keeps a finite, non-negative rate, falling back to the default.
fn non_negative_f64(value: f64, fallback: f64) -> f64 {
    if value.is_finite() && value >= 0.0 {
        value
    } else {
        fallback
    }
}

/// Keeps an interval multiplier inside the range duration arithmetic tolerates.
fn multiple(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(MIN_INTERVAL_MULTIPLE, MAX_INTERVAL_MULTIPLE)
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_the_numbers_section_twelve_documents() {
        let thresholds = Thresholds::default();
        assert!(thresholds.enabled);
        assert!((thresholds.cpu_watch_percent - 80.0).abs() < f32::EPSILON);
        assert!((thresholds.cpu_critical_percent - 95.0).abs() < f32::EPSILON);
        assert!((thresholds.memory_watch_available_percent - 15.0).abs() < f32::EPSILON);
        assert!((thresholds.memory_critical_available_percent - 5.0).abs() < f32::EPSILON);
        assert_eq!(thresholds.sustained_samples, 10);
    }

    #[test]
    fn the_defaults_are_already_sanitary() {
        assert_eq!(Thresholds::default().sanitized(), Thresholds::default());
    }

    #[test]
    fn a_window_narrower_than_the_sample_count_is_widened_not_ignored() {
        // Counting "10 of the last 5" can never be satisfied, which would
        // silently disable every sustained rule.
        let thresholds = Thresholds {
            sustained_samples: 10,
            sustained_window: 5,
            ..Thresholds::default()
        }
        .sanitized();
        assert_eq!(thresholds.sustained_window, 10);
        assert_eq!(thresholds.sustained_samples, 10);
    }

    #[test]
    fn zero_samples_becomes_one_so_a_condition_still_has_to_be_observed() {
        let thresholds = Thresholds {
            sustained_samples: 0,
            sustained_window: 0,
            ..Thresholds::default()
        }
        .sanitized();
        assert_eq!(thresholds.sustained_samples, 1);
        assert_eq!(thresholds.sustained_window, 1);
        assert_eq!(thresholds.minimum_samples(), 1);
    }

    #[test]
    fn an_inverted_pair_of_thresholds_is_ordered_by_severity() {
        let thresholds = Thresholds {
            cpu_watch_percent: 95.0,
            cpu_critical_percent: 40.0,
            ..Thresholds::default()
        }
        .sanitized();
        assert!(thresholds.cpu_critical_percent >= thresholds.cpu_watch_percent);
    }

    #[test]
    fn memory_thresholds_are_inverted_so_critical_is_the_lower_share() {
        let thresholds = Thresholds {
            memory_watch_available_percent: 5.0,
            memory_critical_available_percent: 15.0,
            ..Thresholds::default()
        }
        .sanitized();
        assert!(
            thresholds.memory_critical_available_percent
                <= thresholds.memory_watch_available_percent,
            "less available memory must be the more severe state"
        );
    }

    #[test]
    fn non_finite_percentages_fall_back_to_the_documented_default() {
        let thresholds = Thresholds {
            cpu_watch_percent: f32::NAN,
            psi_watch_percent: f32::INFINITY,
            load_watch_per_cpu: -3.0,
            swap_watch_bytes_per_second: f64::NAN,
            ..Thresholds::default()
        }
        .sanitized();
        assert!((thresholds.cpu_watch_percent - DEFAULT_CPU_WATCH_PERCENT).abs() < f32::EPSILON);
        assert!(thresholds.psi_watch_percent.is_finite());
        assert!(thresholds.load_watch_per_cpu >= 0.0);
        assert!(thresholds.swap_watch_bytes_per_second.is_finite());
    }

    #[test]
    fn interval_multiples_are_bounded_so_duration_arithmetic_cannot_overflow() {
        let thresholds = Thresholds {
            stale_watch_intervals: f32::MAX,
            discontinuity_intervals: 0.0,
            ..Thresholds::default()
        }
        .sanitized();
        assert!(thresholds.stale_watch_intervals <= MAX_INTERVAL_MULTIPLE);
        assert!(thresholds.discontinuity_intervals >= MIN_INTERVAL_MULTIPLE);

        let seconds = Thresholds::intervals_as_seconds(Duration::from_secs(1), f32::MAX);
        assert!(seconds.is_finite());
    }

    #[test]
    fn memory_shares_convert_between_available_and_used() {
        let thresholds = Thresholds::default();
        assert!((thresholds.memory_watch_used_percent() - 85.0).abs() < f32::EPSILON);
        assert!((thresholds.memory_critical_used_percent() - 95.0).abs() < f32::EPSILON);
    }

    #[test]
    fn an_available_share_above_one_hundred_does_not_produce_a_negative_used_share() {
        let thresholds = Thresholds {
            memory_watch_available_percent: 400.0,
            ..Thresholds::default()
        };
        assert!(thresholds.memory_watch_used_percent() >= 0.0);
    }

    #[test]
    fn the_self_overhead_budgets_match_section_sixteen() {
        let thresholds = Thresholds::default();
        assert_eq!(thresholds.self_sample_budget(), Duration::from_millis(200));
        assert_eq!(thresholds.self_rss_budget_bytes, 50 * 1024 * 1024);
        assert!((thresholds.self_cpu_budget_percent - 2.0).abs() < f32::EPSILON);
    }
}

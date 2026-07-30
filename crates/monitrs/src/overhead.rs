//! What monitrs costs the machine it is watching (§26, §16.1).
//!
//! §26: *a system monitor must measure and expose its own overhead.* A monitor
//! that cannot answer "and what do **you** cost?" is asking for trust it has not
//! earned, so this module measures our own process and states plainly, per §16.1
//! budget, whether we are inside it.
//!
//! # How our own process is read
//!
//! Through `sysinfo`, the same library [`monitrs_collectors::CommonCollector`]
//! uses, with a **single-process** refresh of our own PID. Three alternatives were
//! rejected:
//!
//! * Parsing `/proc/self/*` ourselves would add a second platform path for data
//!   the collector layer already knows how to read, and it has no macOS analogue
//!   that avoids FFI — and this crate `#![forbid(unsafe_code)]`.
//! * Picking our own row out of the snapshot the sampler already produces gives
//!   RSS but not *cumulative* CPU time, and cumulative time is what
//!   [`ProcessCpuTracker`] needs in order to warm up instead of reporting a first
//!   sample of zero (§8.2, §26).
//! * Refreshing the whole process table again would make the measurement a
//!   significant part of the thing being measured.
//!
//! # Percentiles without an unbounded history
//!
//! §16.1 states its CPU budget as a median and a 95th percentile, which needs a
//! history — and an unbounded history of our own overhead would itself be an
//! overhead bug. [`RollingQuantiles`] keeps a fixed window (the most recent
//! [`QUANTILE_WINDOW`] samples, about eight and a half minutes at a one-second
//! interval) and computes ranks on demand, so nothing here grows over a
//! twelve-hour run.
//!
//! # Growth, not just the instant
//!
//! §16.1 also forbids unbounded memory and descriptor growth over a long run. A
//! single RSS reading cannot show that, so [`GrowthTrend`] carries the slope
//! between the first and the latest reading together with the peak: a leak shows
//! as a sustained rise, while a transient shows up as a peak well above a flat
//! slope. That is a two-point estimate rather than a regression, and the peak is
//! reported beside it precisely so that one spike is not read as a leak.
//!
//! # What is deliberately absent
//!
//! §16.1's input-to-visible-response budget is not measured here: it needs the
//! timestamp of the keypress that caused a frame, which only the event loop has.
//! This module measures the resources and the durations a sampling loop can see.

// Consumed by the assembled interactive runtime and by the Inspect screen, both of
// which land in later slices; the tests below exercise every item. Scoped to
// non-test builds so a genuinely unused item still shows up while testing.
#![cfg_attr(not(test), allow(dead_code))]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use monitrs_core::diagnostics::Thresholds;
use monitrs_core::model::{MeasuredValue, MetricState, SelfOverhead, UnavailableReason};
use monitrs_core::rates::ProcessCpuTracker;
use monitrs_core::units::Percent;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

/// How many samples each rolling window retains.
///
/// 512 samples is about 8.5 minutes at the default one-second interval, which
/// matches §16.1's "idle" framing: a percentile over a twelve-hour window would
/// hide a monitor that started misbehaving an hour ago. Fixed, because §10.3
/// forbids unbounded accumulation and this is written from the sampling loop.
pub(crate) const QUANTILE_WINDOW: usize = 512;

/// The shortest span over which a growth trend is claimed.
///
/// Below this, start-up allocation is indistinguishable from a leak: a process
/// that has just filled its history ring is "growing" for the first few seconds,
/// and reporting that as unbounded growth would be crying wolf.
pub(crate) const MIN_TREND_SPAN: Duration = Duration::from_secs(60);

/// Resident-memory growth slower than this is reported as flat.
///
/// One MiB per hour is below the noise floor of allocator fragmentation over a
/// long run, and §16.1's concern is *unbounded* growth, not motion.
pub(crate) const RSS_GROWTH_TOLERANCE_PER_HOUR: f64 = 1024.0 * 1024.0;

/// Descriptor growth below this is reported as flat.
///
/// One descriptor per hour is a rounding error in any real run; a descriptor
/// leaked per sample shows up immediately.
pub(crate) const OPEN_FILES_GROWTH_TOLERANCE_PER_HOUR: f64 = 1.0;

/// What one reading says about our own descriptor count.
///
/// Counting descriptors means reading a directory (`/proc/self/fd`) or asking the
/// kernel for the whole descriptor list (macOS). That is cheap but not free, and
/// §16.1 will not tolerate the measurement becoming the overhead, so callers may
/// take it on a slower cadence than CPU and RSS.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum OpenFilesReading {
    /// Counted in this reading.
    Counted(MetricState<u32>),
    /// Not counted this time. The monitor retains the last good value and ages
    /// it, which is exactly what [`MetricState::Stale`] is for (§4).
    NotCounted,
}

/// One raw reading of our own process.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SelfReading {
    /// Cumulative CPU time this process has consumed since it started.
    ///
    /// Cumulative rather than a percentage: the percentage is derived by
    /// [`ProcessCpuTracker`] from two readings and the measured interval between
    /// them, so the first reading warms up instead of claiming 0% (§8.2).
    pub(crate) cpu_time: Duration,
    /// Resident set size, in bytes.
    pub(crate) rss_bytes: u64,
    /// Our open descriptor count, or a note that it was not counted this time.
    pub(crate) open_files: OpenFilesReading,
}

/// Why our own process could not be read.
///
/// A recoverable error in §14.1's taxonomy: not knowing our own overhead is a
/// missing metric, never a reason to stop monitoring the machine.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OverheadError {
    /// The process table does not contain our own PID.
    ///
    /// Possible inside a PID namespace whose `/proc` is not the one we are in, and
    /// on a machine under enough pressure that the read failed outright.
    #[error("monitrs cannot see its own process (pid {pid}), so its overhead is unknown")]
    SelfNotVisible {
        /// The PID that was looked up.
        pid: u32,
    },
}

/// Something that can read our own process.
///
/// A trait so that [`OverheadMonitor`] is testable against a scripted reader:
/// pinning down a p95 needs dozens of controlled samples, which no live read can
/// provide deterministically.
pub(crate) trait SelfProcessReader {
    /// Reads our own process.
    ///
    /// `want_open_files` decides whether the descriptor count is taken; see
    /// [`OpenFilesReading`].
    ///
    /// # Errors
    ///
    /// [`OverheadError::SelfNotVisible`] when our own PID is not in the table.
    fn read(&mut self, want_open_files: bool) -> Result<SelfReading, OverheadError>;
}

/// Reads our own process through `sysinfo`.
///
/// Long-lived on purpose: `sysinfo` keeps its per-process bookkeeping inside the
/// [`System`] handle, and recreating it every tick would throw that away for no
/// gain (§9.1, §26).
#[derive(Debug)]
pub(crate) struct SysinfoSelfReader {
    system: System,
    pid: Pid,
}

impl SysinfoSelfReader {
    /// Builds a reader for the current process.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self {
            system: System::new(),
            pid: Pid::from_u32(std::process::id()),
        }
    }

    /// The PID being measured.
    #[must_use]
    pub(crate) fn pid(&self) -> u32 {
        self.pid.as_u32()
    }
}

impl Default for SysinfoSelfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl SelfProcessReader for SysinfoSelfReader {
    fn read(&mut self, want_open_files: bool) -> Result<SelfReading, OverheadError> {
        // `Some(&[pid])` and `remove_dead_processes = false`: this handle only ever
        // holds our own entry, so there is nothing to prune and no table to walk.
        self.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            false,
            ProcessRefreshKind::nothing().with_cpu().with_memory(),
        );

        let process = self
            .system
            .process(self.pid)
            .ok_or(OverheadError::SelfNotVisible { pid: self.pid() })?;

        let open_files = if want_open_files {
            OpenFilesReading::Counted(count_open_files(process))
        } else {
            OpenFilesReading::NotCounted
        };

        Ok(SelfReading {
            // `accumulated_cpu_time` is CPU-milliseconds: jiffies on Linux and
            // Mach absolute time on macOS, both converted by sysinfo. The
            // granularity is the platform's, not ours — a 10 ms jiffy is 1% of a
            // one-second interval, which is why §16.1's budget is a percentile
            // over many samples rather than a single reading.
            cpu_time: Duration::from_millis(process.accumulated_cpu_time()),
            rss_bytes: process.memory(),
            open_files,
        })
    }
}

/// Turns `sysinfo`'s optional descriptor count into an availability (§4).
///
/// `None` means the read failed, which is not zero open files: a process always
/// has at least its standard streams, so zero would be a visibly wrong number
/// rather than a merely misleading one.
fn count_open_files(process: &sysinfo::Process) -> MetricState<u32> {
    match process.open_files() {
        Some(count) => u32::try_from(count).map_or(
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            MetricState::Available,
        ),
        None => MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
    }
}

/// A bounded rolling window of integer samples, with quantiles on demand.
///
/// Integers rather than floats throughout: a percentile is a rank, so the
/// comparison must be exact and the stored form must be sortable without worrying
/// about NaN. Percentages are held in thousandths of a percent and durations in
/// microseconds, both finer than either platform reports.
#[derive(Clone, Debug)]
pub(crate) struct RollingQuantiles {
    samples: VecDeque<u64>,
    capacity: usize,
}

impl RollingQuantiles {
    /// A window holding at most `capacity` samples.
    ///
    /// A zero capacity is raised to one: a window that can hold nothing would
    /// report "not yet measured" forever, which is worse than a short window.
    #[must_use]
    pub(crate) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Adds a sample, evicting the oldest once the window is full.
    pub(crate) fn push(&mut self, value: u64) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    /// How many samples the window currently holds.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether nothing has been observed yet.
    #[must_use]
    pub(crate) fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The `percentile`th value by nearest rank, or `None` while empty.
    ///
    /// The nearest-rank definition: the sample at rank `ceil(percentile × n /
    /// 100)`, counting from one. Two consequences are deliberate.
    ///
    /// * **No interpolation.** An interpolated p95 of two samples reports a number
    ///   that was never measured, and §4's rule against inventing values does not
    ///   stop at percentiles. Every value here was observed.
    /// * **A single outlier in twenty samples does not move p95**, because one
    ///   sample is 5% of twenty and the rank lands just below it. That is the
    ///   definition working: §16.1's p95 budget is about sustained cost, and one
    ///   slow tick during start-up is not that. [`Self::max`] is there for the
    ///   outlier itself.
    ///
    /// Sorting happens here rather than on `push` because this is read when the
    /// Inspect screen is open and pushed on every single sample.
    #[must_use]
    pub(crate) fn percentile(&self, percentile: u32) -> Option<u64> {
        let len = self.samples.len();
        if len == 0 {
            return None;
        }
        let mut sorted: Vec<u64> = self.samples.iter().copied().collect();
        sorted.sort_unstable();

        let count = u64::try_from(len).unwrap_or(u64::MAX);
        let percentile = u64::from(percentile.min(100));
        // ceil(percentile * count / 100), then clamped into 1..=count so that p0
        // is the smallest sample rather than a rank of zero.
        let rank = percentile
            .saturating_mul(count)
            .saturating_add(99)
            .checked_div(100)
            .unwrap_or(1)
            .clamp(1, count);
        let index = usize::try_from(rank.saturating_sub(1)).unwrap_or(len - 1);
        sorted.get(index.min(len - 1)).copied()
    }

    /// The median.
    #[must_use]
    pub(crate) fn median(&self) -> Option<u64> {
        self.percentile(50)
    }

    /// The 95th percentile, as §16.1 states its budgets.
    #[must_use]
    pub(crate) fn p95(&self) -> Option<u64> {
        self.percentile(95)
    }

    /// The largest sample in the window.
    #[must_use]
    pub(crate) fn max(&self) -> Option<u64> {
        self.samples.iter().copied().max()
    }
}

/// Which way a bounded resource is moving over the whole run (§16.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TrendDirection {
    /// Not observed for long enough to tell (see [`MIN_TREND_SPAN`]).
    TooEarly,
    /// Steady within the tolerance.
    Flat,
    /// Rising: what an unbounded-growth bug looks like.
    Growing,
    /// Falling, e.g. after a peak in the process table.
    Shrinking,
}

impl TrendDirection {
    /// Lower-case label for the Inspect screen.
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::TooEarly => "too early to tell",
            Self::Flat => "flat",
            Self::Growing => "growing",
            Self::Shrinking => "shrinking",
        }
    }

    /// Whether this direction is the one §16.1 asks us to watch for.
    #[must_use]
    pub(crate) const fn is_growing(self) -> bool {
        matches!(self, Self::Growing)
    }
}

/// The slope and the extremes of one resource over the whole run.
///
/// Fixed-size: the whole point is to characterise a twelve-hour run without
/// keeping twelve hours of samples.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GrowthTrend {
    first: Option<(u64, Instant)>,
    latest: Option<(u64, Instant)>,
    peak: u64,
    trough: u64,
    observations: u64,
}

impl GrowthTrend {
    /// A trend with nothing observed.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self {
            first: None,
            latest: None,
            peak: 0,
            trough: u64::MAX,
            observations: 0,
        }
    }

    /// Folds one observation in.
    pub(crate) fn observe(&mut self, value: u64, at: Instant) {
        if self.first.is_none() {
            self.first = Some((value, at));
        }
        self.latest = Some((value, at));
        self.peak = self.peak.max(value);
        self.trough = self.trough.min(value);
        self.observations = self.observations.saturating_add(1);
    }

    /// The most recent observation.
    #[must_use]
    pub(crate) fn latest(&self) -> Option<u64> {
        self.latest.map(|(value, _)| value)
    }

    /// The largest observation, or `None` if there has not been one.
    #[must_use]
    pub(crate) fn peak(&self) -> Option<u64> {
        self.latest.map(|_| self.peak)
    }

    /// The smallest observation, or `None` if there has not been one.
    #[must_use]
    pub(crate) fn trough(&self) -> Option<u64> {
        self.latest.map(|_| self.trough)
    }

    /// How many observations there have been.
    #[must_use]
    pub(crate) const fn observations(&self) -> u64 {
        self.observations
    }

    /// How long the observations span.
    #[must_use]
    pub(crate) fn span(&self) -> Duration {
        match (self.first, self.latest) {
            (Some((_, first)), Some((_, latest))) => latest.saturating_duration_since(first),
            _ => Duration::ZERO,
        }
    }

    /// Signed change per hour, or `None` before [`MIN_TREND_SPAN`] has elapsed.
    ///
    /// Signed because shrinking and growing are different answers, and §26's rule
    /// against inventing numbers applies to a slope measured over no time at all.
    #[must_use]
    pub(crate) fn per_hour(&self) -> Option<f64> {
        let (first, _) = self.first?;
        let (latest, _) = self.latest?;
        let span = self.span();
        if span < MIN_TREND_SPAN {
            return None;
        }
        let hours = span.as_secs_f64() / 3_600.0;
        if hours <= 0.0 {
            return None;
        }
        // Both endpoints are byte or descriptor counts, which f64 represents
        // exactly up to 2^53 — far past any resident set this program can have.
        #[allow(clippy::cast_precision_loss)]
        let change = latest as f64 - first as f64;
        Some(change / hours)
    }

    /// The direction, given how much change per hour counts as flat.
    #[must_use]
    pub(crate) fn direction(&self, tolerance_per_hour: f64) -> TrendDirection {
        match self.per_hour() {
            None => TrendDirection::TooEarly,
            Some(per_hour) if per_hour > tolerance_per_hour => TrendDirection::Growing,
            Some(per_hour) if per_hour < -tolerance_per_hour => TrendDirection::Shrinking,
            Some(_) => TrendDirection::Flat,
        }
    }
}

impl Default for GrowthTrend {
    fn default() -> Self {
        Self::new()
    }
}

/// The §16.1 budgets, as numbers a report can print.
///
/// Separate from [`Thresholds`], which the diagnostic engine uses to *fire a
/// rule*, because §16.1 states two CPU figures — a median and a p95 — while a rule
/// only needs the one it escalates on. [`Self::from_thresholds`] keeps the two in
/// step, so a configured budget moves both the finding and this report.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct OverheadBudgets {
    /// Median self CPU, core-normalized. §16.1: below 1%.
    pub(crate) cpu_median_percent: f32,
    /// 95th-percentile self CPU, core-normalized. §16.1: below 2%.
    pub(crate) cpu_p95_percent: f32,
    /// Resident memory. §16.1: below 50 MiB in the default configuration.
    pub(crate) rss_bytes: u64,
    /// 95th-percentile sample collection. §16.1: below 200 ms at 200 processes.
    pub(crate) sample_p95: Duration,
    /// Ordinary frame render. §16.1: below 16 ms at 160×48.
    pub(crate) frame: Duration,
}

impl Default for OverheadBudgets {
    /// Exactly the numbers in §16.1.
    fn default() -> Self {
        Self {
            cpu_median_percent: 1.0,
            cpu_p95_percent: 2.0,
            rss_bytes: 50 * 1024 * 1024,
            sample_p95: Duration::from_millis(200),
            frame: Duration::from_millis(16),
        }
    }
}

impl OverheadBudgets {
    /// Budgets taken from the configured diagnostic thresholds.
    ///
    /// `self_cpu_budget_percent` is the p95 figure: that is the harder of §16.1's
    /// two limits and the one the
    /// [`monitrs_core::diagnostics::SELF_OVERHEAD`] rule escalates on. The median
    /// budget is half of it, which is §16.1's own ratio (1% median against a 2%
    /// p95), so a user who relaxes one relaxes both.
    #[must_use]
    pub(crate) fn from_thresholds(thresholds: &Thresholds) -> Self {
        let defaults = Self::default();
        Self {
            cpu_median_percent: thresholds.self_cpu_budget_percent / 2.0,
            cpu_p95_percent: thresholds.self_cpu_budget_percent,
            rss_bytes: thresholds.self_rss_budget_bytes,
            sample_p95: thresholds.self_sample_budget(),
            frame: defaults.frame,
        }
    }
}

/// Whether a measurement is inside its budget (§16.1).
///
/// Three states, not two: claiming "within budget" before anything was measured
/// would be the same lie as rendering an unavailable metric as zero (§26).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Verdict {
    /// Measured and inside the budget.
    Within,
    /// Measured and over the budget.
    Over,
    /// Not measured yet.
    NotYetMeasured,
}

impl Verdict {
    /// Plain-text verdict for the Inspect screen (§7.5).
    #[must_use]
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Within => "within budget",
            Self::Over => "over budget",
            Self::NotYetMeasured => "not yet measured",
        }
    }

    /// A redundant non-colour cue (§5.2: colour is supplementary).
    ///
    /// The same three characters [`monitrs_core::model::Severity::symbol`] uses,
    /// so one glossary covers both surfaces.
    #[must_use]
    pub(crate) const fn symbol(self) -> char {
        match self {
            Self::Within => '.',
            Self::Over => '!',
            Self::NotYetMeasured => '?',
        }
    }

    /// Whether this verdict is a budget violation.
    #[must_use]
    pub(crate) const fn is_over(self) -> bool {
        matches!(self, Self::Over)
    }

    /// The verdict for an optional measurement against `budget`.
    fn for_value(measured: Option<u64>, budget: u64) -> Self {
        match measured {
            None => Self::NotYetMeasured,
            Some(value) if value > budget => Self::Over,
            Some(_) => Self::Within,
        }
    }
}

/// One budget line for the Inspect screen (§7.5).
///
/// Carries the measurement *and* the budget it was compared against, because
/// "over budget" without the two numbers is an assertion rather than evidence:
/// §2.3's rule that a signal shows its raw metric applies to us too.
/// A duration row carries the raw [`Duration`], not a pre-formatted string: the
/// Inspect screen must render it with
/// [`monitrs_core::units::format_duration`], because
/// [`MeasuredValue::render`]'s age form (`mm:ss`) collapses every sub-second
/// budget to `00:00`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct BudgetRow {
    /// What was measured, e.g. `"self cpu median"`.
    pub(crate) label: &'static str,
    /// The measurement, or why there is not one. Never a substituted zero (§26).
    pub(crate) measured: MetricState<MeasuredValue>,
    /// The budget from §16.1.
    pub(crate) budget: MeasuredValue,
    /// The plain verdict.
    pub(crate) verdict: Verdict,
}

/// One growth line for the Inspect screen (§16.1's long-run checks).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct TrendRow {
    /// What is being watched, e.g. `"resident memory"`.
    pub(crate) label: &'static str,
    /// Which way it is moving.
    pub(crate) direction: TrendDirection,
    /// The latest reading, or why there is not one.
    pub(crate) latest: MetricState<MeasuredValue>,
    /// The largest reading of the run so far.
    pub(crate) peak: MetricState<MeasuredValue>,
    /// The magnitude of the change per hour; `direction` carries the sign. `None`
    /// until [`MIN_TREND_SPAN`] has elapsed.
    pub(crate) per_hour: Option<MeasuredValue>,
    /// How long the observations span, so a reader can judge the slope.
    pub(crate) span: Duration,
}

/// Everything the Inspect screen needs to state our own overhead (§7.5, §26).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OverheadReport {
    /// One row per §16.1 budget, in a stable order.
    pub(crate) budgets: Vec<BudgetRow>,
    /// One row per resource §16.1 asks us to watch for growth.
    pub(crate) trends: Vec<TrendRow>,
    /// How many self-samples the percentiles are based on.
    pub(crate) samples: usize,
}

impl OverheadReport {
    /// Whether any budget is exceeded.
    #[must_use]
    pub(crate) fn any_over(&self) -> bool {
        self.budgets.iter().any(|row| row.verdict.is_over())
    }

    /// Whether anything is growing without bound so far as we can tell.
    #[must_use]
    pub(crate) fn any_growing(&self) -> bool {
        self.trends.iter().any(|row| row.direction.is_growing())
    }

    /// The budget row with `label`.
    #[must_use]
    pub(crate) fn budget(&self, label: &str) -> Option<&BudgetRow> {
        self.budgets.iter().find(|row| row.label == label)
    }

    /// The trend row with `label`.
    #[must_use]
    pub(crate) fn trend(&self, label: &str) -> Option<&TrendRow> {
        self.trends.iter().find(|row| row.label == label)
    }
}

/// Measures monitrs's own overhead over time (§26).
///
/// One instance for the life of the process, fed one [`SelfReading`] per sampling
/// tick. Holds no OS handle of its own: the reading arrives from a
/// [`SelfProcessReader`], so displaying this performs no I/O in the renderer
/// (§10.5, §26).
#[derive(Debug)]
pub(crate) struct OverheadMonitor {
    cpu: ProcessCpuTracker,
    cpu_millipercent: RollingQuantiles,
    sample_micros: RollingQuantiles,
    frame_micros: RollingQuantiles,
    rss: GrowthTrend,
    open_files: GrowthTrend,
    /// The last counted descriptor total and when it was counted, so a skipped
    /// count can be aged into [`MetricState::Stale`] rather than dropped (§4).
    last_open_files: Option<(u32, Instant)>,
    budgets: OverheadBudgets,
    latest: Option<SelfOverhead>,
    readings: u64,
}

impl OverheadMonitor {
    /// A monitor with the §16.1 budgets and nothing observed.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::with_budgets(OverheadBudgets::default())
    }

    /// A monitor with explicit budgets, e.g. from configured thresholds.
    #[must_use]
    pub(crate) fn with_budgets(budgets: OverheadBudgets) -> Self {
        Self {
            cpu: ProcessCpuTracker::new(),
            cpu_millipercent: RollingQuantiles::new(QUANTILE_WINDOW),
            sample_micros: RollingQuantiles::new(QUANTILE_WINDOW),
            frame_micros: RollingQuantiles::new(QUANTILE_WINDOW),
            rss: GrowthTrend::new(),
            open_files: GrowthTrend::new(),
            last_open_files: None,
            budgets,
            latest: None,
            readings: 0,
        }
    }

    /// The budgets in force.
    #[must_use]
    pub(crate) const fn budgets(&self) -> &OverheadBudgets {
        &self.budgets
    }

    /// How many readings have been folded in.
    #[must_use]
    pub(crate) const fn readings(&self) -> u64 {
        self.readings
    }

    /// The most recently published overhead, if any.
    #[must_use]
    pub(crate) const fn latest(&self) -> Option<&SelfOverhead> {
        self.latest.as_ref()
    }

    /// Folds one reading in and returns the publishable overhead.
    ///
    /// `None` while our own CPU is still warming up. [`SelfOverhead::cpu`] is a
    /// bare [`Percent`], so there is no way to say "warming up" *inside* the
    /// struct, and publishing `0%` would be exactly the lie §26 forbids —
    /// [`monitrs_core::model::CollectorHealth::self_overhead`] is an `Option` for
    /// this reason. The trends and the descriptor count are updated either way,
    /// because they are not deltas and are valid from the first reading.
    pub(crate) fn observe(
        &mut self,
        reading: SelfReading,
        at: Instant,
        history_bytes: u64,
    ) -> Option<SelfOverhead> {
        self.readings = self.readings.saturating_add(1);
        self.rss.observe(reading.rss_bytes, at);

        let open_files = self.fold_open_files(reading.open_files, at);
        let cpu_state = self.cpu.observe(reading.cpu_time, at);
        if let Some(&cpu) = cpu_state.fresh() {
            self.cpu_millipercent.push(percent_to_millipercent(cpu));
            let overhead = SelfOverhead {
                cpu,
                rss_bytes: reading.rss_bytes,
                history_bytes,
                open_files,
            };
            self.latest = Some(overhead);
            return Some(overhead);
        }
        // Warming up, a counter reset, or a zero-length interval. The previously
        // published value stays in place: it is the last thing actually measured,
        // and replacing it with a fabricated one would be worse than leaving it.
        None
    }

    /// Records how long one collection took (§16.1: sample p95 below 200 ms).
    pub(crate) fn observe_sample_duration(&mut self, duration: Duration) {
        self.sample_micros.push(micros(duration));
    }

    /// Records how long one frame took to render (§16.1: below 16 ms at 160×48).
    pub(crate) fn observe_frame_duration(&mut self, duration: Duration) {
        self.frame_micros.push(micros(duration));
    }

    /// Takes a fresh descriptor count, or ages the last one into a stale value.
    fn fold_open_files(&mut self, reading: OpenFilesReading, at: Instant) -> MetricState<u32> {
        match reading {
            OpenFilesReading::Counted(state) => {
                if let Some(&count) = state.fresh() {
                    self.open_files.observe(u64::from(count), at);
                    self.last_open_files = Some((count, at));
                }
                state
            }
            OpenFilesReading::NotCounted => match self.last_open_files {
                Some((value, counted_at)) => MetricState::Stale {
                    value,
                    age: at.saturating_duration_since(counted_at),
                },
                // Never counted: the honest answer is that it needs a sample, not
                // that we have no descriptors open.
                None => MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample),
            },
        }
    }

    /// Median self CPU over the window.
    #[must_use]
    pub(crate) fn cpu_median(&self) -> Option<Percent> {
        self.cpu_millipercent.median().map(millipercent_to_percent)
    }

    /// 95th-percentile self CPU over the window, as §16.1 states its budget.
    #[must_use]
    pub(crate) fn cpu_p95(&self) -> Option<Percent> {
        self.cpu_millipercent.p95().map(millipercent_to_percent)
    }

    /// Worst self CPU seen inside the window.
    #[must_use]
    pub(crate) fn cpu_max(&self) -> Option<Percent> {
        self.cpu_millipercent.max().map(millipercent_to_percent)
    }

    /// Median collection duration.
    #[must_use]
    pub(crate) fn sample_median(&self) -> Option<Duration> {
        self.sample_micros.median().map(Duration::from_micros)
    }

    /// 95th-percentile collection duration.
    #[must_use]
    pub(crate) fn sample_p95(&self) -> Option<Duration> {
        self.sample_micros.p95().map(Duration::from_micros)
    }

    /// Median frame render duration.
    #[must_use]
    pub(crate) fn frame_median(&self) -> Option<Duration> {
        self.frame_micros.median().map(Duration::from_micros)
    }

    /// 95th-percentile frame render duration.
    #[must_use]
    pub(crate) fn frame_p95(&self) -> Option<Duration> {
        self.frame_micros.p95().map(Duration::from_micros)
    }

    /// The resident-memory trend (§16.1: no unbounded growth over a long run).
    #[must_use]
    pub(crate) fn rss_trend(&self) -> TrendDirection {
        self.rss.direction(RSS_GROWTH_TOLERANCE_PER_HOUR)
    }

    /// The descriptor-count trend (§16.1: no unbounded descriptor growth).
    #[must_use]
    pub(crate) fn open_files_trend(&self) -> TrendDirection {
        self.open_files
            .direction(OPEN_FILES_GROWTH_TOLERANCE_PER_HOUR)
    }

    /// Everything the Inspect screen renders about our own cost (§7.5).
    #[must_use]
    pub(crate) fn report(&self) -> OverheadReport {
        let budgets = vec![
            self.percent_row(
                "self cpu median",
                self.cpu_median(),
                self.budgets.cpu_median_percent,
            ),
            self.percent_row("self cpu p95", self.cpu_p95(), self.budgets.cpu_p95_percent),
            BudgetRow {
                label: "resident memory",
                measured: measured_or_warming(
                    self.latest
                        .map(|overhead| MeasuredValue::Bytes(overhead.rss_bytes)),
                ),
                budget: MeasuredValue::Bytes(self.budgets.rss_bytes),
                verdict: Verdict::for_value(
                    self.latest.map(|overhead| overhead.rss_bytes),
                    self.budgets.rss_bytes,
                ),
            },
            BudgetRow {
                label: "history memory",
                measured: measured_or_warming(
                    self.latest
                        .map(|overhead| MeasuredValue::Bytes(overhead.history_bytes)),
                ),
                // §16.1 gives the history ring no budget of its own, so it is
                // reported against the whole-process figure: a ring larger than
                // the process budget is a configuration error worth seeing.
                budget: MeasuredValue::Bytes(self.budgets.rss_bytes),
                verdict: Verdict::for_value(
                    self.latest.map(|overhead| overhead.history_bytes),
                    self.budgets.rss_bytes,
                ),
            },
            self.duration_row(
                "sample collection p95",
                self.sample_p95(),
                self.budgets.sample_p95,
            ),
            self.duration_row("frame render p95", self.frame_p95(), self.budgets.frame),
        ];

        let trends = vec![
            TrendRow {
                label: "resident memory",
                direction: self.rss_trend(),
                latest: measured_or_warming(self.rss.latest().map(MeasuredValue::Bytes)),
                peak: measured_or_warming(self.rss.peak().map(MeasuredValue::Bytes)),
                per_hour: self.rss.per_hour().map(bytes_per_hour),
                span: self.rss.span(),
            },
            TrendRow {
                label: "open files",
                direction: self.open_files_trend(),
                latest: measured_or_warming(self.open_files.latest().map(MeasuredValue::Count)),
                peak: measured_or_warming(self.open_files.peak().map(MeasuredValue::Count)),
                per_hour: self.open_files.per_hour().map(count_per_hour),
                span: self.open_files.span(),
            },
        ];

        OverheadReport {
            budgets,
            trends,
            samples: self.cpu_millipercent.len(),
        }
    }

    /// A percentage row, compared in thousandths so the number shown and the
    /// verdict cannot disagree about rounding.
    fn percent_row(
        &self,
        label: &'static str,
        measured: Option<Percent>,
        budget_percent: f32,
    ) -> BudgetRow {
        let budget = Percent::new(budget_percent.max(0.0)).unwrap_or(Percent::ZERO);
        BudgetRow {
            label,
            measured: measured_or_warming(measured.map(MeasuredValue::Percent)),
            budget: MeasuredValue::Percent(budget),
            verdict: Verdict::for_value(
                measured.map(percent_to_millipercent),
                percent_to_millipercent(budget),
            ),
        }
    }

    /// A duration row.
    fn duration_row(
        &self,
        label: &'static str,
        measured: Option<Duration>,
        budget: Duration,
    ) -> BudgetRow {
        BudgetRow {
            label,
            measured: measured_or_warming(measured.map(MeasuredValue::Duration)),
            budget: MeasuredValue::Duration(budget),
            verdict: Verdict::for_value(measured.map(micros), micros(budget)),
        }
    }
}

impl Default for OverheadMonitor {
    fn default() -> Self {
        Self::new()
    }
}

/// An optional measurement as an availability: absent means warming up, never
/// zero (§26).
fn measured_or_warming(value: Option<MeasuredValue>) -> MetricState<MeasuredValue> {
    value.map_or(MetricState::WarmingUp, MetricState::Available)
}

/// A percentage in thousandths of a percent.
///
/// Integral so that quantile ranks compare exactly. 0.001% is three orders of
/// magnitude finer than anything either platform reports about a process.
fn percent_to_millipercent(percent: Percent) -> u64 {
    let scaled = f64::from(percent.value()) * 1_000.0;
    if !scaled.is_finite() || scaled <= 0.0 {
        return 0;
    }
    // Clamped before the cast, which is what makes the truncation harmless: the
    // value is a percentage of one core, so the ceiling is never approached.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let clamped = scaled.min(u64::MAX as f64) as u64;
    clamped
}

/// Thousandths of a percent back to a [`Percent`].
///
/// Falls back to zero only for a value `Percent` itself rejects, which cannot
/// arise from a number that came *from* a `Percent`.
fn millipercent_to_percent(millipercent: u64) -> Percent {
    // A percentage of a core can exceed 100 (§8.3), so scaling is all that
    // happens here — no clamping to 100.
    #[allow(clippy::cast_precision_loss)]
    let value = millipercent as f64 / 1_000.0;
    #[allow(clippy::cast_possible_truncation)]
    let value = value as f32;
    Percent::new(value).unwrap_or(Percent::ZERO)
}

/// Microseconds, saturating, without a float cast.
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

/// A byte slope as an unsigned magnitude the UI can format.
fn bytes_per_hour(per_hour: f64) -> MeasuredValue {
    MeasuredValue::Bytes(saturating_magnitude(per_hour))
}

/// A descriptor slope as an unsigned magnitude.
fn count_per_hour(per_hour: f64) -> MeasuredValue {
    MeasuredValue::Count(saturating_magnitude(per_hour))
}

/// `|value|` as an integer, saturating and NaN-safe.
fn saturating_magnitude(value: f64) -> u64 {
    let magnitude = value.abs();
    if !magnitude.is_finite() {
        return u64::MAX;
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let integral = magnitude.min(u64::MAX as f64) as u64;
    integral
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    /// A reader that answers from a script, so percentiles and trends are
    /// deterministic. A live read cannot pin down a p95.
    #[derive(Debug, Default)]
    struct ScriptedReader {
        readings: VecDeque<SelfReading>,
        reads: usize,
        wanted_open_files: usize,
    }

    impl ScriptedReader {
        fn new(readings: impl IntoIterator<Item = SelfReading>) -> Self {
            Self {
                readings: readings.into_iter().collect(),
                reads: 0,
                wanted_open_files: 0,
            }
        }
    }

    impl SelfProcessReader for ScriptedReader {
        fn read(&mut self, want_open_files: bool) -> Result<SelfReading, OverheadError> {
            self.reads += 1;
            if want_open_files {
                self.wanted_open_files += 1;
            }
            self.readings
                .pop_front()
                .ok_or(OverheadError::SelfNotVisible { pid: 1 })
        }
    }

    /// Renders a measured value the way the Inspect screen has to: a duration
    /// goes through `format_duration`, because the age form collapses every
    /// sub-second value to `00:00`.
    fn render(value: MeasuredValue) -> String {
        match value {
            MeasuredValue::Duration(duration) => monitrs_core::units::format_duration(duration),
            other => other.render(monitrs_core::units::ByteUnits::Iec),
        }
    }

    fn reading(cpu_millis: u64, rss_bytes: u64, open_files: Option<u32>) -> SelfReading {
        SelfReading {
            cpu_time: Duration::from_millis(cpu_millis),
            rss_bytes,
            open_files: match open_files {
                Some(count) => OpenFilesReading::Counted(MetricState::Available(count)),
                None => OpenFilesReading::NotCounted,
            },
        }
    }

    #[test]
    fn the_first_self_sample_warms_up_rather_than_reporting_no_overhead() {
        // §26: the first sample of delta-based data is not zero. Publishing a
        // `SelfOverhead` with 0% would claim monitrs is free.
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        let first = monitor.observe(reading(0, 12 * MIB, Some(9)), start, 0);
        assert!(first.is_none(), "the first reading has no interval");
        assert!(monitor.latest().is_none());
        assert_eq!(monitor.cpu_median(), None);
        assert_eq!(
            monitor
                .report()
                .budget("self cpu median")
                .map(|row| row.verdict),
            Some(Verdict::NotYetMeasured),
            "an unmeasured budget is not a satisfied budget"
        );
    }

    #[test]
    fn the_second_sample_publishes_a_real_percentage_and_the_ring_size() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        monitor.observe(reading(0, 12 * MIB, Some(9)), start, 0);
        // 10 ms of CPU in one second of wall time is 1% of one core.
        let overhead = monitor
            .observe(
                reading(10, 13 * MIB, Some(9)),
                start + Duration::from_secs(1),
                7 * MIB,
            )
            .expect("the second reading measures an interval");
        assert!((overhead.cpu.value() - 1.0).abs() < 0.001, "{overhead:?}");
        assert_eq!(overhead.rss_bytes, 13 * MIB);
        assert_eq!(overhead.history_bytes, 7 * MIB);
        assert_eq!(overhead.open_files, MetricState::Available(9));
        assert_eq!(
            monitor.latest().map(|latest| latest.rss_bytes),
            Some(13 * MIB)
        );
    }

    #[test]
    fn our_own_cpu_uses_the_measured_interval_rather_than_an_assumed_tick() {
        // Two identical CPU deltas over different intervals must not read the
        // same: assuming a one-second tick is the §8.1 mistake.
        let start = Instant::now();
        let mut quick = OverheadMonitor::new();
        quick.observe(reading(0, MIB, Some(3)), start, 0);
        let quick_overhead = quick
            .observe(
                reading(50, MIB, Some(3)),
                start + Duration::from_millis(500),
                0,
            )
            .expect("measured");

        let mut slow = OverheadMonitor::new();
        slow.observe(reading(0, MIB, Some(3)), start, 0);
        let slow_overhead = slow
            .observe(reading(50, MIB, Some(3)), start + Duration::from_secs(2), 0)
            .expect("measured");

        assert!((quick_overhead.cpu.value() - 10.0).abs() < 0.001);
        assert!((slow_overhead.cpu.value() - 2.5).abs() < 0.001);
    }

    #[test]
    fn a_cpu_counter_that_goes_backwards_keeps_the_last_measured_value() {
        let start = Instant::now();
        let mut monitor = OverheadMonitor::new();
        monitor.observe(reading(0, MIB, Some(3)), start, 0);
        let good = monitor
            .observe(reading(20, MIB, Some(3)), start + Duration::from_secs(1), 0)
            .expect("measured");
        // Cumulative process CPU time cannot fall; if it does, the reading is not
        // comparable, and inventing a percentage from it would be worse than
        // keeping the last real one.
        let reset = monitor.observe(reading(1, MIB, Some(3)), start + Duration::from_secs(2), 0);
        assert!(reset.is_none());
        assert_eq!(monitor.latest().map(|latest| latest.cpu), Some(good.cpu));
    }

    #[test]
    fn the_median_and_p95_come_from_the_window_not_the_last_sample() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        let mut cpu_millis = 0u64;
        monitor.observe(reading(cpu_millis, MIB, Some(3)), start, 0);
        // Ninety samples at 1% and ten at 50%: the median must ignore the busy
        // tenth, and the p95 must land inside it.
        for index in 1..=100u64 {
            cpu_millis += if index > 90 { 500 } else { 10 };
            monitor.observe(
                reading(cpu_millis, MIB, Some(3)),
                start + Duration::from_secs(index),
                0,
            );
        }
        let median = monitor.cpu_median().expect("median").value();
        let p95 = monitor.cpu_p95().expect("p95").value();
        let max = monitor.cpu_max().expect("max").value();
        assert!((median - 1.0).abs() < 0.01, "median {median}");
        assert!(
            (p95 - 50.0).abs() < 0.01,
            "p95 {p95} must see the busy tenth"
        );
        assert!((max - 50.0).abs() < 0.01, "max {max}");
    }

    #[test]
    fn one_slow_tick_in_twenty_does_not_move_the_p95() {
        // The nearest-rank definition, pinned down deliberately: a single outlier
        // is 5% of twenty samples, so p95 stays with the sustained cost §16.1
        // budgets and `max` carries the outlier. An interpolating percentile would
        // report a number nothing ever measured.
        let mut window = RollingQuantiles::new(QUANTILE_WINDOW);
        for _ in 0..19 {
            window.push(1_000);
        }
        window.push(900_000);
        assert_eq!(window.median(), Some(1_000));
        assert_eq!(window.p95(), Some(1_000));
        assert_eq!(window.max(), Some(900_000));
        assert_eq!(window.percentile(100), Some(900_000));
    }

    #[test]
    fn the_percentile_window_is_bounded_so_a_long_run_cannot_grow() {
        let mut window = RollingQuantiles::new(8);
        for value in 0..1_000u64 {
            window.push(value);
        }
        assert_eq!(window.len(), 8, "§16.1: no unbounded growth");
        assert_eq!(
            window.max(),
            Some(999),
            "the newest samples are the kept ones"
        );
        assert_eq!(window.percentile(0), Some(992));
    }

    #[test]
    fn quantiles_of_a_single_sample_are_that_sample_and_of_none_are_absent() {
        let mut window = RollingQuantiles::new(QUANTILE_WINDOW);
        assert!(window.is_empty());
        assert_eq!(window.median(), None);
        assert_eq!(window.p95(), None);
        window.push(42);
        assert_eq!(window.median(), Some(42));
        assert_eq!(window.p95(), Some(42));
        assert_eq!(window.percentile(100), Some(42));
    }

    #[test]
    fn a_zero_capacity_window_still_measures_something() {
        let mut window = RollingQuantiles::new(0);
        window.push(5);
        window.push(6);
        assert_eq!(window.len(), 1);
        assert_eq!(window.median(), Some(6));
    }

    #[test]
    fn the_percentile_rank_is_exact_for_a_known_distribution() {
        let mut window = RollingQuantiles::new(100);
        for value in 1..=100u64 {
            window.push(value);
        }
        assert_eq!(window.median(), Some(50));
        assert_eq!(window.p95(), Some(95));
        assert_eq!(window.percentile(100), Some(100));
        assert_eq!(window.percentile(0), Some(1));
    }

    #[test]
    fn sample_and_frame_durations_are_measured_against_their_own_budgets() {
        let mut monitor = OverheadMonitor::new();
        for millis in [10u64, 12, 15, 400] {
            monitor.observe_sample_duration(Duration::from_millis(millis));
        }
        for millis in [2u64, 3, 4, 60] {
            monitor.observe_frame_duration(Duration::from_millis(millis));
        }
        assert_eq!(monitor.sample_median(), Some(Duration::from_millis(12)));
        assert_eq!(monitor.sample_p95(), Some(Duration::from_millis(400)));
        assert_eq!(monitor.frame_median(), Some(Duration::from_millis(3)));

        let report = monitor.report();
        let sample = report.budget("sample collection p95").expect("row");
        assert_eq!(
            sample.verdict,
            Verdict::Over,
            "400 ms is over the 200 ms budget"
        );
        assert_eq!(
            sample.budget,
            MeasuredValue::Duration(Duration::from_millis(200))
        );
        let frame = report.budget("frame render p95").expect("row");
        assert_eq!(
            frame.verdict,
            Verdict::Over,
            "60 ms is over the 16 ms budget"
        );
        assert!(report.any_over());
    }

    #[test]
    fn a_measurement_inside_every_budget_reports_within() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        monitor.observe(reading(0, 20 * MIB, Some(11)), start, 0);
        for index in 1..=10u64 {
            monitor.observe(
                reading(index * 5, 20 * MIB, Some(11)),
                start + Duration::from_secs(index),
                4 * MIB,
            );
        }
        monitor.observe_sample_duration(Duration::from_millis(30));
        monitor.observe_frame_duration(Duration::from_millis(4));

        let report = monitor.report();
        assert!(!report.any_over(), "{report:#?}");
        for label in [
            "self cpu median",
            "self cpu p95",
            "resident memory",
            "history memory",
            "sample collection p95",
            "frame render p95",
        ] {
            assert_eq!(
                report.budget(label).map(|row| row.verdict),
                Some(Verdict::Within),
                "{label}"
            );
        }
        assert_eq!(report.samples, 10);
    }

    #[test]
    fn resident_memory_over_fifty_mebibytes_is_over_with_the_number_beside_it() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        monitor.observe(reading(0, 80 * MIB, Some(3)), start, 0);
        monitor.observe(
            reading(5, 80 * MIB, Some(3)),
            start + Duration::from_secs(1),
            0,
        );

        let report = monitor.report();
        let row = report.budget("resident memory").expect("row");
        assert_eq!(row.verdict, Verdict::Over);
        assert_eq!(
            row.measured,
            MetricState::Available(MeasuredValue::Bytes(80 * MIB))
        );
        assert_eq!(row.budget, MeasuredValue::Bytes(50 * MIB));
        assert_eq!(row.verdict.label(), "over budget");
        assert_eq!(row.verdict.symbol(), '!', "§5.2: colour is supplementary");
    }

    #[test]
    fn a_measurement_exactly_at_the_limit_is_within_it() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        monitor.observe(reading(0, 50 * MIB, Some(3)), start, 0);
        monitor.observe(
            reading(0, 50 * MIB, Some(3)),
            start + Duration::from_secs(1),
            0,
        );
        assert_eq!(
            monitor
                .report()
                .budget("resident memory")
                .map(|row| row.verdict),
            Some(Verdict::Within),
            "§16.1 says below 50 MiB; the boundary itself is not a violation"
        );
    }

    #[test]
    fn growing_resident_memory_is_reported_as_a_trend_not_just_an_instant() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        // 1 MiB per minute for ten minutes: 60 MiB/hour, far past the tolerance.
        for minute in 0..=10u64 {
            monitor.observe(
                reading(minute * 10, (10 + minute) * MIB, Some(20)),
                start + Duration::from_secs(minute * 60),
                0,
            );
        }
        assert_eq!(monitor.rss_trend(), TrendDirection::Growing);
        let report = monitor.report();
        assert!(report.any_growing());
        let trend = report.trend("resident memory").expect("row");
        assert_eq!(trend.direction.label(), "growing");
        assert_eq!(
            trend.latest,
            MetricState::Available(MeasuredValue::Bytes(20 * MIB))
        );
        assert_eq!(
            trend.peak,
            MetricState::Available(MeasuredValue::Bytes(20 * MIB))
        );
        assert_eq!(trend.span, Duration::from_secs(600));
        assert_eq!(trend.per_hour, Some(MeasuredValue::Bytes(60 * MIB)));
    }

    #[test]
    fn a_short_run_refuses_to_call_a_trend_rather_than_guessing() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        for second in 0..10u64 {
            monitor.observe(
                reading(second, (10 + second) * MIB, Some(5)),
                start + Duration::from_secs(second),
                0,
            );
        }
        assert_eq!(
            monitor.rss_trend(),
            TrendDirection::TooEarly,
            "start-up allocation is not a leak"
        );
        let report = monitor.report();
        assert!(!report.any_growing());
        assert_eq!(
            report.trend("resident memory").and_then(|row| row.per_hour),
            None,
            "no slope is claimed over less than a minute"
        );
    }

    #[test]
    fn steady_memory_over_a_long_run_is_flat_and_the_spike_is_visible_beside_it() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        for minute in 0..=10u64 {
            // One transient spike, then back to the same figure.
            let rss = if minute == 5 { 40 * MIB } else { 20 * MIB };
            monitor.observe(
                reading(minute * 10, rss, Some(7)),
                start + Duration::from_secs(minute * 60),
                0,
            );
        }
        assert_eq!(monitor.rss_trend(), TrendDirection::Flat);
        let report = monitor.report();
        let row = report.trend("resident memory").expect("row");
        assert_eq!(
            row.latest,
            MetricState::Available(MeasuredValue::Bytes(20 * MIB))
        );
        assert_eq!(
            row.peak,
            MetricState::Available(MeasuredValue::Bytes(40 * MIB)),
            "the peak is what distinguishes a spike from a leak"
        );
    }

    #[test]
    fn shrinking_memory_is_not_reported_as_growth() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        for minute in 0..=5u64 {
            monitor.observe(
                reading(minute * 10, (60 - minute * 5) * MIB, Some(7)),
                start + Duration::from_secs(minute * 60),
                0,
            );
        }
        assert_eq!(monitor.rss_trend(), TrendDirection::Shrinking);
        assert!(!monitor.rss_trend().is_growing());
    }

    #[test]
    fn leaking_descriptors_show_up_in_the_open_files_trend() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        for minute in 0..=10u64 {
            let descriptors = u32::try_from(12 + minute * 6).expect("small");
            monitor.observe(
                reading(minute * 10, 20 * MIB, Some(descriptors)),
                start + Duration::from_secs(minute * 60),
                0,
            );
        }
        assert_eq!(monitor.open_files_trend(), TrendDirection::Growing);
        let report = monitor.report();
        let row = report.trend("open files").expect("row");
        assert_eq!(row.latest, MetricState::Available(MeasuredValue::Count(72)));
        assert_eq!(
            row.per_hour,
            Some(MeasuredValue::Count(360)),
            "six descriptors a minute is 360 an hour"
        );
    }

    #[test]
    fn a_skipped_descriptor_count_is_stale_with_its_age_not_a_fresh_zero() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        monitor.observe(reading(0, MIB, Some(14)), start, 0);
        let counted = monitor
            .observe(reading(5, MIB, Some(14)), start + Duration::from_secs(1), 0)
            .expect("measured");
        assert_eq!(counted.open_files, MetricState::Available(14));

        let skipped = monitor
            .observe(reading(10, MIB, None), start + Duration::from_secs(6), 0)
            .expect("measured");
        assert_eq!(
            skipped.open_files,
            MetricState::Stale {
                value: 14,
                age: Duration::from_secs(5)
            },
            "§4: a retained value must carry its age"
        );
    }

    #[test]
    fn a_descriptor_count_that_was_never_taken_is_unavailable_not_zero() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        monitor.observe(reading(0, MIB, None), start, 0);
        let overhead = monitor
            .observe(reading(5, MIB, None), start + Duration::from_secs(1), 0)
            .expect("measured");
        assert_eq!(
            overhead.open_files,
            MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample),
            "§26: unavailable is not zero, and a process always has descriptors open"
        );
        assert_eq!(monitor.open_files_trend(), TrendDirection::TooEarly);
    }

    #[test]
    fn a_failed_descriptor_read_is_carried_through_rather_than_replaced() {
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        let failed = SelfReading {
            cpu_time: Duration::from_millis(5),
            rss_bytes: MIB,
            open_files: OpenFilesReading::Counted(MetricState::PermissionDenied),
        };
        monitor.observe(reading(0, MIB, None), start, 0);
        let overhead = monitor
            .observe(failed, start + Duration::from_secs(1), 0)
            .expect("measured");
        assert_eq!(overhead.open_files, MetricState::PermissionDenied);
    }

    #[test]
    fn the_budgets_default_to_the_numbers_in_section_sixteen() {
        let budgets = OverheadBudgets::default();
        assert!((budgets.cpu_median_percent - 1.0).abs() < f32::EPSILON);
        assert!((budgets.cpu_p95_percent - 2.0).abs() < f32::EPSILON);
        assert_eq!(budgets.rss_bytes, 50 * MIB);
        assert_eq!(budgets.sample_p95, Duration::from_millis(200));
        assert_eq!(budgets.frame, Duration::from_millis(16));
    }

    #[test]
    fn configured_thresholds_move_the_budgets_with_them() {
        let thresholds = Thresholds {
            self_cpu_budget_percent: 8.0,
            self_rss_budget_bytes: 100 * MIB,
            self_sample_budget_millis: 500,
            ..Thresholds::default()
        }
        .sanitized();
        let budgets = OverheadBudgets::from_thresholds(&thresholds);
        assert!((budgets.cpu_p95_percent - 8.0).abs() < f32::EPSILON);
        assert!(
            (budgets.cpu_median_percent - 4.0).abs() < f32::EPSILON,
            "§16.1's median is half its p95"
        );
        assert_eq!(budgets.rss_bytes, 100 * MIB);
        assert_eq!(budgets.sample_p95, Duration::from_millis(500));

        let mut monitor = OverheadMonitor::with_budgets(budgets);
        assert_eq!(monitor.budgets().rss_bytes, 100 * MIB);
        let start = Instant::now();
        monitor.observe(reading(0, 80 * MIB, Some(3)), start, 0);
        monitor.observe(
            reading(5, 80 * MIB, Some(3)),
            start + Duration::from_secs(1),
            0,
        );
        assert_eq!(
            monitor
                .report()
                .budget("resident memory")
                .map(|row| row.verdict),
            Some(Verdict::Within),
            "80 MiB is inside a configured 100 MiB budget"
        );
    }

    #[test]
    fn every_verdict_has_a_label_and_a_distinct_symbol() {
        let verdicts = [Verdict::Within, Verdict::Over, Verdict::NotYetMeasured];
        let mut symbols: Vec<char> = verdicts.iter().map(|verdict| verdict.symbol()).collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), verdicts.len(), "§5.2 needs distinct symbols");
        for verdict in verdicts {
            assert!(!verdict.label().is_empty(), "{verdict:?}");
        }
        assert!(Verdict::Over.is_over());
        assert!(
            !Verdict::NotYetMeasured.is_over(),
            "unknown is not a violation"
        );
    }

    #[test]
    fn every_trend_direction_has_a_label() {
        for direction in [
            TrendDirection::TooEarly,
            TrendDirection::Flat,
            TrendDirection::Growing,
            TrendDirection::Shrinking,
        ] {
            assert!(!direction.label().is_empty(), "{direction:?}");
        }
    }

    #[test]
    fn an_unobserved_trend_reports_nothing_rather_than_zero() {
        let trend = GrowthTrend::new();
        assert_eq!(trend.latest(), None);
        assert_eq!(trend.peak(), None, "no peak is not a peak of zero");
        assert_eq!(trend.trough(), None);
        assert_eq!(trend.observations(), 0);
        assert_eq!(trend.span(), Duration::ZERO);
        assert_eq!(trend.per_hour(), None);
        assert_eq!(
            trend.direction(RSS_GROWTH_TOLERANCE_PER_HOUR),
            TrendDirection::TooEarly
        );
    }

    #[test]
    fn a_trend_records_its_extremes_and_its_observation_count() {
        let start = Instant::now();
        let mut trend = GrowthTrend::new();
        for (index, value) in [30u64, 10, 50, 20].into_iter().enumerate() {
            let offset = u64::try_from(index).expect("small") * 60;
            trend.observe(value, start + Duration::from_secs(offset));
        }
        assert_eq!(trend.latest(), Some(20));
        assert_eq!(trend.peak(), Some(50));
        assert_eq!(trend.trough(), Some(10));
        assert_eq!(trend.observations(), 4);
        assert_eq!(trend.span(), Duration::from_secs(180));
    }

    #[test]
    fn percentage_round_tripping_through_the_window_keeps_three_decimals() {
        for value in [0.0f32, 0.125, 1.0, 2.5, 99.999, 400.0] {
            let percent = Percent::new(value).expect("valid");
            let round_tripped = millipercent_to_percent(percent_to_millipercent(percent));
            assert!(
                (round_tripped.value() - value).abs() < 0.001,
                "{value} became {}",
                round_tripped.value()
            );
        }
    }

    #[test]
    fn nonsense_arithmetic_saturates_instead_of_panicking() {
        // A panic inside the sampling loop is never acceptable (§14.3), so the
        // conversions defend themselves even against inputs `Percent` rejects.
        assert_eq!(percent_to_millipercent(Percent::ZERO), 0);
        assert_eq!(saturating_magnitude(f64::NAN), u64::MAX);
        assert_eq!(saturating_magnitude(f64::INFINITY), u64::MAX);
        assert_eq!(saturating_magnitude(-2.0), 2);
        assert_eq!(micros(Duration::MAX), u64::MAX);
    }

    #[test]
    fn a_scripted_reader_drives_the_monitor_and_open_files_are_optional() {
        let mut reader = ScriptedReader::new([
            reading(0, MIB, Some(4)),
            reading(10, MIB, None),
            reading(20, MIB, Some(5)),
        ]);
        let mut monitor = OverheadMonitor::new();
        let start = Instant::now();
        for index in 0..3u64 {
            let want_open_files = index != 1;
            let sample = reader.read(want_open_files).expect("scripted");
            monitor.observe(sample, start + Duration::from_secs(index), 0);
        }
        assert_eq!(reader.reads, 3);
        assert_eq!(reader.wanted_open_files, 2, "the middle read skipped them");
        assert_eq!(monitor.readings(), 3);
        assert!(matches!(
            monitor.latest().map(|latest| latest.open_files),
            Some(MetricState::Available(5))
        ));

        let exhausted = reader.read(true).expect_err("the script is empty");
        assert!(matches!(exhausted, OverheadError::SelfNotVisible { .. }));
        assert!(
            exhausted.to_string().contains("its own process"),
            "{exhausted}"
        );
    }

    // ------------------------------------------------------------------
    // Live reads of our own process. Ignored by default: what they measure
    // depends on the machine running them, so they are an observation rather
    // than an assertion about every machine. Run with `cargo test -- --ignored`.
    // ------------------------------------------------------------------

    #[test]
    #[ignore = "reads this machine's own process"]
    fn live_reading_our_own_process_warms_up_and_then_measures() {
        let mut reader = SysinfoSelfReader::new();
        assert_eq!(reader.pid(), std::process::id());

        let mut monitor = OverheadMonitor::new();
        let first = reader.read(true).expect("our own process is visible");
        assert!(first.rss_bytes > 0, "we are resident somewhere");
        assert!(
            monitor.observe(first, Instant::now(), 0).is_none(),
            "§26: the first sample warms up"
        );

        std::thread::sleep(Duration::from_millis(300));
        let second = reader.read(true).expect("still visible");
        let overhead = monitor
            .observe(second, Instant::now(), 0)
            .expect("the second sample measures");
        eprintln!(
            "live self overhead: cpu {} rss {} open files {:?}",
            overhead.cpu, overhead.rss_bytes, overhead.open_files
        );
        assert!(overhead.rss_bytes > 0);
        #[cfg(unix)]
        assert!(
            overhead.open_files.fresh().is_some(),
            "both v1 platforms can count descriptors: {:?}",
            overhead.open_files
        );
    }

    #[test]
    #[ignore = "reads this machine's own process"]
    fn live_our_own_overhead_is_inside_the_section_sixteen_budgets() {
        let mut reader = SysinfoSelfReader::new();
        let mut monitor = OverheadMonitor::new();
        for index in 0..10 {
            let started = Instant::now();
            let sample = reader.read(index % 5 == 0).expect("visible");
            let read_cost = started.elapsed();
            monitor.observe(sample, Instant::now(), 0);
            monitor.observe_sample_duration(read_cost);
            std::thread::sleep(Duration::from_millis(200));
        }

        let report = monitor.report();
        for row in &report.budgets {
            let measured = match row.measured {
                MetricState::Available(value) => render(value),
                other => format!("{other:?}"),
            };
            eprintln!(
                "{} {}: measured {measured}, budget {} ({})",
                row.verdict.symbol(),
                row.label,
                render(row.budget),
                row.verdict.label(),
            );
        }
        for row in &report.trends {
            eprintln!(
                "{}: {} over {:?}",
                row.label,
                row.direction.label(),
                row.span
            );
        }

        let rss = report.budget("resident memory").expect("row");
        assert_eq!(
            rss.verdict,
            Verdict::Within,
            "§16.1: below 50 MiB in the default configuration, measured {:?}",
            rss.measured
        );
        assert!(
            report.samples > 0,
            "the percentiles must be based on something"
        );
    }
}

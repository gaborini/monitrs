//! The bounded ring buffer, its configuration bounds, and its memory budget
//! (§8.5).

use core::mem::size_of;
use core::time::Duration;
use std::collections::VecDeque;
use std::time::Instant;

use crate::model::SystemSnapshot;
use crate::units::{ByteUnits, format_bytes, format_duration};

use super::contributors::MAX_RETAINED_TEXT_BYTES;
use super::{
    Contributor, ContributorMetric, ContributorSet, HistoricalSample, HistoricalSystemMetrics,
};

/// Default sample interval (§8.5).
pub const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_secs(1);
/// Smallest accepted sample interval (§8.5).
pub const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
/// Largest accepted sample interval (§8.5).
pub const MAX_SAMPLE_INTERVAL: Duration = Duration::from_secs(60);

/// Default history duration (§8.5).
pub const DEFAULT_HISTORY_DURATION: Duration = Duration::from_secs(5 * 60);
/// Shortest accepted history duration (§8.5).
pub const MIN_HISTORY_DURATION: Duration = Duration::from_secs(30);
/// Longest accepted history duration in v1 (§8.5).
pub const MAX_HISTORY_DURATION: Duration = Duration::from_secs(60 * 60);

/// Default contributors retained per metric per sample (§8.5, §12).
pub const DEFAULT_TOP_CONTRIBUTORS_PER_METRIC: usize = 10;
/// Largest accepted `top_contributors_per_metric`.
///
/// Not in §8.5, but implied by it: `K` multiplies the size of every sample four
/// times over, so an unbounded `K` would defeat the memory budget the same
/// section requires. Fifty rows is already more than any panel in §5 can show.
pub const MAX_TOP_CONTRIBUTORS_PER_METRIC: usize = 50;

/// Default history memory budget (§12's `max_history_memory = "32MiB"`).
pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = 32 * 1024 * 1024;
/// Smallest accepted history memory budget.
///
/// Below this the ring could not hold enough samples for the 30-second
/// comparison §2.5 requires, so accepting the value would silently disable a
/// documented feature.
pub const MIN_MEMORY_BUDGET_BYTES: u64 = 1024 * 1024;
/// Largest accepted history memory budget.
///
/// §16.1 budgets the whole process below 50 MiB resident in the default
/// configuration; a history allocation beyond this cannot be reconciled with that
/// and is almost certainly a mistyped unit.
pub const MAX_MEMORY_BUDGET_BYTES: u64 = 512 * 1024 * 1024;

/// The requested history configuration, before validation.
///
/// Mirrors the `[sampling]` and `[processes]` keys in §12. Out-of-range values
/// are clamped rather than rejected, and every clamp is reported so the UI can
/// warn (§8.5); a configuration mistake must not stop monitrs from starting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryConfig {
    /// Requested interval between retained samples.
    pub interval: Duration,
    /// Requested span of history to keep.
    pub duration: Duration,
    /// Requested contributors retained per metric per sample.
    pub top_contributors_per_metric: usize,
    /// Requested ceiling on history memory use.
    pub memory_budget_bytes: u64,
}

impl Default for HistoryConfig {
    /// The defaults §8.5 specifies: 1 s, 5 min, 300 samples, top 10 per metric.
    fn default() -> Self {
        Self {
            interval: DEFAULT_SAMPLE_INTERVAL,
            duration: DEFAULT_HISTORY_DURATION,
            top_contributors_per_metric: DEFAULT_TOP_CONTRIBUTORS_PER_METRIC,
            memory_budget_bytes: DEFAULT_MEMORY_BUDGET_BYTES,
        }
    }
}

/// Which configuration value was clamped.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum HistoryField {
    /// The sample interval.
    SampleInterval,
    /// The history duration.
    HistoryDuration,
    /// The contributors retained per metric.
    TopContributorsPerMetric,
    /// The history memory budget.
    MemoryBudget,
}

impl HistoryField {
    /// The configuration key this field comes from.
    ///
    /// §12 requires an invalid value to point at the exact key rather than at a
    /// generic message.
    #[must_use]
    pub const fn config_key(self) -> &'static str {
        match self {
            Self::SampleInterval => "sampling.interval",
            Self::HistoryDuration => "sampling.history",
            Self::TopContributorsPerMetric => "processes.top_contributors_per_metric",
            Self::MemoryBudget => "sampling.max_history_memory",
        }
    }
}

/// Why a value was clamped.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClampReason {
    /// The value was below the supported minimum.
    BelowMinimum,
    /// The value was above the supported maximum.
    AboveMaximum,
    /// The value was in range but would not fit the memory budget (§8.5).
    ExceedsMemoryBudget,
}

impl ClampReason {
    /// A short explanation for the warning line.
    #[must_use]
    pub const fn explanation(self) -> &'static str {
        match self {
            Self::BelowMinimum => "below the supported minimum",
            Self::AboveMaximum => "above the supported maximum",
            Self::ExceedsMemoryBudget => "would exceed the history memory budget",
        }
    }
}

/// A clamped value, tagged so it can be rendered in the unit it was written in.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ClampedValue {
    /// A duration, rendered in the form §12's configuration accepts.
    Duration(Duration),
    /// A plain count.
    Count(usize),
    /// A byte size.
    Bytes(u64),
}

impl ClampedValue {
    /// Renders the value so it can be pasted back into a configuration file.
    #[must_use]
    pub fn render(self) -> String {
        match self {
            Self::Duration(duration) => format_duration(duration),
            Self::Count(count) => count.to_string(),
            // IEC regardless of the display setting: `max_history_memory` is
            // written as `32MiB` in §12's example, so echoing SI units back would
            // not round-trip.
            Self::Bytes(bytes) => format_bytes(bytes, ByteUnits::Iec),
        }
    }
}

/// One reported adjustment to the requested configuration (§8.5).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct HistoryClamp {
    /// Which value was adjusted.
    pub field: HistoryField,
    /// What the configuration asked for.
    pub requested: ClampedValue,
    /// What is actually in effect.
    pub applied: ClampedValue,
    /// Why the adjustment was necessary.
    pub reason: ClampReason,
}

impl HistoryClamp {
    /// A complete warning line naming the key, both values, and the reason.
    #[must_use]
    pub fn message(&self) -> String {
        format!(
            "{} {} clamped to {}: {}",
            self.field.config_key(),
            self.requested.render(),
            self.applied.render(),
            self.reason.explanation()
        )
    }
}

/// A validated history configuration: what the ring will actually do.
///
/// Construction never fails. §8.5 asks for clamping plus a warning, not a hard
/// error, so every adjustment is recorded in [`Self::clamps`] and the ring starts
/// regardless.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryLimits {
    interval: Duration,
    requested_duration: Duration,
    capacity: usize,
    top_contributors_per_metric: usize,
    memory_budget_bytes: u64,
    bytes_per_sample: usize,
    clamps: Vec<HistoryClamp>,
}

impl HistoryLimits {
    /// Validates a requested configuration, clamping out-of-range values.
    #[must_use]
    pub fn resolve(config: HistoryConfig) -> Self {
        let mut clamps = Vec::new();

        let interval = clamp_duration(
            config.interval,
            MIN_SAMPLE_INTERVAL,
            MAX_SAMPLE_INTERVAL,
            HistoryField::SampleInterval,
            &mut clamps,
        );
        let top_contributors_per_metric = clamp_count(
            config.top_contributors_per_metric,
            1,
            MAX_TOP_CONTRIBUTORS_PER_METRIC,
            HistoryField::TopContributorsPerMetric,
            &mut clamps,
        );
        let memory_budget_bytes = clamp_bytes(
            config.memory_budget_bytes,
            MIN_MEMORY_BUDGET_BYTES,
            MAX_MEMORY_BUDGET_BYTES,
            HistoryField::MemoryBudget,
            &mut clamps,
        );
        let duration = clamp_duration(
            config.duration,
            MIN_HISTORY_DURATION,
            MAX_HISTORY_DURATION,
            HistoryField::HistoryDuration,
            &mut clamps,
        );

        let bytes_per_sample = estimated_bytes_per_sample(top_contributors_per_metric);
        let requested_capacity = derive_capacity(duration, interval);
        let affordable = affordable_capacity(memory_budget_bytes, bytes_per_sample);
        let capacity = requested_capacity.min(affordable);

        if capacity < requested_capacity {
            // Report the shrink against the history *duration*, because that is
            // the key the user set and the number the header will show.
            clamps.push(HistoryClamp {
                field: HistoryField::HistoryDuration,
                requested: ClampedValue::Duration(duration),
                applied: ClampedValue::Duration(interval.saturating_mul(capacity_as_u32(capacity))),
                reason: ClampReason::ExceedsMemoryBudget,
            });
        }

        Self {
            interval,
            requested_duration: duration,
            capacity,
            top_contributors_per_metric,
            memory_budget_bytes,
            bytes_per_sample,
            clamps,
        }
    }

    /// The interval between retained samples.
    #[must_use]
    pub const fn interval(&self) -> Duration {
        self.interval
    }

    /// How many samples the ring holds.
    ///
    /// Derived as `ceil(duration / interval)`, so §8.5's default 1 s over 5 min
    /// gives exactly 300.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }

    /// The history span the ring can actually cover: `capacity * interval`.
    ///
    /// Differs from [`Self::requested_duration`] when the memory budget forced a
    /// smaller ring, which is exactly the case §8.5 requires a warning for.
    #[must_use]
    pub fn effective_duration(&self) -> Duration {
        self.interval.saturating_mul(capacity_as_u32(self.capacity))
    }

    /// The history duration in effect after range clamping, before the memory
    /// budget was applied.
    #[must_use]
    pub const fn requested_duration(&self) -> Duration {
        self.requested_duration
    }

    /// Contributors retained per metric per sample.
    #[must_use]
    pub const fn top_contributors_per_metric(&self) -> usize {
        self.top_contributors_per_metric
    }

    /// The memory budget in effect.
    #[must_use]
    pub const fn memory_budget_bytes(&self) -> u64 {
        self.memory_budget_bytes
    }

    /// The worst-case bytes one sample can occupy.
    ///
    /// Worst case, not typical: a budget that is only respected for average input
    /// is not a budget. [`HistoryRing::estimated_bytes`] reports what is actually
    /// retained.
    #[must_use]
    pub const fn estimated_bytes_per_sample(&self) -> usize {
        self.bytes_per_sample
    }

    /// The worst-case bytes a full ring can occupy.
    #[must_use]
    pub const fn estimated_capacity_bytes(&self) -> usize {
        self.capacity.saturating_mul(self.bytes_per_sample)
    }

    /// Every adjustment made to the requested configuration.
    ///
    /// §8.5 requires the user to be warned when configuration was clamped, which
    /// is only possible if the clamps survive validation as data.
    #[must_use]
    pub fn clamps(&self) -> &[HistoryClamp] {
        &self.clamps
    }

    /// Whether anything was clamped.
    #[must_use]
    pub fn was_clamped(&self) -> bool {
        !self.clamps.is_empty()
    }
}

impl Default for HistoryLimits {
    /// The validated form of [`HistoryConfig::default`], which clamps nothing.
    fn default() -> Self {
        Self::resolve(HistoryConfig::default())
    }
}

/// Clamps a duration into range, recording the adjustment.
fn clamp_duration(
    value: Duration,
    min: Duration,
    max: Duration,
    field: HistoryField,
    clamps: &mut Vec<HistoryClamp>,
) -> Duration {
    let (applied, reason) = if value < min {
        (min, Some(ClampReason::BelowMinimum))
    } else if value > max {
        (max, Some(ClampReason::AboveMaximum))
    } else {
        (value, None)
    };
    if let Some(reason) = reason {
        clamps.push(HistoryClamp {
            field,
            requested: ClampedValue::Duration(value),
            applied: ClampedValue::Duration(applied),
            reason,
        });
    }
    applied
}

/// Clamps a count into range, recording the adjustment.
fn clamp_count(
    value: usize,
    min: usize,
    max: usize,
    field: HistoryField,
    clamps: &mut Vec<HistoryClamp>,
) -> usize {
    let (applied, reason) = if value < min {
        (min, Some(ClampReason::BelowMinimum))
    } else if value > max {
        (max, Some(ClampReason::AboveMaximum))
    } else {
        (value, None)
    };
    if let Some(reason) = reason {
        clamps.push(HistoryClamp {
            field,
            requested: ClampedValue::Count(value),
            applied: ClampedValue::Count(applied),
            reason,
        });
    }
    applied
}

/// Clamps a byte size into range, recording the adjustment.
fn clamp_bytes(
    value: u64,
    min: u64,
    max: u64,
    field: HistoryField,
    clamps: &mut Vec<HistoryClamp>,
) -> u64 {
    let (applied, reason) = if value < min {
        (min, Some(ClampReason::BelowMinimum))
    } else if value > max {
        (max, Some(ClampReason::AboveMaximum))
    } else {
        (value, None)
    };
    if let Some(reason) = reason {
        clamps.push(HistoryClamp {
            field,
            requested: ClampedValue::Bytes(value),
            applied: ClampedValue::Bytes(applied),
            reason,
        });
    }
    applied
}

/// `ceil(duration / interval)`, never below one sample.
///
/// Rounds up so the configured span is covered rather than fractionally missed:
/// §8.5's 5 min at 1 s must be 300 samples, not 299.
fn derive_capacity(duration: Duration, interval: Duration) -> usize {
    let interval_nanos = interval.as_nanos();
    if interval_nanos == 0 {
        return 1;
    }
    let samples = duration.as_nanos().div_ceil(interval_nanos).max(1);
    usize::try_from(samples).unwrap_or(usize::MAX)
}

/// How many samples of `bytes_per_sample` fit in `budget`, never below one.
fn affordable_capacity(budget: u64, bytes_per_sample: usize) -> usize {
    let per_sample = u64::try_from(bytes_per_sample.max(1)).unwrap_or(u64::MAX);
    let samples = (budget / per_sample).max(1);
    usize::try_from(samples).unwrap_or(usize::MAX)
}

/// A capacity as the multiplier `Duration::saturating_mul` accepts.
///
/// A capacity above `u32::MAX` cannot be allocated on any supported platform;
/// saturating keeps the arithmetic panic-free either way.
fn capacity_as_u32(capacity: usize) -> u32 {
    u32::try_from(capacity).unwrap_or(u32::MAX)
}

/// The worst-case bytes one sample occupies at a given `K`.
///
/// Counts the struct itself plus, for each of the four metrics, `K` contributors
/// and their truncated text at the pessimistic four-bytes-per-cell bound.
const fn estimated_bytes_per_sample(top_contributors_per_metric: usize) -> usize {
    let per_contributor = size_of::<Contributor>() + MAX_RETAINED_TEXT_BYTES;
    let contributors = top_contributors_per_metric
        .saturating_mul(ContributorMetric::COUNT)
        .saturating_mul(per_contributor);
    size_of::<HistoricalSample>().saturating_add(contributors)
}

/// What happened to a snapshot offered to the ring.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RecordOutcome {
    /// The snapshot was appended.
    Recorded {
        /// Whether the oldest sample had to be evicted to make room.
        evicted: bool,
    },
    /// The snapshot was not newer than the newest retained sample.
    ///
    /// Coalesced or re-delivered snapshots (§16.2) and a wall clock that moved
    /// (§8.1) must not be able to push history backwards, so they are dropped
    /// rather than appended out of order.
    NotNewer,
}

impl RecordOutcome {
    /// Whether a sample was appended.
    #[must_use]
    pub const fn is_recorded(self) -> bool {
        matches!(self, Self::Recorded { .. })
    }
}

/// The bounded history ring (§8.5).
///
/// Fixed capacity, oldest-first eviction, and no allocation growth after the
/// first `capacity` samples — §16.1 requires no unbounded memory growth over a
/// twelve-hour run, and a ring is how that is guaranteed rather than hoped for.
#[derive(Debug)]
pub struct HistoryRing {
    limits: HistoryLimits,
    start: Instant,
    samples: VecDeque<HistoricalSample>,
    total_recorded: u64,
    evicted: u64,
    retained_heap_bytes: usize,
}

impl HistoryRing {
    /// Builds a ring from validated limits.
    ///
    /// `start` is the monotonic origin every sample's offset is measured from
    /// (§8.1). Pass the instant the collector started so the first sample's
    /// offset is near zero.
    #[must_use]
    pub fn new(limits: HistoryLimits, start: Instant) -> Self {
        Self {
            samples: VecDeque::with_capacity(limits.capacity()),
            limits,
            start,
            total_recorded: 0,
            evicted: 0,
            retained_heap_bytes: 0,
        }
    }

    /// Builds a ring from a requested configuration, clamping as needed.
    ///
    /// Inspect [`Self::clamps`] afterwards: §8.5 requires the user to be warned.
    #[must_use]
    pub fn with_config(config: HistoryConfig, start: Instant) -> Self {
        Self::new(HistoryLimits::resolve(config), start)
    }

    /// Reduces a snapshot to a sample and appends it.
    ///
    /// Only the aggregate and the top contributors are retained; the process
    /// table is never cloned (§8.5, §26).
    pub fn record(&mut self, snapshot: &SystemSnapshot) -> RecordOutcome {
        let offset = snapshot.captured_at.saturating_duration_since(self.start);
        if let Some(newest) = self.samples.back()
            && (snapshot.sequence <= newest.sequence || offset < newest.monotonic_offset)
        {
            return RecordOutcome::NotNewer;
        }

        let contributors = {
            let previous = self.samples.back().map(|sample| &sample.contributors);
            ContributorSet::from_processes(
                &snapshot.processes,
                previous,
                self.limits.top_contributors_per_metric(),
            )
        };
        let sample = HistoricalSample {
            sequence: snapshot.sequence,
            monotonic_offset: offset,
            wall_time: snapshot.wall_time,
            system: HistoricalSystemMetrics::from_snapshot(snapshot),
            contributors,
        };

        let mut evicted = false;
        while self.samples.len() >= self.limits.capacity() {
            let Some(oldest) = self.samples.pop_front() else {
                break;
            };
            self.retained_heap_bytes = self
                .retained_heap_bytes
                .saturating_sub(oldest.contributors.heap_bytes());
            self.evicted = self.evicted.saturating_add(1);
            evicted = true;
        }

        self.retained_heap_bytes = self
            .retained_heap_bytes
            .saturating_add(sample.contributors.heap_bytes());
        self.samples.push_back(sample);
        self.total_recorded = self.total_recorded.saturating_add(1);
        RecordOutcome::Recorded { evicted }
    }

    /// The limits in effect.
    #[must_use]
    pub const fn limits(&self) -> &HistoryLimits {
        &self.limits
    }

    /// Configuration adjustments the UI should warn about (§8.5).
    #[must_use]
    pub fn clamps(&self) -> &[HistoryClamp] {
        self.limits.clamps()
    }

    /// The monotonic origin sample offsets are measured from.
    #[must_use]
    pub const fn start(&self) -> Instant {
        self.start
    }

    /// How many samples the ring holds when full.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.limits.capacity()
    }

    /// How many samples are retained right now.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// Whether nothing has been recorded yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// Every retained sample, oldest first.
    #[must_use]
    pub fn samples(&self) -> impl DoubleEndedIterator<Item = &HistoricalSample> {
        self.samples.iter()
    }

    /// The `index`th retained sample, counting from the oldest.
    ///
    /// Constant time: the backing store is a deque, not a linked list (§21 M4).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&HistoricalSample> {
        self.samples.get(index)
    }

    /// The newest retained sample.
    #[must_use]
    pub fn newest(&self) -> Option<&HistoricalSample> {
        self.samples.back()
    }

    /// The oldest retained sample.
    #[must_use]
    pub fn oldest(&self) -> Option<&HistoricalSample> {
        self.samples.front()
    }

    /// How many samples have ever been recorded, including evicted ones.
    ///
    /// This is what makes selection stable across eviction: an *absolute* index
    /// keeps naming the same sample as newer ones arrive, so a paused view does
    /// not drift (§2.1).
    #[must_use]
    pub const fn total_recorded(&self) -> u64 {
        self.total_recorded
    }

    /// How many samples have been evicted.
    #[must_use]
    pub const fn evicted(&self) -> u64 {
        self.evicted
    }

    /// The absolute index of the oldest retained sample.
    #[must_use]
    pub fn first_absolute(&self) -> u64 {
        let len = u64::try_from(self.samples.len()).unwrap_or(u64::MAX);
        self.total_recorded.saturating_sub(len)
    }

    /// The absolute index of the newest retained sample.
    #[must_use]
    pub fn newest_absolute(&self) -> Option<u64> {
        if self.samples.is_empty() {
            None
        } else {
            self.total_recorded.checked_sub(1)
        }
    }

    /// The sample at an absolute index, or `None` if it was evicted.
    ///
    /// Constant time: index arithmetic plus one deque lookup (§21 M4).
    #[must_use]
    pub fn get_absolute(&self, absolute: u64) -> Option<&HistoricalSample> {
        let relative = absolute.checked_sub(self.first_absolute())?;
        self.samples.get(usize::try_from(relative).ok()?)
    }

    /// The monotonic span between the oldest and newest retained samples.
    #[must_use]
    pub fn span(&self) -> Duration {
        match (self.oldest(), self.newest()) {
            (Some(oldest), Some(newest)) => newest
                .monotonic_offset
                .saturating_sub(oldest.monotonic_offset),
            _ => Duration::ZERO,
        }
    }

    /// The newest sample whose offset is at or before `offset`.
    ///
    /// Sample offsets increase monotonically (§8.1), so this is a binary search
    /// rather than a scan — the "effectively constant time" seeking §21 M4
    /// requires.
    #[must_use]
    pub fn index_at_or_before_offset(&self, offset: Duration) -> Option<usize> {
        let past_target = partition_point(self.samples.len(), |index| {
            self.samples
                .get(index)
                .is_some_and(|sample| sample.monotonic_offset <= offset)
        });
        past_target.checked_sub(1)
    }

    /// Bytes the retained samples actually occupy, struct plus heap.
    ///
    /// Maintained incrementally so the self-overhead panel §16.1 requires does
    /// not walk the whole ring on every frame.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>()
            .saturating_add(self.samples.capacity() * size_of::<HistoricalSample>())
            .saturating_add(self.retained_heap_bytes)
    }
}

/// The first index in `0..len` for which `predicate` is false.
///
/// `predicate` must be true for a prefix and false for the remainder. Written out
/// rather than borrowed from `slice::partition_point` because a `VecDeque` is not
/// contiguous and making it so would need `&mut self`.
fn partition_point(len: usize, mut predicate: impl FnMut(usize) -> bool) -> usize {
    let mut low = 0usize;
    let mut high = len;
    while low < high {
        let middle = low + (high - low) / 2;
        if predicate(middle) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    low
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::HistoryMetric;
    use crate::model::{
        CpuUsage, DiskSnapshot, MetricState, ProcessIdentity, ProcessIo, ProcessMemory,
        ProcessSnapshot, ProcessState, UnavailableReason,
    };
    use crate::units::{Percent, Rate};
    use std::time::SystemTime;

    /// Builds a snapshot `sequence` intervals after `start`.
    fn snapshot(start: Instant, sequence: u64, interval: Duration) -> SystemSnapshot {
        let captured_at = start + interval.saturating_mul(u32::try_from(sequence).unwrap_or(0));
        let mut snapshot = SystemSnapshot::warming_up(
            captured_at,
            SystemTime::UNIX_EPOCH + Duration::from_secs(sequence),
            8,
        );
        snapshot.sequence = sequence;
        snapshot.elapsed = interval;
        snapshot
    }

    fn with_cpu(mut snapshot: SystemSnapshot, busy: f32) -> SystemSnapshot {
        snapshot.cpu.total =
            MetricState::Available(CpuUsage::plain(Percent::new(busy).expect("valid percent")));
        snapshot
    }

    fn process(pid: u32, cpu: f32) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid) * 31),
            parent_pid: Some(1),
            name: "proc".into(),
            command: "proc --flag".into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::Available(Percent::new(cpu).expect("valid percent")),
            memory: ProcessMemory {
                rss_bytes: MetricState::Available(u64::from(pid) * 1024),
                virtual_bytes: MetricState::Unsupported,
                share_of_total: MetricState::Unsupported,
            },
            io: ProcessIo {
                read: MetricState::Available(Rate::new(f64::from(pid)).expect("valid rate")),
                write: MetricState::Available(Rate::new(f64::from(pid)).expect("valid rate")),
                read_total_bytes: MetricState::Unsupported,
                write_total_bytes: MetricState::Unsupported,
            },
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    #[test]
    fn the_default_configuration_holds_three_hundred_one_second_samples() {
        // §21 M4 acceptance: "default history contains 300 one-second samples".
        let limits = HistoryLimits::default();
        assert_eq!(limits.capacity(), 300);
        assert_eq!(limits.interval(), Duration::from_secs(1));
        assert_eq!(limits.effective_duration(), Duration::from_secs(300));
        assert_eq!(limits.top_contributors_per_metric(), 10);
        assert!(!limits.was_clamped(), "{:?}", limits.clamps());
    }

    #[test]
    fn capacity_is_derived_from_interval_and_duration() {
        let cases = [
            (Duration::from_millis(250), Duration::from_secs(60), 240),
            (Duration::from_secs(1), Duration::from_secs(30), 30),
            (Duration::from_secs(2), Duration::from_secs(300), 150),
            (Duration::from_secs(60), Duration::from_secs(3_600), 60),
        ];
        for (interval, duration, expected) in cases {
            let limits = HistoryLimits::resolve(HistoryConfig {
                interval,
                duration,
                ..HistoryConfig::default()
            });
            assert_eq!(
                limits.capacity(),
                expected,
                "{interval:?} over {duration:?}"
            );
        }
    }

    #[test]
    fn a_duration_that_is_not_a_whole_number_of_intervals_rounds_up() {
        // Rounding down would silently cover less history than configured.
        let limits = HistoryLimits::resolve(HistoryConfig {
            interval: Duration::from_millis(400),
            duration: Duration::from_secs(31),
            ..HistoryConfig::default()
        });
        assert_eq!(limits.capacity(), 78, "ceil(31000 / 400) is 78");
    }

    #[test]
    fn an_interval_below_the_minimum_is_clamped_and_reported() {
        let limits = HistoryLimits::resolve(HistoryConfig {
            interval: Duration::from_millis(10),
            ..HistoryConfig::default()
        });
        assert_eq!(limits.interval(), MIN_SAMPLE_INTERVAL);

        let clamp = limits
            .clamps()
            .iter()
            .find(|clamp| clamp.field == HistoryField::SampleInterval)
            .expect("the clamp is reported so the UI can warn");
        assert_eq!(clamp.reason, ClampReason::BelowMinimum);
        assert_eq!(
            clamp.requested,
            ClampedValue::Duration(Duration::from_millis(10))
        );
        assert_eq!(clamp.applied, ClampedValue::Duration(MIN_SAMPLE_INTERVAL));
        assert_eq!(
            clamp.message(),
            "sampling.interval 10ms clamped to 250ms: below the supported minimum"
        );
    }

    #[test]
    fn every_out_of_range_value_is_clamped_and_reported() {
        let limits = HistoryLimits::resolve(HistoryConfig {
            interval: Duration::from_secs(600),
            duration: Duration::from_secs(1),
            top_contributors_per_metric: 0,
            memory_budget_bytes: 1,
        });

        assert_eq!(limits.interval(), MAX_SAMPLE_INTERVAL);
        assert_eq!(limits.requested_duration(), MIN_HISTORY_DURATION);
        assert_eq!(limits.top_contributors_per_metric(), 1);
        assert_eq!(limits.memory_budget_bytes(), MIN_MEMORY_BUDGET_BYTES);

        let fields: Vec<HistoryField> = limits.clamps().iter().map(|c| c.field).collect();
        for field in [
            HistoryField::SampleInterval,
            HistoryField::HistoryDuration,
            HistoryField::TopContributorsPerMetric,
            HistoryField::MemoryBudget,
        ] {
            assert!(fields.contains(&field), "{field:?} was not reported");
        }
        for clamp in limits.clamps() {
            assert!(clamp.message().contains(clamp.field.config_key()));
        }
    }

    #[test]
    fn an_in_range_configuration_reports_no_clamp() {
        let limits = HistoryLimits::resolve(HistoryConfig {
            interval: Duration::from_secs(2),
            duration: Duration::from_secs(600),
            top_contributors_per_metric: 5,
            memory_budget_bytes: 8 * 1024 * 1024,
        });
        assert!(!limits.was_clamped(), "{:?}", limits.clamps());
        assert_eq!(limits.capacity(), 300);
    }

    #[test]
    fn the_memory_budget_shrinks_capacity_and_reports_the_shorter_history() {
        // The maximum history at the minimum interval cannot fit a small budget.
        let limits = HistoryLimits::resolve(HistoryConfig {
            interval: MIN_SAMPLE_INTERVAL,
            duration: MAX_HISTORY_DURATION,
            top_contributors_per_metric: MAX_TOP_CONTRIBUTORS_PER_METRIC,
            memory_budget_bytes: MIN_MEMORY_BUDGET_BYTES,
        });

        assert!(
            limits.capacity() < derive_capacity(MAX_HISTORY_DURATION, MIN_SAMPLE_INTERVAL),
            "capacity should have been reduced"
        );
        assert!(limits.capacity() >= 1, "at least one sample must fit");
        assert!(
            u64::try_from(limits.estimated_capacity_bytes()).unwrap_or(u64::MAX)
                <= limits.memory_budget_bytes(),
            "worst case {} exceeds budget {}",
            limits.estimated_capacity_bytes(),
            limits.memory_budget_bytes()
        );

        let clamp = limits
            .clamps()
            .iter()
            .find(|clamp| clamp.reason == ClampReason::ExceedsMemoryBudget)
            .expect("the budget clamp is reported");
        assert_eq!(clamp.field, HistoryField::HistoryDuration);
        assert!(
            clamp.message().contains("memory budget"),
            "{}",
            clamp.message()
        );
        assert!(limits.effective_duration() < MAX_HISTORY_DURATION);
    }

    #[test]
    fn the_worst_case_estimate_grows_with_the_contributor_count() {
        let small = estimated_bytes_per_sample(1);
        let large = estimated_bytes_per_sample(10);
        assert!(small < large);
        assert!(estimated_bytes_per_sample(0) >= size_of::<HistoricalSample>());
    }

    #[test]
    fn a_full_ring_evicts_the_oldest_sample() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(
            HistoryLimits::resolve(HistoryConfig {
                interval,
                duration: Duration::from_secs(30),
                ..HistoryConfig::default()
            }),
            start,
        );
        assert_eq!(ring.capacity(), 30);
        assert!(ring.is_empty());

        for sequence in 0..30 {
            let outcome = ring.record(&snapshot(start, sequence, interval));
            assert_eq!(outcome, RecordOutcome::Recorded { evicted: false });
        }
        assert_eq!(ring.len(), 30);
        assert_eq!(ring.evicted(), 0);
        assert_eq!(ring.oldest().map(|s| s.sequence), Some(0));

        let outcome = ring.record(&snapshot(start, 30, interval));
        assert_eq!(outcome, RecordOutcome::Recorded { evicted: true });
        assert_eq!(ring.len(), 30, "capacity is never exceeded");
        assert_eq!(ring.evicted(), 1);
        assert_eq!(ring.oldest().map(|s| s.sequence), Some(1));
        assert_eq!(ring.newest().map(|s| s.sequence), Some(30));
        assert_eq!(ring.total_recorded(), 31);
    }

    #[test]
    fn eviction_keeps_absolute_indexing_stable() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(
            HistoryLimits::resolve(HistoryConfig {
                interval,
                duration: Duration::from_secs(30),
                ..HistoryConfig::default()
            }),
            start,
        );
        for sequence in 0..40 {
            ring.record(&snapshot(start, sequence, interval));
        }

        assert_eq!(ring.first_absolute(), 10);
        assert_eq!(ring.newest_absolute(), Some(39));
        assert_eq!(ring.get_absolute(10).map(|s| s.sequence), Some(10));
        assert_eq!(ring.get_absolute(39).map(|s| s.sequence), Some(39));
        assert!(ring.get_absolute(9).is_none(), "evicted samples are gone");
        assert!(ring.get_absolute(40).is_none(), "the future does not exist");
    }

    #[test]
    fn an_empty_ring_has_no_newest_index() {
        let ring = HistoryRing::new(HistoryLimits::default(), Instant::now());
        assert_eq!(ring.newest_absolute(), None);
        assert_eq!(ring.first_absolute(), 0);
        assert!(ring.get_absolute(0).is_none());
        assert_eq!(ring.span(), Duration::ZERO);
        assert!(ring.index_at_or_before_offset(Duration::ZERO).is_none());
    }

    #[test]
    fn a_resent_or_coalesced_snapshot_does_not_push_history_backwards() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);

        assert!(ring.record(&snapshot(start, 5, interval)).is_recorded());
        assert_eq!(
            ring.record(&snapshot(start, 5, interval)),
            RecordOutcome::NotNewer,
            "the same sequence must not be recorded twice"
        );
        assert_eq!(
            ring.record(&snapshot(start, 4, interval)),
            RecordOutcome::NotNewer,
            "an older sequence must not be appended"
        );
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.total_recorded(), 1);
    }

    #[test]
    fn offsets_are_measured_from_the_rings_start_instant() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);
        for sequence in 0..5 {
            ring.record(&snapshot(start, sequence, interval));
        }

        let offsets: Vec<Duration> = ring.samples().map(|s| s.monotonic_offset).collect();
        assert_eq!(
            offsets,
            vec![
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(3),
                Duration::from_secs(4),
            ]
        );
        assert_eq!(ring.span(), Duration::from_secs(4));
    }

    #[test]
    fn a_snapshot_captured_before_the_ring_started_gets_a_zero_offset() {
        // Defensive: a snapshot from before the ring existed must not underflow.
        let start = Instant::now();
        let Some(earlier) = start.checked_sub(Duration::from_secs(5)) else {
            return;
        };
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);
        let mut source = SystemSnapshot::warming_up(earlier, SystemTime::UNIX_EPOCH, 8);
        source.sequence = 1;

        assert!(ring.record(&source).is_recorded());
        assert_eq!(
            ring.newest().map(|s| s.monotonic_offset),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn the_process_table_is_never_cloned_into_a_sample() {
        // §21 M4 acceptance and §26: the retained contributor count is bounded by
        // K per metric no matter how many processes were observed.
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);

        let mut source = snapshot(start, 1, interval);
        source
            .processes
            .extend((1..=10_000u32).map(|pid| process(pid, 1.0)));
        assert_eq!(source.process_count(), 10_000);

        assert!(ring.record(&source).is_recorded());
        let sample = ring.newest().expect("recorded");
        let top_k = ring.limits().top_contributors_per_metric();
        assert_eq!(
            sample.contributors.retained_count(),
            ContributorSet::max_retained(top_k)
        );
        assert!(sample.contributors.retained_count() <= top_k * 4);
        assert!(
            sample.estimated_bytes() <= ring.limits().estimated_bytes_per_sample(),
            "{} exceeded the budgeted {}",
            sample.estimated_bytes(),
            ring.limits().estimated_bytes_per_sample()
        );
    }

    #[test]
    fn retained_bytes_stay_bounded_by_the_budget_over_a_long_run() {
        // §16.1: no unbounded memory growth. The ring is refilled several times
        // over and its accounting must not drift upward.
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(
            HistoryLimits::resolve(HistoryConfig {
                interval,
                duration: Duration::from_secs(30),
                ..HistoryConfig::default()
            }),
            start,
        );

        let mut peak = 0usize;
        for sequence in 0..300 {
            let mut source = snapshot(start, sequence, interval);
            source
                .processes
                .extend((1..=200u32).map(|pid| process(pid, 1.0)));
            ring.record(&source);
            peak = peak.max(ring.estimated_bytes());
        }
        assert_eq!(ring.len(), 30);
        assert_eq!(ring.estimated_bytes(), peak, "accounting must not drift");
        assert!(
            u64::try_from(ring.estimated_bytes()).unwrap_or(u64::MAX)
                <= ring.limits().memory_budget_bytes()
        );
    }

    #[test]
    fn binary_search_finds_the_newest_sample_at_or_before_an_offset() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);
        for sequence in 0..10 {
            ring.record(&snapshot(start, sequence, interval));
        }

        assert_eq!(ring.index_at_or_before_offset(Duration::ZERO), Some(0));
        assert_eq!(
            ring.index_at_or_before_offset(Duration::from_secs(4)),
            Some(4)
        );
        assert_eq!(
            ring.index_at_or_before_offset(Duration::from_millis(4_500)),
            Some(4),
            "an offset between samples resolves to the older one"
        );
        assert_eq!(
            ring.index_at_or_before_offset(Duration::from_secs(99)),
            Some(9)
        );
    }

    #[test]
    fn an_offset_older_than_the_whole_ring_has_no_sample() {
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(
            HistoryLimits::resolve(HistoryConfig {
                interval,
                duration: Duration::from_secs(30),
                ..HistoryConfig::default()
            }),
            start,
        );
        for sequence in 0..40 {
            ring.record(&snapshot(start, sequence, interval));
        }
        // The oldest retained sample sits at offset 10s.
        assert_eq!(ring.index_at_or_before_offset(Duration::from_secs(5)), None);
        assert_eq!(
            ring.index_at_or_before_offset(Duration::from_secs(10)),
            Some(0)
        );
    }

    #[test]
    fn seeking_probes_a_logarithmic_number_of_samples() {
        // §21 M4 requires seeking to be constant or effectively constant time.
        // Counting probes pins that structurally, without a flaky timing test.
        for len in [1usize, 10, 300, 10_000, 1_000_000] {
            let mut probes = 0usize;
            let found = partition_point(len, |_| {
                probes += 1;
                true
            });
            assert_eq!(found, len);
            let bound =
                usize::try_from(usize::BITS - len.leading_zeros() + 1).unwrap_or(usize::MAX);
            assert!(
                probes <= bound,
                "len {len} took {probes} probes, expected at most {bound}"
            );
        }
    }

    #[test]
    fn a_counter_reset_is_retained_as_unavailable_rather_than_a_spike() {
        // §21 M4 acceptance: counter resets do not create false spikes.
        let start = Instant::now();
        let interval = Duration::from_secs(1);
        let mut ring = HistoryRing::new(HistoryLimits::default(), start);

        let mut busy = with_cpu(snapshot(start, 1, interval), 20.0);
        let mut disk = DiskSnapshot::warming_up("nvme0n1".into());
        disk.read = MetricState::Available(Rate::new(1_000_000.0).expect("valid rate"));
        busy.disks.push(disk);
        ring.record(&busy);

        let mut reset = with_cpu(snapshot(start, 2, interval), 22.0);
        let mut disk = DiskSnapshot::warming_up("nvme0n1".into());
        disk.read = MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        reset.disks.push(disk);
        ring.record(&reset);

        let sample = ring.newest().expect("recorded");
        assert_eq!(
            sample.system.disk_read,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
        assert!(
            sample.system.scalar(HistoryMetric::DiskRead).is_none(),
            "an unavailable input must not produce a number to spike with"
        );
    }

    #[test]
    fn a_capacity_of_one_still_records_and_evicts() {
        let start = Instant::now();
        let interval = Duration::from_secs(60);
        let mut ring = HistoryRing::new(
            HistoryLimits::resolve(HistoryConfig {
                interval,
                duration: Duration::from_secs(30),
                ..HistoryConfig::default()
            }),
            start,
        );
        assert_eq!(ring.capacity(), 1);
        assert!(ring.record(&snapshot(start, 1, interval)).is_recorded());
        assert_eq!(
            ring.record(&snapshot(start, 2, interval)),
            RecordOutcome::Recorded { evicted: true }
        );
        assert_eq!(ring.len(), 1);
        assert_eq!(ring.newest().map(|s| s.sequence), Some(2));
    }

    #[test]
    fn the_clamped_value_renderer_round_trips_configuration_syntax() {
        assert_eq!(
            ClampedValue::Duration(Duration::from_millis(250)).render(),
            "250ms"
        );
        assert_eq!(ClampedValue::Count(10).render(), "10");
        assert_eq!(
            ClampedValue::Bytes(32 * 1024 * 1024).render(),
            "32 MiB",
            "the key is written as 32MiB in configuration"
        );
    }

    #[test]
    fn a_ring_built_from_a_config_reports_its_own_clamps() {
        let ring = HistoryRing::with_config(
            HistoryConfig {
                interval: Duration::from_millis(1),
                ..HistoryConfig::default()
            },
            Instant::now(),
        );
        assert!(!ring.clamps().is_empty());
        assert_eq!(ring.limits().interval(), MIN_SAMPLE_INTERVAL);
    }
}

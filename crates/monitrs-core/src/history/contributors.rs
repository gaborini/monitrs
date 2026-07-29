//! Compact spike-attribution evidence retained with every historical sample
//! (§2.2).
//!
//! §8.5 caps this at "top 10 CPU, memory, read, and write contributors per
//! sample, deduplicated by process identity". Everything in this file exists to
//! make that cap structural: a [`ContributorSet`] cannot grow with the process
//! count, and the command lines it retains are truncated rather than cloned in
//! full.

use core::mem::size_of;

use crate::model::{MeasuredValue, MetricState, ProcessIdentity, ProcessSnapshot};
use crate::units::{
    ByteUnits, Ellipsis, Percent, Rate, format_byte_rate, format_bytes, truncate_middle,
    truncate_tail,
};

use super::{most_representative, propagate_unavailable};

/// Display width the retained process name is truncated to.
///
/// Kernel names are short; the `PROCESS` column in §5.6 is narrow. Retaining
/// more than this would grow every sample for text no panel can show.
pub const MAX_RETAINED_NAME_WIDTH: usize = 24;

/// Display width the retained command line is truncated to.
///
/// Two reasons, both binding. Memory: a full command line can be kilobytes, and
/// §8.5 forbids letting per-sample cost scale with the machine's workload.
/// Privacy: arguments can contain secrets, and §15.2/§14.2 require that monitrs
/// does not retain or emit them wholesale — history keeps an abbreviated form for
/// recognition, and export/logging still redact via
/// [`ProcessSnapshot::redacted_command`].
pub const MAX_RETAINED_COMMAND_WIDTH: usize = 64;

/// Worst-case heap bytes one retained contributor's text can occupy.
///
/// Truncation bounds *display width*, not bytes: a wide CJK character occupies
/// two cells and three UTF-8 bytes, and zero-width marks occupy none. Four bytes
/// per cell is the generous bound used for budgeting, which is the direction a
/// budget should err in (§8.5).
pub(super) const MAX_RETAINED_TEXT_BYTES: usize =
    (MAX_RETAINED_NAME_WIDTH + MAX_RETAINED_COMMAND_WIDTH) * 4;

/// Which resource a contributor list ranks processes by (§2.2).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContributorMetric {
    /// CPU usage, core-normalized and therefore able to exceed 100% (§8.3).
    Cpu,
    /// Resident set size.
    ResidentMemory,
    /// Per-process read throughput.
    DiskRead,
    /// Per-process write throughput.
    DiskWrite,
}

impl ContributorMetric {
    /// Every metric §2.2 requires contributors for, in display order.
    pub const ALL: [Self; 4] = [
        Self::Cpu,
        Self::ResidentMemory,
        Self::DiskRead,
        Self::DiskWrite,
    ];

    /// How many metrics [`Self::ALL`] contains.
    ///
    /// The per-sample contributor count is bounded by `COUNT * K`, which is the
    /// property §8.5 and §26 actually care about.
    pub const COUNT: usize = Self::ALL.len();

    /// The short label for the `METRIC` column in §5.6.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::ResidentMemory => "MEM",
            Self::DiskRead => "READ",
            Self::DiskWrite => "WRITE",
        }
    }

    /// A spelled-out description for the attribution panel heading.
    ///
    /// Deliberately worded as "top contributors": §2.2 forbids implying that a
    /// listed process *caused* the spike.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Cpu => "top CPU contributors",
            Self::ResidentMemory => "top resident-memory contributors",
            Self::DiskRead => "top disk readers",
            Self::DiskWrite => "top disk writers",
        }
    }
}

/// The signed change shown in the `DELTA/RATE` column of §5.6.
///
/// Which variant applies is decided by the metric, not by the caller, so a
/// percentage-point change can never be rendered as a byte count.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ContributorTrend {
    /// Change in percentage points since this process's previous retained value.
    Points(f32),
    /// Change in bytes since this process's previous retained value.
    Bytes(i64),
    /// Change in bytes per second since this process's previous retained value.
    ByteRate(f64),
}

impl ContributorTrend {
    /// Renders the trend with an explicit sign, e.g. `+146%` or `+39 MiB/s`.
    ///
    /// The sign is always printed: §5.6's column is only useful if a rise and a
    /// fall are distinguishable at a glance.
    #[must_use]
    pub fn render(self, units: ByteUnits) -> String {
        match self {
            Self::Points(points) => format!("{points:+.0}%"),
            Self::Bytes(bytes) => {
                format!(
                    "{}{}",
                    sign(bytes < 0),
                    format_bytes(bytes.unsigned_abs(), units)
                )
            }
            Self::ByteRate(per_second) => match Rate::new(per_second.abs()) {
                Some(rate) => format!(
                    "{}{}",
                    sign(per_second < 0.0),
                    format_byte_rate(rate, units)
                ),
                // Unreachable for a difference of two validated finite rates;
                // §4 still forbids inventing a number for it.
                None => "n/a".to_owned(),
            },
        }
    }
}

/// The sign prefix for a rendered trend.
const fn sign(negative: bool) -> char {
    if negative { '-' } else { '+' }
}

/// One process retained as evidence for one metric in one historical sample.
///
/// This is *correlational* evidence (§2.2): the process was among the largest
/// observed consumers at that moment, which is not a claim that it caused
/// anything.
#[derive(Clone, Debug, PartialEq)]
pub struct Contributor {
    /// Stable identity, so PID reuse is detectable when the user later inspects
    /// a historical sample (§2.2, §26).
    pub identity: ProcessIdentity,
    /// Short process name, truncated to [`MAX_RETAINED_NAME_WIDTH`].
    pub name: Box<str>,
    /// Command line, truncated to [`MAX_RETAINED_COMMAND_WIDTH`].
    pub command: Box<str>,
    /// The absolute measurement this contributor was ranked by.
    pub value: MeasuredValue,
    /// The change since this process's previous retained value.
    ///
    /// [`MetricState::WarmingUp`] when the process was not in the previous
    /// sample's retained set — either because it is new or because it ranked
    /// outside the top `K`. §8.2 requires a first delta sample to be warming up
    /// rather than zero, and a reused PID lands here too because the lookup is
    /// keyed on the full identity.
    pub trend: MetricState<ContributorTrend>,
}

impl Contributor {
    /// Bytes this contributor allocates outside its own struct.
    ///
    /// Feeds the self-overhead figure §16.1 requires monitrs to expose about
    /// itself.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.name.len() + self.command.len()
    }
}

/// The retained contributors for one metric, with their evidence coverage.
#[derive(Clone, Debug, PartialEq)]
pub struct MetricContributors {
    metric: ContributorMetric,
    entries: Vec<Contributor>,
    coverage: MetricState<Percent>,
}

impl MetricContributors {
    /// A list with nothing observed yet.
    #[must_use]
    pub const fn warming_up(metric: ContributorMetric) -> Self {
        Self {
            metric,
            entries: Vec::new(),
            coverage: MetricState::WarmingUp,
        }
    }

    /// Which metric these contributors were ranked by.
    #[must_use]
    pub const fn metric(&self) -> ContributorMetric {
        self.metric
    }

    /// The retained contributors, highest first.
    #[must_use]
    pub fn entries(&self) -> &[Contributor] {
        &self.entries
    }

    /// How many contributors were retained.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was retained for this metric.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The share of the *observed* system total these contributors account for.
    ///
    /// This is §2.2's "78% of observed CPU accounted for by retained top
    /// processes" — an honesty figure, not a proof. The denominator is the sum
    /// over every process that actually reported the metric, so a platform that
    /// withholds per-process I/O produces an unavailable coverage rather than a
    /// flattering 100%.
    #[must_use]
    pub const fn coverage(&self) -> MetricState<Percent> {
        self.coverage
    }

    /// Bytes this list allocates outside its own struct.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        self.entries.capacity() * size_of::<Contributor>()
            + self
                .entries
                .iter()
                .map(Contributor::heap_bytes)
                .sum::<usize>()
    }
}

/// The complete per-sample attribution evidence: top-`K` contributors for each
/// of the four metrics in §2.2.
#[derive(Clone, Debug, PartialEq)]
pub struct ContributorSet {
    cpu: MetricContributors,
    memory: MetricContributors,
    disk_read: MetricContributors,
    disk_write: MetricContributors,
}

impl ContributorSet {
    /// A set with nothing observed yet, for the warming-up sample (§8.2).
    #[must_use]
    pub const fn warming_up() -> Self {
        Self {
            cpu: MetricContributors::warming_up(ContributorMetric::Cpu),
            memory: MetricContributors::warming_up(ContributorMetric::ResidentMemory),
            disk_read: MetricContributors::warming_up(ContributorMetric::DiskRead),
            disk_write: MetricContributors::warming_up(ContributorMetric::DiskWrite),
        }
    }

    /// The greatest number of contributors any set built with `top_k` can hold.
    ///
    /// §8.5 and §26 forbid the full process table from reaching a historical
    /// sample; this is the bound that replaces it, and it is independent of how
    /// many processes were observed.
    #[must_use]
    pub const fn max_retained(top_k: usize) -> usize {
        top_k.saturating_mul(ContributorMetric::COUNT)
    }

    /// Selects the top `top_k` contributors per metric from a process table.
    ///
    /// `previous` is the immediately preceding sample's set, used only to derive
    /// [`Contributor::trend`]. Nothing from the process table is retained beyond
    /// the selected contributors' identity, truncated text, and measurement.
    #[must_use]
    pub fn from_processes(
        processes: &[ProcessSnapshot],
        previous: Option<&Self>,
        top_k: usize,
    ) -> Self {
        Self {
            cpu: select(processes, ContributorMetric::Cpu, top_k, previous),
            memory: select(
                processes,
                ContributorMetric::ResidentMemory,
                top_k,
                previous,
            ),
            disk_read: select(processes, ContributorMetric::DiskRead, top_k, previous),
            disk_write: select(processes, ContributorMetric::DiskWrite, top_k, previous),
        }
    }

    /// The contributors retained for `metric`.
    #[must_use]
    pub const fn metric(&self, metric: ContributorMetric) -> &MetricContributors {
        match metric {
            ContributorMetric::Cpu => &self.cpu,
            ContributorMetric::ResidentMemory => &self.memory,
            ContributorMetric::DiskRead => &self.disk_read,
            ContributorMetric::DiskWrite => &self.disk_write,
        }
    }

    /// Total contributors retained across all four metrics.
    #[must_use]
    pub fn retained_count(&self) -> usize {
        ContributorMetric::ALL
            .iter()
            .map(|metric| self.metric(*metric).len())
            .sum()
    }

    /// Whether no metric retained anything.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.retained_count() == 0
    }

    /// Bytes this set allocates outside its own struct.
    #[must_use]
    pub fn heap_bytes(&self) -> usize {
        ContributorMetric::ALL
            .iter()
            .map(|metric| self.metric(*metric).heap_bytes())
            .sum()
    }
}

/// A process's reading for one metric: the ranking scalar plus the absolute
/// measurement to display.
///
/// The scalar is floating point because it only ever feeds a comparison; the
/// measurement keeps the raw counter integral, as §10.4 requires.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Observed {
    scalar: f64,
    value: MeasuredValue,
}

/// A candidate contributor after its reading was found to be fresh.
#[derive(Clone, Copy, Debug)]
struct Ranked {
    identity: ProcessIdentity,
    index: usize,
    observed: Observed,
}

/// Reads one process's value for one metric, preserving unavailability.
fn observe(process: &ProcessSnapshot, metric: ContributorMetric) -> MetricState<Observed> {
    match metric {
        ContributorMetric::Cpu => process.cpu.map(|percent| Observed {
            scalar: f64::from(percent.value()),
            value: MeasuredValue::Percent(percent),
        }),
        ContributorMetric::ResidentMemory => process.memory.rss_bytes.map(|bytes| Observed {
            scalar: bytes as f64,
            value: MeasuredValue::Bytes(bytes),
        }),
        ContributorMetric::DiskRead => process.io.read.map(|rate| Observed {
            scalar: rate.per_second(),
            value: MeasuredValue::ByteRate(rate),
        }),
        ContributorMetric::DiskWrite => process.io.write.map(|rate| Observed {
            scalar: rate.per_second(),
            value: MeasuredValue::ByteRate(rate),
        }),
    }
}

/// The change between two absolute measurements of the same kind.
///
/// Mismatched kinds return `None` rather than a coerced number: that can only
/// happen if a metric changed representation between samples, and §4 forbids
/// inventing a value for it.
fn trend_between(current: MeasuredValue, previous: MeasuredValue) -> Option<ContributorTrend> {
    match (current, previous) {
        (MeasuredValue::Percent(now), MeasuredValue::Percent(before)) => {
            Some(ContributorTrend::Points(now.points_from(before)))
        }
        (MeasuredValue::Bytes(now), MeasuredValue::Bytes(before)) => {
            let delta = i128::from(now) - i128::from(before);
            i64::try_from(delta).ok().map(ContributorTrend::Bytes)
        }
        (MeasuredValue::ByteRate(now), MeasuredValue::ByteRate(before)) => {
            Some(ContributorTrend::ByteRate(now.delta_from(before)))
        }
        _ => None,
    }
}

/// The previous retained measurement for `identity` under `metric`.
///
/// Keyed on the full identity so a reused PID yields `None` and the trend stays
/// warming up rather than reporting a delta between two unrelated processes
/// (§26).
fn previous_value(
    previous: Option<&ContributorSet>,
    metric: ContributorMetric,
    identity: ProcessIdentity,
) -> Option<MeasuredValue> {
    previous?
        .metric(metric)
        .entries()
        .iter()
        .find(|entry| entry.identity == identity)
        .map(|entry| entry.value)
}

/// Ranks the observed processes and keeps the top `top_k`, deduplicated by
/// identity.
///
/// A full sort is used rather than a partial selection algorithm: §16.2 permits
/// top-`K` selection only once profiling shows the sort is material, and §16.3
/// requires measurement before optimization.
fn select(
    processes: &[ProcessSnapshot],
    metric: ContributorMetric,
    top_k: usize,
    previous: Option<&ContributorSet>,
) -> MetricContributors {
    let mut ranked: Vec<Ranked> = Vec::new();
    let mut observed_total = 0.0f64;
    let mut fallback: Option<MetricState<Observed>> = None;

    for (index, process) in processes.iter().enumerate() {
        let state = observe(process, metric);
        match state {
            MetricState::Available(observed) => {
                observed_total += observed.scalar;
                ranked.push(Ranked {
                    identity: process.identity,
                    index,
                    observed,
                });
            }
            other => fallback = Some(most_representative(fallback, other)),
        }
    }

    // Descending by value; ties broken by identity so the retained list is
    // deterministic and snapshot tests are stable.
    ranked.sort_unstable_by(|left, right| {
        right
            .observed
            .scalar
            .total_cmp(&left.observed.scalar)
            .then_with(|| left.identity.cmp(&right.identity))
    });

    let mut entries: Vec<Contributor> = Vec::with_capacity(top_k.min(ranked.len()));
    let mut retained_total = 0.0f64;
    for candidate in &ranked {
        if entries.len() >= top_k {
            break;
        }
        // §8.5: deduplicate by process identity. The list is already sorted, so
        // the first occurrence is the largest reading for that identity.
        if entries
            .iter()
            .any(|entry| entry.identity == candidate.identity)
        {
            continue;
        }
        let Some(process) = processes.get(candidate.index) else {
            continue;
        };
        let trend = previous_value(previous, metric, candidate.identity)
            .and_then(|before| trend_between(candidate.observed.value, before))
            .map_or(MetricState::WarmingUp, MetricState::Available);

        retained_total += candidate.observed.scalar;
        entries.push(Contributor {
            identity: candidate.identity,
            name: truncate_tail(&process.name, MAX_RETAINED_NAME_WIDTH, Ellipsis::Ascii)
                .into_boxed_str(),
            command: truncate_middle(
                process.command_or_name(),
                MAX_RETAINED_COMMAND_WIDTH,
                Ellipsis::Ascii,
            )
            .into_boxed_str(),
            value: candidate.observed.value,
            trend,
        });
    }

    let share = coverage(ranked.is_empty(), observed_total, retained_total, fallback);
    MetricContributors {
        metric,
        entries,
        coverage: share,
    }
}

/// Computes the evidence coverage share (§2.2).
fn coverage(
    nothing_observed: bool,
    observed_total: f64,
    retained_total: f64,
    fallback: Option<MetricState<Observed>>,
) -> MetricState<Percent> {
    if nothing_observed {
        // No process reported the metric, so there is no total to take a share
        // of. §4 forbids reporting that as 0% or 100%; the reason why the
        // readings were missing is what the panel shows instead.
        return fallback.map_or(MetricState::Unsupported, propagate_unavailable);
    }
    if observed_total <= 0.0 {
        // Readings exist but sum to zero: a share of zero is undefined, and both
        // 0% and 100% would be inventions. It resolves as soon as any process
        // reports activity, which is exactly what `WarmingUp` means (§8.2).
        return MetricState::WarmingUp;
    }
    // Narrowing to f32 is safe: the ratio of two same-signed sums is in
    // `0.0..=1.0` and `Percent::new` rejects anything the narrowing loses.
    #[allow(clippy::cast_possible_truncation)]
    let share = ((retained_total / observed_total) * 100.0) as f32;
    // Floating-point summation can land a hair above the total; a coverage above
    // 100% would read as a bug rather than as rounding.
    Percent::new(share).map_or(MetricState::WarmingUp, |percent| {
        MetricState::Available(percent.clamped_to_100())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcessIo, ProcessMemory, ProcessState, UnavailableReason};
    use core::time::Duration;

    fn process(pid: u32, start_key: u64) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, start_key),
            parent_pid: Some(1),
            name: "proc".into(),
            command: "proc".into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::WarmingUp,
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    fn with_cpu(pid: u32, cpu: f32) -> ProcessSnapshot {
        let mut process = process(pid, u64::from(pid) * 100);
        process.cpu = MetricState::Available(Percent::new(cpu).expect("valid percent"));
        process
    }

    fn with_rss(pid: u32, bytes: u64) -> ProcessSnapshot {
        let mut process = process(pid, u64::from(pid) * 100);
        process.memory = ProcessMemory {
            rss_bytes: MetricState::Available(bytes),
            virtual_bytes: MetricState::Unsupported,
            share_of_total: MetricState::Unsupported,
        };
        process
    }

    fn with_io(pid: u32, read: f64, write: f64) -> ProcessSnapshot {
        let mut process = process(pid, u64::from(pid) * 100);
        process.io = ProcessIo {
            read: MetricState::Available(Rate::new(read).expect("valid rate")),
            write: MetricState::Available(Rate::new(write).expect("valid rate")),
            read_total_bytes: MetricState::Unsupported,
            write_total_bytes: MetricState::Unsupported,
        };
        process
    }

    #[test]
    fn only_the_top_k_processes_are_retained_per_metric() {
        let processes: Vec<ProcessSnapshot> = (1..=50u16)
            .map(|pid| with_cpu(u32::from(pid), f32::from(pid)))
            .collect();
        let set = ContributorSet::from_processes(&processes, None, 3);
        let cpu = set.metric(ContributorMetric::Cpu);

        assert_eq!(cpu.len(), 3);
        let pids: Vec<u32> = cpu.entries().iter().map(|e| e.identity.pid).collect();
        assert_eq!(pids, vec![50, 49, 48], "highest CPU first");
    }

    #[test]
    fn the_retained_count_is_bounded_by_k_times_the_metric_count() {
        let processes: Vec<ProcessSnapshot> = (1..=500)
            .map(|pid| {
                let mut process = with_cpu(pid, 1.0);
                process.memory = with_rss(pid, u64::from(pid)).memory;
                process.io = with_io(pid, f64::from(pid), f64::from(pid)).io;
                process
            })
            .collect();

        let set = ContributorSet::from_processes(&processes, None, 10);
        assert_eq!(set.retained_count(), ContributorSet::max_retained(10));
        assert!(set.retained_count() <= 40);
    }

    #[test]
    fn duplicate_identities_are_collapsed_to_their_largest_reading() {
        // Defensive: a collector must not publish the same identity twice, but a
        // duplicate must never consume two of the K slots (§8.5).
        let mut first = with_cpu(7, 10.0);
        first.identity = ProcessIdentity::new(7, 700);
        let mut duplicate = with_cpu(7, 90.0);
        duplicate.identity = ProcessIdentity::new(7, 700);
        let other = with_cpu(8, 50.0);

        let set = ContributorSet::from_processes(&[first, duplicate, other], None, 10);
        let cpu = set.metric(ContributorMetric::Cpu);

        assert_eq!(cpu.len(), 2, "the duplicate must not take a second slot");
        let entry = cpu.entries().first().expect("a contributor was retained");
        assert_eq!(entry.identity, ProcessIdentity::new(7, 700));
        assert_eq!(
            entry.value,
            MeasuredValue::Percent(Percent::new(90.0).expect("valid"))
        );
    }

    #[test]
    fn a_reused_pid_is_a_different_contributor() {
        let original = with_cpu(31_842, 90.0);
        let mut recycled = with_cpu(31_842, 10.0);
        recycled.identity = ProcessIdentity::new(31_842, 999_999);

        let set = ContributorSet::from_processes(&[original, recycled], None, 10);
        assert_eq!(
            set.metric(ContributorMetric::Cpu).len(),
            2,
            "the same PID with a different start key is a different process"
        );
    }

    #[test]
    fn coverage_is_the_share_of_the_observed_total() {
        // 90 + 10 retained out of 90 + 10 + 50 + 50 observed = 50%.
        let processes = vec![
            with_cpu(1, 90.0),
            with_cpu(2, 10.0),
            with_cpu(3, 50.0),
            with_cpu(4, 50.0),
        ];
        let set = ContributorSet::from_processes(&processes, None, 2);
        let coverage = set
            .metric(ContributorMetric::Cpu)
            .coverage()
            .fresh()
            .copied()
            .expect("coverage is available");
        // Top 2 are 90 and 50 => 140 of 200.
        assert!((coverage.value() - 70.0).abs() < 0.01, "got {coverage}");
    }

    #[test]
    fn full_coverage_is_reported_when_every_observed_process_is_retained() {
        let processes = vec![with_cpu(1, 30.0), with_cpu(2, 70.0)];
        let set = ContributorSet::from_processes(&processes, None, 10);
        let coverage = set
            .metric(ContributorMetric::Cpu)
            .coverage()
            .fresh()
            .copied()
            .expect("coverage is available");
        assert!((coverage.value() - 100.0).abs() < 0.01, "got {coverage}");
    }

    #[test]
    fn coverage_is_unavailable_when_no_total_was_observed() {
        // §2.2's honesty figure must not become 0% or 100% when the platform
        // withheld the readings it would be computed from (§4).
        let processes = vec![process(1, 100), process(2, 200)];
        let set = ContributorSet::from_processes(&processes, None, 10);

        let io = set.metric(ContributorMetric::DiskRead);
        assert!(io.is_empty());
        assert_eq!(io.coverage(), MetricState::Unsupported);
        assert!(io.coverage().fresh().is_none());

        let cpu = set.metric(ContributorMetric::Cpu);
        assert_eq!(cpu.coverage(), MetricState::WarmingUp);
    }

    #[test]
    fn coverage_reports_permission_denied_rather_than_a_flattering_number() {
        let mut denied = process(1, 100);
        denied.io = ProcessIo {
            read: MetricState::PermissionDenied,
            write: MetricState::PermissionDenied,
            read_total_bytes: MetricState::PermissionDenied,
            write_total_bytes: MetricState::PermissionDenied,
        };
        let set = ContributorSet::from_processes(&[denied], None, 10);
        assert_eq!(
            set.metric(ContributorMetric::DiskRead).coverage(),
            MetricState::PermissionDenied
        );
    }

    #[test]
    fn coverage_of_an_all_zero_total_is_warming_up_not_a_share_of_nothing() {
        let processes = vec![with_cpu(1, 0.0), with_cpu(2, 0.0)];
        let set = ContributorSet::from_processes(&processes, None, 10);
        assert_eq!(
            set.metric(ContributorMetric::Cpu).coverage(),
            MetricState::WarmingUp
        );
    }

    #[test]
    fn coverage_of_an_empty_process_table_is_unavailable() {
        let set = ContributorSet::from_processes(&[], None, 10);
        assert!(set.is_empty());
        for metric in ContributorMetric::ALL {
            assert_eq!(set.metric(metric).coverage(), MetricState::Unsupported);
        }
    }

    #[test]
    fn a_first_appearance_has_a_warming_up_trend_not_a_zero_delta() {
        let set = ContributorSet::from_processes(&[with_cpu(1, 42.0)], None, 10);
        let entry = set
            .metric(ContributorMetric::Cpu)
            .entries()
            .first()
            .expect("retained");
        assert_eq!(entry.trend, MetricState::WarmingUp);
        assert!(entry.trend.fresh().is_none());
    }

    #[test]
    fn a_trend_is_the_change_against_the_previous_retained_value() {
        let before = ContributorSet::from_processes(&[with_cpu(1, 141.0)], None, 10);
        let after = ContributorSet::from_processes(&[with_cpu(1, 287.0)], Some(&before), 10);

        let entry = after
            .metric(ContributorMetric::Cpu)
            .entries()
            .first()
            .expect("retained");
        match entry.trend.fresh().copied().expect("trend is available") {
            ContributorTrend::Points(points) => {
                assert!((points - 146.0).abs() < 0.01, "got {points}");
            }
            other => panic!("expected percentage points, got {other:?}"),
        }
    }

    #[test]
    fn a_reused_pid_does_not_inherit_the_previous_processes_trend() {
        let before = ContributorSet::from_processes(&[with_cpu(31_842, 300.0)], None, 10);
        let mut recycled = with_cpu(31_842, 1.0);
        recycled.identity = ProcessIdentity::new(31_842, 999_999);
        let after = ContributorSet::from_processes(&[recycled], Some(&before), 10);

        let entry = after
            .metric(ContributorMetric::Cpu)
            .entries()
            .first()
            .expect("retained");
        assert_eq!(
            entry.trend,
            MetricState::WarmingUp,
            "a delta across a PID reuse would be a fabricated -299 points"
        );
    }

    #[test]
    fn memory_trends_are_signed_byte_deltas_and_io_trends_are_rate_deltas() {
        let before = ContributorSet::from_processes(
            &[{
                let mut p = with_rss(1, 8 * 1024 * 1024);
                p.io = with_io(1, 3_000_000.0, 0.0).io;
                p
            }],
            None,
            10,
        );
        let after = ContributorSet::from_processes(
            &[{
                let mut p = with_rss(1, 4 * 1024 * 1024);
                p.io = with_io(1, 42_000_000.0, 0.0).io;
                p
            }],
            Some(&before),
            10,
        );

        let memory = after
            .metric(ContributorMetric::ResidentMemory)
            .entries()
            .first()
            .expect("retained");
        assert_eq!(
            memory.trend.fresh().copied(),
            Some(ContributorTrend::Bytes(-4 * 1024 * 1024))
        );

        let read = after
            .metric(ContributorMetric::DiskRead)
            .entries()
            .first()
            .expect("retained");
        match read.trend.fresh().copied().expect("available") {
            ContributorTrend::ByteRate(delta) => {
                assert!((delta - 39_000_000.0).abs() < 1.0, "got {delta}");
            }
            other => panic!("expected a rate delta, got {other:?}"),
        }
    }

    #[test]
    fn a_process_outside_the_previous_top_k_has_no_trend_to_report() {
        let before =
            ContributorSet::from_processes(&[with_cpu(1, 90.0), with_cpu(2, 1.0)], None, 1);
        let after = ContributorSet::from_processes(
            &[with_cpu(1, 10.0), with_cpu(2, 80.0)],
            Some(&before),
            1,
        );
        let entry = after
            .metric(ContributorMetric::Cpu)
            .entries()
            .first()
            .expect("retained");
        assert_eq!(entry.identity.pid, 2);
        assert_eq!(entry.trend, MetricState::WarmingUp);
    }

    #[test]
    fn full_command_lines_are_never_retained() {
        let mut long = with_cpu(1, 10.0);
        long.command = format!("rustc {}", "--extremely-long-argument ".repeat(40)).into();
        let set = ContributorSet::from_processes(&[long], None, 10);
        let entry = set
            .metric(ContributorMetric::Cpu)
            .entries()
            .first()
            .expect("retained");

        assert!(
            crate::units::display_width(&entry.command) <= MAX_RETAINED_COMMAND_WIDTH,
            "retained {:?}",
            entry.command
        );
        assert!(entry.command.contains("..."), "truncation must be visible");
    }

    #[test]
    fn a_kernel_thread_with_no_command_line_retains_its_name() {
        let mut kernel = with_cpu(9, 1.0);
        kernel.name = "kworker/2:1".into();
        kernel.command = "".into();
        let set = ContributorSet::from_processes(&[kernel], None, 10);
        let entry = set
            .metric(ContributorMetric::Cpu)
            .entries()
            .first()
            .expect("retained");
        assert_eq!(&*entry.command, "kworker/2:1");
    }

    #[test]
    fn top_k_of_zero_retains_nothing_but_still_reports_coverage() {
        let set = ContributorSet::from_processes(&[with_cpu(1, 50.0)], None, 0);
        let cpu = set.metric(ContributorMetric::Cpu);
        assert!(cpu.is_empty());
        let coverage = cpu.coverage().fresh().copied().expect("available");
        assert!((coverage.value() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn trends_render_with_an_explicit_sign() {
        assert_eq!(
            ContributorTrend::Points(146.0).render(ByteUnits::Iec),
            "+146%"
        );
        assert_eq!(
            ContributorTrend::Points(-12.0).render(ByteUnits::Iec),
            "-12%"
        );
        assert_eq!(
            ContributorTrend::Bytes(-4 * 1024 * 1024).render(ByteUnits::Iec),
            "-4.0 MiB"
        );
        assert_eq!(
            ContributorTrend::ByteRate(39.0 * 1024.0 * 1024.0).render(ByteUnits::Iec),
            "+39M/s"
        );
    }

    #[test]
    fn a_non_finite_rate_delta_renders_as_unavailable_rather_than_panicking() {
        assert_eq!(
            ContributorTrend::ByteRate(f64::NAN).render(ByteUnits::Iec),
            "n/a"
        );
    }

    #[test]
    fn mismatched_measurement_kinds_produce_no_trend() {
        assert!(
            trend_between(
                MeasuredValue::Bytes(1),
                MeasuredValue::Percent(Percent::ZERO)
            )
            .is_none()
        );
    }

    #[test]
    fn heap_use_grows_with_the_retained_contributors_only() {
        let processes: Vec<ProcessSnapshot> = (1..=200).map(|pid| with_cpu(pid, 1.0)).collect();
        let small = ContributorSet::from_processes(&processes, None, 1);
        let large = ContributorSet::from_processes(&processes, None, 10);
        assert!(small.heap_bytes() < large.heap_bytes());

        // The bound that matters: heap use is a function of K, not of the 200
        // processes observed (§8.5, §26).
        let per_contributor = size_of::<Contributor>() + MAX_RETAINED_TEXT_BYTES;
        assert!(
            large.heap_bytes() <= ContributorSet::max_retained(10) * per_contributor,
            "heap {} exceeded the budgeted bound",
            large.heap_bytes()
        );
    }

    #[test]
    fn a_warming_up_set_reports_nothing_measured() {
        let set = ContributorSet::warming_up();
        assert!(set.is_empty());
        assert_eq!(set.heap_bytes(), 0);
        for metric in ContributorMetric::ALL {
            assert_eq!(set.metric(metric).coverage(), MetricState::WarmingUp);
            assert_eq!(set.metric(metric).metric(), metric);
        }
    }

    #[test]
    fn a_counter_reset_keeps_a_process_out_of_the_retained_set() {
        // §21 M4: a reset must not look like activity. The process reported a
        // typed reset rather than a rate, so there is nothing to rank it by.
        let mut resetting = process(1, 100);
        resetting.io = ProcessIo {
            read: MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
            write: MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
            read_total_bytes: MetricState::Available(0),
            write_total_bytes: MetricState::Available(0),
        };
        let set = ContributorSet::from_processes(&[resetting], None, 10);
        let read = set.metric(ContributorMetric::DiskRead);
        assert!(read.is_empty());
        assert_eq!(
            read.coverage(),
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
    }

    #[test]
    fn a_stale_reading_is_not_counted_towards_the_observed_total() {
        let mut stale = process(1, 100);
        stale.cpu = MetricState::Available(Percent::new(50.0).expect("valid"))
            .into_stale(Duration::from_secs(2));
        let set = ContributorSet::from_processes(&[stale], None, 10);
        let cpu = set.metric(ContributorMetric::Cpu);
        assert!(cpu.is_empty(), "a stale value must not feed a calculation");
        assert_eq!(
            cpu.coverage(),
            MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample)
        );
    }
}

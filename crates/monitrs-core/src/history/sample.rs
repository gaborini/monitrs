//! One retained point on the timeline: a compact aggregate plus its
//! attribution evidence (§8.5).

use core::mem::size_of;
use core::time::Duration;
use std::time::SystemTime;

use crate::model::{InterfaceKind, MeasuredValue, MetricState, SystemSnapshot, UnavailableReason};
use crate::units::{Percent, Rate};

use super::{ContributorSet, most_representative, propagate_unavailable};

/// One aggregate series a historical sample retains.
///
/// The set is deliberately small and fixed: §8.5 budgets 300 samples by default,
/// so every field added here costs memory on every one of them. Anything a user
/// can only want for the *live* view stays out.
///
/// `#[non_exhaustive]`: the retained aggregate set is expected to grow within
/// 1.x — the pressure event log deferred to 1.1.0 is a likely source — and a new
/// metric must not force a major version bump.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum HistoryMetric {
    /// Aggregate machine CPU utilization, `0..=100` (§8.3).
    CpuBusy,
    /// Share of memory in use, per the platform's memory semantics (§8.4).
    MemoryUsedShare,
    /// Swap bytes in use.
    SwapUsed,
    /// One-minute load average.
    LoadOne,
    /// Summed read throughput across observed block devices.
    DiskRead,
    /// Summed write throughput across observed block devices.
    DiskWrite,
    /// Summed receive throughput across observed non-loopback interfaces.
    NetworkRx,
    /// Summed transmit throughput across observed non-loopback interfaces.
    NetworkTx,
}

impl HistoryMetric {
    /// Every retained aggregate, in the order the timeline offers them.
    pub const ALL: [Self; 8] = [
        Self::CpuBusy,
        Self::MemoryUsedShare,
        Self::SwapUsed,
        Self::LoadOne,
        Self::DiskRead,
        Self::DiskWrite,
        Self::NetworkRx,
        Self::NetworkTx,
    ];

    /// The short label used by the timeline and comparison rows (§5.6).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::CpuBusy => "CPU",
            Self::MemoryUsedShare => "MEM",
            Self::SwapUsed => "SWAP",
            Self::LoadOne => "LOAD",
            Self::DiskRead => "DISK RD",
            Self::DiskWrite => "DISK WR",
            Self::NetworkRx => "NET RX",
            Self::NetworkTx => "NET TX",
        }
    }

    /// Whether a difference in this metric is expressed in percentage points.
    ///
    /// §5.6 prints `+54 points vs now` rather than `+54%` for percentages,
    /// because a change *of* 54 points and a change *by* 54 percent are different
    /// claims.
    #[must_use]
    pub const fn is_percentage(self) -> bool {
        matches!(self, Self::CpuBusy | Self::MemoryUsedShare)
    }
}

/// The compact aggregate retained for every historical sample.
///
/// Every field is a [`MetricState`] so that a metric the platform withheld stays
/// withheld in history: §26's "unavailable is not zero" applies to the timeline
/// as much as to the live view, and a counter reset must leave a gap rather than
/// a spike (§21 M4).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HistoricalSystemMetrics {
    /// Aggregate machine CPU utilization.
    pub cpu_busy: MetricState<Percent>,
    /// Share of memory in use.
    pub memory_used_share: MetricState<Percent>,
    /// Swap bytes in use.
    pub swap_used_bytes: MetricState<u64>,
    /// One-minute load average, a run-queue length rather than a percentage.
    pub load_one: MetricState<f32>,
    /// Summed read throughput across observed block devices.
    pub disk_read: MetricState<Rate>,
    /// Summed write throughput across observed block devices.
    pub disk_write: MetricState<Rate>,
    /// Summed receive throughput across observed non-loopback interfaces.
    pub network_rx: MetricState<Rate>,
    /// Summed transmit throughput across observed non-loopback interfaces.
    pub network_tx: MetricState<Rate>,
}

impl HistoricalSystemMetrics {
    /// An aggregate with nothing measured, for the first sample (§8.2).
    pub const WARMING_UP: Self = Self {
        cpu_busy: MetricState::WarmingUp,
        memory_used_share: MetricState::WarmingUp,
        swap_used_bytes: MetricState::WarmingUp,
        load_one: MetricState::WarmingUp,
        disk_read: MetricState::WarmingUp,
        disk_write: MetricState::WarmingUp,
        network_rx: MetricState::WarmingUp,
        network_tx: MetricState::WarmingUp,
    };

    /// Reduces a full snapshot to the aggregate worth retaining.
    ///
    /// Device and interface figures are summed over everything that *reported* a
    /// value, which is why the coverage figures in §2.2 exist: the sum is what
    /// was observed, not a claim about the whole machine. Loopback is excluded
    /// because its traffic appears on both directions of the same interface, so
    /// including it would double-count local activity as link throughput (§7.4).
    #[must_use]
    pub fn from_snapshot(snapshot: &SystemSnapshot) -> Self {
        let physical = || {
            snapshot
                .networks
                .iter()
                .filter(|interface| interface.kind != InterfaceKind::Loopback)
        };
        Self {
            cpu_busy: snapshot.cpu.total.as_ref().map(|usage| usage.busy),
            memory_used_share: snapshot.memory.usage,
            swap_used_bytes: snapshot.memory.swap.used,
            load_one: snapshot.load.as_ref().map(|load| load.one),
            disk_read: aggregate_rate(snapshot.disks.iter().map(|disk| disk.read)),
            disk_write: aggregate_rate(snapshot.disks.iter().map(|disk| disk.write)),
            network_rx: aggregate_rate(physical().map(|interface| interface.rx)),
            network_tx: aggregate_rate(physical().map(|interface| interface.tx)),
        }
    }

    /// The absolute measurement for `metric`, ready to render.
    #[must_use]
    pub fn measurement(&self, metric: HistoryMetric) -> MetricState<MeasuredValue> {
        match metric {
            HistoryMetric::CpuBusy => self.cpu_busy.map(MeasuredValue::Percent),
            HistoryMetric::MemoryUsedShare => self.memory_used_share.map(MeasuredValue::Percent),
            HistoryMetric::SwapUsed => self.swap_used_bytes.map(MeasuredValue::Bytes),
            HistoryMetric::LoadOne => self.load_one.map(MeasuredValue::Load),
            HistoryMetric::DiskRead => self.disk_read.map(MeasuredValue::ByteRate),
            HistoryMetric::DiskWrite => self.disk_write.map(MeasuredValue::ByteRate),
            HistoryMetric::NetworkRx => self.network_rx.map(MeasuredValue::ByteRate),
            HistoryMetric::NetworkTx => self.network_tx.map(MeasuredValue::ByteRate),
        }
    }

    /// The *freshly measured* value as a comparable number, or `None`.
    ///
    /// Returns `None` for every unavailable state, including a stale retained
    /// value. That is what stops a counter reset from turning into a spike: a
    /// comparison against a sample whose input was unavailable has no answer, and
    /// `None` is the honest one (§21 M4).
    #[must_use]
    pub fn scalar(&self, metric: HistoryMetric) -> Option<f64> {
        match metric {
            HistoryMetric::CpuBusy => self.cpu_busy.fresh().map(|p| f64::from(p.value())),
            HistoryMetric::MemoryUsedShare => {
                self.memory_used_share.fresh().map(|p| f64::from(p.value()))
            }
            HistoryMetric::SwapUsed => self.swap_used_bytes.fresh().map(|bytes| *bytes as f64),
            HistoryMetric::LoadOne => self.load_one.fresh().copied().map(f64::from),
            HistoryMetric::DiskRead => self.disk_read.fresh().map(|rate| rate.per_second()),
            HistoryMetric::DiskWrite => self.disk_write.fresh().map(|rate| rate.per_second()),
            HistoryMetric::NetworkRx => self.network_rx.fresh().map(|rate| rate.per_second()),
            HistoryMetric::NetworkTx => self.network_tx.fresh().map(|rate| rate.per_second()),
        }
    }
}

/// Sums the rates that were actually measured, preserving unavailability.
///
/// A group with no fresh member does not become `0/s`: the most informative
/// explanation among its members is reported instead (§4, §26).
fn aggregate_rate(states: impl Iterator<Item = MetricState<Rate>>) -> MetricState<Rate> {
    let mut total = 0.0f64;
    let mut observed = false;
    let mut fallback: Option<MetricState<Rate>> = None;

    for state in states {
        match state.fresh() {
            Some(rate) => {
                total += rate.per_second();
                observed = true;
            }
            None => fallback = Some(most_representative(fallback, state)),
        }
    }

    if !observed {
        // An empty device list is `Unsupported`: there was nothing to measure,
        // which is different from a device that refused to answer.
        return fallback.map_or(MetricState::Unsupported, propagate_unavailable);
    }
    Rate::new(total).map_or(
        MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
        MetricState::Available,
    )
}

/// One retained point on the timeline (§8.5).
///
/// Deliberately holds no process table. §26 lists "a full process table per
/// historical sample wastes memory" among the notes that must not be forgotten,
/// and the type system is where that is enforced: there is no field a process
/// list could be stored in.
#[derive(Clone, Debug, PartialEq)]
pub struct HistoricalSample {
    /// The snapshot sequence number this sample came from.
    ///
    /// Ordering and identity both use this rather than a timestamp, because §8.1
    /// allows the wall clock to move but not history (§10.4).
    pub sequence: u64,
    /// Monotonic time from the ring's start instant to this sample's capture.
    ///
    /// Measured from a fixed [`Instant`](std::time::Instant) so that seeking and
    /// comparison never consult the wall clock (§8.1).
    pub monotonic_offset: Duration,
    /// Wall-clock capture time, for the `sample 22:14:07` label only (§5.6).
    pub wall_time: SystemTime,
    /// The compact aggregate.
    pub system: HistoricalSystemMetrics,
    /// The top contributors retained as attribution evidence (§2.2).
    pub contributors: ContributorSet,
}

impl HistoricalSample {
    /// A sample that measured nothing, for the warming-up snapshot (§8.2).
    #[must_use]
    pub const fn warming_up(
        sequence: u64,
        monotonic_offset: Duration,
        wall_time: SystemTime,
    ) -> Self {
        Self {
            sequence,
            monotonic_offset,
            wall_time,
            system: HistoricalSystemMetrics::WARMING_UP,
            contributors: ContributorSet::warming_up(),
        }
    }

    /// Approximate total bytes this sample occupies, struct plus heap.
    ///
    /// §16.1 requires monitrs to measure and expose its own overhead, and §8.5
    /// requires a history memory budget; both need a number per sample.
    #[must_use]
    pub fn estimated_bytes(&self) -> usize {
        size_of::<Self>() + self.contributors.heap_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        CpuUsage, DiskSnapshot, LoadSnapshot, MemorySemantics, NetworkSnapshot, ProcessIdentity,
        ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState,
    };
    use std::time::Instant;

    fn snapshot() -> SystemSnapshot {
        SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8)
    }

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("valid percent")
    }

    fn rate(value: f64) -> Rate {
        Rate::new(value).expect("valid rate")
    }

    fn disk(device: &str, read: MetricState<Rate>, write: MetricState<Rate>) -> DiskSnapshot {
        let mut disk = DiskSnapshot::warming_up(device.into());
        disk.read = read;
        disk.write = write;
        disk
    }

    fn interface(name: &str, kind: InterfaceKind, rx: f64, tx: f64) -> NetworkSnapshot {
        let mut interface = NetworkSnapshot::warming_up(name.into(), kind);
        interface.rx = MetricState::Available(rate(rx));
        interface.tx = MetricState::Available(rate(tx));
        interface
    }

    fn process(pid: u32, cpu: f32) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid) * 7),
            parent_pid: Some(1),
            name: "proc".into(),
            command: "proc --flag".into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::Available(percent(cpu)),
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    #[test]
    fn the_aggregate_carries_over_every_system_metric_it_retains() {
        let mut source = snapshot();
        source.cpu.total = MetricState::Available(CpuUsage::plain(percent(91.0)));
        source.memory.usage = MetricState::Available(percent(69.0));
        source.memory.swap.used = MetricState::Available(2 * 1024 * 1024);
        source.load = MetricState::Available(LoadSnapshot {
            one: 4.2,
            five: 3.0,
            fifteen: 2.0,
        });
        source.disks.push(disk(
            "nvme0n1",
            MetricState::Available(rate(1_000.0)),
            MetricState::Available(rate(2_000.0)),
        ));
        source
            .networks
            .push(interface("en0", InterfaceKind::Physical, 500.0, 250.0));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(metrics.cpu_busy, MetricState::Available(percent(91.0)));
        assert_eq!(
            metrics.memory_used_share,
            MetricState::Available(percent(69.0))
        );
        assert_eq!(
            metrics.swap_used_bytes,
            MetricState::Available(2 * 1024 * 1024)
        );
        let load = metrics
            .load_one
            .fresh()
            .copied()
            .expect("load is available");
        assert!((load - 4.2).abs() < 0.001, "got {load}");
        assert_eq!(metrics.disk_read, MetricState::Available(rate(1_000.0)));
        assert_eq!(metrics.disk_write, MetricState::Available(rate(2_000.0)));
        assert_eq!(metrics.network_rx, MetricState::Available(rate(500.0)));
        assert_eq!(metrics.network_tx, MetricState::Available(rate(250.0)));
    }

    #[test]
    fn device_rates_are_summed_across_the_devices_that_reported() {
        let mut source = snapshot();
        source.disks.push(disk(
            "nvme0n1",
            MetricState::Available(rate(1_000.0)),
            MetricState::Available(rate(0.0)),
        ));
        source.disks.push(disk(
            "nvme1n1",
            MetricState::Available(rate(2_500.0)),
            MetricState::Available(rate(0.0)),
        ));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(metrics.disk_read, MetricState::Available(rate(3_500.0)));
    }

    #[test]
    fn loopback_traffic_is_excluded_so_local_activity_is_not_double_counted() {
        let mut source = snapshot();
        source.networks.push(interface(
            "lo0",
            InterfaceKind::Loopback,
            9_000_000.0,
            9_000_000.0,
        ));
        source
            .networks
            .push(interface("en0", InterfaceKind::Physical, 100.0, 200.0));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(metrics.network_rx, MetricState::Available(rate(100.0)));
        assert_eq!(metrics.network_tx, MetricState::Available(rate(200.0)));
    }

    #[test]
    fn an_interface_list_of_only_loopback_reports_nothing_rather_than_zero() {
        let mut source = snapshot();
        source
            .networks
            .push(interface("lo0", InterfaceKind::Loopback, 1.0, 1.0));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(metrics.network_rx, MetricState::Unsupported);
        assert!(metrics.scalar(HistoryMetric::NetworkRx).is_none());
    }

    #[test]
    fn a_counter_reset_stays_unavailable_instead_of_becoming_a_spike() {
        // §21 M4 acceptance: counter resets must not create false spikes. The
        // only device reported a typed reset, so the aggregate reports the reset.
        let mut source = snapshot();
        source.disks.push(disk(
            "nvme0n1",
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
        ));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(
            metrics.disk_read,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
        assert!(metrics.disk_read.fresh().is_none());
        assert!(metrics.scalar(HistoryMetric::DiskRead).is_none());
    }

    #[test]
    fn a_partly_unavailable_group_still_reports_what_was_measured() {
        let mut source = snapshot();
        source.disks.push(disk(
            "nvme0n1",
            MetricState::Available(rate(700.0)),
            MetricState::Available(rate(0.0)),
        ));
        source.disks.push(disk(
            "dm-0",
            MetricState::TemporarilyUnavailable(UnavailableReason::DeviceDisappeared),
            MetricState::Unsupported,
        ));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(metrics.disk_read, MetricState::Available(rate(700.0)));
    }

    #[test]
    fn an_empty_device_list_is_unsupported_not_zero() {
        let metrics = HistoricalSystemMetrics::from_snapshot(&snapshot());
        assert_eq!(metrics.disk_read, MetricState::Unsupported);
        assert_eq!(metrics.disk_write, MetricState::Unsupported);
    }

    #[test]
    fn a_permission_denied_device_is_reported_as_denied() {
        let mut source = snapshot();
        source.disks.push(disk(
            "nvme0n1",
            MetricState::PermissionDenied,
            MetricState::WarmingUp,
        ));
        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert_eq!(metrics.disk_read, MetricState::PermissionDenied);
        assert_eq!(metrics.disk_write, MetricState::WarmingUp);
    }

    #[test]
    fn a_warming_up_snapshot_produces_a_warming_up_aggregate() {
        let mut source = snapshot();
        source.memory =
            crate::model::MemorySnapshot::warming_up(0, MemorySemantics::SysinfoBaseline);
        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        assert!(metrics.cpu_busy.is_warming_up());
        assert!(metrics.memory_used_share.is_warming_up());
        assert!(metrics.load_one.is_warming_up());
        for metric in HistoryMetric::ALL {
            assert!(
                metrics.scalar(metric).is_none(),
                "{} must not report a number",
                metric.label()
            );
        }
    }

    #[test]
    fn every_metric_exposes_both_a_measurement_and_a_scalar() {
        let mut source = snapshot();
        source.cpu.total = MetricState::Available(CpuUsage::plain(percent(50.0)));
        source.memory.usage = MetricState::Available(percent(25.0));
        source.memory.swap.used = MetricState::Available(4_096);
        source.load = MetricState::Available(LoadSnapshot {
            one: 1.5,
            five: 1.0,
            fifteen: 0.5,
        });
        source.disks.push(disk(
            "d",
            MetricState::Available(rate(10.0)),
            MetricState::Available(rate(20.0)),
        ));
        source
            .networks
            .push(interface("en0", InterfaceKind::Physical, 30.0, 40.0));

        let metrics = HistoricalSystemMetrics::from_snapshot(&source);
        let expected = [50.0, 25.0, 4_096.0, 1.5, 10.0, 20.0, 30.0, 40.0];
        for (metric, want) in HistoryMetric::ALL.into_iter().zip(expected) {
            let got = metrics
                .scalar(metric)
                .unwrap_or_else(|| panic!("{} has no scalar", metric.label()));
            assert!((got - want).abs() < 0.001, "{}: {got}", metric.label());
            assert!(metrics.measurement(metric).is_available());
        }
    }

    #[test]
    fn only_shares_are_expressed_in_percentage_points() {
        assert!(HistoryMetric::CpuBusy.is_percentage());
        assert!(HistoryMetric::MemoryUsedShare.is_percentage());
        for metric in [
            HistoryMetric::SwapUsed,
            HistoryMetric::LoadOne,
            HistoryMetric::DiskRead,
            HistoryMetric::DiskWrite,
            HistoryMetric::NetworkRx,
            HistoryMetric::NetworkTx,
        ] {
            assert!(!metric.is_percentage(), "{}", metric.label());
        }
    }

    #[test]
    fn metric_labels_are_distinct_ascii() {
        let mut labels: Vec<&str> = HistoryMetric::ALL.iter().map(|m| m.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), HistoryMetric::ALL.len());
        for label in labels {
            assert!(label.is_ascii(), "{label} is not legal in ASCII glyph mode");
        }
    }

    #[test]
    fn a_samples_size_is_a_function_of_its_contributors_not_the_process_table() {
        let mut source = snapshot();
        source
            .processes
            .extend((1..=2_000u32).map(|pid| process(pid, 1.0)));

        let contributors = ContributorSet::from_processes(&source.processes, None, 10);
        let sample = HistoricalSample {
            sequence: 1,
            monotonic_offset: Duration::from_secs(1),
            wall_time: SystemTime::UNIX_EPOCH,
            system: HistoricalSystemMetrics::from_snapshot(&source),
            contributors,
        };

        assert_eq!(sample.contributors.retained_count(), 10);
        // 2000 `ProcessSnapshot`s would be far larger than this on their own.
        assert!(
            sample.estimated_bytes() < 8 * 1024,
            "sample grew to {} bytes",
            sample.estimated_bytes()
        );
    }

    #[test]
    fn a_warming_up_sample_keeps_its_ordering_fields_and_measures_nothing() {
        let sample = HistoricalSample::warming_up(0, Duration::ZERO, SystemTime::UNIX_EPOCH);
        assert_eq!(sample.sequence, 0);
        assert_eq!(sample.monotonic_offset, Duration::ZERO);
        assert!(sample.contributors.is_empty());
        assert_eq!(sample.system, HistoricalSystemMetrics::WARMING_UP);
    }
}

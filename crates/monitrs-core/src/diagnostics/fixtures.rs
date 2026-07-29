//! Test-only builders for snapshots and history.
//!
//! Every builder starts from [`SystemSnapshot::warming_up`], so a metric a test
//! does not set stays *unmeasured* rather than zero. That is the §26 invariant
//! turned into a test convenience: a rule that accidentally reads an unavailable
//! metric as a number fails immediately instead of passing on a fixture that
//! happened to be full of zeros.

use core::time::Duration;
use std::time::{Instant, SystemTime};

use crate::history::{HistoryConfig, HistoryRing};
use crate::model::{
    CollectorHealth, CpuUsage, DiskSnapshot, InterfaceKind, LoadSnapshot, MemorySemantics,
    MetricState, NetworkSnapshot, ProcessIdentity, ProcessIo, ProcessMemory, ProcessSnapshot,
    ProcessState, PsiResource, PsiSnapshot, SelfOverhead, SystemSnapshot, TierHealth,
};
use crate::units::{Percent, Rate};

use super::HistoryWindow;

/// The logical CPU count every fixture uses, matching §16.1's reference machine.
pub(crate) const LOGICAL_CPUS: u16 = 8;

/// A validated percentage.
pub(crate) fn percent(value: f32) -> Percent {
    Percent::new(value).expect("valid percent")
}

/// A validated rate.
pub(crate) fn rate(per_second: f64) -> Rate {
    Rate::new(per_second).expect("valid rate")
}

/// A snapshot with identity known and every measurement warming up.
pub(crate) fn snapshot() -> SystemSnapshot {
    SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, LOGICAL_CPUS)
}

/// Sets aggregate CPU utilization.
pub(crate) fn set_cpu(snapshot: &mut SystemSnapshot, busy: f32) {
    snapshot.cpu.total = MetricState::Available(CpuUsage::plain(percent(busy)));
}

/// Sets total and available memory, deriving used and the used share.
pub(crate) fn set_memory(snapshot: &mut SystemSnapshot, total: u64, available: u64) {
    let used = total.saturating_sub(available);
    snapshot.memory.total_bytes = total;
    snapshot.memory.available = MetricState::Available(available);
    snapshot.memory.used = MetricState::Available(used);
    snapshot.memory.free = MetricState::Available(available);
    snapshot.memory.usage =
        Percent::ratio(used, total).map_or(MetricState::Unsupported, MetricState::Available);
    snapshot.memory.semantics = MemorySemantics::LinuxMemAvailable;
    snapshot.memory.cgroup_limit_bytes = MetricState::Unsupported;
}

/// Sets the one-minute load average, with the five and fifteen minute figures
/// trailing it.
pub(crate) fn set_load(snapshot: &mut SystemSnapshot, one: f32) {
    snapshot.load = MetricState::Available(LoadSnapshot {
        one,
        five: one * 0.75,
        fifteen: one * 0.5,
    });
}

/// Configures swap capacity, usage, and activity.
pub(crate) fn set_swap(
    snapshot: &mut SystemSnapshot,
    total: u64,
    used: u64,
    in_rate: f64,
    out_rate: f64,
) {
    snapshot.memory.swap.total_bytes = total;
    snapshot.memory.swap.used = MetricState::Available(used);
    snapshot.memory.swap.usage =
        Percent::ratio(used, total).map_or(MetricState::Unsupported, MetricState::Available);
    snapshot.memory.swap.in_rate = MetricState::Available(rate(in_rate));
    snapshot.memory.swap.out_rate = MetricState::Available(rate(out_rate));
}

/// Adds a block device reporting a busy percentage and throughput.
pub(crate) fn set_disk_busy(snapshot: &mut SystemSnapshot, device: &str, busy: f32) {
    let mut disk = DiskSnapshot::warming_up(device.into());
    disk.busy = MetricState::Available(percent(busy));
    disk.read = MetricState::Available(rate(1_000_000.0));
    disk.write = MetricState::Available(rate(2_000_000.0));
    snapshot.disks.push(disk);
}

/// Adds a physical interface with the given throughput and optional link speed.
pub(crate) fn set_network(
    snapshot: &mut SystemSnapshot,
    name: &str,
    rx: f64,
    tx: f64,
    link_speed_mbps: Option<u64>,
) {
    let mut interface = NetworkSnapshot::warming_up(name.into(), InterfaceKind::Physical);
    interface.rx = MetricState::Available(rate(rx));
    interface.tx = MetricState::Available(rate(tx));
    interface.link_speed_mbps =
        link_speed_mbps.map_or(MetricState::Unsupported, MetricState::Available);
    snapshot.networks.push(interface);
}

/// A PSI resource whose `some` averages are all `some_avg10`.
fn psi_resource(some_avg10: f32) -> PsiResource {
    PsiResource {
        some_avg10: percent(some_avg10),
        some_avg60: percent(some_avg10),
        some_avg300: percent(some_avg10),
        full_avg10: MetricState::Available(percent(some_avg10 / 2.0)),
        full_avg60: MetricState::Available(percent(some_avg10 / 2.0)),
        full_avg300: MetricState::Available(percent(some_avg10 / 2.0)),
        total_stalled: Duration::from_secs(42),
    }
}

/// A complete PSI reading for the three resources.
pub(crate) fn psi_snapshot(cpu: f32, memory: f32, io: f32) -> PsiSnapshot {
    PsiSnapshot {
        cpu: psi_resource(cpu),
        memory: psi_resource(memory),
        io: psi_resource(io),
    }
}

/// Attaches Linux PSI figures to a snapshot.
pub(crate) fn set_psi(snapshot: &mut SystemSnapshot, cpu: f32, memory: f32, io: f32) {
    snapshot.pressure.psi = MetricState::Available(psi_snapshot(cpu, memory, io));
}

/// Adds a process row with the fields the diagnostic rules read.
pub(crate) fn add_process(
    snapshot: &mut SystemSnapshot,
    pid: u32,
    name: &str,
    cpu: Option<f32>,
    rss: Option<u64>,
    state: ProcessState,
) {
    let total = snapshot.memory.total_bytes;
    let mut memory = ProcessMemory::WARMING_UP;
    if let Some(rss) = rss {
        memory.rss_bytes = MetricState::Available(rss);
        memory.share_of_total =
            Percent::ratio(rss, total).map_or(MetricState::Unsupported, MetricState::Available);
    }
    snapshot.processes.push(ProcessSnapshot {
        identity: ProcessIdentity::new(pid, u64::from(pid) * 31),
        parent_pid: Some(1),
        name: name.into(),
        command: format!("{name} --serve").into(),
        exe: None,
        user: MetricState::Unsupported,
        state,
        cpu: cpu
            .map(percent)
            .map_or(MetricState::WarmingUp, MetricState::Available),
        memory,
        io: ProcessIo::UNSUPPORTED,
        threads: MetricState::Unsupported,
        age: MetricState::Available(Duration::from_secs(60)),
        started_at: MetricState::Unsupported,
        is_kernel_thread: false,
    });
}

/// Sets collector lag and the fast-tier timing the collector rules read.
pub(crate) fn set_health(snapshot: &mut SystemSnapshot, lag: Duration, fast_p95: Duration) {
    snapshot.health = CollectorHealth {
        fast: TierHealth {
            last_duration: fast_p95,
            max_duration: fast_p95,
            p95_duration: fast_p95,
            completed: 100,
            failed: 0,
            since_last: Some(lag),
        },
        lag,
        ..CollectorHealth::default()
    };
}

/// Sets monitrs's own measured overhead (§16.1).
pub(crate) fn set_self_overhead(snapshot: &mut SystemSnapshot, cpu: f32, rss_bytes: u64) {
    snapshot.health.self_overhead = Some(SelfOverhead {
        cpu: percent(cpu),
        rss_bytes,
        history_bytes: 4 * 1024 * 1024,
        open_files: MetricState::Available(24),
    });
}

/// A recorded timeline of snapshots at a fixed interval.
///
/// Every sample's `captured_at` is derived from one fixed [`Instant`], so ordering
/// and spans are exact and never depend on how fast the test runs (§8.1).
pub(crate) struct Timeline {
    ring: HistoryRing,
    start: Instant,
    interval: Duration,
    next: u64,
}

impl Timeline {
    /// A timeline retaining five minutes of samples at `interval`.
    pub(crate) fn new(interval: Duration) -> Self {
        let start = Instant::now();
        Self {
            ring: HistoryRing::with_config(
                HistoryConfig {
                    interval,
                    duration: Duration::from_secs(300),
                    ..HistoryConfig::default()
                },
                start,
            ),
            start,
            interval,
            next: 0,
        }
    }

    /// Builds the next snapshot without recording it.
    pub(crate) fn build(&self, mutate: impl FnOnce(&mut SystemSnapshot)) -> SystemSnapshot {
        let sequence = self.next;
        let steps = u32::try_from(sequence).unwrap_or(u32::MAX);
        let offset = self.interval.saturating_mul(steps);
        let mut snapshot = SystemSnapshot::warming_up(
            self.start + offset,
            SystemTime::UNIX_EPOCH + offset,
            LOGICAL_CPUS,
        );
        snapshot.sequence = sequence;
        // The first sample has no interval, which is what makes it warming up
        // rather than zero (§8.2).
        snapshot.elapsed = if sequence == 0 {
            Duration::ZERO
        } else {
            self.interval
        };
        mutate(&mut snapshot);
        snapshot
    }

    /// Builds the next snapshot, records it, and returns it.
    pub(crate) fn push(&mut self, mutate: impl FnOnce(&mut SystemSnapshot)) -> SystemSnapshot {
        let snapshot = self.build(mutate);
        assert!(
            self.ring.record(&snapshot).is_recorded(),
            "fixture snapshots must be strictly newer than the previous one"
        );
        self.next += 1;
        snapshot
    }

    /// Records an externally built snapshot, as the runtime does.
    ///
    /// Used by tests that need to derive pressure *after* recording but *before*
    /// evaluating rules, which is the order the runtime uses.
    pub(crate) fn record(&mut self, snapshot: &SystemSnapshot) -> bool {
        let recorded = self.ring.record(snapshot).is_recorded();
        if recorded {
            self.next = snapshot.sequence.saturating_add(1);
        }
        recorded
    }

    /// Records `count` snapshots built by the same closure.
    pub(crate) fn push_many(
        &mut self,
        count: usize,
        mut mutate: impl FnMut(&mut SystemSnapshot),
    ) -> SystemSnapshot {
        assert!(count > 0, "a timeline segment needs at least one sample");
        let mut last = None;
        for _ in 0..count {
            last = Some(self.push(|snapshot| mutate(snapshot)));
        }
        last.expect("count is non-zero")
    }

    /// A live window over the recorded history.
    pub(crate) fn window(&self) -> HistoryWindow<'_> {
        HistoryWindow::live(&self.ring)
    }

    /// The underlying ring, for tests that need an explicit cursor.
    pub(crate) fn ring(&self) -> &HistoryRing {
        &self.ring
    }

    /// The interval samples are spaced at.
    pub(crate) fn interval(&self) -> Duration {
        self.interval
    }
}

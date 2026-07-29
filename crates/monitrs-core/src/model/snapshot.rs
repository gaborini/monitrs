//! The immutable snapshot published to the UI.
//!
//! §10.4: collectors build a new snapshot and publish it as `Arc<SystemSnapshot>`
//! so the UI never observes a partially updated set of metrics. Nothing in this
//! type has interior mutability and nothing is updated in place.

use core::time::Duration;
use std::time::{Instant, SystemTime};

use crate::model::{
    CapabilitySnapshot, CollectorHealth, CpuSnapshot, DiskSnapshot, FilesystemSnapshot,
    HostSnapshot, LoadSnapshot, MemorySemantics, MemorySnapshot, MetricState, NetworkSnapshot,
    PressureSnapshot, ProcessIdentity, ProcessSnapshot, SensorSnapshot,
};
use crate::units::Percent;

/// One complete, internally consistent observation of the system.
#[derive(Clone, Debug)]
pub struct SystemSnapshot {
    /// Monotonically increasing sequence number.
    ///
    /// Used to detect coalescing and to order snapshots without consulting the
    /// wall clock, which can move backwards (§8.1).
    pub sequence: u64,
    /// Monotonic capture time, for rate calculations and ordering (§8.1).
    ///
    /// Deliberately an [`Instant`], which is why this type is not `Serialize`:
    /// export goes through a dedicated DTO that emits `wall_time` instead.
    pub captured_at: Instant,
    /// Wall-clock capture time, for display and export only.
    pub wall_time: SystemTime,
    /// The *actual* interval since the previous snapshot.
    ///
    /// §8.1 forbids assuming one second. Zero on the first snapshot.
    pub elapsed: Duration,
    /// System identity.
    pub host: HostSnapshot,
    /// CPU state.
    pub cpu: CpuSnapshot,
    /// Memory state.
    pub memory: MemorySnapshot,
    /// Load averages.
    pub load: MetricState<LoadSnapshot>,
    /// Every process visible to us.
    pub processes: Vec<ProcessSnapshot>,
    /// Block devices.
    pub disks: Vec<DiskSnapshot>,
    /// Mounted filesystems. Separate from `disks` by §7.3.
    pub filesystems: Vec<FilesystemSnapshot>,
    /// Network interfaces.
    pub networks: Vec<NetworkSnapshot>,
    /// Pressure Radar.
    pub pressure: PressureSnapshot,
    /// Temperature and battery.
    pub sensors: SensorSnapshot,
    /// What this platform can and cannot report.
    pub capabilities: CapabilitySnapshot,
    /// Collector timing and our own overhead.
    pub health: CollectorHealth,
}

impl SystemSnapshot {
    /// The first snapshot: identity is known, every measurement is warming up.
    ///
    /// §26: the first sample of delta-based data is *not* zero. This constructor
    /// is what makes that the default rather than something each collector must
    /// remember.
    #[must_use]
    pub fn warming_up(captured_at: Instant, wall_time: SystemTime, logical_cpus: u16) -> Self {
        Self {
            sequence: 0,
            captured_at,
            wall_time,
            elapsed: Duration::ZERO,
            host: HostSnapshot::warming_up(),
            cpu: CpuSnapshot::warming_up(logical_cpus),
            memory: MemorySnapshot::warming_up(0, MemorySemantics::SysinfoBaseline),
            load: MetricState::WarmingUp,
            processes: Vec::new(),
            disks: Vec::new(),
            filesystems: Vec::new(),
            networks: Vec::new(),
            pressure: PressureSnapshot::warming_up(),
            sensors: SensorSnapshot::warming_up(),
            capabilities: CapabilitySnapshot::default(),
            health: CollectorHealth::default(),
        }
    }

    /// Looks up a process by stable identity.
    ///
    /// Keyed on the full identity rather than the PID, so a reused PID returns
    /// `None` instead of a different process (§26).
    #[must_use]
    pub fn process(&self, identity: ProcessIdentity) -> Option<&ProcessSnapshot> {
        self.processes
            .iter()
            .find(|process| process.identity == identity)
    }

    /// Looks up whatever process currently holds `pid`, whichever it is.
    ///
    /// Only the signal-revalidation path should use this: it needs to discover
    /// that a PID has been reused, which requires deliberately ignoring the
    /// start key (§6.2).
    #[must_use]
    pub fn process_by_pid(&self, pid: u32) -> Option<&ProcessSnapshot> {
        self.processes
            .iter()
            .find(|process| process.identity.pid == pid)
    }

    /// Total process count, for the `218 total` header in §5.5.
    #[must_use]
    pub fn process_count(&self) -> usize {
        self.processes.len()
    }

    /// The sum of all per-process CPU percentages, core-normalized.
    ///
    /// Used as the denominator of the attribution coverage figure in §2.2. This
    /// is deliberately *not* compared against system CPU as though the two were
    /// interchangeable: they are measured differently, and the coverage figure is
    /// presented as evidence rather than proof.
    #[must_use]
    pub fn total_process_cpu(&self) -> Option<Percent> {
        let sum: f32 = self
            .processes
            .iter()
            .filter_map(|process| process.cpu.fresh().map(|cpu| cpu.value()))
            .sum();
        Percent::new(sum)
    }

    /// How many processes are in a state §7.2 requires to stand out.
    #[must_use]
    pub fn notable_process_count(&self) -> usize {
        self.processes
            .iter()
            .filter(|process| process.state.is_notable())
            .count()
    }

    /// Whether this snapshot can produce valid rates.
    ///
    /// False for the first snapshot, whose `elapsed` is zero.
    #[must_use]
    pub fn has_valid_interval(&self) -> bool {
        !self.elapsed.is_zero()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ProcessIo, ProcessMemory, ProcessState};

    fn process(pid: u32, start_key: u64, cpu: Option<f32>) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, start_key),
            parent_pid: Some(1),
            name: "test".into(),
            command: "test".into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: cpu
                .and_then(Percent::new)
                .map_or(MetricState::WarmingUp, MetricState::Available),
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    fn snapshot() -> SystemSnapshot {
        SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8)
    }

    #[test]
    fn the_first_snapshot_measures_nothing_and_has_no_valid_interval() {
        let snapshot = snapshot();
        assert_eq!(snapshot.sequence, 0);
        assert!(!snapshot.has_valid_interval());
        assert!(snapshot.cpu.total.is_warming_up());
        assert!(snapshot.load.is_warming_up());
        assert_eq!(snapshot.process_count(), 0);
        assert_eq!(snapshot.cpu.logical_count, 8);
    }

    #[test]
    fn process_lookup_by_identity_rejects_a_reused_pid() {
        let mut snapshot = snapshot();
        snapshot
            .processes
            .push(process(31_842, 900_100, Some(287.0)));

        let original = ProcessIdentity::new(31_842, 900_100);
        let recycled = ProcessIdentity::new(31_842, 977_400);

        assert!(snapshot.process(original).is_some());
        assert!(
            snapshot.process(recycled).is_none(),
            "a reused PID must not resolve to the previous process"
        );
    }

    #[test]
    fn lookup_by_pid_deliberately_ignores_the_start_key_so_reuse_is_detectable() {
        let mut snapshot = snapshot();
        snapshot.processes.push(process(31_842, 977_400, None));

        let found = snapshot.process_by_pid(31_842).expect("pid is present");
        assert_eq!(found.identity.start_key, 977_400);
        assert!(
            found
                .identity
                .is_reuse_of(&ProcessIdentity::new(31_842, 900_100))
        );
    }

    #[test]
    fn total_process_cpu_sums_only_measured_values() {
        let mut snapshot = snapshot();
        snapshot.processes.push(process(1, 1, Some(287.0)));
        snapshot.processes.push(process(2, 2, Some(54.0)));
        snapshot.processes.push(process(3, 3, None));

        let total = snapshot.total_process_cpu().expect("two measured values");
        assert!((total.value() - 341.0).abs() < 0.01, "got {total}");
    }

    #[test]
    fn total_process_cpu_of_an_empty_table_is_zero_not_undefined() {
        // An empty table is a real state: a container may legitimately show
        // nothing but our own process.
        let total = snapshot().total_process_cpu().expect("empty sum is zero");
        assert!((total.value() - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn notable_processes_are_counted_for_the_header() {
        let mut snapshot = snapshot();
        snapshot.processes.push(process(1, 1, None));
        let mut zombie = process(2, 2, None);
        zombie.state = ProcessState::Zombie;
        snapshot.processes.push(zombie);
        let mut blocked = process(3, 3, None);
        blocked.state = ProcessState::UninterruptibleSleep;
        snapshot.processes.push(blocked);

        assert_eq!(snapshot.notable_process_count(), 2);
    }
}

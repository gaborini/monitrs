//! Deterministic snapshots for the reducer tests (§17.4, §17.5).
//!
//! # Why this is not `monitrs_collectors::fake::FakeCollector`
//!
//! §17.5 asks for integration tests driven by the fake collector, and the shapes
//! here are deliberately the same ones it exposes — `Fake::exiting_at` and
//! `Fake::reused_as` mirror `FakeProcess::exiting_at` and `FakeProcess::reused_as`
//! sample-for-sample — so a reducer test reads the same way whichever source
//! drives it. What it does *not* do is add a dependency: §10.1 fixes the direction
//! `monitrs-tui -> monitrs-core`, and the reducer consumes `Arc<SystemSnapshot>`
//! values rather than a `SnapshotSource`. Wiring the
//! real collector to the reducer is the binary's job and belongs in the
//! workspace-level integration tests, where the dependency direction is not a
//! question.
//!
//! Everything here is a pure function of the sample sequence number, so a failing
//! assertion is reproducible.

use core::time::Duration;
use std::sync::{Arc, OnceLock};
use std::time::{Instant, SystemTime};

use monitrs_core::model::{
    CapabilitySnapshot, CapabilityState, MetricState, ProcessIdentity, ProcessIo, ProcessMemory,
    ProcessSnapshot, ProcessState, SystemSnapshot, UserIdentity,
};
use monitrs_core::units::Percent;

/// Total physical memory the fixtures pretend to have, matching §5.5's mockup.
pub(super) const TOTAL_MEMORY_BYTES: u64 = 32 * 1024 * 1024 * 1024;

/// The nominal interval between fixture samples.
///
/// Fixtures space samples by this so that history offsets are predictable. Real
/// rate arithmetic never assumes an interval (§8.1); this only decides where a
/// sample lands on the timeline.
pub(super) const FIXTURE_INTERVAL: Duration = Duration::from_secs(1);

/// The monotonic origin every fixture snapshot is measured from.
///
/// One [`Instant`] for the whole test binary, so a snapshot built in one test and
/// a history ring built in another agree about what "sequence 3" means.
pub(super) fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The capture time of sample `sequence`.
pub(super) fn captured_at(sequence: u64) -> Instant {
    epoch() + FIXTURE_INTERVAL.saturating_mul(u32::try_from(sequence).unwrap_or(u32::MAX))
}

/// One process in a fixture table.
#[derive(Clone, Debug)]
pub(super) struct Fake {
    identity: ProcessIdentity,
    parent_pid: Option<u32>,
    name: Box<str>,
    command: Box<str>,
    user: Box<str>,
    uid: u32,
    state: ProcessState,
    cpu: Option<f32>,
    rss_bytes: u64,
    threads: u32,
    age: Duration,
    exits_at: Option<u64>,
    reused_start_key: Option<u64>,
    is_kernel_thread: bool,
}

impl Fake {
    /// A process with plausible defaults and no CPU measurement yet.
    ///
    /// CPU starts [`MetricState::WarmingUp`] rather than zero: §26 forbids
    /// presenting an unmeasured delta as `0`, and a fixture that did would let a
    /// first-frame bug through.
    ///
    /// PID 1 defaults to *no* parent, every other process to PID 1. A default of
    /// `Some(1)` for PID 1 itself would be a self-parent, which the tree builder
    /// correctly reports as a broken cycle — a fixture must not manufacture the
    /// pathological case it is not testing.
    #[must_use]
    pub(super) fn new(pid: u32, start_key: u64, name: &str) -> Self {
        Self {
            identity: ProcessIdentity::new(pid, start_key),
            parent_pid: (pid != 1).then_some(1),
            name: name.into(),
            command: format!("{name} --serve").into(),
            user: "gabor".into(),
            uid: 501,
            state: ProcessState::Sleeping,
            cpu: None,
            rss_bytes: 64 * 1024 * 1024,
            threads: 4,
            age: Duration::from_secs(60),
            exits_at: None,
            reused_start_key: None,
            is_kernel_thread: false,
        }
    }

    /// Sets the core-normalized CPU percentage, which may exceed 100 (§8.3).
    #[must_use]
    pub(super) const fn cpu(mut self, percent: f32) -> Self {
        self.cpu = Some(percent);
        self
    }

    /// Sets the resident size.
    #[must_use]
    pub(super) const fn rss(mut self, bytes: u64) -> Self {
        self.rss_bytes = bytes;
        self
    }

    /// Sets the parent PID.
    #[must_use]
    pub(super) const fn parent(mut self, pid: u32) -> Self {
        self.parent_pid = Some(pid);
        self
    }

    /// Sets the owning user.
    #[must_use]
    pub(super) fn user(mut self, name: &str, uid: u32) -> Self {
        self.user = name.into();
        self.uid = uid;
        self
    }

    /// Sets the scheduling state.
    #[must_use]
    pub(super) const fn state(mut self, state: ProcessState) -> Self {
        self.state = state;
        self
    }

    /// Sets the full command line.
    #[must_use]
    pub(super) fn command(mut self, command: &str) -> Self {
        self.command = command.into();
        self
    }

    /// Makes the process disappear from the table at `sequence`.
    ///
    /// Mirrors `FakeProcess::exiting_at`.
    #[must_use]
    pub(super) const fn exiting_at(mut self, sequence: u64) -> Self {
        self.exits_at = Some(sequence);
        self
    }

    /// Makes a different process take over this PID once it has exited.
    ///
    /// Mirrors `FakeProcess::reused_as`, and is what the pin and pending-action
    /// tests need: the same PID with a different start key must inherit nothing
    /// (§26).
    #[must_use]
    pub(super) const fn reused_as(mut self, start_key: u64) -> Self {
        self.reused_start_key = Some(start_key);
        self
    }

    /// Marks this as a kernel thread (§7.2).
    #[must_use]
    pub(super) const fn kernel_thread(mut self) -> Self {
        self.is_kernel_thread = true;
        self
    }

    /// The identity this fixture's PID resolves to at `sequence`.
    #[must_use]
    pub(super) fn identity_at(&self, sequence: u64) -> Option<ProcessIdentity> {
        if self.exits_at.is_none_or(|exit| sequence < exit) {
            return Some(self.identity);
        }
        self.reused_start_key
            .map(|start_key| ProcessIdentity::new(self.identity.pid, start_key))
    }

    /// The row this fixture contributes to sample `sequence`, if any.
    fn row(&self, sequence: u64) -> Option<ProcessSnapshot> {
        let identity = self.identity_at(sequence)?;
        let recycled = identity != self.identity;
        Some(ProcessSnapshot {
            identity,
            parent_pid: self.parent_pid,
            name: self.name.clone(),
            command: self.command.clone(),
            exe: Some(format!("/usr/bin/{}", self.name).into()),
            user: MetricState::Available(UserIdentity {
                uid: self.uid,
                name: Some(self.user.clone()),
            }),
            state: self.state,
            // A recycled PID is a different process: it starts warming up, exactly
            // as a newly discovered process does.
            cpu: if recycled {
                MetricState::WarmingUp
            } else {
                self.cpu
                    .and_then(Percent::new)
                    .map_or(MetricState::WarmingUp, MetricState::Available)
            },
            memory: ProcessMemory {
                rss_bytes: MetricState::Available(self.rss_bytes),
                virtual_bytes: MetricState::Available(self.rss_bytes.saturating_mul(6)),
                share_of_total: Percent::ratio(self.rss_bytes, TOTAL_MEMORY_BYTES)
                    .map_or(MetricState::Unsupported, MetricState::Available),
            },
            io: ProcessIo::WARMING_UP,
            threads: MetricState::Available(self.threads),
            age: MetricState::Available(if recycled { Duration::ZERO } else { self.age }),
            started_at: MetricState::Available(SystemTime::UNIX_EPOCH),
            is_kernel_thread: self.is_kernel_thread,
        })
    }
}

/// Capabilities with everything available, including process control.
#[must_use]
pub(super) fn all_capabilities() -> CapabilitySnapshot {
    let mut capabilities = CapabilitySnapshot::default();
    for state in [
        &mut capabilities.per_process_io,
        &mut capabilities.per_process_threads,
        &mut capabilities.per_process_open_files,
        &mut capabilities.per_process_sockets,
        &mut capabilities.per_process_working_directory,
        &mut capabilities.per_core_cpu,
        &mut capabilities.cpu_breakdown,
        &mut capabilities.load_average,
        &mut capabilities.swap_activity,
        &mut capabilities.disk_io,
        &mut capabilities.disk_busy,
        &mut capabilities.filesystem_capacity,
        &mut capabilities.network_counters,
        &mut capabilities.network_link_speed,
        &mut capabilities.network_errors,
        &mut capabilities.temperatures,
        &mut capabilities.battery,
        &mut capabilities.linux_psi,
        &mut capabilities.cgroup_limits,
        &mut capabilities.kernel_threads,
        &mut capabilities.process_signals,
        &mut capabilities.renice,
    ] {
        *state = CapabilityState::Available;
    }
    capabilities
}

/// A snapshot of `processes` at `sequence`, with every capability available.
#[must_use]
pub(super) fn snapshot_of(sequence: u64, processes: &[Fake]) -> SystemSnapshot {
    snapshot_with(sequence, processes, all_capabilities())
}

/// A snapshot of `processes` at `sequence` with explicit capabilities.
#[must_use]
pub(super) fn snapshot_with(
    sequence: u64,
    processes: &[Fake],
    capabilities: CapabilitySnapshot,
) -> SystemSnapshot {
    let mut snapshot = SystemSnapshot::warming_up(
        captured_at(sequence),
        SystemTime::UNIX_EPOCH + Duration::from_secs(sequence),
        8,
    );
    snapshot.sequence = sequence;
    snapshot.elapsed = if sequence == 0 {
        Duration::ZERO
    } else {
        FIXTURE_INTERVAL
    };
    snapshot.processes = processes
        .iter()
        .filter_map(|process| process.row(sequence))
        .collect();
    snapshot.capabilities = capabilities;
    snapshot
}

/// A shared snapshot, as the collector publishes them (§10.4).
#[must_use]
pub(super) fn arc_snapshot(sequence: u64, processes: &[Fake]) -> Arc<SystemSnapshot> {
    Arc::new(snapshot_of(sequence, processes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_disappears_at_its_exit_sequence() {
        let table = [Fake::new(7, 70, "node").cpu(3.0).exiting_at(2)];

        assert_eq!(snapshot_of(1, &table).processes.len(), 1);
        assert!(
            snapshot_of(2, &table).processes.is_empty(),
            "exiting_at is exclusive, like FakeProcess::exiting_at"
        );
    }

    #[test]
    fn a_reused_pid_arrives_with_a_new_start_key_and_no_measurements() {
        let table = [Fake::new(7, 70, "node")
            .cpu(3.0)
            .exiting_at(2)
            .reused_as(999)];

        let recycled = snapshot_of(2, &table);
        let process = recycled
            .process_by_pid(7)
            .expect("the PID is in use by the new process");
        assert_eq!(process.identity, ProcessIdentity::new(7, 999));
        assert!(
            process.cpu.is_warming_up(),
            "a different process starts warming up (§8.2)"
        );
        assert!(recycled.process(ProcessIdentity::new(7, 70)).is_none());
    }

    #[test]
    fn the_first_sample_has_no_interval_and_later_ones_do() {
        assert_eq!(snapshot_of(0, &[]).elapsed, Duration::ZERO);
        assert_eq!(snapshot_of(1, &[]).elapsed, FIXTURE_INTERVAL);
        assert!(!snapshot_of(0, &[]).has_valid_interval());
        assert!(snapshot_of(1, &[]).has_valid_interval());
    }

    #[test]
    fn capture_times_are_monotonic_in_the_sequence() {
        assert!(captured_at(3) > captured_at(2));
        assert_eq!(
            captured_at(3).saturating_duration_since(captured_at(1)),
            FIXTURE_INTERVAL * 2
        );
    }

    #[test]
    fn an_unmeasured_cpu_is_warming_up_rather_than_zero() {
        let table = [Fake::new(1, 1, "launchd")];
        let snapshot = snapshot_of(4, &table);
        let process = snapshot
            .process(ProcessIdentity::new(1, 1))
            .expect("present");
        assert!(process.cpu.is_warming_up());
    }

    #[test]
    fn every_capability_can_be_made_available() {
        let capabilities = all_capabilities();
        assert!(
            capabilities
                .entries()
                .iter()
                .all(|(_, state)| *state == CapabilityState::Available)
        );
    }
}

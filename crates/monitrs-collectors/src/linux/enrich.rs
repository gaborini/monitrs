//! Turning parsed `/proc` data into an enriched [`SystemSnapshot`].
//!
//! This is where the Linux layer earns its place: the cross-platform baseline
//! already produces a complete snapshot, and everything here *replaces a field with
//! a better-founded one* or fills in a field the baseline had to leave
//! [`MetricState::Unsupported`].
//!
//! | Field | What the baseline gives | What this adds |
//! |---|---|---|
//! | `cpu.total` | a single percentage | the `/proc/stat` delta plus the user/system/iowait/steal split (§8.3) |
//! | `memory` | `sysinfo` semantics | `MemAvailable` semantics and the cache/buffer detail (§8.4) |
//! | `disks` | throughput only | operations, busy time from field 10, and queue depth (§7.3) |
//! | `networks` | bytes, packets, errors | drops, link state, and link speed (§7.4) |
//! | `processes` | whole-second start time | clock-tick start keys, kernel-thread flags, `io` counters |
//! | `pressure.psi` | nothing | all three PSI resources (§2.3) |
//! | `sensors.battery` | `unsupported` everywhere | `/sys/class/power_supply`: cycle count, wear, pack temperature, watts |
//! | `host.environment` | nothing | the container/VM heuristic with evidence (§7.5) |
//! | `memory.cgroup_limit_bytes` | nothing | the container limit, *beside* the host total (§9.2) |
//!
//! Two structural decisions make this testable on a machine with no `/proc`:
//!
//! * [`LinuxEnrichment::apply`] takes already-read [`LinuxSources`] rather than
//!   reading anything itself, so the whole enrichment path runs from fixtures;
//! * every rate goes through the frozen `monitrs_core::rates` engine, so warming up,
//!   counter resets, and wraparound are handled in one tested place rather than
//!   re-derived here (§8.2).
//!
//! The process table is *enriched*, not rebuilt: the baseline enumerates processes,
//! and a PID that appears between its enumeration and ours is picked up on the next
//! tick rather than being added from a partially readable set of files. That keeps
//! one snapshot internally consistent (§10.4).

use core::time::Duration;
use std::collections::{HashMap, HashSet};
use std::time::{Instant, SystemTime};

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    BatterySnapshot, CapabilityState, CpuQuota, CpuUsage, DiskSnapshot, DiskTotals,
    HostEnvironment, InterfaceKind, LinkState, MetricState, NetworkSnapshot, ProcessIdentity,
    ProcessIo, ProcessMemory, PsiResource, PsiSnapshot, Tier, TrafficTotals, UnavailableReason,
    UserIdentity,
};
use monitrs_core::rates::{
    CounterTracker, CounterWidth, KeyedProcessCpuTrackers, KeyedRateTrackers, KeyedTrackers,
    SystemCpuTracker,
};
use monitrs_core::units::Percent;

use crate::linux::cgroup::{
    CgroupVersion, classify_environment, parse_cpu_max, parse_dmi_hypervisor, parse_memory_current,
    parse_memory_max, parse_pid_cgroup,
};
use crate::linux::diskstats::{DiskStats, parse_diskstats};
use crate::linux::loadavg::{parse_loadavg, parse_uptime};
use crate::linux::meminfo::parse_meminfo;
use crate::linux::netdev::{parse_link_speed_mbps, parse_net_dev, parse_operstate};
use crate::linux::power::{BatteryAttributes, PowerSupplyKind, battery_from, classify};
use crate::linux::process::{parse_pid_io, parse_pid_stat, parse_pid_status};
use crate::linux::psi::parse_pressure;
use crate::linux::read::{
    LinuxSources, PowerSupplySources, ReadDiagnostics, ReadFailure, SourceBytes,
};
use crate::linux::stat::ProcStat;
use crate::source::SampleTick;

/// Device and interface names key the rate trackers.
type DeviceKey = Box<str>;

/// The `USER_HZ` value `/proc` counters are expressed in.
///
/// 100 on every architecture Linux supports in practice: `USER_HZ` is a userspace
/// ABI constant, deliberately decoupled from the kernel's internal `HZ`, and only
/// Alpha ever used a different value. The authoritative answer is
/// `sysconf(_SC_CLK_TCK)`, which needs `libc`; [`LinuxEnrichment::with_clock_ticks`]
/// exists so a caller that can call it does not have to trust this constant.
pub const DEFAULT_USER_HZ: u64 = 100;

/// The link speed and operational state read from `/sys/class/net`.
#[derive(Clone, Copy, Debug)]
struct LinkFacts {
    state: MetricState<LinkState>,
    speed_mbps: MetricState<u64>,
}

/// Long-lived Linux enrichment state.
///
/// Holds every rate baseline and every slow-tier cache, and must outlive a single
/// tick: §9.1 and §26 both require the collector to stay alive across refreshes,
/// because recreating it destroys every delta.
#[derive(Debug)]
pub struct LinuxEnrichment {
    ticks_per_second: u64,

    cpu_total: SystemCpuTracker,
    cpu_per_core: KeyedTrackers<u16, SystemCpuTracker>,
    previous_stat: Option<ProcStat>,

    disk_read: KeyedRateTrackers<DeviceKey>,
    disk_write: KeyedRateTrackers<DeviceKey>,
    disk_read_ops: KeyedRateTrackers<DeviceKey>,
    disk_write_ops: KeyedRateTrackers<DeviceKey>,
    previous_diskstats: HashMap<DeviceKey, DiskStats>,
    previous_diskstats_at: Option<Instant>,

    net_rx: KeyedRateTrackers<DeviceKey>,
    net_tx: KeyedRateTrackers<DeviceKey>,
    net_rx_packets: KeyedRateTrackers<DeviceKey>,
    net_tx_packets: KeyedRateTrackers<DeviceKey>,
    launch_baseline: HashMap<DeviceKey, TrafficTotals>,

    process_cpu: KeyedProcessCpuTrackers,
    process_read: KeyedTrackers<ProcessIdentity, CounterTracker>,
    process_write: KeyedTrackers<ProcessIdentity, CounterTracker>,

    // Carried between ticks for tiers that are not due (§9.1).
    cached_uptime: Option<Duration>,
    cached_boot_time_secs: Option<u64>,
    cached_links: HashMap<DeviceKey, LinkFacts>,
    cached_battery: MetricState<BatterySnapshot>,
    /// When `cached_battery` was actually measured, so a carried-over reading can
    /// state its age (§4). `None` until the sensor group has read it once.
    battery_read_at: Option<Instant>,
    cached_environment: MetricState<HostEnvironment>,
    cached_memory_limit: MetricState<u64>,
    cached_cpu_quota: MetricState<CpuQuota>,
    cached_cgroup_version: Option<CgroupVersion>,
    cgroup_memory_current: MetricState<u64>,

    diagnostics: ReadDiagnostics,
}

impl Default for LinuxEnrichment {
    fn default() -> Self {
        Self::new()
    }
}

impl LinuxEnrichment {
    /// Builds enrichment state assuming the standard [`DEFAULT_USER_HZ`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_clock_ticks(DEFAULT_USER_HZ)
    }

    /// Builds enrichment state with an explicit `USER_HZ`.
    ///
    /// A zero is replaced by [`DEFAULT_USER_HZ`]: a clock rate of zero would make
    /// every CPU time zero, which §26 forbids far more strongly than it forbids
    /// assuming a constant.
    #[must_use]
    pub fn with_clock_ticks(ticks_per_second: u64) -> Self {
        // §8.2: `/proc` counters are 64-bit on every supported kernel, but a
        // backwards move is still treated as a reset rather than a wrap, because a
        // 64-bit byte counter takes decades to wrap and a device re-creation takes
        // none.
        let counters = || KeyedRateTrackers::new(CounterWidth::Unknown);
        Self {
            ticks_per_second: if ticks_per_second == 0 {
                DEFAULT_USER_HZ
            } else {
                ticks_per_second
            },
            cpu_total: SystemCpuTracker::new(),
            cpu_per_core: KeyedTrackers::new(()),
            previous_stat: None,
            disk_read: counters(),
            disk_write: counters(),
            disk_read_ops: counters(),
            disk_write_ops: counters(),
            previous_diskstats: HashMap::new(),
            previous_diskstats_at: None,
            net_rx: counters(),
            net_tx: counters(),
            net_rx_packets: counters(),
            net_tx_packets: counters(),
            launch_baseline: HashMap::new(),
            process_cpu: KeyedProcessCpuTrackers::default(),
            process_read: KeyedTrackers::new(CounterWidth::Unknown),
            process_write: KeyedTrackers::new(CounterWidth::Unknown),
            cached_uptime: None,
            cached_boot_time_secs: None,
            cached_links: HashMap::new(),
            // Warming up rather than unsupported until the sensor group has actually
            // looked: claiming "this machine has no battery" before reading
            // `/sys/class/power_supply` would be a fact asserted without evidence.
            cached_battery: MetricState::WarmingUp,
            battery_read_at: None,
            cached_environment: MetricState::WarmingUp,
            cached_memory_limit: MetricState::WarmingUp,
            cached_cpu_quota: MetricState::WarmingUp,
            cached_cgroup_version: None,
            cgroup_memory_current: MetricState::WarmingUp,
            diagnostics: ReadDiagnostics::default(),
        }
    }

    /// The `USER_HZ` in use.
    #[must_use]
    pub const fn ticks_per_second(&self) -> u64 {
        self.ticks_per_second
    }

    /// Read failures worth reporting, and the count of the routine ones (§7.5).
    #[must_use]
    pub const fn diagnostics(&self) -> &ReadDiagnostics {
        &self.diagnostics
    }

    /// The cgroup CPU bandwidth limit as last read, for tests and diagnostics.
    ///
    /// The snapshot carries this too, in
    /// [`CpuSnapshot::cgroup_quota`](monitrs_core::model::CpuSnapshot::cgroup_quota),
    /// beside — never instead of — the host's `logical_count`, which is what §9.2
    /// requires: the machine has its CPUs and the group has its ceiling, and a view
    /// needs both to say which of the two is the wall a process is hitting.
    #[must_use]
    pub const fn cgroup_cpu_limit(&self) -> MetricState<CpuQuota> {
        self.cached_cpu_quota
    }

    /// The cgroup memory limit as last read, alongside — never instead of — the
    /// host total in [`monitrs_core::model::MemorySnapshot::total_bytes`].
    #[must_use]
    pub const fn cgroup_memory_limit(&self) -> MetricState<u64> {
        self.cached_memory_limit
    }

    /// Current charge against the cgroup memory limit, where readable.
    #[must_use]
    pub const fn cgroup_memory_current(&self) -> MetricState<u64> {
        self.cgroup_memory_current
    }

    /// Which cgroup hierarchy this host uses, once the slow tier has run (§9.2).
    #[must_use]
    pub const fn cgroup_version(&self) -> Option<CgroupVersion> {
        self.cached_cgroup_version
    }

    /// Applies every source that was read to `snapshot`.
    ///
    /// Fields whose sources were not read this tick are left exactly as the baseline
    /// produced them (§9.1: never an all-fields refresh).
    pub fn apply(
        &mut self,
        snapshot: &mut SystemSnapshot,
        sources: &LinuxSources,
        tick: &SampleTick,
    ) {
        // The recorded failures describe *this* sample; the suppression count is
        // cumulative.
        self.diagnostics.clear_entries();

        self.apply_slow_tier(sources);
        self.apply_medium_tier(sources, snapshot);
        self.apply_cpu(sources, snapshot, tick);
        self.apply_memory(sources, snapshot);
        self.apply_disks(sources, snapshot, tick);
        self.apply_networks(sources, snapshot, tick);
        self.apply_pressure(sources, snapshot);
        self.apply_battery(sources, snapshot, tick);
        self.apply_processes(sources, snapshot, tick);

        if sources.processes_truncated {
            // §16.2: shedding work is acceptable; hiding it is not.
            self.diagnostics
                .record("/proc process enumeration", ReadFailure::Oversized);
        }
        snapshot.host.environment = self.cached_environment.clone();
        // Published from the cache rather than at the point of reading, because the
        // ceiling belongs on every snapshot and `cpu.max` is only read on the slow tier
        // (§8.6): a limit is a configuration fact, not a measurement, so republishing
        // the cached value is honest where republishing a cached *reading* would not be.
        snapshot.cpu.cgroup_quota = self.cached_cpu_quota;
        self.diagnostics
            .apply_to(&mut snapshot.health, tick.elapsed);
    }

    /// Reads one source, recording an unexpected failure and mapping the rest.
    fn take<'a>(
        &mut self,
        source: &'static str,
        bytes: Option<&'a SourceBytes>,
    ) -> Option<&'a [u8]> {
        match bytes {
            // Not due this tick: not a failure, and nothing to report.
            None => None,
            Some(Ok(bytes)) => Some(bytes),
            Some(Err(failure)) => {
                self.diagnostics.record(source, *failure);
                None
            }
        }
    }

    /// Slow tier: cgroup limits, hierarchy version, and the environment heuristic.
    fn apply_slow_tier(&mut self, sources: &LinuxSources) {
        if let Some(bytes) = self.take(
            "/sys/fs/cgroup/memory.max",
            sources.cgroup.memory_max.as_ref(),
        ) {
            match parse_memory_max(bytes) {
                // §9.2's sentinel rule lives in `CgroupLimit::state`: `max` becomes
                // Unsupported, never a number.
                Ok(limit) => self.cached_memory_limit = limit.state(),
                Err(failure) => {
                    self.cached_memory_limit =
                        MetricState::TemporarilyUnavailable(failure.reason());
                }
            }
        } else if sources.cgroup.memory_max.is_some() {
            // The file was read and failed; the limit is unknown rather than absent.
            self.cached_memory_limit = MetricState::Unsupported;
        }

        if let Some(bytes) = self.take("/sys/fs/cgroup/cpu.max", sources.cgroup.cpu_max.as_ref()) {
            self.cached_cpu_quota = match parse_cpu_max(bytes) {
                // `CpuMax::state` holds §9.2's sentinel rule: a `max` quota becomes
                // Unsupported rather than a very large number of CPUs.
                Ok(limit) => limit.state(),
                // Previously this was `.ok()`, which kept the last good value and so
                // presented a ceiling read minutes ago as the current one. A quota that
                // cannot be parsed is unknown, and saying so is the whole point of §4.
                Err(failure) => MetricState::TemporarilyUnavailable(failure.reason()),
            };
        } else if sources.cgroup.cpu_max.is_some() {
            // Read and failed — `take` has already recorded it. Unsupported rather than
            // a retained quota, matching `memory.max` directly above.
            self.cached_cpu_quota = MetricState::Unsupported;
        }
        if let Some(bytes) = self.take(
            "/sys/fs/cgroup/cgroup.controllers",
            sources.cgroup.controllers.as_ref(),
        ) {
            // The unified hierarchy is the only one with this file, so its presence
            // is the cgroup v2 detection §9.2 asks for.
            if !bytes.is_empty() {
                self.cached_cgroup_version = Some(CgroupVersion::V2);
            }
        }

        if let Some(environment) = sources.environment.as_ref() {
            let membership = self
                .take("/proc/self/cgroup", Some(&environment.self_cgroup))
                .and_then(|bytes| parse_pid_cgroup(bytes).ok())
                .unwrap_or_default();
            if let Some(version) = membership.version() {
                self.cached_cgroup_version = Some(version);
            }
            // A DMI read failure is expected on a machine without DMI (every ARM
            // board), so it is not recorded as an issue.
            let hypervisor = match &environment.dmi_sys_vendor {
                Ok(bytes) => parse_dmi_hypervisor(bytes),
                Err(_) => None,
            };
            self.cached_environment = MetricState::Available(classify_environment(
                &membership,
                // The `/.dockerenv` marker is deliberately not consulted here: it
                // lives in the filesystem root rather than under `/proc` or `/sys`,
                // so reading it would take this layer outside the two directories it
                // is rooted at and make the check untestable. The cgroup path is
                // strictly better evidence anyway — a marker file can be baked into
                // an image and left behind — and `classify_environment` keeps the
                // parameter for a caller that does have the knowledge.
                false,
                hypervisor.as_deref(),
            ));
        }
    }

    /// Medium tier: load, uptime, and static per-interface facts.
    fn apply_medium_tier(&mut self, sources: &LinuxSources, snapshot: &mut SystemSnapshot) {
        if let Some(bytes) = self.take("/proc/loadavg", sources.loadavg.as_ref()) {
            match parse_loadavg(bytes) {
                Ok(parsed) => snapshot.load = MetricState::Available(parsed.load),
                Err(failure) => {
                    snapshot.load = MetricState::TemporarilyUnavailable(failure.reason());
                }
            }
        }

        if let Some(bytes) = self.take("/proc/uptime", sources.uptime.as_ref()) {
            if let Ok(parsed) = parse_uptime(bytes) {
                self.cached_uptime = Some(parsed.since_boot);
                snapshot.host.uptime = MetricState::Available(parsed.since_boot);
            }
        } else if let Some(uptime) = self.cached_uptime {
            // Not due this tick: keep the last figure rather than blanking it.
            snapshot.host.uptime = MetricState::Available(uptime);
        }

        if let Some(interfaces) = sources.interfaces.as_ref() {
            self.cached_links.clear();
            for interface in interfaces {
                let state = match &interface.operstate {
                    Ok(bytes) => parse_operstate(bytes)
                        .map_or(MetricState::Unsupported, MetricState::Available),
                    Err(failure) => failure.system_state(),
                };
                // §7.4: an unreadable or unnegotiated speed must leave the
                // utilisation percentage unavailable, so it maps to a state that
                // carries no number.
                let speed_mbps = match &interface.speed {
                    Ok(bytes) => match parse_link_speed_mbps(bytes) {
                        Ok(Some(speed)) => MetricState::Available(speed),
                        Ok(None) | Err(_) => {
                            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown)
                        }
                    },
                    Err(_) => {
                        MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown)
                    }
                };
                self.cached_links
                    .insert(interface.name.clone(), LinkFacts { state, speed_mbps });
            }
        }
    }

    /// `/proc/stat` into CPU utilisation and the per-state breakdown.
    fn apply_cpu(
        &mut self,
        sources: &LinuxSources,
        snapshot: &mut SystemSnapshot,
        tick: &SampleTick,
    ) {
        let Some(bytes) = self.take("/proc/stat", sources.stat.as_ref()) else {
            return;
        };
        let stat = match crate::linux::stat::parse_proc_stat(bytes) {
            Ok(stat) => stat,
            Err(failure) => {
                self.diagnostics.record("/proc/stat", ReadFailure::Failed);
                snapshot.cpu.total = MetricState::TemporarilyUnavailable(failure.reason());
                return;
            }
        };

        if let Some(boot_time) = stat.boot_time_secs {
            self.cached_boot_time_secs = Some(boot_time);
            snapshot.host.boot_time =
                MetricState::Available(SystemTime::UNIX_EPOCH + Duration::from_secs(boot_time));
        }

        let previous = self.previous_stat.as_ref().map(|previous| previous.total);
        let busy = self
            .cpu_total
            .observe(stat.total.totals(self.ticks_per_second), tick.captured_at);
        let breakdown = previous
            .and_then(|previous| stat.total.breakdown_since(previous))
            .map_or(MetricState::WarmingUp, MetricState::Available);
        snapshot.cpu.total = busy.map(|busy| CpuUsage {
            busy: busy.clamped_to_100(),
            breakdown,
        });

        let previous_cores = self
            .previous_stat
            .as_ref()
            .map(|previous| previous.per_cpu.clone())
            .unwrap_or_default();
        let mut cores: Vec<CpuUsage> = Vec::with_capacity(stat.per_cpu.len());
        let mut all_measured = true;
        for (index, times) in stat.per_cpu.iter().enumerate() {
            let key = u16::try_from(index).unwrap_or(u16::MAX);
            // Every core is observed even when an earlier one is still warming up:
            // stopping early would leave the remaining cores without a baseline, so
            // they would warm up again on the next tick and the panel would never
            // populate.
            let usage = self.cpu_per_core.observe(
                key,
                times.totals(self.ticks_per_second),
                tick.captured_at,
            );
            let Some(busy) = usage.fresh().copied() else {
                all_measured = false;
                continue;
            };
            let core_breakdown = previous_cores
                .get(index)
                .and_then(|previous| times.breakdown_since(*previous))
                .map_or(MetricState::WarmingUp, MetricState::Available);
            cores.push(CpuUsage {
                busy: busy.clamped_to_100(),
                breakdown: core_breakdown,
            });
        }
        snapshot.cpu.per_core = if stat.per_cpu.is_empty() {
            // A container can hide the per-CPU lines entirely.
            MetricState::Unsupported
        } else if all_measured {
            MetricState::Available(cores)
        } else {
            MetricState::WarmingUp
        };
        // Cores can be hotplugged away; dropping their baselines keeps the set
        // proportional to the machine (§10.3).
        let core_count = stat.per_cpu.len();
        self.cpu_per_core
            .retain(|index| usize::from(*index) < core_count);

        snapshot.capabilities.cpu_breakdown = if snapshot
            .cpu
            .total
            .fresh()
            .is_some_and(|usage| usage.breakdown.fresh().is_some())
        {
            CapabilityState::Available
        } else if self.previous_stat.is_some() {
            CapabilityState::Unsupported
        } else {
            // Still warming up: the breakdown needs two readings.
            snapshot.capabilities.cpu_breakdown
        };
        snapshot.capabilities.per_core_cpu = if stat.per_cpu.is_empty() {
            CapabilityState::Unsupported
        } else {
            CapabilityState::Available
        };
        self.previous_stat = Some(stat);
    }

    /// `/proc/meminfo` and the cgroup limit into the memory snapshot.
    fn apply_memory(&mut self, sources: &LinuxSources, snapshot: &mut SystemSnapshot) {
        if let Some(bytes) = self.take(
            "/sys/fs/cgroup/memory.current",
            sources.cgroup.memory_current.as_ref(),
        ) {
            self.cgroup_memory_current = parse_memory_current(bytes)
                .map_or(MetricState::Unsupported, MetricState::Available);
        }
        snapshot.capabilities.cgroup_limits = match sources.cgroup.memory_max.as_ref() {
            Some(Ok(_)) => CapabilityState::Available,
            Some(Err(ReadFailure::Denied)) => CapabilityState::PermissionDenied,
            Some(Err(_)) => CapabilityState::Unsupported,
            None => snapshot.capabilities.cgroup_limits,
        };

        let Some(bytes) = self.take("/proc/meminfo", sources.meminfo.as_ref()) else {
            return;
        };
        let Ok(info) = parse_meminfo(bytes) else {
            self.diagnostics
                .record("/proc/meminfo", ReadFailure::Failed);
            return;
        };
        // `None` means this kernel has no `MemAvailable`, in which case §8.4 forbids
        // silently substituting a different definition of "used": the baseline's
        // numbers, with their own declared semantics, stay.
        if let Some(memory) = info.to_snapshot(self.cached_memory_limit, self.cgroup_memory_current)
        {
            snapshot.memory = memory;
        }
    }

    /// `/proc/diskstats` into per-device throughput, operations, and busy time.
    fn apply_disks(
        &mut self,
        sources: &LinuxSources,
        snapshot: &mut SystemSnapshot,
        tick: &SampleTick,
    ) {
        let Some(bytes) = self.take("/proc/diskstats", sources.diskstats.as_ref()) else {
            return;
        };
        let Ok(devices) = parse_diskstats(bytes) else {
            self.diagnostics
                .record("/proc/diskstats", ReadFailure::Failed);
            return;
        };
        if devices.is_empty() {
            return;
        }

        // The interval between the two *diskstats* readings, which is not necessarily
        // the tick interval: this file may have been skipped on a previous tick.
        let elapsed = self
            .previous_diskstats_at
            .map(|previous| tick.captured_at.saturating_duration_since(previous));
        let at = tick.captured_at;
        let mut snapshots = Vec::with_capacity(devices.len());
        let mut busy_available = false;

        for device in &devices {
            let key: DeviceKey = device.device.clone();
            let previous = self.previous_diskstats.get(&key);
            let read = self.disk_read.observe(key.clone(), device.read_bytes(), at);
            let write = self
                .disk_write
                .observe(key.clone(), device.written_bytes(), at);
            let read_ops = self
                .disk_read_ops
                .observe(key.clone(), device.reads_completed, at);
            let write_ops = self
                .disk_write_ops
                .observe(key.clone(), device.writes_completed, at);

            let (busy, queue_length) = match (previous, elapsed) {
                (Some(previous), Some(elapsed)) => {
                    let busy = device
                        .busy_since(previous, elapsed)
                        .map_or(MetricState::WarmingUp, MetricState::Available);
                    if busy.is_available() {
                        busy_available = true;
                    }
                    let queue = device
                        .queue_length_since(previous, elapsed)
                        .map_or(MetricState::WarmingUp, MetricState::Available);
                    (busy, queue)
                }
                // §7.3 permits a busy percentage only where it is semantically
                // correct, and one reading is not enough to have one at all.
                _ => (MetricState::WarmingUp, MetricState::WarmingUp),
            };

            // Carry over what only the baseline knows: the device model and the
            // mount points, whose discovery §8.6 puts in a slower tier.
            let baseline = snapshot
                .disks
                .iter()
                .find(|existing| existing.device == key);
            snapshots.push(DiskSnapshot {
                device: key,
                model: baseline.and_then(|disk| disk.model.clone()),
                read,
                write,
                read_ops,
                write_ops,
                busy,
                queue_length,
                totals: MetricState::Available(DiskTotals {
                    read_bytes: device.read_bytes(),
                    write_bytes: device.written_bytes(),
                }),
                mount_points: baseline
                    .map(|disk| disk.mount_points.clone())
                    .unwrap_or_default(),
            });
        }

        let live: HashSet<DeviceKey> = devices.iter().map(|device| device.device.clone()).collect();
        self.disk_read.retain(|key| live.contains(key));
        self.disk_write.retain(|key| live.contains(key));
        self.disk_read_ops.retain(|key| live.contains(key));
        self.disk_write_ops.retain(|key| live.contains(key));
        self.previous_diskstats = devices
            .into_iter()
            .map(|device| (device.device.clone(), device))
            .collect();
        self.previous_diskstats_at = Some(at);

        snapshot.disks = snapshots;
        snapshot.capabilities.disk_io = CapabilityState::Available;
        // Only claim the capability once a busy figure has actually been produced;
        // the 4-field reduced form never produces one.
        if busy_available {
            snapshot.capabilities.disk_busy = CapabilityState::Available;
        }
    }

    /// `/proc/net/dev` plus `/sys/class/net` into interface snapshots.
    fn apply_networks(
        &mut self,
        sources: &LinuxSources,
        snapshot: &mut SystemSnapshot,
        tick: &SampleTick,
    ) {
        let Some(bytes) = self.take("/proc/net/dev", sources.net_dev.as_ref()) else {
            return;
        };
        let Ok(interfaces) = parse_net_dev(bytes) else {
            self.diagnostics
                .record("/proc/net/dev", ReadFailure::Failed);
            return;
        };
        if interfaces.is_empty() {
            return;
        }

        let at = tick.captured_at;
        let mut snapshots = Vec::with_capacity(interfaces.len());
        let mut speed_known = false;

        for interface in &interfaces {
            let key: DeviceKey = interface.name.clone();
            let rx = self
                .net_rx
                .observe(key.clone(), interface.totals.rx_bytes, at);
            let tx = self
                .net_tx
                .observe(key.clone(), interface.totals.tx_bytes, at);
            let rx_packets =
                self.net_rx_packets
                    .observe(key.clone(), interface.totals.rx_packets, at);
            let tx_packets =
                self.net_tx_packets
                    .observe(key.clone(), interface.totals.tx_packets, at);

            // Totals since launch start at zero, because the OS counter may have
            // wrapped or been reset long before monitrs started (§7.4).
            let baseline_totals = self
                .launch_baseline
                .entry(key.clone())
                .or_insert(interface.totals);
            let since_launch = TrafficTotals {
                rx_bytes: interface
                    .totals
                    .rx_bytes
                    .saturating_sub(baseline_totals.rx_bytes),
                tx_bytes: interface
                    .totals
                    .tx_bytes
                    .saturating_sub(baseline_totals.tx_bytes),
                rx_packets: interface
                    .totals
                    .rx_packets
                    .saturating_sub(baseline_totals.rx_packets),
                tx_packets: interface
                    .totals
                    .tx_packets
                    .saturating_sub(baseline_totals.tx_packets),
            };

            let facts = self.cached_links.get(&key).copied();
            if let Some(facts) = facts
                && facts.speed_mbps.is_available()
            {
                speed_known = true;
            }
            let baseline = snapshot
                .networks
                .iter()
                .find(|existing| existing.name == key);

            snapshots.push(NetworkSnapshot {
                name: key.clone(),
                // Interface classification and addresses come from the baseline,
                // which reads them from the same kernel but through `getifaddrs`.
                kind: baseline.map_or(InterfaceKind::Unknown, |existing| existing.kind),
                addresses: baseline
                    .map(|existing| existing.addresses.clone())
                    .unwrap_or_default(),
                mac: baseline.and_then(|existing| existing.mac.clone()),
                state: facts.map_or(MetricState::Unsupported, |facts| facts.state),
                rx,
                tx,
                rx_packets,
                tx_packets,
                errors: MetricState::Available(interface.errors),
                link_speed_mbps: speed_mbps_or_unknown(facts),
                since_launch,
                os_totals: MetricState::Available(interface.totals),
            });
        }

        let live: HashSet<DeviceKey> = interfaces
            .iter()
            .map(|interface| interface.name.clone())
            .collect();
        // An interface that vanished must not keep a baseline: §8.2 lists interface
        // rename and disappearance as cases that must not produce a delta.
        self.net_rx.retain(|key| live.contains(key));
        self.net_tx.retain(|key| live.contains(key));
        self.net_rx_packets.retain(|key| live.contains(key));
        self.net_tx_packets.retain(|key| live.contains(key));
        self.launch_baseline.retain(|key, _| live.contains(key));

        snapshot.networks = snapshots;
        snapshot.capabilities.network_counters = CapabilityState::Available;
        snapshot.capabilities.network_errors = CapabilityState::Available;
        snapshot.capabilities.network_link_speed = if speed_known {
            CapabilityState::Available
        } else {
            CapabilityState::Unsupported
        };
    }

    /// `/proc/pressure/*` into the PSI section of the radar (§2.3).
    ///
    /// Only the raw figures are filled in. Deriving a [`monitrs_core::model::PressureState`]
    /// is policy and belongs to the diagnostic engine, not to a collector.
    fn apply_pressure(&mut self, sources: &LinuxSources, snapshot: &mut SystemSnapshot) {
        let resource = |enrichment: &mut Self, name: &'static str, bytes: Option<&SourceBytes>| {
            enrichment
                .take(name, bytes)
                .and_then(|bytes| parse_pressure(bytes).ok())
        };
        let cpu = resource(self, "/proc/pressure/cpu", sources.pressure_cpu.as_ref());
        let memory = resource(
            self,
            "/proc/pressure/memory",
            sources.pressure_memory.as_ref(),
        );
        let io = resource(self, "/proc/pressure/io", sources.pressure_io.as_ref());

        match (cpu, memory, io) {
            (Some(cpu), Some(memory), Some(io)) => {
                snapshot.pressure.psi = MetricState::Available(PsiSnapshot { cpu, memory, io });
                snapshot.capabilities.linux_psi = CapabilityState::Available;
            }
            (cpu, memory, io) => {
                // Partial PSI is possible: `/proc/pressure/io` exists only with a
                // block layer. Reporting the resources we have beats reporting none,
                // and an unmeasured resource is explicitly unavailable rather than
                // quiet zeroes.
                if cpu.is_none() && memory.is_none() && io.is_none() {
                    if sources.pressure_cpu.is_some() {
                        snapshot.pressure.psi = MetricState::Unsupported;
                        snapshot.capabilities.linux_psi = CapabilityState::Unsupported;
                    }
                    return;
                }
                let unavailable = PsiResource {
                    some_avg10: Percent::ZERO,
                    some_avg60: Percent::ZERO,
                    some_avg300: Percent::ZERO,
                    full_avg10: MetricState::Unsupported,
                    full_avg60: MetricState::Unsupported,
                    full_avg300: MetricState::Unsupported,
                    total_stalled: Duration::ZERO,
                };
                snapshot.pressure.psi = MetricState::Available(PsiSnapshot {
                    cpu: cpu.unwrap_or(unavailable),
                    memory: memory.unwrap_or(unavailable),
                    io: io.unwrap_or(unavailable),
                });
                snapshot.capabilities.linux_psi = CapabilityState::Available;
            }
        }
    }

    /// `/sys/class/power_supply` into the battery, or into an honest absence.
    ///
    /// The baseline leaves `sensors.battery` [`MetricState::Unsupported`] on every
    /// tick, so the cached reading has to be written back on every tick too — not
    /// only on the ones the sensor group was due. §9.1 forbids re-reading for that,
    /// which is exactly what the cache is for. What the write-back publishes on a
    /// tick that did not read is the same value marked stale with the real gap since
    /// it was measured, so no frame presents a carried charge as a measured one (§4).
    ///
    /// **A machine with no battery is the case this method exists to get right.** A
    /// server, a container, a CI runner and a desktop all reach the same two lines:
    /// the class directory lists no system battery, and the metric stays
    /// [`MetricState::Unsupported`] — a fact about the hardware, not a failed read,
    /// not 0%, and not an omitted field (§4, §26). Staleness cannot change that:
    /// only a value that was once measured can go stale.
    fn apply_battery(
        &mut self,
        sources: &LinuxSources,
        snapshot: &mut SystemSnapshot,
        tick: &SampleTick,
    ) {
        if let Some(supplies) = sources.power_supplies.as_ref() {
            // `Some` *is* the read: the sensor group's gate in
            // [`crate::linux::read::collect_sources`] is what decides whether these
            // attributes were opened this tick, so this is where the read happened.
            self.cached_battery = self.read_battery(supplies);
            self.battery_read_at = Some(tick.captured_at);
        }
        // Derived from the cached reading rather than from the value published below.
        // The published one is stale-marked on a carried tick, and `is_available()`
        // is false for it — deriving the capability from that would flip the battery
        // to unsupported every few seconds, a capability flickering for a reason the
        // reader cannot see (§4), which the Inspect screen would render as "this
        // machine cannot report battery".
        snapshot.capabilities.battery = if self.cached_battery.is_available() {
            CapabilityState::Available
        } else if self.cached_battery.is_warming_up() {
            CapabilityState::Unknown
        } else {
            CapabilityState::Unsupported
        };
        // Through the same rule every sensor publish site uses, so a carried charge
        // states the real gap since it was read and a fresh one does not pretend to
        // have one (§4, §8.1).
        snapshot.sensors.battery =
            crate::common::published_sensor(self.cached_battery, tick, self.battery_read_at);
    }

    /// The first system battery among this tick's power supplies.
    ///
    /// "First" is by sorted directory name, which [`crate::linux::read::ProcRoot`]
    /// guarantees, so a laptop with `BAT0` and `BAT1` always reports the same one
    /// rather than alternating between them. Two packs are not summed: their charge
    /// percentages are of different capacities, and averaging them would produce a
    /// figure that describes neither.
    fn read_battery(&mut self, supplies: &[PowerSupplySources]) -> MetricState<BatterySnapshot> {
        for supply in supplies {
            let path = |file: &str| format!("/sys/class/power_supply/{}/{file}", supply.name);
            // An absent `type` or `scope` is not a failure worth reporting: `scope`
            // is missing on most drivers by design, and a directory with no `type`
            // is not something this layer claims to understand.
            let kind = supply.kind.as_deref().ok();
            let scope = supply.scope.as_deref().ok();
            if classify(kind, scope) != PowerSupplyKind::SystemBattery {
                continue;
            }
            let attributes = BatteryAttributes {
                // Only the charge level is worth a diagnostic line: it is the one
                // attribute whose absence discards the whole reading, and every
                // other file here is legitimately missing on some driver.
                status: supply.status.as_deref().ok(),
                capacity: self.take_owned(path("capacity"), &supply.capacity),
                cycle_count: supply.cycle_count.as_deref().ok(),
                energy_full_design: supply.energy_full_design.as_deref().ok(),
                energy_full: supply.energy_full.as_deref().ok(),
                charge_full_design: supply.charge_full_design.as_deref().ok(),
                charge_full: supply.charge_full.as_deref().ok(),
                voltage_min_design: supply.voltage_min_design.as_deref().ok(),
                power_now: supply.power_now.as_deref().ok(),
                current_now: supply.current_now.as_deref().ok(),
                voltage_now: supply.voltage_now.as_deref().ok(),
                temp: supply.temp.as_deref().ok(),
                time_to_empty: supply.time_to_empty.as_deref().ok(),
                time_to_full: supply.time_to_full.as_deref().ok(),
            };
            return battery_from(&attributes);
        }
        // No system battery among the supplies — or no supplies at all, which is what
        // `list_power_supplies` failing looks like from here.
        MetricState::Unsupported
    }

    /// Reads one attribute, recording an unexpected failure against a runtime path.
    ///
    /// [`Self::take`] wants a `&'static str` source name because it is called per
    /// process per tick and an owned string there would allocate thousands of times
    /// a second. A power supply is read once every five seconds, so it can afford
    /// the path that actually failed.
    fn take_owned<'a>(&mut self, source: String, bytes: &'a SourceBytes) -> Option<&'a [u8]> {
        match bytes {
            Ok(bytes) => Some(bytes),
            Err(failure) => {
                self.diagnostics.record(&source, *failure);
                None
            }
        }
    }

    /// Enriches the baseline's process rows with native data.
    fn apply_processes(
        &mut self,
        sources: &LinuxSources,
        snapshot: &mut SystemSnapshot,
        tick: &SampleTick,
    ) {
        if sources.processes.is_empty() {
            return;
        }
        let total_memory = snapshot.memory.total_bytes;
        let at = tick.captured_at;
        let uptime = self.cached_uptime;
        let boot_time = self.cached_boot_time_secs;
        let ticks = self.ticks_per_second;

        // Index the baseline rows by PID once, so enrichment is linear rather than
        // quadratic in the process count (§16.1).
        let mut by_pid: HashMap<u32, usize> = HashMap::with_capacity(snapshot.processes.len());
        for (index, process) in snapshot.processes.iter().enumerate() {
            by_pid.insert(process.identity.pid, index);
        }

        let mut live: HashSet<ProcessIdentity> = HashSet::with_capacity(sources.processes.len());
        let mut io_available = false;
        let mut io_denied = false;

        for source in &sources.processes {
            // A vanished process is expected and is only ever counted as suppressed,
            // never turned into a diagnostic line (§9.2).
            let stat_bytes = match source.stat.as_ref() {
                Ok(bytes) => bytes,
                Err(failure) => {
                    self.diagnostics.record("/proc/<pid>/stat", *failure);
                    continue;
                }
            };
            let Ok(stat) = parse_pid_stat(stat_bytes) else {
                continue;
            };
            let identity = stat.identity();
            live.insert(identity);

            let status = source
                .status
                .as_ref()
                .ok()
                .and_then(|bytes| parse_pid_status(bytes).ok());

            let cpu = self.process_cpu.observe(identity, stat.cpu_time(ticks), at);
            let io = match source.io.as_ref() {
                Ok(bytes) => match parse_pid_io(bytes) {
                    Ok(counters) => {
                        io_available = true;
                        ProcessIo {
                            read: self.process_read.observe(identity, counters.read_bytes, at),
                            write: self
                                .process_write
                                .observe(identity, counters.written_bytes, at),
                            read_total_bytes: MetricState::Available(counters.read_bytes),
                            write_total_bytes: MetricState::Available(counters.written_bytes),
                        }
                    }
                    Err(failure) => {
                        let reason = failure.reason();
                        ProcessIo {
                            read: MetricState::TemporarilyUnavailable(reason),
                            write: MetricState::TemporarilyUnavailable(reason),
                            read_total_bytes: MetricState::TemporarilyUnavailable(reason),
                            write_total_bytes: MetricState::TemporarilyUnavailable(reason),
                        }
                    }
                },
                Err(failure) => {
                    if failure.privileges_might_help() {
                        io_denied = true;
                    }
                    self.diagnostics.record("/proc/<pid>/io", *failure);
                    // Each field infers its own value type, so the permission denial
                    // reaches the rate *and* the total rather than either becoming a
                    // zero (§9.2).
                    ProcessIo {
                        read: failure.process_state(),
                        write: failure.process_state(),
                        read_total_bytes: failure.process_state(),
                        write_total_bytes: failure.process_state(),
                    }
                }
            };

            let rss = status.as_ref().and_then(|status| status.rss_bytes);
            let memory = ProcessMemory {
                rss_bytes: rss.map_or(MetricState::Unsupported, MetricState::Available),
                virtual_bytes: MetricState::Available(stat.virtual_bytes),
                share_of_total: rss
                    .and_then(|rss| Percent::ratio(rss, total_memory))
                    .map_or(MetricState::Unsupported, MetricState::Available),
            };
            let command = source
                .cmdline
                .as_ref()
                .ok()
                .map(|bytes| crate::linux::process::join_cmdline(bytes));
            let age = uptime.map(|uptime| stat.age(uptime, ticks));
            let started_at = boot_time.map(|boot| {
                SystemTime::UNIX_EPOCH
                    + Duration::from_secs(boot)
                    + crate::linux::parse::ticks_to_duration(stat.start_time_ticks, ticks)
            });

            let Some(&index) = by_pid.get(&stat.pid) else {
                // The baseline did not see this process. Adding a row from a
                // partially readable set of files would make one snapshot describe
                // two different moments (§10.4); the next tick picks it up.
                continue;
            };
            let Some(process) = snapshot.processes.get_mut(index) else {
                continue;
            };

            // The identity is replaced deliberately: the baseline's whole-second
            // start time cannot distinguish a PID reused inside one second, and every
            // downstream consumer — history attribution, pinning, the signal path —
            // keys on this value (§9.2, §26).
            process.identity = identity;
            process.parent_pid = Some(stat.ppid);
            process.state = stat.state;
            process.is_kernel_thread = stat.is_kernel_thread();
            process.cpu = cpu;
            process.memory = memory;
            process.io = io;
            process.threads = MetricState::Available(stat.threads);
            if let Some(command) = command
                && !command.is_empty()
            {
                process.command = command;
            }
            if let Some(name) = status.as_ref().and_then(|status| status.name.clone()) {
                process.name = name;
            } else {
                // `comm` from `stat` is the fallback, and it is the field the
                // parenthesis handling exists for.
                process.name = stat.name.clone();
            }
            if let Some(age) = age {
                process.age = MetricState::Available(age);
            }
            if let Some(started_at) = started_at {
                process.started_at = MetricState::Available(started_at);
            }
            if !process.user.is_available()
                && let Some(uid) = status.as_ref().and_then(|status| status.uid)
            {
                // The numeric id is always readable even when name resolution is
                // not, and §7.2 needs *something* in the USER column.
                process.user = MetricState::Available(UserIdentity { uid, name: None });
            }
        }

        // Trackers must follow live identities only, or PID churn grows them without
        // bound (§10.3).
        self.process_cpu.retain(|identity| live.contains(identity));
        self.process_read.retain(|identity| live.contains(identity));
        self.process_write
            .retain(|identity| live.contains(identity));

        snapshot.capabilities.per_process_threads = CapabilityState::Available;
        snapshot.capabilities.kernel_threads = CapabilityState::Available;
        snapshot.capabilities.per_process_io = if io_available {
            CapabilityState::Available
        } else if io_denied {
            // Every readable process refused: root would help, and §4 wants that
            // said once rather than per row.
            CapabilityState::PermissionDenied
        } else {
            snapshot.capabilities.per_process_io
        };
    }
}

/// Which tiers a Linux enrichment pass needs in order to be complete.
///
/// Exposed so the collector can ask for the first tick's sources without
/// duplicating the tier mapping in [`crate::linux::read::collect_sources`].
#[must_use]
pub const fn tier_of_source(source: LinuxSource) -> Tier {
    match source {
        LinuxSource::Stat
        | LinuxSource::MemInfo
        | LinuxSource::DiskStats
        | LinuxSource::NetDev
        | LinuxSource::Pressure
        | LinuxSource::ProcessFiles
        | LinuxSource::CgroupCurrent => Tier::Fast,
        LinuxSource::LoadAvg | LinuxSource::Uptime | LinuxSource::InterfaceAttributes => {
            Tier::Medium
        }
        LinuxSource::CgroupLimits | LinuxSource::Environment => Tier::Slow,
    }
}

/// The `/proc` and `/sys` sources this layer reads, for tier documentation (§8.6).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum LinuxSource {
    /// `/proc/stat`.
    Stat,
    /// `/proc/meminfo`.
    MemInfo,
    /// `/proc/loadavg`.
    LoadAvg,
    /// `/proc/uptime`.
    Uptime,
    /// `/proc/diskstats`.
    DiskStats,
    /// `/proc/net/dev`.
    NetDev,
    /// `/proc/pressure/*`.
    Pressure,
    /// `/proc/<pid>/*`.
    ProcessFiles,
    /// `/sys/class/net/*/{operstate,speed}`.
    InterfaceAttributes,
    /// cgroup `memory.max` and `cpu.max`.
    CgroupLimits,
    /// cgroup `memory.current`.
    CgroupCurrent,
    /// Container and VM evidence.
    Environment,
}

/// Builds the `link_speed_mbps` field, keeping §7.4's rule in one place.
///
/// A helper rather than an inline expression because the rule is easy to break: an
/// interface with no `/sys` entry at all must still report *unknown* rather than
/// nothing, so that [`NetworkSnapshot::utilization`] refuses to compute a percentage.
fn speed_mbps_or_unknown(facts: Option<LinkFacts>) -> MetricState<u64> {
    match facts {
        Some(facts) => facts.speed_mbps,
        None => MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures as fx;
    use crate::linux::read::{
        CgroupSources, EnvironmentSources, InterfaceSources, ProcessSources, SourceBytes,
    };
    use crate::tier::DueTiers;
    use monitrs_core::model::{
        Confidence, EnvironmentKind, MemorySemantics, ProcessSnapshot, ProcessState, Tier,
    };

    /// Wraps a fixture as a successful read.
    fn ok(bytes: &[u8]) -> SourceBytes {
        Ok(bytes.to_vec())
    }

    /// A power supply with nothing but a name and a `type`.
    ///
    /// Every other attribute is [`ReadFailure::Missing`], which is what a real
    /// `/sys/class/power_supply` entry looks like: no driver exports all sixteen,
    /// and the interesting cases are built by overriding the few that matter.
    fn power_supply(name: &str, kind: SourceBytes) -> PowerSupplySources {
        PowerSupplySources {
            name: name.into(),
            kind,
            scope: Err(ReadFailure::Missing),
            status: Err(ReadFailure::Missing),
            capacity: Err(ReadFailure::Missing),
            cycle_count: Err(ReadFailure::Missing),
            energy_full_design: Err(ReadFailure::Missing),
            energy_full: Err(ReadFailure::Missing),
            charge_full_design: Err(ReadFailure::Missing),
            charge_full: Err(ReadFailure::Missing),
            voltage_min_design: Err(ReadFailure::Missing),
            power_now: Err(ReadFailure::Missing),
            current_now: Err(ReadFailure::Missing),
            voltage_now: Err(ReadFailure::Missing),
            temp: Err(ReadFailure::Missing),
            time_to_empty: Err(ReadFailure::Missing),
            time_to_full: Err(ReadFailure::Missing),
        }
    }

    /// One tick's worth of sources, from the fixtures.
    ///
    /// `second` selects the later reading of every counter file, so a two-call
    /// sequence exercises exactly the warming-up-then-measured path a real collector
    /// takes (§8.2).
    fn sources(second: bool) -> LinuxSources {
        let pick = |first: &[u8], next: &[u8]| if second { ok(next) } else { ok(first) };
        LinuxSources {
            stat: Some(pick(fx::PROC_STAT_TYPICAL, fx::PROC_STAT_NEXT_TICK)),
            meminfo: Some(ok(fx::MEMINFO_TYPICAL)),
            loadavg: Some(ok(fx::LOADAVG_TYPICAL)),
            uptime: Some(ok(fx::UPTIME_TYPICAL)),
            diskstats: Some(pick(fx::DISKSTATS_TYPICAL, fx::DISKSTATS_NEXT_TICK)),
            net_dev: Some(pick(fx::NET_DEV_TYPICAL, fx::NET_DEV_NEXT_TICK)),
            // The realistic kernel: no `full` line for CPU.
            pressure_cpu: Some(ok(fx::PRESSURE_CPU_WITHOUT_FULL)),
            pressure_memory: Some(ok(fx::PRESSURE_MEMORY)),
            pressure_io: Some(ok(fx::PRESSURE_IO_IDLE)),
            interfaces: Some(vec![
                InterfaceSources {
                    name: "eth0".into(),
                    operstate: ok(fx::OPERSTATE_UP),
                    speed: ok(fx::SPEED_1000),
                },
                InterfaceSources {
                    name: "lo".into(),
                    operstate: ok(fx::OPERSTATE_UNKNOWN),
                    // Loopback has no `speed` attribute at all.
                    speed: Err(ReadFailure::Failed),
                },
                InterfaceSources {
                    name: "wlan0".into(),
                    operstate: ok(fx::OPERSTATE_DOWN),
                    speed: ok(fx::SPEED_UNKNOWN_NEGATIVE),
                },
            ]),
            // A laptop: one energy-reporting ACPI battery, the charger beside it,
            // and a bluetooth mouse whose own cell must not be mistaken for it.
            power_supplies: Some(vec![
                power_supply("AC", ok(fx::POWER_TYPE_MAINS)),
                PowerSupplySources {
                    status: ok(fx::POWER_STATUS_DISCHARGING),
                    capacity: ok(fx::POWER_CAPACITY_82),
                    cycle_count: ok(fx::POWER_CYCLE_COUNT_214),
                    energy_full_design: ok(fx::POWER_ENERGY_FULL_DESIGN),
                    energy_full: ok(fx::POWER_ENERGY_FULL),
                    power_now: ok(fx::POWER_POWER_NOW),
                    temp: ok(fx::POWER_TEMP_314),
                    ..power_supply("BAT0", ok(fx::POWER_TYPE_BATTERY))
                },
                PowerSupplySources {
                    scope: ok(fx::POWER_SCOPE_DEVICE),
                    status: ok(fx::POWER_STATUS_DISCHARGING),
                    capacity: ok(b"55\n"),
                    ..power_supply("hid-e4-battery", ok(fx::POWER_TYPE_BATTERY))
                },
            ]),
            cgroup: CgroupSources {
                controllers: Some(ok(b"cpuset cpu io memory pids\n")),
                memory_max: Some(ok(fx::CGROUP_MEMORY_MAX_LIMITED)),
                memory_current: Some(ok(fx::CGROUP_MEMORY_CURRENT)),
                cpu_max: Some(ok(fx::CGROUP_CPU_MAX_LIMITED)),
            },
            environment: Some(EnvironmentSources {
                self_cgroup: ok(fx::CGROUP_V2_DOCKER),
                dmi_sys_vendor: ok(fx::DMI_SYS_VENDOR_QEMU),
            }),
            processes: vec![
                ProcessSources {
                    pid: 4_242,
                    stat: pick(fx::PID_STAT_SIMPLE, fx::PID_STAT_SIMPLE_NEXT_TICK),
                    status: ok(fx::PID_STATUS_TYPICAL),
                    io: pick(fx::PID_IO_TYPICAL, fx::PID_IO_NEXT_TICK),
                    cmdline: ok(fx::CMDLINE_TYPICAL),
                    cgroup: Some(ok(fx::CGROUP_V2_DOCKER)),
                },
                ProcessSources {
                    pid: 2,
                    stat: ok(fx::PID_STAT_KERNEL_THREAD),
                    status: ok(fx::PID_STATUS_KERNEL_THREAD),
                    // A kernel thread's `io` is root-only.
                    io: Err(ReadFailure::Denied),
                    cmdline: ok(fx::CMDLINE_EMPTY),
                    cgroup: None,
                },
                ProcessSources {
                    pid: 9_182,
                    stat: ok(fx::PID_STAT_WEIRD_NAME),
                    // This process exited between the `stat` read and the rest.
                    status: Err(ReadFailure::Missing),
                    io: Err(ReadFailure::Missing),
                    cmdline: Err(ReadFailure::Missing),
                    cgroup: None,
                },
            ],
            processes_truncated: false,
        }
    }

    /// A baseline process row, as the cross-platform collector would produce it.
    ///
    /// `start_key` is deliberately the whole-second start time, which is what makes
    /// the identity replacement visible in the tests.
    fn baseline_process(pid: u32, name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, 882_137),
            parent_pid: None,
            name: name.into(),
            command: "baseline command".into(),
            exe: None,
            user: MetricState::PermissionDenied,
            state: ProcessState::Unknown,
            cpu: MetricState::WarmingUp,
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    /// A baseline snapshot with the three processes the fixtures describe.
    fn baseline(captured_at: Instant) -> SystemSnapshot {
        let mut snapshot = SystemSnapshot::warming_up(captured_at, SystemTime::UNIX_EPOCH, 4);
        snapshot.processes = vec![
            baseline_process(4_242, "baseline-rustc"),
            baseline_process(2, "baseline-kthreadd"),
            baseline_process(9_182, "baseline-weird"),
        ];
        snapshot
    }

    /// Runs two ticks two seconds apart and returns the second snapshot.
    fn two_ticks() -> (LinuxEnrichment, SystemSnapshot) {
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();

        let first_tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &first_tick);

        let later = start + Duration::from_secs(2);
        let second_tick = first_tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        enrichment.apply(&mut second, &sources(true), &second_tick);
        (enrichment, second)
    }

    #[test]
    fn the_first_pass_measures_nothing_and_the_second_measures_everything() {
        // §8.2 and §26: the first sample of delta-based data is warming up, not zero.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &sources(false), &tick);

        assert!(snapshot.cpu.total.is_warming_up());
        assert!(snapshot.cpu.per_core.is_warming_up());
        for disk in &snapshot.disks {
            assert!(
                disk.read.fresh().is_none(),
                "{} reported a rate",
                disk.device
            );
            assert!(
                disk.busy.fresh().is_none(),
                "{} reported busy time",
                disk.device
            );
        }
        for nic in &snapshot.networks {
            assert!(nic.rx.fresh().is_none(), "{} reported a rate", nic.name);
        }
        for process in &snapshot.processes {
            assert!(process.cpu.fresh().is_none());
        }
        // Non-delta values are available immediately.
        assert!(snapshot.memory.available.fresh().is_some());
        assert!(snapshot.load.fresh().is_some());
        assert!(snapshot.pressure.psi.fresh().is_some());
    }

    #[test]
    fn cpu_comes_from_the_proc_stat_delta_with_a_full_breakdown() {
        let (_, snapshot) = two_ticks();
        let cpu = snapshot
            .cpu
            .total
            .fresh()
            .expect("measured on the second pass");
        // busy advanced 300 ticks against 710 total: 42.25%.
        assert!((cpu.busy.value() - 42.25).abs() < 0.1, "got {}", cpu.busy);
        let breakdown = cpu
            .breakdown
            .fresh()
            .expect("two readings give a breakdown");
        assert!(breakdown.user.value() > 0.0);
        assert!(breakdown.iowait.fresh().is_some(), "Linux reports iowait");
        assert!(breakdown.steal.fresh().is_some(), "and steal");
        assert_eq!(
            snapshot.capabilities.cpu_breakdown,
            CapabilityState::Available
        );

        let cores = snapshot.cpu.per_core.fresh().expect("four cores");
        assert_eq!(cores.len(), 4);
        assert_eq!(
            snapshot.capabilities.per_core_cpu,
            CapabilityState::Available
        );
    }

    #[test]
    fn memory_uses_memavailable_semantics_and_keeps_the_cgroup_limit_beside_the_host_total() {
        // §8.4 and §9.2 together: the definition changes and the limit is additional.
        let (enrichment, snapshot) = two_ticks();
        assert_eq!(
            snapshot.memory.semantics,
            MemorySemantics::LinuxMemAvailable
        );
        assert_eq!(snapshot.memory.total_bytes, 32_784_156 * 1024);
        assert_eq!(
            snapshot.memory.used.fresh(),
            Some(&((32_784_156 - 9_531_072) * 1024))
        );
        assert!(snapshot.memory.detail.cached.fresh().is_some());
        assert!(snapshot.memory.detail.buffers.fresh().is_some());
        assert!(
            snapshot.memory.detail.wired.is_unsupported(),
            "a macOS-only concept must stay unsupported on Linux"
        );

        assert_eq!(
            snapshot.memory.cgroup_limit_bytes.fresh(),
            Some(&(2 * 1024 * 1024 * 1024))
        );
        assert_eq!(
            snapshot.memory.effective_limit_bytes(),
            2 * 1024 * 1024 * 1024
        );
        assert!(
            snapshot.memory.total_bytes > snapshot.memory.effective_limit_bytes(),
            "both figures must remain observable"
        );
        assert_eq!(
            snapshot.capabilities.cgroup_limits,
            CapabilityState::Available
        );
        // The CPU quota travels on the snapshot beside the host count, not instead of
        // it: §9.2 wants both observable, and a view has to be able to say which of the
        // two is the ceiling that applies.
        let quota = enrichment
            .cgroup_cpu_limit()
            .fresh()
            .copied()
            .expect("cpu.max is configured");
        assert!((quota.cores() - 1.5).abs() < f32::EPSILON);
        assert_eq!(snapshot.cpu.cgroup_quota.fresh(), Some(&quota));
        assert_eq!(snapshot.cpu.logical_count, 4, "the host count is untouched");
        assert!((snapshot.cpu.effective_cores() - 1.5).abs() < f32::EPSILON);
        assert!(snapshot.cpu.is_cpu_limited());
        // The raw pair survives, because someone debugging a throttled container wants
        // to see the figures they configured rather than only the ratio.
        assert_eq!((quota.quota_us(), quota.period_us()), (150_000, 100_000));
        assert_eq!(
            enrichment.cgroup_memory_current().fresh(),
            Some(&1_503_238_553)
        );
        // And the group's own charge, which is the number `memory.max` is enforced
        // against — `used` above is the host's and would divide by the wrong total.
        assert_eq!(
            snapshot.memory.cgroup_used_bytes.fresh(),
            Some(&1_503_238_553)
        );
        assert_eq!(
            snapshot.memory.effective_used_bytes().fresh(),
            Some(&1_503_238_553)
        );
        assert_eq!(enrichment.cgroup_version(), Some(CgroupVersion::V2));
    }

    #[test]
    fn an_unlimited_cgroup_does_not_shrink_the_memory_ceiling() {
        // §9.2's `max` sentinel, end to end: it must not become a limit at all.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut sources = sources(false);
        sources.cgroup.memory_max = Some(ok(fx::CGROUP_MEMORY_MAX_UNLIMITED));
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &sources, &tick);

        assert!(snapshot.memory.cgroup_limit_bytes.is_unsupported());
        assert!(snapshot.memory.cgroup_limit_bytes.fresh().is_none());
        assert_eq!(
            snapshot.memory.effective_limit_bytes(),
            snapshot.memory.total_bytes,
            "unlimited must fall back to the host total, not to a tiny limit"
        );
        // The file was readable, so the capability is still available.
        assert_eq!(
            snapshot.capabilities.cgroup_limits,
            CapabilityState::Available
        );
    }

    /// One tick with `sources`, returning the enriched snapshot.
    fn one_tick(sources: &LinuxSources) -> SystemSnapshot {
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, sources, &tick);
        snapshot
    }

    #[test]
    fn the_system_battery_is_read_past_the_charger_and_past_the_mouse() {
        // All three are in `/sys/class/power_supply` on an ordinary laptop. Picking
        // the wrong one would put a bluetooth mouse's 55% on the Battery screen.
        let snapshot = one_tick(&sources(false));
        let battery = snapshot
            .sensors
            .battery
            .fresh()
            .copied()
            .expect("the fixture laptop has a battery");
        assert!((battery.charge.value() - 82.0).abs() < 0.01);
        assert_eq!(battery.state, monitrs_core::model::ChargeState::Discharging);
        assert_eq!(battery.cycle_count.fresh().copied(), Some(214));
        assert_eq!(battery.temperature_celsius.fresh().copied(), Some(31.4));
        assert_eq!(battery.power_watts.fresh().copied(), Some(12.4));
        assert_eq!(
            snapshot.capabilities.battery,
            CapabilityState::Available,
            "a battery that read must be declared available (§4)"
        );
    }

    #[test]
    fn a_desktop_reports_no_battery_rather_than_a_flat_one() {
        // The case every server, container and CI runner takes. §4 and §26: the
        // answer is "this machine has none", never 0%, and never a missing field.
        let mut without = sources(false);
        without.power_supplies = Some(Vec::new());
        let snapshot = one_tick(&without);

        assert!(snapshot.sensors.battery.is_unsupported());
        assert!(snapshot.sensors.battery.fresh().is_none());
        assert!(snapshot.sensors.battery.displayable().is_none());
        assert_eq!(snapshot.sensors.battery.placeholder(), Some("n/a"));
        assert_eq!(snapshot.capabilities.battery, CapabilityState::Unsupported);
    }

    #[test]
    fn a_machine_with_only_a_charger_still_reports_no_battery() {
        // A desktop with a UPS or a monitored PSU lists power supplies and has no
        // battery. An empty-list check alone would report the charger as one.
        let mut mains_only = sources(false);
        mains_only.power_supplies = Some(vec![power_supply("AC", ok(fx::POWER_TYPE_MAINS))]);
        assert!(one_tick(&mains_only).sensors.battery.is_unsupported());
    }

    #[test]
    fn a_tick_that_did_not_read_the_battery_keeps_the_previous_reading_and_marks_its_age() {
        // §9.1: the sensor group reads the battery on its own cadence and the baseline
        // blanks the field every tick, so the ticks in between must not lose it. §4:
        // what they publish was not measured on this tick, so it carries its age.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &tick);
        let measured = first
            .sensors
            .battery
            .fresh()
            .copied()
            .expect("the fixture laptop has a battery");

        // Five seconds on, the medium tier is due and the sensor group is not, so
        // `power_supplies` is `None` — not read rather than absent.
        //
        // `elapsed` is one second, not five: it is the measured interval since the
        // previous fast tick, and an age taken from it rather than from the read time
        // would be wrong by four seconds here (§8.1). Built by hand rather than with
        // `advance` precisely so the two numbers differ.
        let later = start + Duration::from_secs(5);
        let mut carried = sources(false);
        carried.power_supplies = None;
        let next = SampleTick {
            sequence: 1,
            captured_at: later,
            wall_time: SystemTime::UNIX_EPOCH,
            elapsed: Duration::from_secs(1),
            due: DueTiers::fast_and_medium(),
        };
        let mut second = baseline(later);
        enrichment.apply(&mut second, &carried, &next);

        let (value, age) = second
            .sensors
            .battery
            .displayable()
            .expect("the reading is kept rather than blanked");
        assert_eq!(
            value.charge, measured.charge,
            "the same reading, not a new one"
        );
        assert_eq!(
            age,
            Duration::from_secs(5),
            "the age is the real gap since the read, never the tick interval (§8.1)"
        );
        assert!(
            second.sensors.battery.fresh().is_none(),
            "a carried reading must not present itself as measured (§4)"
        );
        // Derived from the cached reading rather than from the stale-marked one: a
        // capability that flipped to unsupported on every carried tick would render
        // as "this machine cannot report battery" every few seconds (§4).
        assert_eq!(
            second.capabilities.battery,
            CapabilityState::Available,
            "the battery capability changed because a reading was carried"
        );
    }

    #[test]
    fn a_machine_with_no_battery_never_claims_a_stale_one() {
        // Every server, container and CI runner takes this path on every tick the
        // sensor group is not due. `Unsupported` with an age would claim a
        // measurement that never happened (§4, §26).
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut desktop = sources(false);
        desktop.power_supplies = Some(vec![power_supply("AC", ok(fx::POWER_TYPE_MAINS))]);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &desktop, &tick);
        assert!(first.sensors.battery.is_unsupported());

        let mut carried = sources(false);
        carried.power_supplies = None;
        let later = start + Duration::from_secs(30);
        let next = tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::fast_and_medium());
        let mut second = baseline(later);
        enrichment.apply(&mut second, &carried, &next);

        assert!(
            second.sensors.battery.is_unsupported(),
            "a machine with no battery must keep saying so"
        );
        assert!(second.sensors.battery.displayable().is_none());
        assert_eq!(second.sensors.battery.placeholder(), Some("n/a"));
        assert_eq!(second.capabilities.battery, CapabilityState::Unsupported);
    }

    #[test]
    fn a_battery_never_looked_at_is_warming_up_rather_than_declared_absent() {
        // Asserting "this machine has no battery" before reading
        // `/sys/class/power_supply` would be a fact with no evidence behind it.
        let mut unread = sources(false);
        unread.power_supplies = None;
        let snapshot = one_tick(&unread);
        assert!(snapshot.sensors.battery.is_warming_up());
        assert!(snapshot.sensors.battery.fresh().is_none());
        assert_eq!(snapshot.capabilities.battery, CapabilityState::Unknown);
    }

    #[test]
    fn a_kernel_without_memavailable_leaves_the_baseline_definition_alone() {
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut sources = sources(false);
        sources.meminfo = Some(ok(fx::MEMINFO_NO_MEMAVAILABLE));
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        let before = snapshot.memory;
        enrichment.apply(&mut snapshot, &sources, &tick);

        assert_eq!(
            snapshot.memory.semantics, before.semantics,
            "§8.4 forbids silently changing what `used` means"
        );
    }

    #[test]
    fn disk_busy_time_comes_from_field_ten_and_only_on_the_second_pass() {
        // §7.3: the one place a busy percentage is allowed.
        let (_, snapshot) = two_ticks();
        let nvme = snapshot
            .disks
            .iter()
            .find(|disk| &*disk.device == "nvme0n1")
            .expect("nvme0n1 is in the fixture");

        let busy = nvme
            .busy
            .fresh()
            .expect("field 10 present in both readings");
        assert!((busy.value() - 40.0).abs() < 0.01, "got {busy}");
        let read = nvme.read.fresh().expect("measured");
        assert!(
            (read.per_second() - 1_048_576.0).abs() < 1.0,
            "2 MiB over 2 s is 1 MiB/s, got {read}"
        );
        let read_ops = nvme.read_ops.fresh().expect("measured");
        assert!((read_ops.per_second() - 100.0).abs() < 0.01);
        let queue = nvme.queue_length.fresh().expect("field 11 present");
        assert!((queue - 0.45).abs() < 0.01, "got {queue}");
        assert_eq!(snapshot.capabilities.disk_busy, CapabilityState::Available);
        assert_eq!(snapshot.capabilities.disk_io, CapabilityState::Available);
        assert!(nvme.totals.fresh().is_some());
    }

    #[test]
    fn a_device_counter_reset_yields_a_typed_state_rather_than_a_huge_rate() {
        // §8.2 and §17.2's counter-reset fixture.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let first_tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &first_tick);

        let later = start + Duration::from_secs(2);
        let mut reset_sources = sources(true);
        reset_sources.diskstats = Some(ok(fx::DISKSTATS_AFTER_RESET));
        let second_tick = first_tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        enrichment.apply(&mut second, &reset_sources, &second_tick);

        let nvme = second
            .disks
            .iter()
            .find(|disk| &*disk.device == "nvme0n1")
            .expect("still present");
        assert_eq!(
            nvme.read,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
        assert!(
            nvme.busy.fresh().is_none(),
            "a counter that moved backwards has no busy percentage"
        );
        // A device that did not reset is unaffected.
        let sda = second
            .disks
            .iter()
            .find(|disk| &*disk.device == "sda")
            .expect("present");
        assert!(sda.read.is_available());
    }

    #[test]
    fn a_near_u64_max_device_counter_produces_no_absurd_rate() {
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let first_tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &first_tick);

        let later = start + Duration::from_secs(2);
        let mut huge = sources(true);
        huge.diskstats = Some(ok(fx::DISKSTATS_HUGE));
        let second_tick = first_tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        enrichment.apply(&mut second, &huge, &second_tick);

        let nvme = second
            .disks
            .iter()
            .find(|disk| &*disk.device == "nvme0n1")
            .expect("present");
        // The jump forward is real as far as the counter is concerned, but it must be
        // a finite number derived from the measured interval, never an overflow.
        if let Some(rate) = nvme.read.fresh() {
            assert!(rate.per_second().is_finite());
        }
    }

    #[test]
    fn network_drops_link_state_and_speed_all_arrive_from_the_native_layer() {
        let (_, snapshot) = two_ticks();
        let eth0 = snapshot
            .networks
            .iter()
            .find(|nic| &*nic.name == "eth0")
            .expect("eth0 is in the fixture");

        let rx = eth0.rx.fresh().expect("measured");
        assert!(
            (rx.per_second() - 18_400.0).abs() < 0.01,
            "36 800 bytes over 2 s, got {rx}"
        );
        let errors = eth0.errors.fresh().expect("counters read");
        assert_eq!(
            errors.rx_dropped, 9,
            "drops are what the baseline cannot see"
        );
        assert_eq!(eth0.state.fresh(), Some(&LinkState::Up));
        assert_eq!(eth0.link_speed_mbps.fresh(), Some(&1_000));
        // With a known speed a utilisation percentage is finally legitimate (§7.4).
        assert!(eth0.utilization().fresh().is_some());
        assert_eq!(
            snapshot.capabilities.network_link_speed,
            CapabilityState::Available
        );
        assert_eq!(
            snapshot.capabilities.network_errors,
            CapabilityState::Available
        );
    }

    #[test]
    fn an_interface_without_a_known_speed_renders_no_utilization() {
        // §7.4, the case that must never produce a number: loopback has no `speed`
        // attribute and wlan0 reports the `-1` sentinel.
        let (_, snapshot) = two_ticks();
        for name in ["lo", "wlan0"] {
            let nic = snapshot
                .networks
                .iter()
                .find(|nic| &*nic.name == name)
                .unwrap_or_else(|| panic!("{name} is in the fixture"));
            assert!(
                nic.link_speed_mbps.fresh().is_none(),
                "{name} claimed a link speed"
            );
            assert_eq!(
                nic.utilization(),
                MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
                "{name} rendered a utilisation percentage"
            );
        }
    }

    #[test]
    fn totals_since_launch_start_at_zero_even_though_the_os_counters_do_not() {
        // §7.4: the OS counter may have wrapped long before monitrs started.
        let (_, snapshot) = two_ticks();
        let eth0 = snapshot
            .networks
            .iter()
            .find(|nic| &*nic.name == "eth0")
            .expect("present");
        assert_eq!(eth0.since_launch.rx_bytes, 36_800);
        assert_eq!(
            eth0.os_totals.fresh().map(|totals| totals.rx_bytes),
            Some(8_123_493_589)
        );
    }

    #[test]
    fn an_interface_counter_reset_yields_a_typed_state_rather_than_a_huge_rate() {
        // The realistic cause is a driver reload, which zeroes the counters. §8.2
        // forbids reporting 8 GB of traffic that never happened.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let first_tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &first_tick);

        let later = start + Duration::from_secs(2);
        let mut reset = sources(true);
        reset.net_dev = Some(ok(fx::NET_DEV_AFTER_RESET));
        let second_tick = first_tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        enrichment.apply(&mut second, &reset, &second_tick);

        let eth0 = second
            .networks
            .iter()
            .find(|nic| &*nic.name == "eth0")
            .expect("present");
        assert_eq!(
            eth0.rx,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );
        assert!(eth0.rx.fresh().is_none());
        // The OS totals are still shown, because they are what the kernel says.
        assert_eq!(
            eth0.os_totals.fresh().map(|totals| totals.rx_bytes),
            Some(1_024)
        );
        // Totals since launch saturate to zero rather than underflowing.
        assert_eq!(eth0.since_launch.rx_bytes, 0);
    }

    #[test]
    fn an_interface_that_vanishes_does_not_leave_a_baseline_behind() {
        // §8.2 lists interface disappearance and rename as cases that must not
        // produce a delta across the gap.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let first_tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &first_tick);
        assert_eq!(enrichment.net_rx.len(), 4);

        let later = start + Duration::from_secs(2);
        let mut fewer = sources(true);
        fewer.net_dev = Some(ok(fx::NET_DEV_HEADER_ONLY));
        // A header-only file leaves the previous interfaces in place rather than
        // blanking the panel, but a file listing only some interfaces prunes the
        // rest.
        fewer.net_dev = Some(ok(b"Inter-|   Receive  |  Transmit\n face |x|y\n  eth0: 8123493589 9123556 12 9 0 0 0 40213 1234571890 4123556 3 2 0 0 0 0\n"));
        let second_tick = first_tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        enrichment.apply(&mut second, &fewer, &second_tick);

        assert_eq!(second.networks.len(), 1);
        assert_eq!(
            enrichment.net_rx.len(),
            1,
            "baselines for absent interfaces must be dropped"
        );
    }

    #[test]
    fn psi_arrives_and_a_missing_full_line_stays_unsupported() {
        // §9.2: many kernels omit `full` for CPU. Reporting 0% would be a fabricated
        // all-clear.
        let (_, snapshot) = two_ticks();
        let psi = snapshot
            .pressure
            .psi
            .fresh()
            .expect("all three resources read");
        assert!((psi.cpu.some_avg10.value() - 12.34).abs() < 0.01);
        assert!(psi.cpu.full_avg10.is_unsupported());
        assert!((psi.memory.some_avg10.value() - 41.20).abs() < 0.01);
        assert!(psi.memory.full_avg10.fresh().is_some());
        assert_eq!(psi.io.total_stalled, Duration::ZERO);
        assert_eq!(snapshot.capabilities.linux_psi, CapabilityState::Available);
        // Deriving a pressure *state* is the diagnostic engine's job, not a
        // collector's, so the radar's signals are untouched.
        assert!(snapshot.pressure.worst_state().fresh().is_none());
    }

    #[test]
    fn a_kernel_without_psi_reports_unsupported_rather_than_no_pressure() {
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut without = sources(false);
        without.pressure_cpu = Some(Err(ReadFailure::Missing));
        without.pressure_memory = Some(Err(ReadFailure::Missing));
        without.pressure_io = Some(Err(ReadFailure::Missing));
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &without, &tick);

        assert!(snapshot.pressure.psi.is_unsupported());
        assert_eq!(
            snapshot.capabilities.linux_psi,
            CapabilityState::Unsupported
        );
        assert!(
            enrichment.diagnostics().is_empty(),
            "an absent kernel feature is not a collector issue"
        );
    }

    #[test]
    fn a_process_gains_a_clock_tick_start_key_that_the_baseline_could_not_provide() {
        // §9.2 and §26: the baseline's whole-second start time cannot distinguish a
        // PID reused inside one second. This is the fix, observed end to end.
        let (_, snapshot) = two_ticks();
        let rustc = snapshot
            .process_by_pid(4_242)
            .expect("the baseline row was enriched");
        assert_eq!(rustc.identity.start_key, 88_213_700);
        assert_ne!(
            rustc.identity.start_key, 882_137,
            "the whole-second key must have been replaced"
        );
        assert_eq!(rustc.parent_pid, Some(1_221));
        assert_eq!(rustc.state, ProcessState::Running);
        assert_eq!(&*rustc.name, "rustc");
        assert_eq!(&*rustc.command, "cargo build --release");
        assert_eq!(rustc.threads.fresh(), Some(&9));
        assert_eq!(rustc.memory.rss_bytes.fresh(), Some(&(625_664 * 1024)));
        assert!(rustc.memory.share_of_total.fresh().is_some());
        assert_eq!(rustc.user.fresh().map(|user| user.uid), Some(1_000));
        assert!(rustc.age.fresh().is_some());
        assert!(rustc.started_at.fresh().is_some());
    }

    #[test]
    fn process_cpu_and_io_rates_use_the_measured_interval() {
        let (_, snapshot) = two_ticks();
        let rustc = snapshot.process_by_pid(4_242).expect("enriched");
        // 250 clock ticks of CPU (2.5 s) inside a 2 s interval is 125% of one core,
        // which §8.3 permits and requires.
        let cpu = rustc.cpu.fresh().expect("measured");
        assert!((cpu.value() - 125.0).abs() < 0.1, "got {cpu}");
        assert!(cpu.value() > 100.0);

        let read = rustc.io.read.fresh().expect("measured");
        assert!(
            (read.per_second() - 1_048_576.0).abs() < 1.0,
            "2 MiB over 2 s, got {read}"
        );
        assert_eq!(rustc.io.read_total_bytes.fresh(), Some(&43_327_488));
        assert_eq!(
            snapshot.capabilities.per_process_io,
            CapabilityState::Available
        );
    }

    #[test]
    fn an_unreadable_process_io_is_permission_denied_and_never_zero() {
        // §9.2's `EACCES` case, reaching the snapshot.
        let (_, snapshot) = two_ticks();
        let kthreadd = snapshot.process_by_pid(2).expect("enriched");
        assert_eq!(kthreadd.io.read, MetricState::PermissionDenied);
        assert_eq!(kthreadd.io.read_total_bytes, MetricState::PermissionDenied);
        assert!(kthreadd.io.read.fresh().is_none());
        assert_eq!(kthreadd.io.read.placeholder(), Some("permission denied"));
    }

    #[test]
    fn when_every_process_io_is_refused_the_capability_says_privileges_would_help() {
        // §4 wants the privilege hint said once rather than once per row, and §9.2
        // requires the denial to be a metric state rather than a fatal error.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut denied = sources(false);
        for process in &mut denied.processes {
            process.io = Err(ReadFailure::Denied);
        }
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &denied, &tick);

        assert_eq!(
            snapshot.capabilities.per_process_io,
            CapabilityState::PermissionDenied
        );
        assert!(snapshot.capabilities.any_permission_denied());
        for process in &snapshot.processes {
            assert_eq!(process.io.read, MetricState::PermissionDenied);
        }
        assert!(
            enrichment.diagnostics().is_empty(),
            "a routine denial is not a collector issue"
        );
    }

    #[test]
    fn a_kernel_thread_is_flagged_from_its_task_flags() {
        let (_, snapshot) = two_ticks();
        let kthreadd = snapshot.process_by_pid(2).expect("enriched");
        assert!(kthreadd.is_kernel_thread);
        assert_eq!(&*kthreadd.name, "kthreadd");
        assert!(
            kthreadd.memory.rss_bytes.is_unsupported(),
            "a kernel thread has no user address space to measure"
        );
        assert_eq!(
            snapshot.capabilities.kernel_threads,
            CapabilityState::Available
        );

        let rustc = snapshot.process_by_pid(4_242).expect("enriched");
        assert!(!rustc.is_kernel_thread);
    }

    #[test]
    fn a_process_that_vanishes_mid_read_produces_no_diagnostic_line() {
        // §9.2: no log line per vanished process, and §14.1: it is not an error.
        // PID 9182's `status`, `io`, and `cmdline` all failed with ENOENT.
        let (enrichment, snapshot) = two_ticks();
        assert!(
            enrichment.diagnostics().is_empty(),
            "nothing may be reported for a vanished process"
        );
        assert!(enrichment.diagnostics().suppressed() > 0);
        assert!(snapshot.health.issues.is_empty());

        // What it did read is still used, including the awkward name.
        let weird = snapshot
            .process_by_pid(9_182)
            .expect("enriched from stat alone");
        assert_eq!(&*weird.name, "((weird) name) with spaces");
        assert_eq!(weird.identity.start_key, 88_100_000);
        assert!(weird.io.read.fresh().is_none());
    }

    #[test]
    fn a_process_the_baseline_never_saw_is_not_added_mid_snapshot() {
        // §10.4: one snapshot describes one moment. A row assembled from files read
        // after the baseline enumerated would describe a different one.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut extra = sources(false);
        extra.processes.push(ProcessSources {
            pid: 7_331,
            stat: ok(fx::PID_STAT_ZOMBIE),
            status: Err(ReadFailure::Missing),
            io: Err(ReadFailure::Missing),
            cmdline: Err(ReadFailure::Missing),
            cgroup: None,
        });
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &extra, &tick);

        assert_eq!(snapshot.processes.len(), 3);
        assert!(snapshot.process_by_pid(7_331).is_none());
    }

    #[test]
    fn process_rate_trackers_follow_live_identities_only() {
        // §10.3: PID churn must not grow the trackers without bound.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &sources(false), &tick);
        assert_eq!(enrichment.process_cpu.len(), 3);

        // The next pass sees only one process.
        let later = start + Duration::from_secs(2);
        let mut fewer = sources(true);
        fewer.processes.truncate(1);
        let second_tick = tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        enrichment.apply(&mut second, &fewer, &second_tick);
        assert_eq!(enrichment.process_cpu.len(), 1);
        assert_eq!(enrichment.process_read.len(), 1);
    }

    #[test]
    fn the_environment_is_a_labelled_heuristic_with_evidence_and_confidence() {
        // §7.5: clearly labelled heuristic, and there is no bare-metal conclusion.
        let (_, snapshot) = two_ticks();
        let environment = snapshot
            .host
            .environment
            .fresh()
            .expect("the slow tier ran");
        assert_eq!(environment.kind, EnvironmentKind::Container);
        assert!(environment.evidence.contains("docker"));
        assert!(
            environment.evidence.contains("QEMU/KVM"),
            "the VM evidence must not be hidden: {}",
            environment.evidence
        );
        assert_eq!(environment.confidence, Confidence::High);
    }

    #[test]
    fn a_tier_that_is_not_due_leaves_its_fields_exactly_as_they_were() {
        // §9.1: never an all-fields refresh. `None` means "keep what you have".
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut first = baseline(start);
        enrichment.apply(&mut first, &sources(false), &tick);

        let later = start + Duration::from_secs(2);
        let fast_only = LinuxSources {
            stat: Some(ok(fx::PROC_STAT_NEXT_TICK)),
            meminfo: Some(ok(fx::MEMINFO_TYPICAL)),
            ..LinuxSources::default()
        };
        let second_tick = tick.advance(later, SystemTime::UNIX_EPOCH, DueTiers::ALL);
        let mut second = baseline(later);
        second.load = MetricState::Available(monitrs_core::model::LoadSnapshot {
            one: 1.0,
            five: 1.0,
            fifteen: 1.0,
        });
        enrichment.apply(&mut second, &fast_only, &second_tick);

        // CPU was refreshed...
        assert!(second.cpu.total.fresh().is_some());
        // ...load was not read, so the value already on the snapshot survives...
        assert!((second.load.fresh().expect("kept").one - 1.0).abs() < f32::EPSILON);
        // ...the disks and networks were not read, so the baseline's lists stand...
        assert!(second.disks.is_empty());
        // ...and the cached uptime is still shown rather than blanked.
        assert!(second.host.uptime.fresh().is_some());
        assert!(second.host.environment.fresh().is_some());
    }

    #[test]
    fn a_malformed_system_file_is_reported_once_and_leaves_a_typed_state() {
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut broken = sources(false);
        broken.stat = Some(ok(fx::PROC_STAT_TRUNCATED));
        broken.diskstats = Some(Err(ReadFailure::Failed));
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &broken, &tick);

        assert!(snapshot.cpu.total.fresh().is_none());
        assert_eq!(
            snapshot.cpu.total,
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed)
        );
        assert_eq!(enrichment.diagnostics().len(), 2);
        assert_eq!(snapshot.health.issues.len(), 2);
    }

    #[test]
    fn a_truncated_process_list_is_surfaced_rather_than_hidden() {
        // §16.2: shedding work is acceptable; pretending the machine is smaller is
        // not.
        let start = Instant::now();
        let mut enrichment = LinuxEnrichment::new();
        let mut capped = sources(false);
        capped.processes_truncated = true;
        let tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshot = baseline(start);
        enrichment.apply(&mut snapshot, &capped, &tick);

        assert_eq!(enrichment.diagnostics().len(), 1);
        assert!(
            snapshot
                .health
                .issues
                .iter()
                .any(|issue| issue.source.contains("enumeration"))
        );
    }

    #[test]
    fn a_zero_clock_rate_falls_back_instead_of_zeroing_every_cpu_time() {
        let enrichment = LinuxEnrichment::with_clock_ticks(0);
        assert_eq!(enrichment.ticks_per_second(), DEFAULT_USER_HZ);
        assert_eq!(
            LinuxEnrichment::with_clock_ticks(1_000).ticks_per_second(),
            1_000
        );
    }

    #[test]
    fn every_declared_source_has_a_tier_and_the_expensive_ones_are_not_fast() {
        // §8.6: cgroup metadata is slow-tier and static device state is medium.
        assert_eq!(tier_of_source(LinuxSource::Stat), Tier::Fast);
        assert_eq!(tier_of_source(LinuxSource::ProcessFiles), Tier::Fast);
        assert_eq!(tier_of_source(LinuxSource::LoadAvg), Tier::Medium);
        assert_eq!(
            tier_of_source(LinuxSource::InterfaceAttributes),
            Tier::Medium
        );
        assert_eq!(tier_of_source(LinuxSource::CgroupLimits), Tier::Slow);
        assert_eq!(tier_of_source(LinuxSource::Environment), Tier::Slow);
    }
}

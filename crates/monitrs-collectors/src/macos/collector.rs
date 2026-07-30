//! The macOS collector: the `sysinfo` baseline plus native enrichment.
//!
//! # Composition, not replacement
//!
//! [`MacosCollector`] owns a [`CommonCollector`] and samples it first, then
//! overwrites the fields it can measure better. That direction matters: the
//! baseline knows how to enumerate disks and mounts, join command lines, and read
//! interface counters, and reimplementing any of that natively would be work with
//! no payoff. What the baseline *cannot* do is see a process it is not allowed to
//! read, distinguish wired from compressed memory, or tell a permission failure
//! from a zero — and those are exactly the fields this collector replaces.
//!
//! # Enrichment upgrades, it does not downgrade
//!
//! Every merge here follows one rule: a native read that succeeded wins, and a
//! native read that failed leaves the baseline's value alone — with one deliberate
//! exception. When the native read failed with `EPERM` on a per-process counter,
//! the baseline's value for that field is not a measurement at all: the frozen
//! baseline substitutes a zeroed structure when the same call is refused. In that
//! one case the permission failure wins, because §26's "unavailable is not zero"
//! outranks "keep whatever was there".
//!
//! # Tiers
//!
//! Fast: CPU ticks, memory statistics, the process table, per-process counters.
//! Medium: battery. Slow: machine facts, interface link state and speed. Nothing
//! expensive is read more often than §8.6 asks for.

use core::time::Duration;
use std::time::SystemTime;

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    CapabilityState, MetricState, ProcessDetailResult, ProcessIdentity, Tier,
};
use monitrs_core::rates::{CounterTracker, CounterWidth};

use crate::common::CommonCollector;
use crate::error::CollectorError;
use crate::source::{SampleTick, SnapshotSource};

use super::cpu::{self, CpuTracker};
use super::memory::{self, SwapActivity};
use super::network;
use super::power;
use super::process::{self, KernelProcess, ProcessEnricher, Timebase};
use super::sysctl;

/// Static facts about the machine, read once from `sysctl`.
///
/// All of these come from MIBs that cannot change while the process runs, so they
/// are read at construction and refreshed only on the slow tier — where "refresh"
/// means re-reading the boot time, which is the only one worth confirming.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MachineFacts {
    /// Logical CPU count from `hw.logicalcpu`.
    pub logical_cpus: Option<u16>,
    /// Physical core count from `hw.physicalcpu`.
    ///
    /// On an Apple Silicon machine with performance and efficiency cores this is
    /// their sum, and it equals the logical count because there is no SMT.
    pub physical_cpus: Option<u16>,
    /// Total physical memory from `hw.memsize`.
    pub memory_bytes: Option<u64>,
    /// Hardware model identifier from `hw.model`, e.g. `Mac16,8`.
    pub model: Option<Box<str>>,
    /// CPU brand string from `machdep.cpu.brand_string`, e.g. `Apple M4 Pro`.
    pub cpu_brand: Option<Box<str>>,
    /// Boot time from `kern.boottime`, which carries microseconds.
    pub boot_time: Option<SystemTime>,
    /// Page size the memory statistics are counted in.
    pub page_size: Option<u64>,
    /// Statistics-clock frequency the CPU tick counters are counted in.
    pub tick_rate: Option<u32>,
}

impl MachineFacts {
    /// Reads every static fact, tolerating any individual absence.
    ///
    /// Nothing here is fatal: a missing MIB means one field of the Inspect screen
    /// says "n/a", not that the machine cannot be monitored.
    #[must_use]
    pub fn query() -> Self {
        Self {
            logical_cpus: sysctl::scalar_by_name::<i32>(c"hw.logicalcpu")
                .ok()
                .and_then(|count| u16::try_from(count).ok()),
            physical_cpus: sysctl::scalar_by_name::<i32>(c"hw.physicalcpu")
                .ok()
                .and_then(|count| u16::try_from(count).ok()),
            memory_bytes: memory::read_memory_size().ok(),
            model: sysctl::string_by_name(c"hw.model").ok(),
            // Present on Intel and on Apple Silicon; `hw.model` is the fallback the
            // host snapshot uses when it is not, since the frozen model has one
            // field for "what CPU is this" and none for "what Mac is this".
            cpu_brand: sysctl::string_by_name(c"machdep.cpu.brand_string").ok(),
            boot_time: read_boot_time(),
            page_size: memory::read_page_size().ok(),
            tick_rate: cpu::ticks_per_second(),
        }
    }
}

/// Reads `kern.boottime`, which is a `timeval` and so has microsecond resolution.
fn read_boot_time() -> Option<SystemTime> {
    let mut mib = [libc::CTL_KERN, libc::KERN_BOOTTIME];
    let boot = sysctl::scalar::<libc::timeval>(&mut mib).ok()?;
    let seconds = u64::try_from(boot.tv_sec).ok()?;
    let micros = u64::try_from(boot.tv_usec).unwrap_or(0);
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(seconds) + Duration::from_micros(micros))
}

/// The `sysinfo` baseline with native macOS enrichment on top.
#[derive(Debug)]
pub struct MacosCollector {
    baseline: CommonCollector,
    facts: MachineFacts,
    timebase: Timebase,
    cpu: CpuTracker,
    processes: ProcessEnricher,
    /// Reused buffer for `kern.proc.all`, so the fast tier allocates nothing.
    process_buffer: Vec<u8>,
    /// The kernel's process table from the most recent fast tick, kept so the
    /// on-demand detail path can resolve ancestry without re-reading it.
    last_table: Vec<KernelProcess>,
    swap_in: CounterTracker,
    swap_out: CounterTracker,
    capabilities: monitrs_core::model::CapabilitySnapshot,
    /// Battery, refreshed on the medium tier (§8.6).
    cached_battery: MetricState<monitrs_core::model::BatterySnapshot>,
    /// Interface link state and speed, refreshed on the slow tier (§8.6).
    cached_links: std::collections::HashMap<Box<str>, network::InterfaceLink>,
}

impl MacosCollector {
    /// Builds the collector, reading the machine's static facts once.
    ///
    /// Fails only if the baseline fails: every native read here is optional, and a
    /// machine where all of them fail is still monitorable at baseline fidelity.
    pub fn new() -> Result<Self, CollectorError> {
        let baseline = CommonCollector::new()?;
        let facts = MachineFacts::query();
        let timebase = Timebase::query();
        let cpu = CpuTracker::new();
        let mut capabilities = baseline.capabilities();

        // Declared from what was actually resolved, not from what macOS usually
        // provides (§4).
        capabilities.cpu_breakdown = if cpu.resolved_tick_rate().is_some() {
            CapabilityState::Available
        } else {
            CapabilityState::Unsupported
        };
        capabilities.per_core_cpu = capabilities.cpu_breakdown;
        capabilities.per_process_threads = CapabilityState::Available;
        capabilities.per_process_open_files = CapabilityState::Available;
        capabilities.process_signals = CapabilityState::Available;
        // Reading a socket count means one `proc_pidfdinfo` per descriptor, which
        // §16.1's budget does not stretch to; absent beats sometimes-slow.
        capabilities.per_process_sockets = CapabilityState::Unsupported;
        // §7.3 forbids approximating device busy time, and there is no documented
        // macOS API for it — IOKit's is private.
        capabilities.disk_busy = CapabilityState::Unsupported;
        capabilities.linux_psi = CapabilityState::Unsupported;
        capabilities.cgroup_limits = CapabilityState::Unsupported;
        // macOS has no per-process kernel threads to hide (§7.2).
        capabilities.kernel_threads = CapabilityState::Unsupported;
        // `setpriority(2)` plus the identity revalidation §15.1 requires, both of
        // which this build has: see `crate::renice`. Available does not promise that
        // every value will be accepted — lowering a nice value needs privileges
        // monitrs never acquires — because that is a per-attempt question, answered
        // by `renice::dry_run` and by an `EPERM` outcome, not by the capability.
        capabilities.renice = crate::renice::capability_state();

        Ok(Self {
            baseline,
            facts,
            timebase,
            cpu,
            processes: ProcessEnricher::new(timebase),
            process_buffer: Vec::new(),
            last_table: Vec::new(),
            // Swap page counters are 64-bit and only reset on reboot.
            swap_in: CounterTracker::new(CounterWidth::Bits64),
            swap_out: CounterTracker::new(CounterWidth::Bits64),
            capabilities,
            cached_battery: MetricState::WarmingUp,
            cached_links: std::collections::HashMap::new(),
        })
    }

    /// The static facts this collector read at startup.
    #[must_use]
    pub const fn machine(&self) -> &MachineFacts {
        &self.facts
    }

    /// The mach timebase this collector converts process CPU time through.
    ///
    /// Exposed because absolute time units are meaningless without it, so a caller
    /// holding a raw `mach_absolute_time` value needs the same ratio.
    #[must_use]
    pub const fn timebase(&self) -> Timebase {
        self.timebase
    }

    /// The kernel's process table from the most recent sample.
    ///
    /// Exposed so a caller that already has a snapshot can resolve ancestry without
    /// a second enumeration.
    #[must_use]
    pub fn kernel_table(&self) -> &[KernelProcess] {
        &self.last_table
    }

    /// Refreshes the slow-tier native data.
    fn refresh_slow(&mut self) {
        self.facts = MachineFacts::query();
        if let Ok(links) = network::read_interface_links() {
            self.cached_links = links;
        }
        self.capabilities.network_link_speed = if self
            .cached_links
            .values()
            .any(|link| link.speed_mbps.is_some())
        {
            CapabilityState::Available
        } else {
            // No interface reports a rate, so §7.4's rule means no utilization
            // anywhere. Saying so is what suppresses the column.
            CapabilityState::Unsupported
        };
    }

    /// Refreshes the medium-tier native data.
    fn refresh_medium(&mut self) {
        self.cached_battery = power::read_battery();
        self.capabilities.battery = match self.cached_battery {
            MetricState::Available(_) => CapabilityState::Available,
            MetricState::PermissionDenied => CapabilityState::PermissionDenied,
            _ => CapabilityState::Unsupported,
        };
    }

    /// Overwrites the baseline's memory snapshot with the `host_statistics64` one.
    fn enrich_memory(&mut self, snapshot: &mut SystemSnapshot, tick: &SampleTick) {
        let (Some(total), Some(page_size)) = (self.facts.memory_bytes, self.facts.page_size) else {
            return;
        };
        let Ok(statistics) = memory::read_vm_statistics() else {
            return;
        };

        let activity = match (statistics.swapins, statistics.swapouts) {
            (Some(swapins), Some(swapouts)) => {
                self.capabilities.swap_activity = CapabilityState::Available;
                SwapActivity {
                    in_rate: self.swap_in.rate(swapins, tick.captured_at),
                    out_rate: self.swap_out.rate(swapouts, tick.captured_at),
                }
            }
            _ => {
                self.capabilities.swap_activity = CapabilityState::Unsupported;
                SwapActivity::UNSUPPORTED
            }
        };

        if let Some(memory) = memory::memory_snapshot(
            total,
            page_size,
            &statistics,
            memory::read_swap_usage().ok(),
            activity,
        ) {
            snapshot.memory = memory;
        }
    }

    /// Overwrites the baseline's process table with the kernel's enumeration.
    fn enrich_processes(&mut self, snapshot: &mut SystemSnapshot, tick: &SampleTick) {
        let Ok(table) = process::enumerate(&mut self.process_buffer) else {
            return;
        };
        let baseline = core::mem::take(&mut snapshot.processes);
        snapshot.processes = self.processes.enrich(
            baseline,
            &table,
            tick.captured_at,
            tick.wall_time,
            snapshot.memory.total_bytes,
            tick.can_compute_rates(),
        );
        self.last_table = table;

        self.capabilities.per_process_io = if self.processes.counters_readable() {
            CapabilityState::Available
        } else if self.processes.counters_denied() {
            CapabilityState::PermissionDenied
        } else {
            CapabilityState::Unknown
        };
    }

    /// Fills in the host fields sysctl knows better than the baseline.
    fn enrich_host(&self, snapshot: &mut SystemSnapshot, tick: &SampleTick) {
        if let Some(boot_time) = self.facts.boot_time {
            snapshot.host.boot_time = MetricState::Available(boot_time);
            snapshot.host.uptime = tick
                .wall_time
                .duration_since(boot_time)
                .map_or(MetricState::WarmingUp, MetricState::Available);
        }
        if snapshot.host.cpu_brand.fresh().is_none() {
            // `machdep.cpu.brand_string` first, then the hardware model: the frozen
            // host snapshot has one field for the processor and none for the
            // machine, so `hw.model` is the honest fallback rather than a discard.
            if let Some(brand) = self
                .facts
                .cpu_brand
                .clone()
                .or_else(|| self.facts.model.clone())
            {
                snapshot.host.cpu_brand = MetricState::Available(brand);
            }
        }
        // macOS is neither a container host nor, from inside, detectable as a VM
        // through any documented interface. Claiming "no evidence found" would be a
        // conclusion; the baseline's `Unsupported` is the honest state.
    }

    /// Applies the CPU counters and the CPU counts.
    fn enrich_cpu(&mut self, snapshot: &mut SystemSnapshot, tick: &SampleTick) {
        if let Some(physical) = self.facts.physical_cpus {
            snapshot.cpu.physical_count = MetricState::Available(physical);
        }
        let Ok(ticks) = cpu::read_processor_ticks() else {
            return;
        };
        if let Some(observed) = self.cpu.observe(&ticks, tick.captured_at) {
            let baseline = snapshot.cpu.clone();
            snapshot.cpu = observed.merge_into(baseline);
        }
    }
}

impl SnapshotSource for MacosCollector {
    fn name(&self) -> &'static str {
        "macos-native"
    }

    fn capabilities(&self) -> monitrs_core::model::CapabilitySnapshot {
        self.capabilities
    }

    fn sample(&mut self, tick: &SampleTick) -> Result<SystemSnapshot, CollectorError> {
        let mut snapshot = self.baseline.sample(tick)?;

        if tick.due.contains(Tier::Slow) {
            self.refresh_slow();
        }
        if tick.due.contains(Tier::Medium) {
            self.refresh_medium();
        }

        // Deliberately not gated on the fast tier. The tiers are scheduled
        // independently, so a tick can carry Medium or Slow without Fast — and on
        // such a tick the baseline republishes its previous CPU, memory, and
        // process values. Re-reading here costs a few milliseconds once every few
        // seconds and is what keeps `MemorySemantics` and the identity scheme from
        // flipping between consecutive frames, which §10.4 forbids.
        self.enrich_host(&mut snapshot, tick);
        self.enrich_cpu(&mut snapshot, tick);
        // Memory before processes: a process's share of total memory is computed
        // against the total this collector publishes, not the baseline's.
        self.enrich_memory(&mut snapshot, tick);
        self.enrich_processes(&mut snapshot, tick);
        network::merge_into(&mut snapshot.networks, &self.cached_links);
        snapshot.sensors.battery = self.cached_battery;
        // macOS has no PSI, and the baseline leaves the field `WarmingUp` because on
        // Linux it is filled in by the enrichment. Left alone, the radar's three PSI
        // rows would read `warming` for the entire session — a promise that the value
        // is coming, for a value this kernel has no concept of. §4 has a state for
        // exactly this, and it is the one the capability flag already declares.
        snapshot.pressure.psi = MetricState::Unsupported;
        snapshot.capabilities = self.capabilities;
        Ok(snapshot)
    }

    fn process_detail(&mut self, identity: ProcessIdentity) -> ProcessDetailResult {
        // Revalidate before doing any expensive read: an exited process is the
        // normal case, not an error (§14.1).
        let current = match process::read_one(identity.pid) {
            Ok(Some(current)) => current,
            // `kern.proc.pid` answers for every process including another user's, so
            // the only reasons it comes back empty or failing are that the process
            // is gone or that the query itself was malformed. Both are reported as
            // vanished: attributing a working directory to a process whose identity
            // could not be confirmed is the worse of the two failures (§15.1).
            Ok(None) | Err(_) => return ProcessDetailResult::Vanished(identity),
        };
        if current.identity != identity {
            return ProcessDetailResult::Reused {
                requested: identity,
                found: current.identity,
            };
        }

        let mut detail = process::detail_for(identity.pid, SystemTime::now(), identity);
        // The tree fields need the whole table, and re-reading it here keeps the
        // ancestry consistent with the moment of the detail read rather than with
        // the last fast tick.
        if let Ok(table) = process::enumerate(&mut self.process_buffer) {
            self.last_table = table;
        }
        let table = &self.last_table;
        detail.ancestry = MetricState::Available(process::ancestry(identity.pid, table));
        detail.children = MetricState::Available(process::children(identity.pid, table));
        detail.descendants = MetricState::Available(process::descendants(identity.pid, table));
        ProcessDetailResult::Loaded(Box::new(detail))
    }
}

/// Whether every process row carries a state rather than a fabricated zero.
///
/// Used by the smoke tests below and by the same invariant check in the module's
/// own tests; a row whose CPU is `Available(0)` while its memory is
/// `PermissionDenied` would mean the two came from different evidence.
#[cfg(test)]
fn row_is_self_consistent(process: &monitrs_core::model::ProcessSnapshot) -> bool {
    let cpu_denied = process.cpu == MetricState::PermissionDenied;
    let memory_denied = process.memory.rss_bytes == MetricState::PermissionDenied;
    cpu_denied == memory_denied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::{DueTiers, TierIntervals, TierScheduler};
    use monitrs_core::model::MemorySemantics;
    use std::time::Instant;

    /// Samples twice with a real interval between, which is what any delta needs.
    fn sample_twice(collector: &mut MacosCollector) -> (SystemSnapshot, SystemSnapshot) {
        let tick = SampleTick::first(Instant::now(), SystemTime::now());
        let first = collector.sample(&tick).expect("first sample");
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL * 2);
        let tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        let second = collector.sample(&tick).expect("second sample");
        (first, second)
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn the_machine_facts_describe_this_mac() {
        let facts = MachineFacts::query();
        assert!(
            facts.logical_cpus.is_some_and(|count| count >= 1),
            "hw.logicalcpu must answer"
        );
        assert!(facts.memory_bytes.is_some_and(|bytes| bytes > 0));
        assert!(facts.model.is_some_and(|model| model.starts_with("Mac")
            || model.starts_with("iMac")
            || model.starts_with("VirtualMac")));
        assert!(facts.page_size.is_some_and(|size| size.is_power_of_two()));
        assert!(facts.tick_rate.is_some_and(|hz| hz > 0));
        let boot = facts.boot_time.expect("kern.boottime must answer");
        assert!(
            boot < SystemTime::now(),
            "the machine cannot have booted in the future"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn the_collector_declares_macos_memory_semantics_and_fills_the_native_fields() {
        let mut collector = MacosCollector::new().expect("the collector must construct");
        let (_, second) = sample_twice(&mut collector);

        assert_eq!(second.memory.semantics, MemorySemantics::MacosVmStatistics);
        // The four fields the baseline leaves unsupported and §8.4 demands.
        assert!(second.memory.detail.wired.fresh().is_some(), "wired");
        assert!(
            second.memory.detail.compressed.fresh().is_some(),
            "compressed"
        );
        assert!(second.memory.detail.active.fresh().is_some(), "active");
        assert!(second.memory.detail.inactive.fresh().is_some(), "inactive");
        // And the ones that belong to Linux stay absent.
        assert!(second.memory.detail.buffers.is_unsupported());
        assert!(second.memory.detail.dirty.is_unsupported());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn cpu_warms_up_then_reports_a_per_core_row_with_a_breakdown() {
        let mut collector = MacosCollector::new().expect("constructs");
        let (first, second) = sample_twice(&mut collector);

        assert!(
            first.cpu.total.fresh().is_none(),
            "the first sample must not report a number"
        );
        let total = second.cpu.total.fresh().expect("machine CPU");
        assert!((0.0..=100.0).contains(&total.busy.value()));

        let cores = second.cpu.per_core.fresh().expect("per-core row");
        assert_eq!(
            cores.len(),
            usize::from(second.cpu.logical_count),
            "one entry per logical CPU"
        );
        for core in cores {
            assert!(core.breakdown.fresh().is_some(), "macOS reports a split");
        }
        assert_eq!(
            second.capabilities.cpu_breakdown,
            CapabilityState::Available
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn every_process_has_a_microsecond_start_key_including_root_owned_ones() {
        let mut collector = MacosCollector::new().expect("constructs");
        let (_, second) = sample_twice(&mut collector);

        assert!(second.processes.len() > 50, "a Mac runs many processes");
        for process in &second.processes {
            assert!(
                process.identity.start_key > 1_000_000_000_000_000,
                "{} has a start key of {}, which cannot detect PID reuse",
                process.name,
                process.identity.start_key
            );
            assert!(
                !process.name.is_empty(),
                "pid {} has no name",
                process.identity.pid
            );
        }

        // The processes the baseline cannot read at all must still be here.
        let root_owned = second
            .processes
            .iter()
            .filter(|process| process.user.fresh().is_some_and(|user| user.uid == 0))
            .count();
        assert!(root_owned > 5, "saw only {root_owned} root-owned processes");
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn a_root_owned_process_reports_permission_denied_and_never_a_zero() {
        // The single most important invariant of this collector: the baseline turns
        // a refused `proc_pidinfo` into 0 bytes resident and 0% CPU, and §26 forbids
        // exactly that.
        let mut collector = MacosCollector::new().expect("constructs");
        let (_, second) = sample_twice(&mut collector);

        let launchd = second
            .process_by_pid(1)
            .expect("pid 1 is always in the table");
        assert_eq!(launchd.identity.pid, 1);
        assert_eq!(
            launchd.memory.rss_bytes,
            MetricState::PermissionDenied,
            "launchd's memory must be denied, not zero"
        );
        assert_eq!(launchd.cpu, MetricState::PermissionDenied);
        assert_ne!(
            launchd.cpu,
            MetricState::Available(monitrs_core::units::Percent::ZERO)
        );
        assert_eq!(launchd.io.read, MetricState::PermissionDenied);
        assert_eq!(launchd.user.fresh().map(|user| user.uid), Some(0));
        assert_eq!(
            launchd.user.fresh().map(|user| user.display_name()),
            Some("root".to_owned()),
            "the uid must resolve to a name, not be shown as 0"
        );

        for process in &second.processes {
            assert!(
                row_is_self_consistent(process),
                "pid {} mixes a measured value with a denied one",
                process.identity.pid
            );
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn our_own_process_reports_real_numbers() {
        let mut collector = MacosCollector::new().expect("constructs");
        let (_, second) = sample_twice(&mut collector);
        let me = second
            .process_by_pid(std::process::id())
            .expect("our own process");

        assert!(me.memory.rss_bytes.fresh().is_some_and(|rss| *rss > 0));
        assert!(me.threads.fresh().is_some_and(|threads| *threads >= 1));
        assert!(me.cpu.fresh().is_some(), "cpu: {:?}", me.cpu);
        assert!(me.started_at.fresh().is_some());
        assert!(me.age.fresh().is_some());
        assert!(me.io.read_total_bytes.fresh().is_some());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn interfaces_report_a_link_state_and_only_a_real_speed() {
        let mut collector = MacosCollector::new().expect("constructs");
        let (_, second) = sample_twice(&mut collector);
        assert!(!second.networks.is_empty());
        for interface in &second.networks {
            assert!(
                interface.state.fresh().is_some(),
                "{} has no link state",
                interface.name
            );
            // §7.4: no capacity, no percentage.
            if interface.link_speed_mbps.fresh().is_none() {
                assert!(interface.utilization().fresh().is_none());
            }
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn the_detail_path_resolves_our_ancestry_and_refuses_a_reused_identity() {
        let mut collector = MacosCollector::new().expect("constructs");
        let (_, second) = sample_twice(&mut collector);
        let identity = second
            .process_by_pid(std::process::id())
            .expect("our own process")
            .identity;

        match collector.process_detail(identity) {
            ProcessDetailResult::Loaded(detail) => {
                assert_eq!(detail.identity, identity);
                assert!(detail.working_directory.fresh().is_some());
                assert!(detail.open_files.fresh().is_some_and(|count| *count > 0));
                let ancestry = detail.ancestry.fresh().expect("ancestry");
                assert!(
                    !ancestry.is_empty(),
                    "every process except pid 0 and 1 has a parent"
                );
                assert!(detail.nice.fresh().is_some());
                assert!(detail.cgroup.is_unsupported(), "macOS has no cgroups");
            }
            other => panic!("expected our own detail to load, got {other:?}"),
        }

        let stale = ProcessIdentity::new(identity.pid, identity.start_key.wrapping_add(1));
        assert!(matches!(
            collector.process_detail(stale),
            ProcessDetailResult::Reused { .. }
        ));
        assert!(matches!(
            collector.process_detail(ProcessIdentity::new(0x7fff_0000, 1)),
            ProcessDetailResult::Vanished(_)
        ));
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn the_capability_report_claims_only_what_this_collector_can_do() {
        let mut collector = MacosCollector::new().expect("constructs");
        let (_, second) = sample_twice(&mut collector);
        let capabilities = second.capabilities;

        assert_eq!(capabilities.per_core_cpu, CapabilityState::Available);
        assert_eq!(capabilities.cpu_breakdown, CapabilityState::Available);
        assert_eq!(capabilities.per_process_threads, CapabilityState::Available);
        assert_eq!(
            capabilities.per_process_open_files,
            CapabilityState::Available
        );
        // §9.3: no private APIs, so no device busy time and no per-GPU metrics.
        assert_eq!(capabilities.disk_busy, CapabilityState::Unsupported);
        assert_eq!(capabilities.linux_psi, CapabilityState::Unsupported);
        // And the field agrees with the flag. It used to stay `WarmingUp`, which the
        // Pressure Radar rendered as three rows promising a value macOS has no
        // concept of, for as long as the session lasted (§4).
        assert_eq!(
            second.pressure.psi,
            MetricState::Unsupported,
            "a metric this platform cannot produce is unsupported, not warming up"
        );
        assert_eq!(capabilities.cgroup_limits, CapabilityState::Unsupported);
        assert_eq!(capabilities.kernel_threads, CapabilityState::Unsupported);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn repeated_sampling_does_not_grow_the_rate_trackers() {
        // §10.3 and §16.1: PID churn must not accumulate baselines.
        let mut collector = MacosCollector::new().expect("constructs");
        let mut tick = SampleTick::first(Instant::now(), SystemTime::now());
        let mut processes = 0usize;
        for _ in 0..10 {
            let snapshot = collector.sample(&tick).expect("sample");
            processes = snapshot.process_count();
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
            tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);
        }
        let tracked = collector.processes.tracked_processes();
        assert!(
            tracked <= processes + 64,
            "trackers grew to {tracked} against {processes} live processes"
        );
    }

    #[test]
    #[ignore = "platform smoke test: times a fast-tier sample"]
    fn a_fast_tier_sample_stays_inside_its_budget() {
        // §16.1 budgets the fast tier in tens of milliseconds. The tiers are asked
        // for through a real scheduler rather than `DueTiers::ALL`, because the
        // medium and slow tiers of the *baseline* are expensive on macOS — user
        // enumeration goes through OpenDirectory and component enumeration probes
        // IOKit — and charging that to every tick would measure something the
        // sampler never does.
        let mut collector = MacosCollector::new().expect("constructs");
        // Deliberately not `derived_from`: the derived slow interval is thirty fast
        // intervals, which would come due during the settling sleep below.
        let mut scheduler = TierScheduler::new(TierIntervals {
            fast: Duration::from_millis(1),
            medium: Duration::from_secs(60),
            slow: Duration::from_secs(300),
        });
        let start = Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::now());
        let _ = collector.sample(&tick).expect("first sample");
        scheduler.mark_completed(DueTiers::ALL, start);
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL * 2);

        let mut worst = Duration::ZERO;
        for _ in 0..5 {
            let now = Instant::now();
            let due = scheduler.due_at(now);
            assert!(due.contains(Tier::Fast), "the fast tier must be due");
            assert!(
                !due.contains(Tier::Slow),
                "the slow tier must not be due again this soon"
            );
            tick = tick.advance(now, SystemTime::now(), due);
            let started = Instant::now();
            let snapshot = collector.sample(&tick).expect("sample");
            worst = worst.max(started.elapsed());
            scheduler.mark_completed(due, now);
            assert!(!snapshot.processes.is_empty());
            std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        }
        // Measured at 53 ms on an M4 Pro with 975 processes, of which about 5 ms is
        // this module and the rest is the baseline's own process refresh. The bound
        // leaves room for a loaded CI runner without becoming meaningless.
        assert!(
            worst < Duration::from_millis(200),
            "the slowest fast-tier sample took {worst:?}"
        );
    }

    #[test]
    #[ignore = "platform smoke test: times the native enrichment alone"]
    fn the_native_enrichment_is_a_small_part_of_a_sample() {
        // Attribution matters: the baseline's own process refresh dominates a macOS
        // sample, and this collector may not modify it (§9.1 keeps the baseline
        // authoritative for what it already does). What this measures is the cost
        // this module *adds*.
        let mut collector = MacosCollector::new().expect("constructs");
        let mut tick = SampleTick::first(Instant::now(), SystemTime::now());
        let _ = collector.sample(&tick).expect("first sample");
        std::thread::sleep(sysinfo::MINIMUM_CPU_UPDATE_INTERVAL * 2);
        tick = tick.advance(Instant::now(), SystemTime::now(), DueTiers::ALL);

        let mut snapshot = collector.baseline.sample(&tick).expect("baseline sample");
        let started = Instant::now();
        collector.enrich_host(&mut snapshot, &tick);
        collector.enrich_cpu(&mut snapshot, &tick);
        collector.enrich_memory(&mut snapshot, &tick);
        collector.enrich_processes(&mut snapshot, &tick);
        let elapsed = started.elapsed();
        // Measured at 5.0 ms on an M4 Pro with 975 processes.
        assert!(
            elapsed < Duration::from_millis(60),
            "enriching {} processes took {elapsed:?}",
            snapshot.processes.len()
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine"]
    fn nothing_in_a_snapshot_reports_an_unavailable_metric_as_zero() {
        // A sweep of the invariant §26 puts first, across a whole live snapshot.
        let mut collector = MacosCollector::new().expect("constructs");
        let (first, second) = sample_twice(&mut collector);

        for snapshot in [&first, &second] {
            // A metric that is not measured must have no value at all.
            for state in [
                snapshot.memory.detail.buffers,
                snapshot.memory.detail.dirty,
                snapshot.memory.detail.shared,
            ] {
                assert_eq!(state.fresh(), None);
            }
            if snapshot.sensors.battery.is_unsupported() {
                assert!(snapshot.sensors.battery.fresh().is_none());
            }
        }
        // The first snapshot cannot have any rate at all.
        assert!(first.cpu.total.fresh().is_none());
        for process in &first.processes {
            assert_ne!(
                process.cpu,
                MetricState::Available(monitrs_core::units::Percent::ZERO),
                "pid {} reported 0% on the very first sample",
                process.identity.pid
            );
        }
        // And the second must actually have produced something.
        assert!(second.cpu.total.fresh().is_some());
    }
}

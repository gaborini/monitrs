//! A deterministic collector for tests, benchmarks, and UI snapshots.
//!
//! §17.5 requires integration tests to run against a fake collector emitting
//! deterministic snapshots, and §17.3 requires UI snapshots of states a real
//! machine cannot be put into on demand — empty process list, permission-denied
//! metrics, stale data, the warming-up first frame.
//!
//! Every value here is a pure function of the sample sequence number, so a test
//! that asserts a rendered frame will keep asserting the same frame. Nothing in
//! this module reads the real system.
//!
//! This is a *fake*, not a mock of production data. §0.9 forbids inventing
//! production data to make a screen look populated, and the distinction that
//! keeps this honest is that a `FakeCollector` is only ever constructed by a
//! test, a benchmark, or an explicit developer flag — never on a normal launch.

use core::time::Duration;
use std::net::{IpAddr, Ipv4Addr};
use std::time::SystemTime;

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{
    AncestorEntry, BatteryCapacity, BatterySnapshot, CapabilitySnapshot, CapabilityState,
    ChargeState, CollectorHealth, Confidence, ContainerIdentity, ContainerRuntime, CpuQuota,
    CpuSnapshot, CpuUsage, DiskSnapshot, DiskTotals, EnvironmentKind, FilesystemKind,
    FilesystemSnapshot, HostEnvironment, HostSnapshot, InodeUsage, InterfaceAddress,
    InterfaceErrors, InterfaceKind, LinkState, LoadSnapshot, MemoryDetail, MemorySemantics,
    MemorySnapshot, MetricState, NetworkSnapshot, OpenFileEntry, OpenFileKind, OpenFileList,
    PressureSnapshot, ProcessDetail, ProcessDetailResult, ProcessIdentity, ProcessIo,
    ProcessMemory, ProcessSnapshot, ProcessState, SensorSnapshot, SwapSnapshot, TemperatureReading,
    TrafficTotals, UnavailableReason, UserIdentity,
};
use monitrs_core::units::{Percent, Rate};

use crate::error::CollectorError;
use crate::source::{SampleTick, SnapshotSource};

const GIB: u64 = 1024 * 1024 * 1024;
const MIB: u64 = 1024 * 1024;

/// How a scalar metric evolves over successive samples.
///
/// Patterns are deterministic functions of the sequence number rather than
/// randomised, so a snapshot test is stable and a failure is reproducible.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    /// The same value every sample.
    Steady(f32),
    /// A triangle wave between `low` and `high` with a period of `period`
    /// samples. Useful for history graphs, which look wrong with a flat line.
    Sawtooth {
        /// Trough value.
        low: f32,
        /// Peak value.
        high: f32,
        /// Samples per full cycle. Treated as 1 when zero, so no division by zero.
        period: u64,
    },
    /// `base` everywhere except one sample at `at`, which is `peak`.
    ///
    /// This is the shape Time Lens exists to find (§2.1).
    Spike {
        /// The baseline.
        base: f32,
        /// The value at `at`.
        peak: f32,
        /// The sequence number of the spike.
        at: u64,
    },
    /// Never available, for the reason given.
    Unavailable(UnavailableReason),
    /// Always refused by the OS.
    PermissionDenied,
    /// Not provided by this platform.
    Unsupported,
}

impl Pattern {
    /// The value at `sequence`, or why there is none.
    ///
    /// Sequence 0 is always [`MetricState::WarmingUp`]: the first sample of
    /// delta-based data is not zero (§8.2, §26), and a fake that skipped that
    /// would let a first-frame bug through every test.
    #[must_use]
    pub fn at(self, sequence: u64) -> MetricState<Percent> {
        if sequence == 0 {
            return MetricState::WarmingUp;
        }
        match self {
            Self::Unavailable(reason) => MetricState::TemporarilyUnavailable(reason),
            Self::PermissionDenied => MetricState::PermissionDenied,
            Self::Unsupported => MetricState::Unsupported,
            Self::Steady(value) => Self::wrap(value),
            Self::Spike { base, peak, at } => Self::wrap(if sequence == at { peak } else { base }),
            Self::Sawtooth { low, high, period } => {
                let period = period.max(1);
                let phase = sequence % period;
                let half = period / 2;
                let fraction = if half == 0 {
                    0.0
                } else if phase <= half {
                    phase as f32 / half as f32
                } else {
                    (period - phase) as f32 / half as f32
                };
                Self::wrap(low + (high - low) * fraction)
            }
        }
    }

    fn wrap(value: f32) -> MetricState<Percent> {
        Percent::new(value).map_or(
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            MetricState::Available,
        )
    }
}

/// One process in a fake scenario.
#[derive(Clone, Debug)]
pub struct FakeProcess {
    /// Stable identity.
    pub identity: ProcessIdentity,
    /// Parent PID, or `None` for a root.
    pub parent_pid: Option<u32>,
    /// Short name.
    pub name: Box<str>,
    /// Full command line.
    pub command: Box<str>,
    /// Owning user name.
    pub user: Box<str>,
    /// Owning user id.
    pub uid: u32,
    /// Scheduling state.
    pub state: ProcessState,
    /// Core-normalized CPU percentage. May exceed 100 (§8.3).
    pub cpu: Pattern,
    /// Resident bytes.
    pub rss_bytes: u64,
    /// Thread count.
    pub threads: u32,
    /// Age at sequence 0; grows by the tick interval afterwards.
    pub age: Duration,
    /// Sequence at which this process disappears from the table.
    ///
    /// Drives the §17.5 "selected process exit" test.
    pub exits_at: Option<u64>,
    /// A different process that takes over this PID once `exits_at` passes.
    ///
    /// Drives the PID-reuse abort test (§21 M3): the same PID with a different
    /// start key must not inherit a pin or a pending signal.
    pub reused_start_key: Option<u64>,
    /// Whether this is a kernel thread, which §7.2 allows hiding on Linux.
    pub is_kernel_thread: bool,
}

impl FakeProcess {
    /// A process with plausible defaults, to be adjusted by the caller.
    #[must_use]
    pub fn new(pid: u32, start_key: u64, name: &str, command: &str) -> Self {
        Self {
            identity: ProcessIdentity::new(pid, start_key),
            parent_pid: Some(1),
            name: name.into(),
            command: command.into(),
            user: "gabor".into(),
            uid: 501,
            state: ProcessState::Sleeping,
            cpu: Pattern::Steady(0.0),
            rss_bytes: 32 * MIB,
            threads: 4,
            age: Duration::from_secs(60),
            exits_at: None,
            reused_start_key: None,
            is_kernel_thread: false,
        }
    }

    /// Sets the parent PID, which is what makes a scenario a *tree*.
    #[must_use]
    pub const fn with_parent(mut self, pid: u32) -> Self {
        self.parent_pid = Some(pid);
        self
    }

    /// Sets the CPU pattern.
    #[must_use]
    pub const fn with_cpu(mut self, cpu: Pattern) -> Self {
        self.cpu = cpu;
        self
    }

    /// Sets the resident size.
    #[must_use]
    pub const fn with_rss(mut self, rss_bytes: u64) -> Self {
        self.rss_bytes = rss_bytes;
        self
    }

    /// Sets the scheduling state.
    #[must_use]
    pub const fn with_state(mut self, state: ProcessState) -> Self {
        self.state = state;
        self
    }

    /// Sets the owning user.
    #[must_use]
    pub fn with_user(mut self, user: &str, uid: u32) -> Self {
        self.user = user.into();
        self.uid = uid;
        self
    }

    /// Makes the process exit at `sequence`.
    #[must_use]
    pub const fn exiting_at(mut self, sequence: u64) -> Self {
        self.exits_at = Some(sequence);
        self
    }

    /// Makes a different process take over this PID after it exits.
    #[must_use]
    pub const fn reused_as(mut self, start_key: u64) -> Self {
        self.reused_start_key = Some(start_key);
        self
    }

    /// Marks this as a kernel thread.
    #[must_use]
    pub const fn as_kernel_thread(mut self) -> Self {
        self.is_kernel_thread = true;
        self
    }

    /// Whether this process is present in the table at `sequence`.
    #[must_use]
    fn is_present_at(&self, sequence: u64) -> bool {
        self.exits_at.is_none_or(|exit| sequence < exit)
    }

    /// The identity this PID resolves to at `sequence`.
    fn identity_at(&self, sequence: u64) -> Option<ProcessIdentity> {
        if self.is_present_at(sequence) {
            return Some(self.identity);
        }
        self.reused_start_key
            .map(|start_key| ProcessIdentity::new(self.identity.pid, start_key))
    }
}

/// What a fake system looks like.
#[derive(Clone, Debug)]
pub struct Scenario {
    /// Host name shown in the header.
    pub hostname: Box<str>,
    /// Target architecture reported by the fake host.
    ///
    /// Fixed rather than read from `std::env::consts::ARCH`, because a UI snapshot
    /// accepted on an arm64 developer machine must also pass on an x86_64 CI
    /// runner. §17.3 requires host-dependent values to be normalized in fixtures,
    /// and the architecture is the same class of value as the hostname.
    pub arch: &'static str,
    /// Logical CPU count.
    pub logical_cpus: u16,
    /// Whether the fake machine has asymmetric cores.
    ///
    /// `true` by default, so the snapshot suites exercise the grouped per-core path —
    /// which is the interesting one, and the one every Apple Silicon Mac takes. Set it
    /// `false` for a homogeneous machine: an Intel box, most servers, and most virtual
    /// machines, where the platform reports no classes and one group is no
    /// classification.
    pub asymmetric_cores: bool,
    /// Physical core count.
    pub physical_cpus: u16,
    /// Total physical memory.
    pub total_memory_bytes: u64,
    /// Configured swap. Zero means swap is disabled, which is a real state.
    pub swap_total_bytes: u64,
    /// System-wide CPU utilization over time.
    pub cpu: Pattern,
    /// Memory utilization over time.
    pub memory: Pattern,
    /// The process table.
    pub processes: Vec<FakeProcess>,
    /// Whether per-process I/O is readable, or refused as it is for another
    /// user's processes without privileges (§9.2).
    pub process_io: CapabilityState,
    /// Whether a process's open descriptors can be listed (§7.2, §8.6).
    ///
    /// Three states matter and a real machine can only be asked for one of them at a
    /// time: `Available` for a process the user owns, `PermissionDenied` for one
    /// owned by somebody else, and `Unsupported` for a platform whose collector
    /// cannot name descriptors at all — which is every platform on the `sysinfo`
    /// baseline. §17.3 wants a snapshot of each.
    pub process_open_files: CapabilityState,
    /// Whether the network link speed is known. When it is not, no utilization
    /// percentage may be rendered (§7.4).
    pub link_speed_mbps: Option<u64>,
    /// Whether temperature sensors exist.
    pub temperatures: bool,
    /// Whether a battery exists.
    ///
    /// `false` is not a degraded scenario: it is the desktop, the server, and every
    /// CI runner, which is why the Battery screen's absent case is snapshot-tested
    /// from it rather than from a hand-written struct (§17.5).
    pub battery: bool,
    /// What the battery is doing, when there is one.
    ///
    /// Ignored entirely when `battery` is `false`, so the two cannot describe a
    /// machine that is charging a battery it does not have.
    pub battery_state: ChargeState,
    /// A cgroup memory limit, to exercise the container-vs-host display (§9.2).
    ///
    /// Also what makes the scenario a container: it drives the CPU quota, the group's
    /// own memory charge, and the environment classification, because those four facts
    /// arrive together on a real containerised host and four independent knobs could
    /// produce a machine that has a memory limit but no container.
    pub cgroup_limit_bytes: Option<u64>,
    /// Artificial delay per `sample()` call.
    ///
    /// Drives the §21 M2 acceptance criterion that "input stays responsive while
    /// the fake collector delays": the sampler thread blocks here and the UI
    /// thread must keep processing keys.
    pub collect_delay: Duration,
    /// A sequence at which `sample()` returns an error, to exercise the
    /// recoverable-collector-error path (§14.1).
    pub fail_at: Option<u64>,
    /// A sequence from which metrics go stale rather than fresh, to exercise the
    /// stale-marking requirement (§4, §17.3).
    ///
    /// Every metric the scenario can age, the sensor group included. That last part was
    /// missing until the 1.0.0 review: `sensors()` built its states without going through
    /// `FakeCollector::age`, so no fixture in the suite had ever rendered a stale
    /// sensor and the Battery screen printed the fields of a retained pack undated
    /// without a snapshot noticing. Sensors are also the metrics a real host is *most*
    /// likely to show retained, because they are read on their own cadence rather than
    /// every tick.
    pub stale_from: Option<u64>,
}

impl Default for Scenario {
    /// The reference scenario, matching the layout mockup in §5.5.
    ///
    /// Using the mockup's own numbers means a UI snapshot test can be read
    /// against the specification directly.
    fn default() -> Self {
        Self {
            hostname: "dev-mbp".into(),
            arch: "aarch64",
            logical_cpus: 8,
            asymmetric_cores: true,
            physical_cpus: 8,
            total_memory_bytes: 32 * GIB,
            swap_total_bytes: 2 * GIB,
            cpu: Pattern::Sawtooth {
                low: 12.0,
                high: 91.0,
                period: 40,
            },
            memory: Pattern::Steady(71.0),
            processes: vec![
                FakeProcess::new(31_842, 900_100, "rustc", "cargo build --release")
                    .with_cpu(Pattern::Spike {
                        base: 120.0,
                        peak: 287.0,
                        at: 20,
                    })
                    .with_rss(2 * GIB + 614 * MIB),
                FakeProcess::new(1_221, 700_050, "postgres", "postgres: checkpointer")
                    .with_cpu(Pattern::Steady(54.0))
                    .with_rss(982 * MIB)
                    .with_user("postgres", 70),
                FakeProcess::new(507, 100_010, "WindowServer", "/System/Library/WindowServer")
                    .with_cpu(Pattern::Steady(21.0))
                    .with_rss(GIB + 307 * MIB),
                FakeProcess::new(9_182, 850_300, "node", "node server.js")
                    .with_cpu(Pattern::Steady(12.0))
                    .with_rss(392 * MIB),
                FakeProcess::new(1, 1, "launchd", "/sbin/launchd").with_cpu(Pattern::Steady(0.1)),
            ],
            process_io: CapabilityState::Available,
            process_open_files: CapabilityState::Available,
            link_speed_mbps: None,
            temperatures: true,
            battery: true,
            battery_state: ChargeState::Discharging,
            cgroup_limit_bytes: None,
            collect_delay: Duration::ZERO,
            fail_at: None,
            stale_from: None,
        }
    }
}

impl Scenario {
    /// A system with no processes visible at all.
    ///
    /// A real state — a locked-down container can show almost nothing — and one
    /// §17.3 requires a UI snapshot for.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            processes: Vec::new(),
            ..Self::default()
        }
    }

    /// A system where every optional metric is refused by the OS.
    #[must_use]
    pub fn permission_denied() -> Self {
        Self {
            process_io: CapabilityState::PermissionDenied,
            process_open_files: CapabilityState::PermissionDenied,
            cpu: Pattern::PermissionDenied,
            temperatures: false,
            battery: false,
            ..Self::default()
        }
    }

    /// A platform whose collector cannot name a process's descriptors at all.
    ///
    /// What the `sysinfo` baseline produces on its own, and the third state §7.2's
    /// "where supported and permitted" has to be renderable in: not refused, not
    /// slow, simply absent (§4).
    #[must_use]
    pub fn without_open_file_listing() -> Self {
        Self {
            process_open_files: CapabilityState::Unsupported,
            ..Self::default()
        }
    }

    /// A machine with no battery: a desktop, a server, or a CI runner.
    ///
    /// The case §4 exists for, and the one every automated run of this monitor hits.
    /// Named rather than left to callers setting `battery: false` inline, so the
    /// snapshot suites all mean the same thing by it.
    #[must_use]
    pub fn no_battery() -> Self {
        Self {
            battery: false,
            ..Self::default()
        }
    }

    /// A laptop on mains with the pack charging.
    #[must_use]
    pub fn charging() -> Self {
        Self {
            battery_state: ChargeState::Charging,
            ..Self::default()
        }
    }

    /// A machine with many cores, to exercise the per-core aggregation §7.1
    /// requires instead of hundreds of rows.
    #[must_use]
    pub fn many_cores() -> Self {
        Self {
            logical_cpus: 256,
            physical_cpus: 128,
            ..Self::default()
        }
    }

    /// A build under a shell: `make` with two compilers and one assembler beneath it.
    ///
    /// Three deliberate properties, because this is the fixture the subtree aggregation is
    /// snapshotted against:
    ///
    /// * **The family is not contiguous in table order.** The unrelated processes sit
    ///   between its members, so a scope that happened to keep a run of adjacent rows
    ///   could not pass by accident.
    /// * **It is three levels deep.** A subtree that only ever collected direct children
    ///   would look right until `as` went missing.
    /// * **One member's CPU is refused.** The summed figure is then a *lower* bound, which
    ///   is the case §4 is about: a partial sum presented as the whole answer is worse
    ///   than one marked as partial, and worse still than the zero a naive sum produces.
    #[must_use]
    pub fn a_build() -> Self {
        let mut processes = vec![
            FakeProcess::new(400, 400_000, "zsh", "-zsh").with_cpu(Pattern::Steady(0.2)),
            FakeProcess::new(410, 410_000, "make", "make -j4")
                .with_parent(400)
                .with_cpu(Pattern::Steady(3.0))
                .with_rss(48 * MIB),
        ];
        processes.push(
            FakeProcess::new(411, 411_000, "cc", "cc -O2 -c parser.c")
                .with_parent(410)
                .with_cpu(Pattern::Steady(96.0))
                .with_rss(212 * MIB),
        );
        // A compiler owned by another user, whose CPU the OS will not report. It is still
        // a member, and it is still counted in the family's size.
        processes.push(
            FakeProcess::new(412, 412_000, "cc", "cc -O2 -c codegen.c")
                .with_parent(410)
                .with_cpu(Pattern::PermissionDenied)
                .with_rss(198 * MIB)
                .with_user("builder", 900),
        );
        processes.push(
            FakeProcess::new(413, 413_000, "as", "as -o codegen.o")
                .with_parent(412)
                .with_cpu(Pattern::Steady(8.0))
                .with_rss(21 * MIB),
        );
        let mut scenario = Self::default();
        // Interleaved: the default scenario's processes first, then the family's, so the
        // sort by CPU scatters them rather than grouping them.
        let mut all = scenario.processes;
        all.splice(2..2, processes);
        scenario.processes = all;
        scenario
    }

    /// A container: cgroup limits far below the host's, and an identity to name it.
    #[must_use]
    pub fn containerised() -> Self {
        Self {
            cgroup_limit_bytes: Some(2 * GIB),
            ..Self::default()
        }
    }

    /// The CPU quota this scenario implies: 1.5 CPUs, a fraction that exercises the
    /// non-integer case a whole number would hide.
    fn cgroup_quota(&self) -> MetricState<CpuQuota> {
        if self.cgroup_limit_bytes.is_none() {
            return MetricState::Unsupported;
        }
        CpuQuota::new(150_000, 100_000).map_or(MetricState::Unsupported, MetricState::Available)
    }

    /// The group's own memory charge: well below the limit, and far below the host's
    /// `used`, so a view that pairs the host figure with the group's limit is visibly
    /// wrong rather than merely different.
    fn cgroup_used(&self) -> MetricState<u64> {
        self.cgroup_limit_bytes
            .map_or(MetricState::Unsupported, |limit| {
                MetricState::Available(limit / 4)
            })
    }

    /// The container this scenario is, if it is one.
    fn container(&self) -> Option<ContainerIdentity> {
        self.cgroup_limit_bytes.map(|_| ContainerIdentity {
            runtime: ContainerRuntime::Docker,
            // A full-length digest, so `short_id`'s truncation is exercised rather
            // than passed through.
            id: "3f4a1b2c9d8e7f60a1b2c3d4e5f60718293a4b5c6d7e8f9012345678909abcdef".into(),
            kubernetes: false,
        })
    }

    /// A busy machine, for the high-load behaviour of §16.2.
    ///
    /// Generates `count` synthetic processes on top of the reference set.
    #[must_use]
    pub fn with_process_count(count: usize) -> Self {
        let mut scenario = Self::default();
        let base = scenario.processes.len();
        for index in base..count {
            // Saturating rather than wrapping: a scenario asking for more than
            // u32::MAX processes should produce duplicate PIDs, not silently
            // wrap round to PID 1 and collide with launchd.
            let pid = 10_000u32.saturating_add(u32::try_from(index).unwrap_or(u32::MAX));
            let index64 = index as u64;
            scenario.processes.push(
                FakeProcess::new(pid, u64::from(pid) * 7, "worker", "worker --loop")
                    .with_cpu(Pattern::Steady((index64 % 17) as f32))
                    .with_rss((8 + (index64 % 64)) * MIB),
            );
        }
        scenario
    }
}

/// A collector that produces deterministic snapshots from a [`Scenario`].
#[derive(Clone, Debug)]
pub struct FakeCollector {
    scenario: Scenario,
    interval: Duration,
}

impl Default for FakeCollector {
    fn default() -> Self {
        Self::new(Scenario::default())
    }
}

impl FakeCollector {
    /// Builds a collector for `scenario`, assuming a one-second interval for the
    /// purpose of ageing processes and accumulating totals.
    #[must_use]
    pub fn new(scenario: Scenario) -> Self {
        Self {
            scenario,
            interval: Duration::from_secs(1),
        }
    }

    /// Overrides the nominal interval used to age processes and grow counters.
    ///
    /// This is presentation only: real rate arithmetic uses the measured
    /// `tick.elapsed`, never this value (§8.1).
    #[must_use]
    pub const fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// The scenario being played.
    #[must_use]
    pub const fn scenario(&self) -> &Scenario {
        &self.scenario
    }

    /// Mutable access, so a running test can change the system underneath the UI.
    pub const fn scenario_mut(&mut self) -> &mut Scenario {
        &mut self.scenario
    }

    /// Applies the scenario's staleness rule to a freshly computed value.
    fn age<T>(&self, value: MetricState<T>, sequence: u64) -> MetricState<T> {
        match self.scenario.stale_from {
            Some(from) if sequence >= from => {
                let age = self
                    .interval
                    .saturating_mul(u32::try_from(sequence - from).unwrap_or(1));
                value.into_stale(age.max(self.interval))
            }
            _ => value,
        }
    }

    fn host(&self, sequence: u64) -> HostSnapshot {
        HostSnapshot {
            hostname: MetricState::Available(self.scenario.hostname.clone()),
            os_name: MetricState::Available("fake-os".into()),
            os_version: MetricState::Available("1.0".into()),
            kernel_version: MetricState::Available("fake-kernel 1.0".into()),
            arch: self.scenario.arch,
            cpu_brand: MetricState::Available("Fake CPU".into()),
            // A fixed 3d 04:12 plus one interval per sample, matching §5.5.
            uptime: MetricState::Available(
                Duration::from_secs(3 * 86_400 + 4 * 3_600 + 12 * 60)
                    + self
                        .interval
                        .saturating_mul(u32::try_from(sequence).unwrap_or(u32::MAX)),
            ),
            boot_time: MetricState::Available(SystemTime::UNIX_EPOCH),
            environment: MetricState::Available(HostEnvironment {
                kind: if self.scenario.cgroup_limit_bytes.is_some() {
                    EnvironmentKind::Container
                } else {
                    EnvironmentKind::NoEvidenceFound
                },
                evidence: "synthetic scenario".into(),
                confidence: Confidence::High,
                container: self.scenario.container(),
            }),
        }
    }

    fn cpu(&self, sequence: u64) -> CpuSnapshot {
        let total = self.scenario.cpu.at(sequence);
        let per_core = total.fresh().map_or_else(
            || total.as_ref().map(|_| Vec::new()),
            |busy| {
                // Spread the aggregate across cores deterministically so the
                // per-core view has structure rather than N identical bars.
                let cores = (0..self.scenario.logical_cpus)
                    .map(|index| {
                        let skew = 1.0 + ((index % 4) as f32 - 1.5) * 0.25;
                        let value = (busy.value() * skew).clamp(0.0, 100.0);
                        CpuUsage::plain(Percent::new(value).unwrap_or(Percent::ZERO))
                    })
                    .collect();
                MetricState::Available(cores)
            },
        );
        CpuSnapshot {
            logical_count: self.scenario.logical_cpus,
            physical_count: MetricState::Available(self.scenario.physical_cpus),
            total: self.age(total.map(CpuUsage::plain), sequence),
            per_core: self.age(per_core, sequence),
            frequency_mhz: MetricState::Available(3_200),
            cgroup_quota: self.scenario.cgroup_quota(),
            // Two classes, so every snapshot test and the CPU screen exercise the
            // heterogeneous path rather than only the flat one. The split mirrors the
            // shape of a real asymmetric machine: the faster cores come first.
            core_classes: if !self.scenario.asymmetric_cores {
                // A homogeneous machine: no classes at all, which is a fact rather than
                // an absence.
                Vec::new()
            } else {
                let logical = self.scenario.logical_cpus;
                let performance = logical.saturating_sub(logical / 4).max(1);
                vec![
                    monitrs_core::model::CoreClass {
                        name: "Performance".into(),
                        logical: (0..performance).collect(),
                        physical_count: Some(performance),
                    },
                    monitrs_core::model::CoreClass {
                        name: "Efficiency".into(),
                        logical: (performance..logical).collect(),
                        physical_count: Some(logical.saturating_sub(performance)),
                    },
                ]
                .into_iter()
                .filter(|class| !class.is_empty())
                .collect()
            },
        }
    }

    fn memory(&self, sequence: u64) -> MemorySnapshot {
        let total = self.scenario.total_memory_bytes;
        let usage = self.scenario.memory.at(sequence);
        let used = usage.map(|percent| {
            let fraction = f64::from(percent.clamped_to_100().value()) / 100.0;
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let bytes = (total as f64 * fraction) as u64;
            bytes
        });
        let available = used.map(|bytes| total.saturating_sub(bytes));
        let swap_used = if self.scenario.swap_total_bytes == 0 {
            MetricState::Available(0)
        } else {
            MetricState::Available(205 * MIB)
        };

        MemorySnapshot {
            total_bytes: total,
            available: self.age(available, sequence),
            used: self.age(used, sequence),
            free: MetricState::Available(total / 16),
            usage: self.age(usage, sequence),
            detail: MemoryDetail {
                cached: MetricState::Available(total / 8),
                buffers: MetricState::Unsupported,
                shared: MetricState::Available(256 * MIB),
                active: MetricState::Available(total / 4),
                inactive: MetricState::Available(total / 8),
                wired: MetricState::Available(3 * GIB),
                compressed: MetricState::Available(GIB),
                dirty: MetricState::Unsupported,
            },
            swap: SwapSnapshot {
                total_bytes: self.scenario.swap_total_bytes,
                used: swap_used,
                usage: if self.scenario.swap_total_bytes == 0 {
                    MetricState::Unsupported
                } else {
                    Percent::ratio(205 * MIB, self.scenario.swap_total_bytes)
                        .map_or(MetricState::Unsupported, MetricState::Available)
                },
                in_rate: rate_state(sequence, 0.0),
                out_rate: rate_state(sequence, 0.0),
            },
            semantics: MemorySemantics::SysinfoBaseline,
            cgroup_limit_bytes: self
                .scenario
                .cgroup_limit_bytes
                .map_or(MetricState::Unsupported, MetricState::Available),
            cgroup_used_bytes: self.scenario.cgroup_used(),
        }
    }

    fn processes(&self, sequence: u64) -> Vec<ProcessSnapshot> {
        let total_memory = self.scenario.total_memory_bytes;
        let io_state = self.scenario.process_io;
        self.scenario
            .processes
            .iter()
            .filter_map(|process| {
                let identity = process.identity_at(sequence)?;
                let reused = identity != process.identity;
                let cpu = if reused {
                    // A recycled PID is a *different* process. It starts warming
                    // up, exactly as a newly discovered process does.
                    MetricState::WarmingUp
                } else {
                    process.cpu.at(sequence)
                };
                let age = if reused {
                    Duration::ZERO
                } else {
                    process.age
                        + self
                            .interval
                            .saturating_mul(u32::try_from(sequence).unwrap_or(u32::MAX))
                };
                Some(ProcessSnapshot {
                    identity,
                    parent_pid: process.parent_pid,
                    name: process.name.clone(),
                    command: process.command.clone(),
                    exe: Some(format!("/usr/bin/{}", process.name).into()),
                    user: MetricState::Available(UserIdentity {
                        uid: process.uid,
                        name: Some(process.user.clone()),
                    }),
                    state: process.state,
                    cpu: self.age(cpu, sequence),
                    memory: ProcessMemory {
                        rss_bytes: MetricState::Available(process.rss_bytes),
                        virtual_bytes: MetricState::Available(process.rss_bytes.saturating_mul(6)),
                        share_of_total: Percent::ratio(process.rss_bytes, total_memory)
                            .map_or(MetricState::Unsupported, MetricState::Available),
                    },
                    io: fake_io(sequence, io_state, process.rss_bytes),
                    threads: MetricState::Available(process.threads),
                    age: MetricState::Available(age),
                    started_at: MetricState::Available(SystemTime::UNIX_EPOCH),
                    is_kernel_thread: process.is_kernel_thread,
                })
            })
            .collect()
    }

    fn disks(&self, sequence: u64) -> Vec<DiskSnapshot> {
        vec![DiskSnapshot {
            device: "disk0".into(),
            model: Some("Fake NVMe".into()),
            read: rate_state(sequence, 18.0 * MIB as f64),
            write: rate_state(sequence, 42.0 * MIB as f64),
            read_ops: rate_state(sequence, 320.0),
            write_ops: rate_state(sequence, 810.0),
            // §7.3: only where semantically correct. The fake platform does not
            // provide it, so it is unsupported rather than an invented number.
            busy: MetricState::Unsupported,
            queue_length: MetricState::Unsupported,
            totals: counter_state(
                sequence,
                DiskTotals {
                    read_bytes: 18 * MIB * sequence.max(1),
                    write_bytes: 42 * MIB * sequence.max(1),
                },
            ),
            mount_points: vec!["/".into()],
        }]
    }

    /// The mount table.
    ///
    /// Two of these mounts share `disk0s1` on purpose. It is what an APFS container
    /// really looks like — `/` and `/System/Volumes/Data` both report the container's
    /// full size — and it is the case the Storage screen has to mark, because a reader
    /// who adds the two `SIZE` columns together gets twice the disk the machine has.
    /// A fake that showed one tidy mount per device would let that bug through.
    fn filesystems(&self) -> Vec<FilesystemSnapshot> {
        let total = 494 * GIB;
        let used = 374 * GIB;
        let shared = |mount: &str, inodes: MetricState<InodeUsage>| FilesystemSnapshot {
            mount_point: mount.into(),
            device: Some("disk0s1".into()),
            fs_type: Some("fakefs".into()),
            total_bytes: total,
            available_bytes: MetricState::Available(total - used),
            used_bytes: MetricState::Available(used),
            usage: Percent::ratio(used, total)
                .map_or(MetricState::Unsupported, MetricState::Available),
            inodes,
            kind: FilesystemKind::Physical,
            read_only: false,
        };
        vec![
            shared("/", InodeUsage::from_counts(4_882_812, 4_398_046)),
            // The data volume is where the files are, so its table is the fuller one.
            shared(
                "/System/Volumes/Data",
                InodeUsage::from_counts(4_882_812, 341_796),
            ),
            FilesystemSnapshot {
                mount_point: "/dev".into(),
                device: None,
                fs_type: Some("devfs".into()),
                total_bytes: 200 * 1024,
                available_bytes: MetricState::Available(0),
                used_bytes: MetricState::Available(200 * 1024),
                usage: MetricState::Available(Percent::FULL),
                // A pseudo-filesystem has no inode table to run out of, and §4 wants
                // that said rather than reported as `0 of 0`.
                inodes: MetricState::Unsupported,
                kind: FilesystemKind::Virtual,
                read_only: true,
            },
        ]
    }

    fn networks(&self, sequence: u64) -> Vec<NetworkSnapshot> {
        let link_speed = self.scenario.link_speed_mbps.map_or(
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
            MetricState::Available,
        );
        vec![
            NetworkSnapshot {
                name: "en0".into(),
                kind: InterfaceKind::Physical,
                state: MetricState::Available(LinkState::Up),
                addresses: vec![InterfaceAddress {
                    ip: IpAddr::V4(Ipv4Addr::new(192, 168, 1, 42)),
                    prefix_len: Some(24),
                }],
                mac: Some("02:00:00:00:00:01".into()),
                rx: rate_state(sequence, 18.2 * MIB as f64),
                tx: rate_state(sequence, 2.3 * MIB as f64),
                rx_packets: rate_state(sequence, 14_200.0),
                tx_packets: rate_state(sequence, 3_100.0),
                errors: counter_state(sequence, InterfaceErrors::default()),
                link_speed_mbps: link_speed,
                since_launch: TrafficTotals {
                    rx_bytes: 18 * MIB * sequence,
                    tx_bytes: 2 * MIB * sequence,
                    rx_packets: 14_200 * sequence,
                    tx_packets: 3_100 * sequence,
                },
                os_totals: counter_state(
                    sequence,
                    TrafficTotals {
                        rx_bytes: 900 * GIB + 18 * MIB * sequence,
                        tx_bytes: 120 * GIB + 2 * MIB * sequence,
                        rx_packets: 9_000_000 + 14_200 * sequence,
                        tx_packets: 1_200_000 + 3_100 * sequence,
                    },
                ),
            },
            NetworkSnapshot {
                name: "lo0".into(),
                kind: InterfaceKind::Loopback,
                state: MetricState::Available(LinkState::Up),
                addresses: vec![InterfaceAddress {
                    ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    prefix_len: Some(8),
                }],
                mac: None,
                rx: rate_state(sequence, 42.0 * 1024.0),
                tx: rate_state(sequence, 42.0 * 1024.0),
                rx_packets: rate_state(sequence, 120.0),
                tx_packets: rate_state(sequence, 120.0),
                errors: counter_state(sequence, InterfaceErrors::default()),
                // A loopback interface has no link speed, ever.
                link_speed_mbps: MetricState::Unsupported,
                since_launch: TrafficTotals::default(),
                os_totals: counter_state(sequence, TrafficTotals::default()),
            },
        ]
    }

    /// The sensor group, which unlike the other metrics is *expected* to be retained.
    ///
    /// Both fields go through [`FakeCollector::age`] like every other metric. They were
    /// the two that did not, and the consequence was that no fixture in the suite had
    /// ever rendered a stale sensor: the Battery screen unwrapped a `Stale` envelope and
    /// printed its inner fields undated for a whole release cycle without a snapshot
    /// noticing. Sensors are the metrics *most* likely to be stale on a real host, since
    /// they are read on their own cadence rather than every tick, so a fake that could
    /// not produce one was missing the case that matters most.
    fn sensors(&self, sequence: u64) -> SensorSnapshot {
        SensorSnapshot {
            temperatures: if self.scenario.temperatures {
                self.age(
                    counter_state(
                        sequence,
                        vec![
                            TemperatureReading {
                                label: "performance".into(),
                                celsius: 62.5,
                                peak_celsius: Some(95.0),
                                critical_celsius: Some(105.0),
                            },
                            TemperatureReading {
                                label: "efficiency".into(),
                                celsius: 44.0,
                                peak_celsius: Some(95.0),
                                critical_celsius: Some(105.0),
                            },
                        ],
                    ),
                    sequence,
                )
            } else {
                MetricState::Unsupported
            },
            battery: if self.scenario.battery {
                self.age(counter_state(sequence, self.battery()), sequence)
            } else {
                // §4: a machine with no battery reports the fact, not a flat pack.
                MetricState::Unsupported
            },
        }
    }

    /// The fake battery: a four-year-old laptop pack, worn to 91.6% of design.
    ///
    /// The figures are chosen so every field on the Battery screen is exercised and
    /// none of them can be confused with another: 82% charge against 91.6% health
    /// against 214 cycles reads wrong immediately if two of them are swapped.
    ///
    /// A time remaining only in the two states where the platform has a direction to
    /// estimate toward. §4 forbids deriving one, so a full pack on mains has none —
    /// and the screen has to say that rather than print the last figure it saw.
    fn battery(&self) -> BatterySnapshot {
        let state = self.scenario.battery_state;
        BatterySnapshot {
            charge: Percent::new(if matches!(state, ChargeState::Full) {
                100.0
            } else {
                82.0
            })
            .unwrap_or(Percent::ZERO),
            state,
            time_remaining: match state {
                ChargeState::Discharging => MetricState::Available(Duration::from_secs(4 * 3_600)),
                ChargeState::Charging => MetricState::Available(Duration::from_secs(2_940)),
                ChargeState::Full | ChargeState::NotCharging | ChargeState::Unknown => {
                    MetricState::Unsupported
                }
            },
            cycle_count: MetricState::Available(214),
            capacity: MetricState::Available(BatteryCapacity {
                design_microwatt_hours: 52_600_000,
                full_microwatt_hours: 48_200_000,
            }),
            temperature_celsius: MetricState::Available(
                if matches!(state, ChargeState::Charging) {
                    // A charging pack runs warmer, and the screen is worth reading on a
                    // scenario where the two states do not look identical.
                    36.8
                } else {
                    31.4
                },
            ),
            power_watts: MetricState::Available(match state {
                ChargeState::Discharging => 12.4,
                ChargeState::Charging => 44.6,
                // A full pack on mains draws nothing, which is a measured zero
                // rather than an absent reading.
                ChargeState::Full | ChargeState::NotCharging | ChargeState::Unknown => 0.0,
            }),
        }
    }

    fn capabilities_for(&self) -> CapabilitySnapshot {
        let sensors = if self.scenario.temperatures {
            CapabilityState::Available
        } else {
            CapabilityState::Unsupported
        };
        CapabilitySnapshot {
            per_process_io: self.scenario.process_io,
            per_process_threads: CapabilityState::Available,
            per_process_open_files: self.scenario.process_open_files,
            per_process_sockets: CapabilityState::Available,
            per_process_working_directory: CapabilityState::Available,
            per_core_cpu: CapabilityState::Available,
            // The fake platform reports no CPU time split, exactly as macOS
            // reports no iowait/steal.
            cpu_breakdown: CapabilityState::Unsupported,
            load_average: CapabilityState::Available,
            swap_activity: CapabilityState::Unsupported,
            disk_io: CapabilityState::Available,
            disk_busy: CapabilityState::Unsupported,
            filesystem_capacity: CapabilityState::Available,
            network_counters: CapabilityState::Available,
            network_link_speed: if self.scenario.link_speed_mbps.is_some() {
                CapabilityState::Available
            } else {
                CapabilityState::Unsupported
            },
            network_errors: CapabilityState::Available,
            temperatures: sensors,
            battery: if self.scenario.battery {
                CapabilityState::Available
            } else {
                CapabilityState::Unsupported
            },
            linux_psi: CapabilityState::Unsupported,
            cgroup_limits: if self.scenario.cgroup_limit_bytes.is_some() {
                CapabilityState::Available
            } else {
                CapabilityState::Unsupported
            },
            kernel_threads: CapabilityState::Available,
            // A fake system cannot be signalled. This keeps a test that reaches
            // the signal path honest: it can assert the effect, not the delivery.
            process_signals: CapabilityState::Unsupported,
            renice: CapabilityState::Unsupported,
        }
    }
}

fn rate_state(sequence: u64, per_second: f64) -> MetricState<Rate> {
    if sequence == 0 {
        return MetricState::WarmingUp;
    }
    Rate::new(per_second).map_or(MetricState::WarmingUp, MetricState::Available)
}

fn counter_state<T>(sequence: u64, value: T) -> MetricState<T> {
    if sequence == 0 {
        MetricState::WarmingUp
    } else {
        MetricState::Available(value)
    }
}

fn fake_io(sequence: u64, capability: CapabilityState, rss_bytes: u64) -> ProcessIo {
    match capability {
        CapabilityState::PermissionDenied => ProcessIo {
            read: MetricState::PermissionDenied,
            write: MetricState::PermissionDenied,
            read_total_bytes: MetricState::PermissionDenied,
            write_total_bytes: MetricState::PermissionDenied,
        },
        CapabilityState::Unsupported | CapabilityState::Unknown => ProcessIo::UNSUPPORTED,
        CapabilityState::Available => {
            if sequence == 0 {
                return ProcessIo::WARMING_UP;
            }
            // Deterministically derived from RSS so busier processes read busier.
            let scale = (rss_bytes / MIB).max(1) as f64;
            ProcessIo {
                read: rate_state(sequence, scale * 8_192.0),
                write: rate_state(sequence, scale * 16_384.0),
                read_total_bytes: MetricState::Available(rss_bytes.saturating_mul(sequence)),
                write_total_bytes: MetricState::Available(rss_bytes.saturating_mul(2 * sequence)),
            }
        }
    }
}

impl SnapshotSource for FakeCollector {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn capabilities(&self) -> CapabilitySnapshot {
        self.capabilities_for()
    }

    fn sample(&mut self, tick: &SampleTick) -> Result<SystemSnapshot, CollectorError> {
        if self.scenario.fail_at == Some(tick.sequence) {
            return Err(CollectorError::Refresh {
                group: "processes",
                reason: "scenario-injected failure".into(),
            });
        }
        if !self.scenario.collect_delay.is_zero() {
            std::thread::sleep(self.scenario.collect_delay);
        }

        let sequence = tick.sequence;
        Ok(SystemSnapshot {
            sequence,
            captured_at: tick.captured_at,
            wall_time: tick.wall_time,
            elapsed: tick.elapsed,
            host: self.host(sequence),
            cpu: self.cpu(sequence),
            memory: self.memory(sequence),
            load: counter_state(
                sequence,
                LoadSnapshot {
                    one: 4.12,
                    five: 3.84,
                    fifteen: 3.21,
                },
            ),
            processes: self.processes(sequence),
            disks: self.disks(sequence),
            filesystems: self.filesystems(),
            networks: self.networks(sequence),
            // Pressure is evaluated by the diagnostic engine over the snapshot
            // and its history, which is a core concern rather than a collector
            // one. A collector never derives a pressure state.
            pressure: PressureSnapshot::warming_up(),
            sensors: self.sensors(sequence),
            capabilities: self.capabilities_for(),
            health: CollectorHealth::default(),
        })
    }

    fn process_detail(&mut self, identity: ProcessIdentity) -> ProcessDetailResult {
        let Some(process) = self
            .scenario
            .processes
            .iter()
            .find(|process| process.identity.pid == identity.pid)
        else {
            return ProcessDetailResult::Vanished(identity);
        };
        if process.identity != identity {
            return ProcessDetailResult::Reused {
                requested: identity,
                found: process.identity,
            };
        }

        let children: Vec<ProcessIdentity> = self
            .scenario
            .processes
            .iter()
            .filter(|candidate| candidate.parent_pid == Some(identity.pid))
            .map(|candidate| candidate.identity)
            .collect();
        let ancestry = process
            .parent_pid
            .and_then(|ppid| {
                self.scenario
                    .processes
                    .iter()
                    .find(|candidate| candidate.identity.pid == ppid)
            })
            .map(|parent| {
                vec![AncestorEntry {
                    identity: parent.identity,
                    name: parent.name.clone(),
                }]
            });

        let mut detail = ProcessDetail::pending(identity, SystemTime::UNIX_EPOCH);
        detail.working_directory = MetricState::Available("/Users/dev/pgit/monitrs".into());
        detail.root = MetricState::Available("/".into());
        let descriptors = fake_descriptors(self.scenario.process_open_files);
        detail.open_files = descriptors.count;
        detail.sockets = descriptors.sockets;
        detail.open_file_list = descriptors.list;
        detail.descendants = MetricState::Available(u32::try_from(children.len()).unwrap_or(0));
        detail.children = MetricState::Available(children);
        detail.ancestry =
            ancestry.map_or(MetricState::Available(Vec::new()), MetricState::Available);
        detail.nice = MetricState::Available(0);
        detail.cgroup = MetricState::Unsupported;
        detail.container = MetricState::Unsupported;
        ProcessDetailResult::Loaded(Box::new(detail))
    }
}

/// Everything one scenario says about a process's descriptors.
#[derive(Clone, Debug)]
struct FakeDescriptors {
    count: MetricState<u32>,
    sockets: MetricState<u32>,
    list: MetricState<OpenFileList>,
}

/// The descriptors a scenario produces for its selected process (§7.2).
///
/// The three states are kept consistent with each other, because a real collector
/// cannot produce an arbitrary mixture:
///
/// * `Available` — everything readable. The list is deliberately not a tidy row of
///   files: it holds a pipe and a socket, whose paths are `Unsupported` because they
///   have none; a descriptor the OS refused, which is `PermissionDenied`; and a total
///   larger than the number of entries, so the "did not list" row is exercised.
/// * `PermissionDenied` — another user's process. The count is refused along with the
///   list, because the same read produces both.
/// * `Unsupported` — a platform that can *count* descriptors but not name them, which
///   is the case [`monitrs_core::model::ProcessDetail::open_file_list`] exists to
///   express separately from the count.
fn fake_descriptors(capability: CapabilityState) -> FakeDescriptors {
    let list = match capability {
        CapabilityState::Available => MetricState::Available(OpenFileList::listed(
            vec![
                OpenFileEntry {
                    descriptor: 0,
                    kind: OpenFileKind::File,
                    path: MetricState::Available("/dev/null".into()),
                },
                OpenFileEntry {
                    descriptor: 1,
                    kind: OpenFileKind::Pipe,
                    path: MetricState::Unsupported,
                },
                OpenFileEntry {
                    descriptor: 4,
                    kind: OpenFileKind::File,
                    path: MetricState::Available(
                        "/Users/dev/pgit/monitrs/target/debug/deps/libmonitrs_core.rlib".into(),
                    ),
                },
                OpenFileEntry {
                    descriptor: 5,
                    kind: OpenFileKind::Socket,
                    path: MetricState::Unsupported,
                },
                OpenFileEntry {
                    descriptor: 9,
                    kind: OpenFileKind::File,
                    path: MetricState::PermissionDenied,
                },
                OpenFileEntry {
                    descriptor: 12,
                    kind: OpenFileKind::EventQueue,
                    path: MetricState::Unsupported,
                },
            ],
            42,
        )),
        CapabilityState::PermissionDenied => MetricState::PermissionDenied,
        CapabilityState::Unsupported => MetricState::Unsupported,
        // A capability nobody has probed cannot have produced a listing either.
        CapabilityState::Unknown => MetricState::WarmingUp,
    };
    let (count, sockets) = if capability == CapabilityState::PermissionDenied {
        (MetricState::PermissionDenied, MetricState::PermissionDenied)
    } else {
        (MetricState::Available(42), MetricState::Available(3))
    };
    FakeDescriptors {
        count,
        sockets,
        list,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn collect(collector: &mut FakeCollector, count: u64) -> Vec<SystemSnapshot> {
        let start = Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        let mut snapshots = Vec::new();
        for index in 0..count {
            if index > 0 {
                tick = tick.advance(
                    start + Duration::from_secs(index),
                    SystemTime::UNIX_EPOCH,
                    crate::tier::DueTiers::ALL,
                );
            }
            match collector.sample(&tick) {
                Ok(snapshot) => snapshots.push(snapshot),
                Err(_) => continue,
            }
        }
        snapshots
    }

    #[test]
    fn the_first_snapshot_is_warming_up_not_zero() {
        let mut collector = FakeCollector::default();
        let snapshots = collect(&mut collector, 1);
        let first = snapshots.first().expect("one snapshot");

        assert!(first.cpu.total.is_warming_up());
        assert!(first.load.is_warming_up());
        assert!(!first.has_valid_interval());
        for process in &first.processes {
            assert!(
                process.cpu.is_warming_up(),
                "{} should warm up",
                process.name
            );
            assert!(process.io.read.is_warming_up());
        }
    }

    #[test]
    fn the_second_snapshot_has_real_values() {
        let mut collector = FakeCollector::default();
        let snapshots = collect(&mut collector, 2);
        let second = snapshots.get(1).expect("two snapshots");

        assert!(second.has_valid_interval());
        assert_eq!(second.elapsed, Duration::from_secs(1));
        assert!(second.cpu.total.fresh().is_some());
        assert!(second.load.fresh().is_some());
    }

    #[test]
    fn nothing_in_a_snapshot_depends_on_the_host_it_runs_on() {
        // The whole value of this collector is that a frame rendered from it is the
        // same frame everywhere. `arch` was read from `env::consts::ARCH` once, which
        // made every UI snapshot fail on a CI runner with a different architecture.
        let mut collector = FakeCollector::default();
        let snapshots = collect(&mut collector, 2);
        let host = &snapshots.last().expect("snapshots").host;
        assert_eq!(
            host.arch, "aarch64",
            "the fake host must not report the real one"
        );
        assert_eq!(
            host.hostname.fresh().map(|name| &**name),
            Some("dev-mbp"),
            "and neither must the hostname"
        );
    }

    #[test]
    fn snapshots_are_deterministic_across_collectors() {
        let mut first = FakeCollector::default();
        let mut second = FakeCollector::default();
        let a = collect(&mut first, 30);
        let b = collect(&mut second, 30);

        for (left, right) in a.iter().zip(b.iter()) {
            assert_eq!(
                left.cpu.total, right.cpu.total,
                "sequence {}",
                left.sequence
            );
            assert_eq!(left.processes.len(), right.processes.len());
            for (lp, rp) in left.processes.iter().zip(right.processes.iter()) {
                assert_eq!(lp.identity, rp.identity);
                assert_eq!(lp.cpu, rp.cpu);
            }
        }
    }

    #[test]
    fn a_spike_pattern_produces_exactly_one_peak() {
        let pattern = Pattern::Spike {
            base: 10.0,
            peak: 91.0,
            at: 20,
        };
        let peaks: Vec<u64> = (1..60)
            .filter(|sequence| {
                pattern
                    .at(*sequence)
                    .fresh()
                    .is_some_and(|p| p.value() > 50.0)
            })
            .collect();
        assert_eq!(peaks, vec![20]);
    }

    #[test]
    fn a_sawtooth_stays_within_its_bounds_and_actually_varies() {
        let pattern = Pattern::Sawtooth {
            low: 12.0,
            high: 91.0,
            period: 40,
        };
        let values: Vec<f32> = (1..200)
            .filter_map(|s| pattern.at(s).fresh().map(|p| p.value()))
            .collect();
        assert!(
            values.iter().all(|v| (12.0..=91.0).contains(v)),
            "out of bounds"
        );
        let min = values.iter().copied().fold(f32::MAX, f32::min);
        let max = values.iter().copied().fold(f32::MIN, f32::max);
        assert!(max - min > 50.0, "sawtooth barely moved: {min}..{max}");
    }

    #[test]
    fn a_zero_period_sawtooth_does_not_divide_by_zero() {
        let pattern = Pattern::Sawtooth {
            low: 5.0,
            high: 50.0,
            period: 0,
        };
        for sequence in 1..10 {
            let value = pattern.at(sequence).fresh().map(|p| p.value());
            assert_eq!(value, Some(5.0));
        }
    }

    #[test]
    fn a_process_that_exits_leaves_the_table() {
        let scenario = Scenario {
            processes: vec![
                FakeProcess::new(100, 1, "keeper", "keeper"),
                FakeProcess::new(200, 2, "leaver", "leaver").exiting_at(3),
            ],
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let snapshots = collect(&mut collector, 5);

        let present_at = |index: usize| {
            snapshots
                .get(index)
                .expect("snapshot")
                .process_by_pid(200)
                .is_some()
        };
        assert!(present_at(2), "still present just before exiting");
        assert!(!present_at(3), "gone at the exit sequence");
        assert!(!present_at(4));
        assert!(
            snapshots
                .get(4)
                .expect("snapshot")
                .process_by_pid(100)
                .is_some()
        );
    }

    #[test]
    fn a_reused_pid_appears_with_a_different_identity_and_warms_up_again() {
        let scenario = Scenario {
            processes: vec![
                FakeProcess::new(31_842, 900_100, "rustc", "rustc")
                    .with_cpu(Pattern::Steady(287.0))
                    .exiting_at(3)
                    .reused_as(977_400),
            ],
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let snapshots = collect(&mut collector, 5);

        let original = ProcessIdentity::new(31_842, 900_100);
        let recycled = ProcessIdentity::new(31_842, 977_400);

        assert!(
            snapshots
                .get(2)
                .expect("snapshot")
                .process(original)
                .is_some()
        );
        let after = snapshots.get(4).expect("snapshot");
        assert!(
            after.process(original).is_none(),
            "the original must not resolve"
        );
        let taken_over = after.process(recycled).expect("the PID was reused");
        assert!(
            taken_over.cpu.is_warming_up(),
            "a different process behind the same PID starts warming up"
        );
        assert!(recycled.is_reuse_of(&original));
    }

    #[test]
    fn permission_denied_metrics_are_denied_rather_than_zero() {
        let mut collector = FakeCollector::new(Scenario::permission_denied());
        let snapshots = collect(&mut collector, 3);
        let latest = snapshots.last().expect("snapshots");

        assert_eq!(latest.cpu.total, MetricState::PermissionDenied);
        for process in &latest.processes {
            assert_eq!(process.io.read, MetricState::PermissionDenied);
            assert!(process.io.read.fresh().is_none());
        }
        assert!(latest.capabilities.any_permission_denied());
    }

    #[test]
    fn an_empty_scenario_is_a_real_state_not_a_failure() {
        let mut collector = FakeCollector::new(Scenario::empty());
        let snapshots = collect(&mut collector, 2);
        let latest = snapshots.last().expect("snapshots");
        assert_eq!(latest.process_count(), 0);
        // The system itself is still measurable.
        assert!(latest.cpu.total.fresh().is_some());
    }

    #[test]
    fn no_link_speed_means_no_utilization_percentage() {
        let mut collector = FakeCollector::new(Scenario::default());
        let snapshots = collect(&mut collector, 2);
        let latest = snapshots.last().expect("snapshots");
        let en0 = latest.networks.first().expect("en0");

        assert!(en0.rx.fresh().is_some(), "throughput is known");
        assert_eq!(
            en0.utilization(),
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown)
        );
    }

    #[test]
    fn a_known_link_speed_yields_a_utilization_percentage() {
        let scenario = Scenario {
            link_speed_mbps: Some(1_000),
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let snapshots = collect(&mut collector, 2);
        let en0 = snapshots
            .last()
            .expect("snapshots")
            .networks
            .first()
            .expect("en0");
        assert!(en0.utilization().fresh().is_some());
    }

    #[test]
    fn stale_values_are_marked_and_carry_an_age() {
        let scenario = Scenario {
            stale_from: Some(3),
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let snapshots = collect(&mut collector, 6);

        assert!(snapshots.get(2).expect("fresh").cpu.total.is_available());
        let stale = &snapshots.get(5).expect("stale").cpu.total;
        assert!(stale.is_stale());
        assert!(stale.fresh().is_none(), "stale must not feed a calculation");
        let (_, age) = stale.displayable().expect("displayable with an age");
        assert!(age >= Duration::from_secs(1));
    }

    #[test]
    fn an_injected_failure_is_recoverable() {
        let scenario = Scenario {
            fail_at: Some(2),
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let snapshots = collect(&mut collector, 5);
        // One sample lost, the rest fine.
        assert_eq!(snapshots.len(), 4);
        let sequences: Vec<u64> = snapshots.iter().map(|s| s.sequence).collect();
        assert_eq!(sequences, vec![0, 1, 3, 4]);
    }

    #[test]
    fn swap_disabled_reports_zero_of_zero_but_no_percentage() {
        let scenario = Scenario {
            swap_total_bytes: 0,
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let snapshots = collect(&mut collector, 2);
        let swap = snapshots.last().expect("snapshots").memory.swap;

        assert!(!swap.is_enabled());
        assert_eq!(swap.used.fresh(), Some(&0));
        assert!(
            swap.usage.fresh().is_none(),
            "a percentage of zero capacity is undefined"
        );
    }

    #[test]
    fn a_containerised_scenario_exposes_the_cgroup_limit_beside_the_host_total() {
        let mut collector = FakeCollector::new(Scenario::containerised());
        let snapshots = collect(&mut collector, 2);
        let memory = snapshots.last().expect("snapshots").memory;

        assert_eq!(memory.total_bytes, 32 * GIB, "the host total stays visible");
        assert_eq!(memory.cgroup_limit_bytes.fresh(), Some(&(2 * GIB)));
        assert_eq!(memory.effective_limit_bytes(), 2 * GIB);
    }

    #[test]
    fn per_core_values_have_structure_and_stay_within_bounds() {
        let mut collector = FakeCollector::new(Scenario::many_cores());
        let snapshots = collect(&mut collector, 3);
        let cores = snapshots
            .last()
            .expect("snapshots")
            .cpu
            .per_core
            .fresh()
            .expect("per-core available");

        assert_eq!(cores.len(), 256);
        assert!(
            cores
                .iter()
                .all(|core| (0.0..=100.0).contains(&core.busy.value()))
        );
        let distinct = cores
            .iter()
            .map(|core| core.busy.value().to_bits())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            distinct.len() > 1,
            "per-core view should not be N identical bars"
        );
    }

    #[test]
    fn a_large_scenario_produces_the_requested_process_count() {
        let mut collector = FakeCollector::new(Scenario::with_process_count(10_000));
        let snapshots = collect(&mut collector, 2);
        assert_eq!(snapshots.last().expect("snapshots").process_count(), 10_000);
    }

    #[test]
    fn detail_lookup_reports_vanished_and_reused_rather_than_erroring() {
        let scenario = Scenario {
            processes: vec![FakeProcess::new(100, 1, "keeper", "keeper")],
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);

        let present = ProcessIdentity::new(100, 1);
        assert!(matches!(
            collector.process_detail(present),
            ProcessDetailResult::Loaded(_)
        ));
        assert!(matches!(
            collector.process_detail(ProcessIdentity::new(999, 1)),
            ProcessDetailResult::Vanished(_)
        ));
        assert!(matches!(
            collector.process_detail(ProcessIdentity::new(100, 42)),
            ProcessDetailResult::Reused { .. }
        ));
    }

    #[test]
    fn detail_never_exposes_environment_values() {
        // The type has no environment field at all, which is the point: §7.5's
        // rule is enforced by the model rather than by a display filter.
        let mut collector = FakeCollector::default();
        let detail = collector.process_detail(ProcessIdentity::new(31_842, 900_100));
        let ProcessDetailResult::Loaded(detail) = detail else {
            panic!("expected a loaded detail");
        };
        assert!(detail.working_directory.fresh().is_some());
    }

    #[test]
    fn the_reference_scenario_matches_the_specification_mockup() {
        let mut collector = FakeCollector::default();
        let snapshots = collect(&mut collector, 21);
        let at_spike = snapshots.get(20).expect("21 snapshots");

        let rustc = at_spike
            .process(ProcessIdentity::new(31_842, 900_100))
            .expect("rustc is in the reference scenario");
        let cpu = rustc.cpu.fresh().expect("measured");
        assert!(
            (cpu.value() - 287.0).abs() < 0.01,
            "§5.5 shows rustc at 287%"
        );
        assert!(
            cpu.value() > 100.0,
            "core normalization allows exceeding 100%"
        );

        let postgres = at_spike
            .process(ProcessIdentity::new(1_221, 700_050))
            .expect("postgres is in the reference scenario");
        assert!((postgres.cpu.fresh().expect("measured").value() - 54.0).abs() < 0.01);
    }

    #[test]
    fn a_collect_delay_is_honoured_so_responsiveness_can_be_tested() {
        let scenario = Scenario {
            collect_delay: Duration::from_millis(30),
            ..Scenario::default()
        };
        let mut collector = FakeCollector::new(scenario);
        let start = Instant::now();
        let _ = collect(&mut collector, 3);
        assert!(start.elapsed() >= Duration::from_millis(90));
    }

    #[test]
    fn a_collector_never_derives_a_pressure_state() {
        // Pressure is a core concern evaluated over snapshot plus history; a
        // collector that invented one would be deciding policy.
        let mut collector = FakeCollector::default();
        let snapshots = collect(&mut collector, 5);
        for snapshot in &snapshots {
            for signal in &snapshot.pressure.signals {
                assert!(
                    signal.state.fresh().is_none(),
                    "{:?} was pre-decided",
                    signal.id
                );
            }
        }
    }
}

//! CPU and load metrics.
//!
//! §8.3 fixes the semantics: *system* CPU is aggregate machine usage in
//! `0..=100`, while *process* CPU defaults to "one core = 100%" and may exceed
//! 100% for a multi-threaded process.

use crate::model::MetricState;
use crate::units::Percent;

/// Aggregate or per-core CPU utilization.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CpuUsage {
    /// Non-idle time as a share of elapsed time, in `0..=100`.
    pub busy: Percent,
    /// The `/proc/stat`-style split, where the platform exposes it.
    pub breakdown: MetricState<CpuBreakdown>,
}

impl CpuUsage {
    /// Builds a usage value with no breakdown available.
    #[must_use]
    pub const fn plain(busy: Percent) -> Self {
        Self {
            busy,
            breakdown: MetricState::Unsupported,
        }
    }
}

/// A cgroup CPU quota: how much CPU time the group may use per period.
///
/// Constructed only from a period that can produce a meaningful ratio, so there is no
/// representable quota that divides by zero or yields a non-finite core count. An
/// *unlimited* group is not a `CpuQuota` at all — it is
/// [`MetricState::Unsupported`] on [`CpuSnapshot::cgroup_quota`], mirroring how
/// `memory.max` reading `max` becomes unsupported rather than `u64::MAX`.
///
/// Both raw figures are kept, not just the derived core count: someone debugging why
/// their container is being throttled wants to see the `100000 200000` they configured,
/// and deriving it back from `2.0` would lose the period.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CpuQuota {
    /// Microseconds of CPU time allowed per period.
    quota_us: u64,
    /// The accounting period in microseconds.
    period_us: u64,
}

impl CpuQuota {
    /// Builds a quota, or `None` when it could not describe a real ceiling.
    ///
    /// Rejects a zero period — division by zero — and a ratio that is not finite. A zero
    /// *quota* is accepted: a group allowed no CPU time at all is a real, if hostile,
    /// configuration, and reporting it as absent would hide it.
    #[must_use]
    pub fn new(quota_us: u64, period_us: u64) -> Option<Self> {
        if period_us == 0 {
            return None;
        }
        let quota = Self {
            quota_us,
            period_us,
        };
        quota.cores().is_finite().then_some(quota)
    }

    /// The ceiling as a number of CPUs, e.g. `1.5`.
    #[must_use]
    pub fn cores(&self) -> f32 {
        // Narrowing to f32 for a figure displayed with one decimal; the ratio of two
        // microsecond counts cannot exceed f32's range in any real configuration.
        #[allow(clippy::cast_precision_loss)]
        let cores = self.quota_us as f64 / self.period_us as f64;
        #[allow(clippy::cast_possible_truncation)]
        let cores = cores as f32;
        cores
    }

    /// Microseconds of CPU time allowed per period, as configured.
    #[must_use]
    pub const fn quota_us(&self) -> u64 {
        self.quota_us
    }

    /// The accounting period in microseconds, as configured.
    #[must_use]
    pub const fn period_us(&self) -> u64 {
        self.period_us
    }
}

/// A class of logical CPUs that differ in kind, not just in load.
///
/// Apple Silicon has performance and efficiency cores; big.LITTLE ARM machines have
/// the same split under other names. It matters for reading a per-core view: four
/// efficiency cores at 90% and four performance cores idle is a machine doing very
/// little, and the opposite is a machine working hard — the same eight numbers.
///
/// The name comes from the platform (`hw.perflevelN.name` on macOS) rather than from
/// a table here, because inventing the vocabulary would mean guessing at hardware
/// this code has never seen.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CoreClass {
    /// What the platform calls it, e.g. `Performance` or `Efficiency`.
    pub name: Box<str>,
    /// The logical CPUs in this class, as indices into
    /// [`CpuSnapshot::per_core`].
    ///
    /// Indices rather than a count, because a renderer needs to know *which* cores
    /// they are to colour or group them, and a count would force it to assume the
    /// classes are contiguous.
    pub logical: Vec<u16>,
    /// Physical cores in this class, where the platform reports it separately.
    pub physical_count: Option<u16>,
}

impl CoreClass {
    /// How many logical CPUs this class holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.logical.len()
    }

    /// Whether the class holds no CPUs, which a platform should never report.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.logical.is_empty()
    }
}

/// The per-state split of CPU time.
///
/// macOS exposes only `user`, `system`, `nice`, and `idle`; the Linux-only
/// fields are [`MetricState::Unsupported`] there rather than zero (§4).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CpuBreakdown {
    /// Time in user mode.
    pub user: Percent,
    /// Time in kernel mode.
    pub system: Percent,
    /// Time in low-priority user mode.
    pub nice: Percent,
    /// Idle time.
    pub idle: Percent,
    /// Time waiting on I/O. Linux only.
    pub iowait: MetricState<Percent>,
    /// Time servicing hardware interrupts. Linux only.
    pub irq: MetricState<Percent>,
    /// Time servicing soft interrupts. Linux only.
    pub softirq: MetricState<Percent>,
    /// Time stolen by the hypervisor. Linux only, and the most useful signal
    /// that a VM is oversubscribed.
    pub steal: MetricState<Percent>,
}

/// How process CPU percentages are scaled (§8.3).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "lowercase"))]
pub enum CpuNormalization {
    /// One core = 100%. A process using four cores fully reads 400%.
    ///
    /// The default, matching `top` and `htop`.
    #[default]
    Core,
    /// The whole machine = 100%. A process using four of eight cores reads 50%.
    Machine,
}

impl CpuNormalization {
    /// The documentation string shown in help and `docs/metrics.md` (§8.3).
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::Core => "one core = 100%, so a multi-threaded process may exceed 100%",
            Self::Machine => "the whole machine = 100%, so no process exceeds 100%",
        }
    }

    /// Converts a core-normalized percentage into this convention.
    ///
    /// Returns `None` when `logical_cpus` is zero, because there is no defined
    /// machine share to scale against.
    #[must_use]
    pub fn apply(self, core_normalized: Percent, logical_cpus: u16) -> Option<Percent> {
        match self {
            Self::Core => Some(core_normalized),
            Self::Machine => {
                if logical_cpus == 0 {
                    return None;
                }
                Percent::new(core_normalized.value() / f32::from(logical_cpus))
            }
        }
    }
}

/// System-wide CPU state.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CpuSnapshot {
    /// Logical CPU count, including SMT siblings. Always known.
    pub logical_count: u16,
    /// Physical core count, where the platform reports it.
    pub physical_count: MetricState<u16>,
    /// Aggregate machine utilization, `0..=100` (§8.3).
    pub total: MetricState<CpuUsage>,
    /// Per-logical-CPU utilization, in stable index order.
    pub per_core: MetricState<Vec<CpuUsage>>,
    /// Current clock, where reported.
    pub frequency_mhz: MetricState<u64>,
    /// The CPU ceiling a cgroup imposes, **beside** the host's CPU count.
    ///
    /// §9.2 requires a container limit to be reported separately from the host total, so
    /// `logical_count` stays the machine's real CPU count and this is the ceiling that
    /// actually applies to the processes in it. A container limited to 1.5 CPUs on a
    /// 64-CPU host is not "2% of the machine"; it is a hard wall a process will be
    /// throttled against, and a monitor that showed only the 64 would be describing a
    /// machine the user does not have.
    ///
    /// [`MetricState::Unsupported`] where no quota is configured — `cpu.max` reading
    /// `max` is *not* a very large number, it is the absence of a limit — and on every
    /// platform without cgroups.
    pub cgroup_quota: MetricState<CpuQuota>,
    /// The machine's core classes, where the platform names them.
    ///
    /// Empty where there is one class or none is reported, which is the honest
    /// answer for a homogeneous machine — an empty list is not "unknown", it is
    /// "there is nothing to distinguish". `MetricState` is deliberately not used:
    /// this is topology, fixed for the life of the machine, and a topology that
    /// could be `WarmingUp` would invite a renderer to wait for it.
    pub core_classes: Vec<CoreClass>,
}

impl CpuSnapshot {
    /// The number of CPUs that actually applies to processes here.
    ///
    /// The cgroup quota where one is configured and below the host's CPU count, the host
    /// count otherwise. This is the divisor a load average should be read against and the
    /// ceiling a per-core view is bounded by — the exact counterpart of
    /// [`MemorySnapshot::effective_limit_bytes`](crate::model::MemorySnapshot::effective_limit_bytes),
    /// and required by §9.2 for the same reason.
    ///
    /// A quota *above* the host count is ignored rather than reported: a group allowed
    /// more CPU than the machine has is a configuration artefact, not a ceiling, and
    /// dividing a load average by it would understate the pressure.
    ///
    /// A *stale* reading counts here, which is the one place this crate deliberately
    /// breaks [`MetricState::fresh`]'s "use fresh values for calculations" rule. A limit
    /// is configuration, not a measurement: if the last successful read said 1.5 CPUs and
    /// this tick's read failed, the group is still limited to 1.5 CPUs, and falling back
    /// to the host's 64 would present a machine 42 times larger than the one the process
    /// is actually being throttled against. Keeping the retained value is wrong only if
    /// the limit changed in the last few seconds, and the row is marked stale either way.
    #[must_use]
    pub fn effective_cores(&self) -> f32 {
        let host = f32::from(self.logical_count);
        match self.cgroup_quota.displayable() {
            Some((quota, _)) if quota.cores() > 0.0 && quota.cores() < host => quota.cores(),
            _ => host,
        }
    }

    /// Whether a cgroup quota, rather than the hardware, is the ceiling here.
    ///
    /// What a renderer checks before showing the host CPU count unqualified. Stale
    /// counts, for the reason [`Self::effective_cores`] gives.
    #[must_use]
    pub fn is_cpu_limited(&self) -> bool {
        self.cgroup_quota.displayable().is_some_and(|(quota, _)| {
            quota.cores() > 0.0 && quota.cores() < f32::from(self.logical_count)
        })
    }

    /// A snapshot with no measurements yet, for the first frame.
    #[must_use]
    pub const fn warming_up(logical_count: u16) -> Self {
        Self {
            logical_count,
            physical_count: MetricState::WarmingUp,
            total: MetricState::WarmingUp,
            per_core: MetricState::WarmingUp,
            frequency_mhz: MetricState::WarmingUp,
            cgroup_quota: MetricState::WarmingUp,
            // Empty rather than warming up: topology is not measured, it is read
            // once, and a collector that knows of no classes is reporting a fact.
            core_classes: Vec::new(),
        }
    }
}

/// Load averages, which are run-queue lengths rather than percentages.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct LoadSnapshot {
    /// One-minute average.
    pub one: f32,
    /// Five-minute average.
    pub five: f32,
    /// Fifteen-minute average.
    pub fifteen: f32,
}

impl LoadSnapshot {
    /// The one-minute load expressed per logical CPU.
    ///
    /// This is the only form in which load can be compared across machines, and
    /// it is what the `load high relative to logical CPU count` rule uses
    /// (§11.2). Returns `None` when the CPU count is unknown.
    #[must_use]
    pub fn per_cpu(&self, logical_cpus: u16) -> Option<f32> {
        if logical_cpus == 0 {
            return None;
        }
        let value = self.one / f32::from(logical_cpus);
        value.is_finite().then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use super::*;
    use crate::model::UnavailableReason;

    /// Whether two core counts are the same figure.
    ///
    /// `assert_eq!` on an `f32` is a clippy error and rightly so, but these are exact
    /// integers-as-floats: the host count comes from a `u16` and the quotas below divide
    /// exactly. An epsilon comparison states that plainly.
    fn same(left: f32, right: f32) -> bool {
        (left - right).abs() < f32::EPSILON
    }

    #[test]
    fn a_cgroup_quota_below_the_host_count_becomes_the_effective_ceiling() {
        // §9.2: the host's CPUs and the group's ceiling are both observable, and the
        // ceiling is the one a load average should be read against.
        let mut cpu = CpuSnapshot::warming_up(64);
        assert!(same(cpu.effective_cores(), 64.0));
        assert!(!cpu.is_cpu_limited());

        cpu.cgroup_quota = MetricState::Available(CpuQuota::new(150_000, 100_000).expect("1.5"));
        assert!(same(cpu.effective_cores(), 1.5));
        assert!(cpu.is_cpu_limited());
        assert_eq!(cpu.logical_count, 64, "the host count is untouched");
    }

    #[test]
    fn a_quota_above_the_host_count_is_not_a_ceiling() {
        // A group allowed 128 CPUs on a 64-CPU machine is a configuration artefact.
        // Reporting it as the ceiling would halve every load figure read against it.
        let mut cpu = CpuSnapshot::warming_up(64);
        cpu.cgroup_quota = MetricState::Available(CpuQuota::new(12_800_000, 100_000).expect("128"));
        assert!(same(cpu.effective_cores(), 64.0));
        assert!(!cpu.is_cpu_limited());
    }

    #[test]
    fn an_unavailable_quota_leaves_the_host_count_as_the_ceiling() {
        // Every kind of nothing, because §4's states must not each need their own
        // caller-side special case. Unsupported is the ordinary case off Linux.
        let mut cpu = CpuSnapshot::warming_up(8);
        for state in [
            MetricState::Unsupported,
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
        ] {
            cpu.cgroup_quota = state;
            assert!(same(cpu.effective_cores(), 8.0), "{state:?}");
            assert!(!cpu.is_cpu_limited(), "{state:?}");
        }
    }

    #[test]
    fn a_stale_quota_still_bounds_the_machine() {
        // A limit is configuration, not a measurement: a reading a minute old is still
        // the wall processes are hitting, and falling back to the host count would
        // silently widen the machine.
        let mut cpu = CpuSnapshot::warming_up(16);
        cpu.cgroup_quota = MetricState::Stale {
            value: CpuQuota::new(200_000, 100_000).expect("2.0"),
            age: Duration::from_secs(45),
        };
        assert!(same(cpu.effective_cores(), 2.0));
        assert!(cpu.is_cpu_limited());
    }

    #[test]
    fn a_quota_cannot_be_built_from_a_period_it_cannot_divide_by() {
        // Unrepresentable rather than checked at every use site.
        assert!(CpuQuota::new(100_000, 0).is_none());
        // A group allowed no CPU time at all is hostile but real, and hiding it would
        // report an unrestricted machine.
        let starved = CpuQuota::new(0, 100_000).expect("zero quota is a real limit");
        assert!(same(starved.cores(), 0.0));
        // …but it is not a *ceiling* below the host count in any useful sense, so the
        // effective figure stays the machine's rather than becoming zero cores.
        let mut cpu = CpuSnapshot::warming_up(4);
        cpu.cgroup_quota = MetricState::Available(starved);
        assert!(same(cpu.effective_cores(), 4.0));
    }

    #[test]
    fn a_quota_keeps_the_figures_it_was_configured_with() {
        // The ratio is what a view shows; the pair is what someone comparing this with
        // `cpu.max` needs, and deriving it back from 1.5 would lose the period.
        let quota = CpuQuota::new(150_000, 100_000).expect("1.5");
        assert_eq!((quota.quota_us(), quota.period_us()), (150_000, 100_000));
    }

    #[test]
    fn core_normalization_is_the_identity() {
        let cpu = Percent::new(287.0).expect("valid");
        let out = CpuNormalization::Core.apply(cpu, 8).expect("valid");
        assert!((out.value() - 287.0).abs() < f32::EPSILON);
    }

    #[test]
    fn machine_normalization_divides_by_the_logical_cpu_count() {
        let cpu = Percent::new(400.0).expect("valid");
        let out = CpuNormalization::Machine.apply(cpu, 8).expect("valid");
        assert!((out.value() - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn machine_normalization_is_undefined_without_a_cpu_count() {
        let cpu = Percent::new(400.0).expect("valid");
        assert!(CpuNormalization::Machine.apply(cpu, 0).is_none());
    }

    #[test]
    fn the_default_convention_is_documented_as_per_core() {
        assert_eq!(CpuNormalization::default(), CpuNormalization::Core);
        assert!(
            CpuNormalization::default()
                .description()
                .contains("exceed 100%")
        );
    }

    #[test]
    fn load_per_cpu_is_undefined_without_a_cpu_count() {
        let load = LoadSnapshot {
            one: 11.4,
            five: 8.0,
            fifteen: 4.0,
        };
        assert!(load.per_cpu(0).is_none());
        let per_cpu = load.per_cpu(8).expect("valid");
        assert!((per_cpu - 1.425).abs() < 0.001);
    }

    #[test]
    fn a_warming_up_cpu_snapshot_reports_no_utilization() {
        let cpu = CpuSnapshot::warming_up(8);
        assert_eq!(cpu.logical_count, 8);
        assert!(cpu.total.fresh().is_none());
        assert!(cpu.total.is_warming_up());
    }
}

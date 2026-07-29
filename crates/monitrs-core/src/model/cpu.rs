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
}

impl CpuSnapshot {
    /// A snapshot with no measurements yet, for the first frame.
    #[must_use]
    pub const fn warming_up(logical_count: u16) -> Self {
        Self {
            logical_count,
            physical_count: MetricState::WarmingUp,
            total: MetricState::WarmingUp,
            per_core: MetricState::WarmingUp,
            frequency_mhz: MetricState::WarmingUp,
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
    use super::*;

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

//! Memory and swap metrics.
//!
//! §8.4 and §26 both insist that Linux and macOS memory semantics are *not*
//! equivalent. Rather than papering over the difference, every snapshot records
//! which definition produced its headline numbers in [`MemorySemantics`], and
//! the Inspect screen shows it.

use crate::model::MetricState;
use crate::units::{Percent, Rate};

/// Which platform definition produced the headline memory numbers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MemorySemantics {
    /// `available` is `/proc/meminfo`'s `MemAvailable`, the kernel's own
    /// estimate of allocatable memory without swapping. `used` is
    /// `total - MemAvailable`, so page cache is *not* counted as application use.
    LinuxMemAvailable,
    /// `available` is derived from `host_statistics64` free plus inactive plus
    /// purgeable pages. Wired and compressed pages are reported separately
    /// because neither is reclaimable the way Linux page cache is.
    MacosVmStatistics,
    /// The cross-platform baseline reported by `sysinfo`, used when native
    /// enrichment is unavailable. Coarser than either native definition.
    SysinfoBaseline,
}

impl MemorySemantics {
    /// The explanation rendered on the Inspect screen and in `docs/metrics.md`.
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::LinuxMemAvailable => {
                "used = total - MemAvailable; page cache and buffers are not counted as \
                 application use"
            }
            Self::MacosVmStatistics => {
                "available = free + inactive + purgeable; wired and compressed pages are \
                 reported separately and are not reclaimable like Linux page cache"
            }
            Self::SysinfoBaseline => {
                "cross-platform baseline; coarser than the native definition and not \
                 byte-for-byte comparable with it"
            }
        }
    }
}

/// The secondary memory breakdown.
///
/// Every field is a [`MetricState`] because the two platforms expose disjoint
/// subsets: `buffers` is Linux-only, `wired` and `compressed` are macOS-only.
/// §8.4 forbids labelling all non-free memory as application use, so these are
/// presented as detail rather than folded into `used`.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct MemoryDetail {
    /// Page cache.
    pub cached: MetricState<u64>,
    /// Block-device buffers. Linux only.
    pub buffers: MetricState<u64>,
    /// Shared memory.
    pub shared: MetricState<u64>,
    /// Recently used pages.
    pub active: MetricState<u64>,
    /// Reclaimable pages.
    pub inactive: MetricState<u64>,
    /// Pages that cannot be paged out. macOS only.
    pub wired: MetricState<u64>,
    /// Pages held in the compressor. macOS only.
    pub compressed: MetricState<u64>,
    /// Pages awaiting writeback. Linux only.
    pub dirty: MetricState<u64>,
}

impl MemoryDetail {
    /// A breakdown with nothing measured, for the first frame.
    pub const WARMING_UP: Self = Self {
        cached: MetricState::WarmingUp,
        buffers: MetricState::WarmingUp,
        shared: MetricState::WarmingUp,
        active: MetricState::WarmingUp,
        inactive: MetricState::WarmingUp,
        wired: MetricState::WarmingUp,
        compressed: MetricState::WarmingUp,
        dirty: MetricState::WarmingUp,
    };
}

/// Swap capacity and activity.
///
/// `in_rate` and `out_rate` are the metrics that actually indicate memory
/// distress: a large but idle swap file is unremarkable, while sustained
/// swap-in on a small one is not (§11.2).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct SwapSnapshot {
    /// Configured swap size. Zero means swap is disabled, which is a fact
    /// rather than an unavailable metric.
    pub total_bytes: u64,
    /// Swap currently in use.
    pub used: MetricState<u64>,
    /// Share of swap in use.
    pub usage: MetricState<Percent>,
    /// Pages read back from swap per second.
    pub in_rate: MetricState<Rate>,
    /// Pages written to swap per second.
    pub out_rate: MetricState<Rate>,
}

impl SwapSnapshot {
    /// Whether swap is configured at all.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.total_bytes > 0
    }

    /// A snapshot for a system with swap disabled.
    #[must_use]
    pub const fn disabled() -> Self {
        Self {
            total_bytes: 0,
            used: MetricState::Available(0),
            usage: MetricState::Unsupported,
            in_rate: MetricState::Unsupported,
            out_rate: MetricState::Unsupported,
        }
    }
}

/// System memory state.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct MemorySnapshot {
    /// Total physical memory. Always known.
    pub total_bytes: u64,
    /// Memory allocatable without reclaim pressure, per [`Self::semantics`].
    pub available: MetricState<u64>,
    /// `total_bytes - available`, per [`Self::semantics`].
    pub used: MetricState<u64>,
    /// Completely unused memory. Usually much smaller than `available`.
    pub free: MetricState<u64>,
    /// Share of memory in use.
    pub usage: MetricState<Percent>,
    /// The secondary breakdown.
    pub detail: MemoryDetail,
    /// Swap capacity and activity.
    pub swap: SwapSnapshot,
    /// Which definition produced `available` and `used`.
    pub semantics: MemorySemantics,
    /// The cgroup memory limit, when running under one, alongside the host
    /// total in `total_bytes`.
    ///
    /// §9.2 requires container limits to be exposed *separately* from host
    /// totals and both to be shown and labelled where observable.
    pub cgroup_limit_bytes: MetricState<u64>,
}

impl MemorySnapshot {
    /// A snapshot with only the total known, for the first frame.
    #[must_use]
    pub const fn warming_up(total_bytes: u64, semantics: MemorySemantics) -> Self {
        Self {
            total_bytes,
            available: MetricState::WarmingUp,
            used: MetricState::WarmingUp,
            free: MetricState::WarmingUp,
            usage: MetricState::WarmingUp,
            detail: MemoryDetail::WARMING_UP,
            swap: SwapSnapshot {
                total_bytes: 0,
                used: MetricState::WarmingUp,
                usage: MetricState::WarmingUp,
                in_rate: MetricState::WarmingUp,
                out_rate: MetricState::WarmingUp,
            },
            semantics,
            cgroup_limit_bytes: MetricState::WarmingUp,
        }
    }

    /// The memory ceiling that actually applies to this process tree.
    ///
    /// Inside a container this is the cgroup limit, not the host total; §9.2
    /// requires the distinction to be observable rather than silently folded
    /// into one number.
    #[must_use]
    pub fn effective_limit_bytes(&self) -> u64 {
        match self.cgroup_limit_bytes.fresh() {
            Some(&limit) if limit > 0 && limit < self.total_bytes => limit,
            _ => self.total_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_platform_semantics_explains_itself() {
        for semantics in [
            MemorySemantics::LinuxMemAvailable,
            MemorySemantics::MacosVmStatistics,
            MemorySemantics::SysinfoBaseline,
        ] {
            assert!(!semantics.description().is_empty());
        }
        // The Linux description must state that cache is not application use.
        assert!(
            MemorySemantics::LinuxMemAvailable
                .description()
                .contains("page cache")
        );
    }

    #[test]
    fn disabled_swap_is_a_fact_not_an_unavailable_metric() {
        let swap = SwapSnapshot::disabled();
        assert!(!swap.is_enabled());
        assert_eq!(
            swap.used.fresh(),
            Some(&0),
            "0 of 0 bytes used is a real measurement"
        );
        // ...but a percentage of zero capacity is genuinely undefined.
        assert!(swap.usage.fresh().is_none());
    }

    #[test]
    fn a_cgroup_limit_below_the_host_total_becomes_the_effective_ceiling() {
        let mut memory =
            MemorySnapshot::warming_up(32 * 1024 * 1024 * 1024, MemorySemantics::LinuxMemAvailable);
        assert_eq!(memory.effective_limit_bytes(), 32 * 1024 * 1024 * 1024);

        memory.cgroup_limit_bytes = MetricState::Available(2 * 1024 * 1024 * 1024);
        assert_eq!(memory.effective_limit_bytes(), 2 * 1024 * 1024 * 1024);
    }

    #[test]
    fn an_unlimited_cgroup_does_not_shrink_the_ceiling() {
        let mut memory =
            MemorySnapshot::warming_up(32 * 1024 * 1024 * 1024, MemorySemantics::LinuxMemAvailable);
        // cgroup v2 writes an enormous sentinel for "max".
        memory.cgroup_limit_bytes = MetricState::Available(u64::MAX);
        assert_eq!(memory.effective_limit_bytes(), 32 * 1024 * 1024 * 1024);
        memory.cgroup_limit_bytes = MetricState::Available(0);
        assert_eq!(memory.effective_limit_bytes(), 32 * 1024 * 1024 * 1024);
    }

    #[test]
    fn warming_up_preserves_the_requested_semantics() {
        let memory = MemorySnapshot::warming_up(1024, MemorySemantics::MacosVmStatistics);
        assert_eq!(memory.semantics, MemorySemantics::MacosVmStatistics);
        assert_eq!(memory.total_bytes, 1024);
        assert!(memory.available.is_warming_up());
    }
}

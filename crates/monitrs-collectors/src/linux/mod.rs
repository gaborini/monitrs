//! Native Linux enrichment: `/proc`, `/sys`, cgroups, PSI, and signals (§9.2).
//!
//! # What this layer is for
//!
//! [`crate::common::CommonCollector`] already produces a complete snapshot on Linux.
//! This module exists for the things the cross-platform baseline cannot know:
//!
//! * the `/proc/stat` CPU-time split, including `steal`, which is the clearest signal
//!   that a VM is oversubscribed (§8.3);
//! * `MemAvailable` semantics, so page cache is not counted as application use (§8.4);
//! * device busy time from `/proc/diskstats` field 10 — the only place §7.3 allows a
//!   busy percentage;
//! * filesystem inode counts from `statfs(2)`, which no `/proc` file reports and which
//!   are the difference between seeing an `ENOSPC` coming and being surprised by it;
//! * interface drop counters, link state, and link speed, without which §7.4 forbids
//!   a utilisation percentage;
//! * PSI, the strongest input the Pressure Radar can have (§2.3);
//! * the battery, from `/sys/class/power_supply` — cycle count, wear against design
//!   capacity, pack temperature and instantaneous watts, none of which the baseline
//!   or the documented macOS APIs can reach (§9.3);
//! * cgroup limits, exposed *beside* host totals rather than instead of them (§9.2);
//! * a process start key in clock ticks rather than whole seconds, which is what
//!   makes PID reuse inside one second detectable (§26);
//! * kernel-thread detection from task flags, so §7.2 can hide them safely.
//!
//! # The structural rule
//!
//! **Every parser takes `&[u8]`, never a path.** §17.2 requires the parsers to be
//! testable without a live filesystem, and that requirement is why this module is
//! shaped the way it is: [`read`] is the only code that touches the filesystem, and
//! even that is rooted rather than hard-coded, so the reading layer itself is
//! exercised by a checked-in `/proc` tree on any platform. Only `ProcRoot::live`,
//! `LinuxCollector`, the `kill(2)` sink, and `statfs` are gated to Linux — they are
//! not linkable in this documentation build for that reason — and all four are thin.
//! `statfs` is the one that could not have been written any other way: inode counts
//! exist nowhere under `/proc`, so there is no byte stream to parse from a fixture.
//!
//! One consequence is worth stating plainly: this code was written and tested on
//! macOS. Everything in [`parse`], [`stat`], [`meminfo`], [`loadavg`], [`diskstats`],
//! [`netdev`], [`psi`], [`power`], [`process`], [`cgroup`], [`signal`], [`read`], and [`enrich`]
//! compiles and runs its tests on every platform. The Linux-gated code is
//! type-checked by cross-compiling; it is not exercised by these tests, and the
//! module boundary is drawn so that the un-exercised part is as small as possible.
//!
//! # Rules this module never breaks
//!
//! * **Unavailable is never zero.** A missing `steal` counter, an absent PSI `full`
//!   line, an `EACCES` on `/proc/<pid>/io`, a cgroup `max` sentinel: every one of them
//!   produces an explicit [`monitrs_core::model::MetricState`], and there is a test
//!   for each (§4, §26).
//! * **A vanished process is not an error.** It cannot produce a log line, by
//!   construction — see [`read::ReadDiagnostics`] (§9.2, §14.1).
//! * **No unbounded `/proc` walks and no unbounded reads.** One directory level, and
//!   every read capped (§9.2, §16.1).
//! * **No external commands.** Nothing here spawns anything.

pub mod cgroup;
#[cfg(all(target_os = "linux", feature = "linux-native"))]
pub mod collector;
pub mod diskstats;
pub mod enrich;
pub mod loadavg;
pub mod meminfo;
pub mod netdev;
pub mod parse;
pub mod power;
pub mod process;
pub mod psi;
pub mod read;
pub mod signal;
pub mod stat;
#[cfg(all(target_os = "linux", feature = "linux-native"))]
pub mod statfs;

pub use enrich::{DEFAULT_USER_HZ, LinuxEnrichment};
pub use parse::{ParseFailure, ParseResult};
pub use power::{BatteryAttributes, PowerSupplyKind, battery_from, classify};
pub use read::{LinuxSources, ProcRoot, ReadFailure, SourceRequest, collect_sources};
pub use signal::{
    LinuxSignal, SignalDecision, SignalError, SignalSink, revalidate, signal_process,
};

/// The sanitized fixtures every parser test reads.
///
/// Loaded with [`include_bytes!`] rather than at runtime for three reasons: the tests
/// stay hermetic, they need no working directory, and a renamed or deleted fixture is
/// a compile error rather than a test that quietly stops covering anything (§17.2).
///
/// None of these files came from a real machine. §15.2 forbids leaking host names,
/// user names, and command-line arguments, so every counter, UID, and path here was
/// written by hand.
#[cfg(test)]
pub(crate) mod fixtures {
    /// Expands to a crate-visible fixture constant loaded from `fixtures/linux/`.
    macro_rules! fixture {
        ($name:ident => $path:literal) => {
            #[doc = concat!("`fixtures/linux/", $path, "`")]
            pub(crate) const $name: &[u8] = include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/fixtures/linux/",
                $path
            ));
        };
    }

    fixture!(PROC_STAT_TYPICAL => "cases/proc_stat/typical.txt");
    fixture!(PROC_STAT_NEXT_TICK => "cases/proc_stat/next_tick.txt");
    fixture!(PROC_STAT_NO_PER_CORE => "cases/proc_stat/no_per_core.txt");
    fixture!(PROC_STAT_OLD_KERNEL => "cases/proc_stat/old_kernel_four_fields.txt");
    fixture!(PROC_STAT_TRUNCATED => "cases/proc_stat/truncated.txt");
    fixture!(PROC_STAT_EMPTY => "cases/proc_stat/empty.txt");
    fixture!(PROC_STAT_HUGE => "cases/proc_stat/huge_counters.txt");
    fixture!(PROC_STAT_AFTER_RESET => "cases/proc_stat/after_reset.txt");

    fixture!(MEMINFO_TYPICAL => "cases/meminfo/typical.txt");
    fixture!(MEMINFO_NO_MEMAVAILABLE => "cases/meminfo/no_memavailable.txt");
    fixture!(MEMINFO_TRUNCATED => "cases/meminfo/truncated.txt");
    fixture!(MEMINFO_MALFORMED_UNITS => "cases/meminfo/malformed_units.txt");

    fixture!(LOADAVG_TYPICAL => "cases/loadavg/typical.txt");
    fixture!(LOADAVG_TRUNCATED => "cases/loadavg/truncated.txt");
    fixture!(UPTIME_TYPICAL => "cases/uptime/typical.txt");
    fixture!(UPTIME_MALFORMED => "cases/uptime/malformed.txt");

    fixture!(DISKSTATS_TYPICAL => "cases/diskstats/typical.txt");
    fixture!(DISKSTATS_NEXT_TICK => "cases/diskstats/next_tick.txt");
    fixture!(DISKSTATS_SHORT_FIELDS => "cases/diskstats/short_fields.txt");
    fixture!(DISKSTATS_MALFORMED => "cases/diskstats/malformed.txt");
    fixture!(DISKSTATS_HUGE => "cases/diskstats/huge_counters.txt");
    fixture!(DISKSTATS_AFTER_RESET => "cases/diskstats/after_reset.txt");
    fixture!(DISKSTATS_EMPTY => "cases/diskstats/empty.txt");

    fixture!(NET_DEV_TYPICAL => "cases/net_dev/typical.txt");
    fixture!(NET_DEV_NEXT_TICK => "cases/net_dev/next_tick.txt");
    fixture!(NET_DEV_HEADER_ONLY => "cases/net_dev/header_only.txt");
    fixture!(NET_DEV_TRUNCATED => "cases/net_dev/truncated.txt");
    fixture!(NET_DEV_HUGE => "cases/net_dev/huge_counters.txt");
    fixture!(NET_DEV_AFTER_RESET => "cases/net_dev/after_reset.txt");
    fixture!(NET_DEV_EMPTY => "cases/net_dev/empty.txt");

    fixture!(POWER_TYPE_BATTERY => "cases/power_supply/type_battery.txt");
    fixture!(POWER_TYPE_MAINS => "cases/power_supply/type_mains.txt");
    fixture!(POWER_SCOPE_SYSTEM => "cases/power_supply/scope_system.txt");
    fixture!(POWER_SCOPE_DEVICE => "cases/power_supply/scope_device.txt");
    fixture!(POWER_STATUS_DISCHARGING => "cases/power_supply/status_discharging.txt");
    fixture!(POWER_STATUS_CHARGING => "cases/power_supply/status_charging.txt");
    fixture!(POWER_STATUS_FULL => "cases/power_supply/status_full.txt");
    fixture!(POWER_STATUS_NOT_CHARGING => "cases/power_supply/status_not_charging.txt");
    fixture!(POWER_CAPACITY_82 => "cases/power_supply/capacity_82.txt");
    fixture!(POWER_CYCLE_COUNT_214 => "cases/power_supply/cycle_count_214.txt");
    fixture!(POWER_ENERGY_FULL_DESIGN => "cases/power_supply/energy_full_design.txt");
    fixture!(POWER_ENERGY_FULL => "cases/power_supply/energy_full.txt");
    fixture!(POWER_POWER_NOW => "cases/power_supply/power_now.txt");
    fixture!(POWER_TEMP_314 => "cases/power_supply/temp_314.txt");

    fixture!(PID_STAT_SIMPLE => "cases/pid_stat/simple.txt");
    fixture!(PID_STAT_SIMPLE_NEXT_TICK => "cases/pid_stat/simple_next_tick.txt");
    fixture!(PID_STAT_WEIRD_NAME => "cases/pid_stat/parens_and_spaces_in_name.txt");
    fixture!(PID_STAT_KERNEL_THREAD => "cases/pid_stat/kernel_thread.txt");
    fixture!(PID_STAT_ZOMBIE => "cases/pid_stat/zombie.txt");
    fixture!(PID_STAT_TRUNCATED => "cases/pid_stat/truncated.txt");
    fixture!(PID_STAT_UNTERMINATED_NAME => "cases/pid_stat/unterminated_name.txt");
    fixture!(PID_STAT_EMPTY => "cases/pid_stat/empty.txt");
    fixture!(PID_STAT_REUSED_SAME_SECOND => "cases/pid_stat/reused_pid_same_second.txt");

    fixture!(PID_STATUS_TYPICAL => "cases/pid_status/typical.txt");
    fixture!(PID_STATUS_KERNEL_THREAD => "cases/pid_status/kernel_thread.txt");
    fixture!(PID_STATUS_TRUNCATED => "cases/pid_status/truncated.txt");

    fixture!(PID_IO_TYPICAL => "cases/pid_io/typical.txt");
    fixture!(PID_IO_NEXT_TICK => "cases/pid_io/next_tick.txt");
    fixture!(PID_IO_EMPTY => "cases/pid_io/empty.txt");
    fixture!(PID_IO_TRUNCATED => "cases/pid_io/truncated.txt");

    fixture!(CMDLINE_TYPICAL => "cases/pid_cmdline/typical.bin");
    fixture!(CMDLINE_WITH_SECRET => "cases/pid_cmdline/with_secret_argument.bin");
    fixture!(CMDLINE_EMPTY => "cases/pid_cmdline/empty.bin");
    fixture!(CMDLINE_SPACE_SEPARATED => "cases/pid_cmdline/space_separated_no_nul.bin");
    fixture!(CMDLINE_NO_TRAILING_NUL => "cases/pid_cmdline/no_trailing_nul.bin");
    fixture!(CMDLINE_INVALID_UTF8 => "cases/pid_cmdline/invalid_utf8.bin");

    fixture!(CGROUP_V2_DOCKER => "cases/pid_cgroup/v2_docker.txt");
    fixture!(CGROUP_V2_ROOT => "cases/pid_cgroup/v2_root.txt");
    fixture!(CGROUP_V2_USER_SESSION => "cases/pid_cgroup/v2_user_session.txt");
    fixture!(CGROUP_V2_KUBERNETES => "cases/pid_cgroup/v2_kubernetes.txt");
    fixture!(CGROUP_V2_LXC => "cases/pid_cgroup/v2_lxc.txt");
    fixture!(CGROUP_V1_DOCKER => "cases/pid_cgroup/v1_docker.txt");
    fixture!(CGROUP_HYBRID_PODMAN => "cases/pid_cgroup/hybrid_podman.txt");
    fixture!(CGROUP_MALFORMED => "cases/pid_cgroup/malformed.txt");

    fixture!(CGROUP_MEMORY_MAX_UNLIMITED => "cases/cgroup/memory.max_unlimited.txt");
    fixture!(CGROUP_MEMORY_MAX_LIMITED => "cases/cgroup/memory.max_limited.txt");
    fixture!(CGROUP_MEMORY_MAX_V1_SENTINEL => "cases/cgroup/memory.max_v1_sentinel.txt");
    fixture!(CGROUP_MEMORY_CURRENT => "cases/cgroup/memory.current.txt");
    fixture!(CGROUP_CPU_MAX_LIMITED => "cases/cgroup/cpu.max_limited.txt");
    fixture!(CGROUP_CPU_MAX_UNLIMITED => "cases/cgroup/cpu.max_unlimited.txt");
    fixture!(CGROUP_CPU_MAX_MALFORMED => "cases/cgroup/cpu.max_malformed.txt");

    fixture!(PRESSURE_CPU_WITH_FULL => "cases/pressure/cpu_with_full.txt");
    fixture!(PRESSURE_CPU_WITHOUT_FULL => "cases/pressure/cpu_without_full.txt");
    fixture!(PRESSURE_MEMORY => "cases/pressure/memory.txt");
    fixture!(PRESSURE_IO_IDLE => "cases/pressure/io_idle.txt");
    fixture!(PRESSURE_FULL_ONLY => "cases/pressure/full_only.txt");
    fixture!(PRESSURE_MALFORMED => "cases/pressure/malformed.txt");
    fixture!(PRESSURE_EMPTY => "cases/pressure/empty.txt");

    fixture!(OPERSTATE_UP => "cases/sys_class_net/operstate_up.txt");
    fixture!(OPERSTATE_DOWN => "cases/sys_class_net/operstate_down.txt");
    fixture!(OPERSTATE_DORMANT => "cases/sys_class_net/operstate_dormant.txt");
    fixture!(OPERSTATE_UNKNOWN => "cases/sys_class_net/operstate_unknown.txt");
    fixture!(SPEED_1000 => "cases/sys_class_net/speed_1000.txt");
    fixture!(SPEED_UNKNOWN_NEGATIVE => "cases/sys_class_net/speed_unknown_negative.txt");
    fixture!(SPEED_EMPTY => "cases/sys_class_net/speed_empty.txt");

    fixture!(DMI_SYS_VENDOR_QEMU => "cases/dmi/sys_vendor_qemu.txt");
    fixture!(DMI_SYS_VENDOR_PHYSICAL => "cases/dmi/sys_vendor_physical.txt");
}

//! `/proc/stat`: cumulative CPU time, boot time, and run-queue counts.
//!
//! This is the file that makes §8.3's "use delta CPU times when available" true on
//! Linux. Every value in it is a cumulative counter in `USER_HZ` clock ticks, so
//! nothing here produces a percentage on its own — [`CpuTimes`] is turned into a
//! [`CpuTimeTotals`] and handed to the frozen
//! [`SystemCpuTracker`](monitrs_core::rates::SystemCpuTracker), which owns the
//! reset, wrap, and warming-up rules.

use monitrs_core::model::{CpuBreakdown, MetricState};
use monitrs_core::rates::CpuTimeTotals;
use monitrs_core::units::Percent;

use crate::linux::parse::{ParseFailure, ParseResult, fields, lines, parse_u64, ticks_to_duration};

/// One `cpu` line from `/proc/stat`, in `USER_HZ` clock ticks.
///
/// The trailing fields were appended to the kernel ABI over many years —
/// `iowait`, `irq`, and `softirq` in 2.6.0, `steal` in 2.6.11, `guest` in 2.6.24,
/// `guest_nice` in 2.6.33 — so each is an [`Option`]. A kernel that does not report
/// `steal` is not reporting zero steal, and §26 forbids treating the two as the
/// same thing: an oversubscribed VM is exactly the case where `steal` matters.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CpuTimes {
    /// Time in user mode.
    pub user: u64,
    /// Time in low-priority user mode.
    pub nice: u64,
    /// Time in kernel mode.
    pub system: u64,
    /// Time with nothing to run.
    pub idle: u64,
    /// Time idle with outstanding block I/O.
    pub iowait: Option<u64>,
    /// Time servicing hardware interrupts.
    pub irq: Option<u64>,
    /// Time servicing soft interrupts.
    pub softirq: Option<u64>,
    /// Time the hypervisor gave to another guest.
    pub steal: Option<u64>,
    /// Time running a guest under this kernel.
    pub guest: Option<u64>,
    /// Time running a low-priority guest.
    pub guest_nice: Option<u64>,
}

impl CpuTimes {
    /// Ticks that count as work.
    ///
    /// `iowait` is deliberately excluded: a CPU waiting on a disk is not doing
    /// work, and counting it as busy is what makes some monitors show a pegged CPU
    /// during a storage stall (§8.3, and the note on
    /// [`CpuTimeTotals::idle`](monitrs_core::rates::CpuTimeTotals::idle)).
    ///
    /// `guest` and `guest_nice` are excluded because the kernel already counts
    /// them inside `user` and `nice`; adding them would double-count a
    /// virtualisation host's busy time.
    #[must_use]
    pub fn busy_ticks(self) -> u64 {
        self.user
            .saturating_add(self.nice)
            .saturating_add(self.system)
            .saturating_add(self.irq.unwrap_or(0))
            .saturating_add(self.softirq.unwrap_or(0))
            .saturating_add(self.steal.unwrap_or(0))
    }

    /// Ticks that count as not working: `idle` plus `iowait`.
    #[must_use]
    pub fn idle_ticks(self) -> u64 {
        self.idle.saturating_add(self.iowait.unwrap_or(0))
    }

    /// Every tick accounted for.
    #[must_use]
    pub fn total_ticks(self) -> u64 {
        self.busy_ticks().saturating_add(self.idle_ticks())
    }

    /// Converts to the platform-neutral totals the rate engine consumes.
    #[must_use]
    pub fn totals(self, ticks_per_second: u64) -> CpuTimeTotals {
        CpuTimeTotals::new(
            ticks_to_duration(self.busy_ticks(), ticks_per_second),
            ticks_to_duration(self.idle_ticks(), ticks_per_second),
        )
    }

    /// The per-state percentage split between this reading and an earlier one.
    ///
    /// Returns `None` when the counters did not advance or moved backwards. That
    /// is the reset case §8.2 names, and the answer is an absent breakdown rather
    /// than a set of zeroes that would read as a completely idle CPU.
    ///
    /// Fields the kernel does not report stay [`MetricState::Unsupported`] all the
    /// way through, so the Inspect screen can say `steal: n/a` on a machine that
    /// has no hypervisor rather than `steal: 0%`.
    #[must_use]
    pub fn breakdown_since(self, previous: Self) -> Option<CpuBreakdown> {
        let total = self.total_ticks().checked_sub(previous.total_ticks())?;
        if total == 0 {
            return None;
        }
        let share = |current: u64, earlier: u64| -> Option<Percent> {
            Percent::ratio(current.checked_sub(earlier)?, total)
        };
        let optional_share = |current: Option<u64>, earlier: Option<u64>| match (current, earlier) {
            (Some(current), Some(earlier)) => {
                share(current, earlier).map_or(MetricState::Unsupported, MetricState::Available)
            }
            // The field appeared or vanished between two reads of the same file,
            // which means the kernel was replaced underneath us. One unavailable
            // sample is the honest answer.
            _ => MetricState::Unsupported,
        };

        Some(CpuBreakdown {
            user: share(self.user, previous.user)?,
            system: share(self.system, previous.system)?,
            nice: share(self.nice, previous.nice)?,
            idle: share(self.idle, previous.idle)?,
            iowait: optional_share(self.iowait, previous.iowait),
            irq: optional_share(self.irq, previous.irq),
            softirq: optional_share(self.softirq, previous.softirq),
            steal: optional_share(self.steal, previous.steal),
        })
    }

    /// Parses the numeric tail of a `cpu` or `cpuN` line.
    fn parse_tail(line: &[u8]) -> ParseResult<Self> {
        let mut values = fields(line);
        let mut next = |name: &'static str| -> ParseResult<u64> {
            match values.next() {
                Some(field) => parse_u64(field, name),
                None => Err(ParseFailure::Truncated(name)),
            }
        };
        let user = next("cpu.user")?;
        let nice = next("cpu.nice")?;
        let system = next("cpu.system")?;
        let idle = next("cpu.idle")?;
        // Everything past `idle` is optional by kernel version, so a parse failure
        // on a *present* field is still a failure while absence is not.
        let mut optional = |name: &'static str| -> ParseResult<Option<u64>> {
            match values.next() {
                Some(field) => parse_u64(field, name).map(Some),
                None => Ok(None),
            }
        };
        Ok(Self {
            user,
            nice,
            system,
            idle,
            iowait: optional("cpu.iowait")?,
            irq: optional("cpu.irq")?,
            softirq: optional("cpu.softirq")?,
            steal: optional("cpu.steal")?,
            guest: optional("cpu.guest")?,
            guest_nice: optional("cpu.guest_nice")?,
        })
    }
}

/// The whole of `/proc/stat`, as far as a monitor needs it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcStat {
    /// The aggregate `cpu` line, which is the sum of the per-CPU lines.
    pub total: CpuTimes,
    /// The `cpuN` lines in index order.
    ///
    /// Empty inside some containers, which is why per-core CPU is a capability
    /// rather than an assumption (§4).
    pub per_cpu: Vec<CpuTimes>,
    /// Context switches since boot.
    pub context_switches: Option<u64>,
    /// Boot time as seconds since the Unix epoch.
    ///
    /// The one genuinely wall-clock value in the file, and the only sound way to
    /// turn a process's start time in clock ticks into a `SystemTime` (§8.1).
    pub boot_time_secs: Option<u64>,
    /// Processes (and threads) created since boot.
    pub processes_created: Option<u64>,
    /// Tasks currently runnable.
    pub procs_running: Option<u64>,
    /// Tasks currently blocked on I/O.
    ///
    /// The direct measurement behind the `D`-state pressure §7.2 wants visible.
    pub procs_blocked: Option<u64>,
}

/// Parses `/proc/stat`.
///
/// The aggregate `cpu` line is the only hard requirement: without it there is no
/// system CPU metric at all, so its absence is a parse failure rather than a
/// snapshot full of unavailable fields.
pub fn parse_proc_stat(bytes: &[u8]) -> ParseResult<ProcStat> {
    if crate::linux::parse::trim_ascii(bytes).is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut stat = ProcStat::default();
    let mut seen_total = false;
    // Per-CPU lines are indexed by the number in `cpuN`, not by their order in the
    // file: a hotplugged CPU can leave a gap, and silently compacting the list
    // would attribute core 5's usage to core 4.
    let mut indexed: Vec<(u64, CpuTimes)> = Vec::new();

    for line in lines(bytes) {
        let mut parts = line.splitn(2, u8::is_ascii_whitespace);
        let Some(key) = parts.next() else { continue };
        let tail = parts.next().unwrap_or_default();
        match key {
            b"cpu" => {
                stat.total = CpuTimes::parse_tail(tail)?;
                seen_total = true;
            }
            key if key.starts_with(b"cpu") => {
                let index = parse_u64(key.get(3..).unwrap_or_default(), "cpu.index")?;
                indexed.push((index, CpuTimes::parse_tail(tail)?));
            }
            b"ctxt" => stat.context_switches = Some(parse_u64(tail, "ctxt")?),
            b"btime" => stat.boot_time_secs = Some(parse_u64(tail, "btime")?),
            b"processes" => stat.processes_created = Some(parse_u64(tail, "processes")?),
            b"procs_running" => stat.procs_running = Some(parse_u64(tail, "procs_running")?),
            b"procs_blocked" => stat.procs_blocked = Some(parse_u64(tail, "procs_blocked")?),
            // `intr` and `softirq` are thousands of columns of interrupt counts
            // that no screen in §7 renders. Parsing them every second would be
            // pure cost (§16.1).
            _ => {}
        }
    }

    if !seen_total {
        return Err(ParseFailure::Missing("cpu"));
    }
    indexed.sort_unstable_by_key(|(index, _)| *index);
    stat.per_cpu = indexed.into_iter().map(|(_, times)| times).collect();
    Ok(stat)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    #[test]
    fn a_typical_file_yields_the_aggregate_and_every_core() {
        let stat = parse_proc_stat(fixtures::PROC_STAT_TYPICAL).expect("valid");
        assert_eq!(stat.total.user, 1_092_381);
        assert_eq!(stat.total.idle, 88_213_764);
        assert_eq!(stat.total.iowait, Some(21_093));
        assert_eq!(stat.total.steal, Some(8_210));
        assert_eq!(stat.per_cpu.len(), 4);
        assert_eq!(stat.boot_time_secs, Some(1_748_001_600));
        assert_eq!(stat.context_switches, Some(3_892_017_453));
        assert_eq!(stat.procs_running, Some(3));
        assert_eq!(stat.procs_blocked, Some(1));
    }

    #[test]
    fn iowait_is_counted_as_idle_rather_than_busy() {
        // §8.3: a CPU blocked on I/O is not doing work. Counting iowait as busy is
        // what makes a monitor show 100% CPU during a disk stall.
        let times = CpuTimes {
            user: 100,
            nice: 0,
            system: 0,
            idle: 800,
            iowait: Some(100),
            ..CpuTimes::default()
        };
        assert_eq!(times.busy_ticks(), 100);
        assert_eq!(times.idle_ticks(), 900);
        assert_eq!(times.total_ticks(), 1_000);
    }

    #[test]
    fn guest_time_is_not_double_counted_into_busy() {
        // The kernel already includes guest time in user and nice.
        let times = CpuTimes {
            user: 500,
            nice: 0,
            system: 0,
            idle: 500,
            guest: Some(400),
            guest_nice: Some(100),
            ..CpuTimes::default()
        };
        assert_eq!(times.busy_ticks(), 500);
    }

    #[test]
    fn the_two_tick_delta_produces_the_expected_machine_percentage() {
        let first = parse_proc_stat(fixtures::PROC_STAT_TYPICAL).expect("valid");
        let second = parse_proc_stat(fixtures::PROC_STAT_NEXT_TICK).expect("valid");
        // busy advanced 300 ticks, idle+iowait advanced 410, so 300/710 = 42.25%.
        let breakdown = second
            .total
            .breakdown_since(first.total)
            .expect("counters advanced");
        let busy =
            100.0 - breakdown.idle.value() - breakdown.iowait.fresh().expect("present").value();
        assert!((busy - 42.25).abs() < 0.1, "got {busy}");
        assert!(breakdown.user.value() > 0.0);
        assert!(breakdown.steal.fresh().is_some());
    }

    #[test]
    fn an_old_kernel_reports_no_iowait_rather_than_zero_iowait() {
        // §4: a field the kernel does not have is unsupported, not zero.
        let stat = parse_proc_stat(fixtures::PROC_STAT_OLD_KERNEL).expect("valid");
        assert_eq!(stat.total.iowait, None);
        assert_eq!(stat.total.steal, None);
        let doubled = CpuTimes {
            user: 800,
            nice: 20,
            system: 180,
            idle: 7_000,
            ..CpuTimes::default()
        };
        let breakdown = doubled
            .breakdown_since(stat.total)
            .expect("counters advanced");
        assert!(
            breakdown.iowait.is_unsupported(),
            "an absent counter must not become 0%"
        );
        assert!(breakdown.steal.is_unsupported());
    }

    #[test]
    fn a_container_without_per_core_lines_still_parses() {
        let stat = parse_proc_stat(fixtures::PROC_STAT_NO_PER_CORE).expect("valid");
        assert!(stat.per_cpu.is_empty());
        assert_eq!(stat.total.user, 100);
    }

    #[test]
    fn an_empty_file_is_a_typed_failure_not_a_zeroed_snapshot() {
        assert_eq!(parse_proc_stat(b""), Err(ParseFailure::Empty));
        assert_eq!(parse_proc_stat(b"   \n\n"), Err(ParseFailure::Empty));
        assert_eq!(
            parse_proc_stat(fixtures::PROC_STAT_EMPTY),
            Err(ParseFailure::Empty)
        );
    }

    #[test]
    fn a_truncated_cpu_line_fails_rather_than_inventing_an_idle_count() {
        assert_eq!(
            parse_proc_stat(fixtures::PROC_STAT_TRUNCATED),
            Err(ParseFailure::Truncated("cpu.idle"))
        );
    }

    #[test]
    fn a_file_without_the_aggregate_line_has_no_system_cpu_metric() {
        assert_eq!(
            parse_proc_stat(b"btime 1748001600\nprocs_running 2\n"),
            Err(ParseFailure::Missing("cpu"))
        );
    }

    #[test]
    fn a_near_u64_max_counter_neither_panics_nor_wraps() {
        let stat = parse_proc_stat(fixtures::PROC_STAT_HUGE).expect("valid");
        assert_eq!(stat.total.user, 18_446_744_073_709_551_000);
        // user + nice + system still fits...
        assert_eq!(stat.total.busy_ticks(), 18_446_744_073_709_551_600);
        // ...but adding idle does not, and saturating is what keeps the answer a
        // bounded number rather than a wrapped one that would read as a tiny total.
        assert_eq!(stat.total.total_ticks(), u64::MAX);
        let totals = stat.total.totals(100);
        assert!(totals.busy > core::time::Duration::ZERO);
    }

    #[test]
    fn a_counter_that_moved_backwards_yields_no_breakdown() {
        // The reset case: §8.2 forbids a huge or negative percentage, and an
        // absent breakdown is what the UI renders as unavailable.
        let before = parse_proc_stat(fixtures::PROC_STAT_TYPICAL).expect("valid");
        let after = parse_proc_stat(fixtures::PROC_STAT_AFTER_RESET).expect("valid");
        assert!(after.total.breakdown_since(before.total).is_none());
    }

    #[test]
    fn a_stalled_counter_yields_no_breakdown_rather_than_all_zeroes() {
        let stat = parse_proc_stat(fixtures::PROC_STAT_TYPICAL).expect("valid");
        assert!(stat.total.breakdown_since(stat.total).is_none());
    }

    #[test]
    fn per_core_lines_are_ordered_by_index_not_by_position() {
        // A hotplug gap must not shift core 5's usage onto core 4.
        let stat = parse_proc_stat(
            b"cpu  10 0 5 85 0 0 0 0 0 0\n\
              cpu3 4 0 0 96 0 0 0 0 0 0\n\
              cpu0 1 0 0 99 0 0 0 0 0 0\n\
              cpu10 2 0 0 98 0 0 0 0 0 0\n",
        )
        .expect("valid");
        let users: Vec<u64> = stat.per_cpu.iter().map(|times| times.user).collect();
        assert_eq!(users, vec![1, 4, 2]);
    }

    #[test]
    fn a_malformed_per_core_index_is_a_failure_not_a_silent_skip() {
        assert!(parse_proc_stat(b"cpu 1 0 0 9 0 0 0 0\ncpuX 1 0 0 9\n").is_err());
    }

    #[test]
    fn totals_convert_ticks_to_durations_at_the_declared_clock_rate() {
        let times = CpuTimes {
            user: 100,
            nice: 0,
            system: 100,
            idle: 800,
            iowait: Some(0),
            ..CpuTimes::default()
        };
        let totals = times.totals(100);
        assert_eq!(totals.busy, core::time::Duration::from_secs(2));
        assert_eq!(totals.idle, core::time::Duration::from_secs(8));
    }
}

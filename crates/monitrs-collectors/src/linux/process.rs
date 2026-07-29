//! `/proc/<pid>/{stat,status,io,cmdline}`.
//!
//! # Why field 2 of `/proc/<pid>/stat` is the hardest thing in this crate
//!
//! The kernel writes the process name in parentheses, unescaped:
//!
//! ```text
//! 9182 (((weird) name) with spaces) S 1 9182 ...
//! ```
//!
//! Nothing separates the name from the numbers except those parentheses, and the
//! name may contain spaces, `(`, and `)`. Splitting the line on whitespace
//! therefore mis-reads *every field after the name* — the state character becomes
//! `name)`, the parent PID becomes a fragment of the name, and the start time,
//! which is half of the process identity, lands on whatever number happens to fall
//! at that index. §9.2 and §17.2 both call this out, and
//! [`parse_pid_stat`] handles it by finding the **last** `)` in the buffer: every
//! field after field 2 is numeric and contains no parenthesis, so the last `)` is
//! unambiguously the one the kernel added.
//!
//! # Why field 22 is the process identity
//!
//! `start_key` on [`ProcessIdentity`] comes from field 22, the start time in
//! `USER_HZ` clock ticks since boot. The cross-platform baseline uses whole
//! seconds, which cannot distinguish a PID reused *within the same second* — the
//! exact case a rapid fork/exec loop produces, and the case where signalling the
//! wrong process does the most damage (§15.1). Clock ticks are 100 per second on
//! every architecture Linux supports in practice, so this closes the window by two
//! orders of magnitude while staying an integer the kernel guarantees is stable for
//! the life of the process.
//!
//! # Why RSS comes from `status` and not from `stat`
//!
//! Field 24 of `stat` is RSS in *pages*, so converting it needs the page size,
//! which needs `sysconf(_SC_PAGESIZE)` and therefore `libc`. `VmRSS` in
//! `/proc/<pid>/status` is already in kibibytes. Preferring `status` keeps this
//! layer free of FFI, and [`ProcPidStat::rss_pages`] remains available for callers
//! that know their page size.

use core::time::Duration;

use monitrs_core::model::{ProcessIdentity, ProcessState};

use crate::linux::parse::{
    ParseFailure, ParseResult, fields, lines, parse_i64, parse_u64, split_key_value,
    ticks_to_duration, to_text, trim_ascii,
};

/// `PF_KTHREAD` from the kernel's `sched.h`: this task has no user address space.
///
/// The authoritative kernel-thread test, and the only one that does not depend on
/// naming conventions or on PID 2 being `kthreadd`.
pub const PF_KTHREAD: u64 = 0x0020_0000;

/// The fields of `/proc/<pid>/stat` a monitor needs.
///
/// Field numbers in the comments are the ones `proc(5)` uses, so this struct can be
/// read against the manual page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcPidStat {
    /// Field 1: the process id.
    pub pid: u32,
    /// Field 2: `comm`, with the kernel's parentheses removed.
    ///
    /// Truncated to 15 bytes by the kernel, and *not* the executable name: a
    /// process can rewrite it at will, which is why the command line is read
    /// separately.
    pub name: Box<str>,
    /// Field 3: scheduling state.
    pub state: ProcessState,
    /// Field 4: parent process id.
    pub ppid: u32,
    /// Field 9: kernel task flags, holding [`PF_KTHREAD`].
    pub flags: u64,
    /// Field 12: major faults, which are the ones that touched the disk.
    pub major_faults: u64,
    /// Field 14 + 15: user plus system CPU time, in clock ticks.
    pub cpu_ticks: u64,
    /// Field 19: niceness, `-20..=19`.
    pub nice: i64,
    /// Field 20: thread count.
    pub threads: u32,
    /// Field 22: start time in clock ticks since boot. Half of the identity.
    pub start_time_ticks: u64,
    /// Field 23: virtual address space size in bytes.
    pub virtual_bytes: u64,
    /// Field 24: resident set size in *pages*, not bytes.
    pub rss_pages: u64,
    /// Field 39: the CPU this task last ran on.
    pub processor: Option<u32>,
    /// Field 42: aggregated block-I/O delay in clock ticks.
    ///
    /// The direct measurement behind "this process is blocked on storage", which
    /// §7.2 wants visible and which a `D` state alone only hints at.
    pub blkio_delay_ticks: Option<u64>,
}

impl ProcPidStat {
    /// The stable identity: PID plus start time in clock ticks.
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        ProcessIdentity::new(self.pid, self.start_time_ticks)
    }

    /// Whether this task is a kernel thread.
    ///
    /// Reads [`PF_KTHREAD`] rather than guessing from the name: `[kworker/…]`
    /// bracket conventions are a `ps` presentation detail, not kernel data, and a
    /// user process is free to call itself `kworker/0:1`. §7.2 lets Linux hide
    /// kernel threads, so a wrong answer here hides a real process.
    #[must_use]
    pub const fn is_kernel_thread(&self) -> bool {
        self.flags & PF_KTHREAD != 0
    }

    /// Total CPU time consumed since the process started.
    #[must_use]
    pub fn cpu_time(&self, ticks_per_second: u64) -> Duration {
        ticks_to_duration(self.cpu_ticks, ticks_per_second)
    }

    /// Time since the process started, given the host's uptime.
    ///
    /// Computed entirely from monotonic quantities — uptime and clock ticks — so a
    /// wall-clock adjustment cannot make a process appear to have started in the
    /// future (§8.1). Saturating, because `/proc/uptime` and `/proc/<pid>/stat` are
    /// two separate reads and a process created between them has a start time a few
    /// milliseconds past the uptime we hold.
    #[must_use]
    pub fn age(&self, uptime: Duration, ticks_per_second: u64) -> Duration {
        uptime.saturating_sub(ticks_to_duration(self.start_time_ticks, ticks_per_second))
    }

    /// Block-I/O delay as a duration, where the kernel accounts for it.
    #[must_use]
    pub fn blkio_delay(&self, ticks_per_second: u64) -> Option<Duration> {
        Some(ticks_to_duration(self.blkio_delay_ticks?, ticks_per_second))
    }
}

/// Maps the single-character state field.
///
/// `W` (paging) existed only on pre-2.6 kernels and `P` (parked) is reported for
/// some kernel threads; both map to `Unknown` rather than being guessed at, because
/// §7.2 gives `?` a distinct meaning the user can act on.
fn parse_state(byte: u8) -> ProcessState {
    match byte {
        b'R' => ProcessState::Running,
        b'S' => ProcessState::Sleeping,
        b'D' => ProcessState::UninterruptibleSleep,
        b'Z' => ProcessState::Zombie,
        b'T' => ProcessState::Stopped,
        b't' => ProcessState::Traced,
        b'I' => ProcessState::Idle,
        b'X' | b'x' => ProcessState::Dead,
        _ => ProcessState::Unknown,
    }
}

/// Parses `/proc/<pid>/stat`.
///
/// See the module documentation for why the name is extracted by searching for the
/// last `)` rather than by splitting on whitespace.
pub fn parse_pid_stat(bytes: &[u8]) -> ParseResult<ProcPidStat> {
    let line = trim_ascii(bytes);
    if line.is_empty() {
        return Err(ParseFailure::Empty);
    }
    let open = line
        .iter()
        .position(|byte| *byte == b'(')
        .ok_or(ParseFailure::Malformed("stat.comm"))?;
    let close = line
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or(ParseFailure::Malformed("stat.comm"))?;
    if close < open {
        return Err(ParseFailure::Malformed("stat.comm"));
    }

    let pid = parse_u64(trim_ascii(line.get(..open).unwrap_or_default()), "stat.pid")?;
    let pid = u32::try_from(pid).map_err(|_| ParseFailure::Malformed("stat.pid"))?;
    let name = to_text(line.get(open + 1..close).unwrap_or_default());

    // Everything from here on is numeric and parenthesis-free, so ordinary
    // whitespace splitting is safe. Field 3 is index 0 of this tail.
    let tail: Vec<&[u8]> = fields(line.get(close + 1..).unwrap_or_default()).collect();
    /// Field numbers below are `proc(5)`'s, so this offset is the only arithmetic
    /// that has to be right.
    const FIRST_TAIL_FIELD: usize = 3;
    let field = |number: usize| -> Option<&[u8]> {
        tail.get(number.checked_sub(FIRST_TAIL_FIELD)?).copied()
    };
    let required = |number: usize, name: &'static str| -> ParseResult<u64> {
        parse_u64(field(number).ok_or(ParseFailure::Truncated(name))?, name)
    };

    let state = field(3)
        .and_then(|value| value.first().copied())
        .map(parse_state)
        .ok_or(ParseFailure::Truncated("stat.state"))?;
    let ppid = u32::try_from(required(4, "stat.ppid")?)
        .map_err(|_| ParseFailure::Malformed("stat.ppid"))?;
    let flags = required(9, "stat.flags")?;
    let major_faults = required(12, "stat.majflt")?;
    let utime = required(14, "stat.utime")?;
    let stime = required(15, "stat.stime")?;
    let nice = parse_i64(
        field(19).ok_or(ParseFailure::Truncated("stat.nice"))?,
        "stat.nice",
    )?;
    let threads = u32::try_from(required(20, "stat.num_threads")?)
        .map_err(|_| ParseFailure::Malformed("stat.num_threads"))?;
    let start_time_ticks = required(22, "stat.starttime")?;
    let virtual_bytes = required(23, "stat.vsize")?;
    let rss_pages = required(24, "stat.rss")?;

    Ok(ProcPidStat {
        pid,
        name,
        state,
        ppid,
        flags,
        major_faults,
        cpu_ticks: utime.saturating_add(stime),
        nice,
        threads,
        start_time_ticks,
        virtual_bytes,
        rss_pages,
        // Fields past 24 were appended over the years; absence is not zero.
        processor: field(39)
            .and_then(|value| parse_u64(value, "stat.processor").ok())
            .and_then(|value| u32::try_from(value).ok()),
        blkio_delay_ticks: field(42).and_then(|value| parse_u64(value, "stat.blkio").ok()),
    })
}

/// The fields of `/proc/<pid>/status` a monitor needs.
///
/// Every field is optional: the file's content depends on kernel version, on
/// namespace, and on whether the task has a user address space at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcPidStatus {
    /// `Name`, the same `comm` as field 2 of `stat` but without the parenthesis
    /// problem.
    pub name: Option<Box<str>>,
    /// `Uid`'s first column: the real user id.
    pub uid: Option<u32>,
    /// `VmRSS` in bytes: resident set size.
    pub rss_bytes: Option<u64>,
    /// `VmSize` in bytes: virtual address space.
    pub virtual_bytes: Option<u64>,
    /// `VmSwap` in bytes: how much of this process is swapped out.
    pub swap_bytes: Option<u64>,
    /// `Threads`.
    pub threads: Option<u32>,
    /// `FDSize`: the size of the descriptor table, an upper bound on open files
    /// that costs nothing, unlike counting `/proc/<pid>/fd`.
    pub fd_size: Option<u32>,
    /// `Kthread`, present since Linux 6.4. `None` on older kernels, where
    /// [`ProcPidStat::is_kernel_thread`] answers the same question from flags.
    pub kernel_thread: Option<bool>,
    /// `voluntary_ctxt_switches`.
    pub voluntary_switches: Option<u64>,
    /// `nonvoluntary_ctxt_switches`: the counter that rises when a task is being
    /// preempted rather than waiting.
    pub involuntary_switches: Option<u64>,
}

/// Parses a `NNN kB` value from `/proc/<pid>/status` into bytes.
fn parse_status_kib(value: &[u8]) -> Option<u64> {
    let mut parts = fields(value);
    let number = parse_u64(parts.next()?, "status.size").ok()?;
    match parts.next() {
        None | Some(b"kB") => number.checked_mul(1024),
        // An unexpected unit means a file this parser does not understand; §26's
        // "unavailable is not zero" makes `None` the only safe answer.
        Some(_) => None,
    }
}

/// Parses `/proc/<pid>/status`.
///
/// Unknown keys are skipped and a malformed value leaves its field absent: the file
/// has around sixty lines, most of which no screen renders, and one unreadable line
/// must not cost the whole record.
pub fn parse_pid_status(bytes: &[u8]) -> ParseResult<ProcPidStatus> {
    if trim_ascii(bytes).is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut status = ProcPidStatus::default();
    for line in lines(bytes) {
        let Some((key, value)) = split_key_value(line) else {
            continue;
        };
        match key {
            b"Name" => status.name = Some(to_text(value)),
            b"Uid" => {
                status.uid = fields(value)
                    .next()
                    .and_then(|first| parse_u64(first, "status.uid").ok())
                    .and_then(|uid| u32::try_from(uid).ok());
            }
            b"VmRSS" => status.rss_bytes = parse_status_kib(value),
            b"VmSize" => status.virtual_bytes = parse_status_kib(value),
            b"VmSwap" => status.swap_bytes = parse_status_kib(value),
            b"Threads" => {
                status.threads = parse_u64(value, "status.threads")
                    .ok()
                    .and_then(|count| u32::try_from(count).ok());
            }
            b"FDSize" => {
                status.fd_size = parse_u64(value, "status.fdsize")
                    .ok()
                    .and_then(|size| u32::try_from(size).ok());
            }
            b"Kthread" => status.kernel_thread = Some(value != b"0"),
            b"voluntary_ctxt_switches" => {
                status.voluntary_switches = parse_u64(value, "status.vctx").ok();
            }
            b"nonvoluntary_ctxt_switches" => {
                status.involuntary_switches = parse_u64(value, "status.nvctx").ok();
            }
            _ => {}
        }
    }
    Ok(status)
}

/// The block-layer counters from `/proc/<pid>/io`.
///
/// This file is readable only by the process owner and by root — `ptrace`-level
/// access is required — so a read of another user's process fails with `EACCES`.
/// That is a [`monitrs_core::model::MetricState::PermissionDenied`], never a zero
/// (§9.2), and [`crate::linux::read::ReadFailure`] is what carries the distinction.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProcPidIo {
    /// `rchar`: bytes read through system calls, including from page cache.
    pub read_chars: u64,
    /// `wchar`: bytes written through system calls.
    pub written_chars: u64,
    /// `read_bytes`: bytes actually fetched from a block device.
    pub read_bytes: u64,
    /// `write_bytes`: bytes actually sent to a block device.
    pub written_bytes: u64,
    /// `cancelled_write_bytes`: pages dirtied and then truncated before writeback.
    pub cancelled_write_bytes: u64,
}

/// Parses `/proc/<pid>/io`.
///
/// All five counters are required: they are written by one kernel function in one
/// order, so a missing line means a truncated read rather than an older kernel.
pub fn parse_pid_io(bytes: &[u8]) -> ParseResult<ProcPidIo> {
    if trim_ascii(bytes).is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut io = ProcPidIo::default();
    let mut seen = 0u8;
    for line in lines(bytes) {
        let Some((key, value)) = split_key_value(line) else {
            continue;
        };
        let target = match key {
            b"rchar" => &mut io.read_chars,
            b"wchar" => &mut io.written_chars,
            b"read_bytes" => &mut io.read_bytes,
            b"write_bytes" => &mut io.written_bytes,
            b"cancelled_write_bytes" => &mut io.cancelled_write_bytes,
            _ => continue,
        };
        *target = parse_u64(value, "io.counter")?;
        seen = seen.saturating_add(1);
    }
    if seen < 5 {
        return Err(ParseFailure::Truncated("io.counters"));
    }
    Ok(io)
}

/// The maximum number of command-line bytes turned into text.
///
/// `/proc/<pid>/cmdline` is capped by the kernel at one page for most processes but
/// can be far larger for one built by a shell loop. §16.1 budgets the whole process
/// pass, so a single pathological argument vector must not dominate it. The
/// truncation is visible: the joined string ends where the data ended.
pub const MAX_CMDLINE_BYTES: usize = 4096;

/// Parses `/proc/<pid>/cmdline` into the joined command line of §7.2.
///
/// The file is NUL-separated with an optional trailing NUL. Three real-world shapes
/// have to survive:
///
/// * **empty** — every kernel thread, and any zombie. The caller falls back to the
///   process name via [`monitrs_core::model::ProcessSnapshot::command_or_name`].
/// * **no NUL at all** — a process that rewrote its own `argv` into one string, as
///   `nginx` and `postgres` do to label their workers. Treated as a single argument
///   rather than discarded.
/// * **embedded empty arguments** — a genuine empty `argv` element, which must not
///   produce a run of spaces that looks like corruption.
#[must_use]
pub fn parse_cmdline(bytes: &[u8]) -> Vec<Box<str>> {
    let capped = bytes
        .get(..bytes.len().min(MAX_CMDLINE_BYTES))
        .unwrap_or(bytes);
    capped
        .split(|byte| *byte == 0)
        .filter(|argument| !argument.is_empty())
        .map(to_text)
        .collect()
}

/// The command line joined by single spaces, as [`ProcessSnapshot`] stores it.
///
/// Pre-joined because §7.2's table holds one string per process rather than a
/// `Vec<String>` per process per tick, and §14.2 requires the arguments to be
/// redactable — which they are, through
/// [`monitrs_core::model::ProcessSnapshot::redacted_command`].
///
/// [`ProcessSnapshot`]: monitrs_core::model::ProcessSnapshot
#[must_use]
pub fn join_cmdline(bytes: &[u8]) -> Box<str> {
    parse_cmdline(bytes).join(" ").into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    #[test]
    fn a_name_containing_parentheses_and_spaces_does_not_shift_any_field() {
        // The case §9.2 and §17.2 name explicitly. If the parser split on
        // whitespace, `state` would be `n` and `start_time_ticks` would be a
        // fragment of the name — which would silently corrupt the process identity.
        let stat = parse_pid_stat(fixtures::PID_STAT_WEIRD_NAME).expect("valid");
        assert_eq!(&*stat.name, "((weird) name) with spaces");
        assert_eq!(stat.pid, 9_182);
        assert_eq!(stat.state, ProcessState::Sleeping);
        assert_eq!(stat.ppid, 1);
        assert_eq!(stat.threads, 4);
        assert_eq!(stat.start_time_ticks, 88_100_000);
        assert_eq!(stat.virtual_bytes, 123_456_789);
        assert_eq!(stat.cpu_ticks, 400);
        assert_eq!(stat.identity(), ProcessIdentity::new(9_182, 88_100_000));
    }

    #[test]
    fn a_simple_process_parses_every_field_we_read() {
        let stat = parse_pid_stat(fixtures::PID_STAT_SIMPLE).expect("valid");
        assert_eq!(stat.pid, 4_242);
        assert_eq!(&*stat.name, "rustc");
        assert_eq!(stat.state, ProcessState::Running);
        assert_eq!(stat.ppid, 1_221);
        assert_eq!(stat.major_faults, 42);
        assert_eq!(stat.cpu_ticks, 15_400);
        assert_eq!(stat.nice, 0);
        assert_eq!(stat.threads, 9);
        assert_eq!(stat.start_time_ticks, 88_213_700);
        assert_eq!(stat.rss_pages, 641_024);
        assert_eq!(stat.processor, Some(3));
        assert!(!stat.is_kernel_thread());
    }

    #[test]
    fn the_start_key_distinguishes_a_pid_reused_within_the_same_second() {
        // The reason field 22 is read at all. The baseline's whole-second start
        // time is identical for both of these, so it would report the recycled PID
        // as the same process — and a pending signal would go to the wrong one
        // (§15.1, §26).
        let original = parse_pid_stat(fixtures::PID_STAT_SIMPLE).expect("valid");
        let recycled = parse_pid_stat(fixtures::PID_STAT_REUSED_SAME_SECOND).expect("valid");

        assert_eq!(original.pid, recycled.pid);
        let ticks_per_second = 100;
        assert_eq!(
            original.start_time_ticks / ticks_per_second,
            recycled.start_time_ticks / ticks_per_second,
            "the fixtures deliberately start in the same whole second"
        );
        assert_ne!(original.start_time_ticks, recycled.start_time_ticks);
        assert!(recycled.identity().is_reuse_of(&original.identity()));
        assert_ne!(original.identity(), recycled.identity());
    }

    #[test]
    fn a_kernel_thread_is_detected_from_its_flags_rather_than_its_name() {
        let stat = parse_pid_stat(fixtures::PID_STAT_KERNEL_THREAD).expect("valid");
        assert!(stat.is_kernel_thread());
        assert_eq!(stat.flags & PF_KTHREAD, PF_KTHREAD);
        assert_eq!(
            stat.virtual_bytes, 0,
            "kernel threads have no address space"
        );

        // A user process that names itself like a kernel worker is still a user
        // process: §7.2 hides kernel threads, and hiding this would hide a real one.
        let impostor = parse_pid_stat(
            b"4242 (kworker/0:1) S 1 4242 4242 0 -1 4194304 0 0 0 0 1 1 0 0 20 0 1 0 100 4096 1 \
              18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 0 0 0 0 0 0 0 0 0 0 0 0 0",
        )
        .expect("valid");
        assert!(!impostor.is_kernel_thread());
    }

    #[test]
    fn a_zombie_is_reported_as_a_zombie_so_it_is_not_offered_a_signal() {
        let stat = parse_pid_stat(fixtures::PID_STAT_ZOMBIE).expect("valid");
        assert_eq!(stat.state, ProcessState::Zombie);
        assert!(stat.state.is_notable());
        assert!(!stat.state.is_signalable());
    }

    #[test]
    fn every_state_character_maps_to_a_distinct_model_state() {
        assert_eq!(parse_state(b'R'), ProcessState::Running);
        assert_eq!(parse_state(b'S'), ProcessState::Sleeping);
        assert_eq!(parse_state(b'D'), ProcessState::UninterruptibleSleep);
        assert_eq!(parse_state(b'Z'), ProcessState::Zombie);
        assert_eq!(parse_state(b'T'), ProcessState::Stopped);
        assert_eq!(parse_state(b't'), ProcessState::Traced);
        assert_eq!(parse_state(b'I'), ProcessState::Idle);
        assert_eq!(parse_state(b'X'), ProcessState::Dead);
        assert_eq!(parse_state(b'W'), ProcessState::Unknown);
        assert_eq!(parse_state(b'?'), ProcessState::Unknown);
    }

    #[test]
    fn a_truncated_stat_line_fails_instead_of_inventing_an_identity() {
        // The dangerous failure: a half-read line that yields a plausible-looking
        // start time would produce an identity that matches nothing.
        assert_eq!(
            parse_pid_stat(fixtures::PID_STAT_TRUNCATED),
            Err(ParseFailure::Truncated("stat.flags"))
        );
        assert_eq!(
            parse_pid_stat(fixtures::PID_STAT_UNTERMINATED_NAME),
            Err(ParseFailure::Malformed("stat.comm"))
        );
        assert_eq!(
            parse_pid_stat(fixtures::PID_STAT_EMPTY),
            Err(ParseFailure::Empty)
        );
        assert_eq!(
            parse_pid_stat(b"4242 rustc R 1 1"),
            Err(ParseFailure::Malformed("stat.comm"))
        );
        assert_eq!(
            parse_pid_stat(b") 4242 ("),
            Err(ParseFailure::Malformed("stat.comm"))
        );
    }

    #[test]
    fn a_pid_that_does_not_fit_a_u32_is_rejected() {
        assert!(parse_pid_stat(b"99999999999999999999 (x) R 1").is_err());
    }

    #[test]
    fn cpu_time_and_age_are_computed_from_ticks_and_uptime_only() {
        // No wall clock is involved, so §8.1's rule about clock jumps holds by
        // construction.
        let stat = parse_pid_stat(fixtures::PID_STAT_SIMPLE).expect("valid");
        assert_eq!(stat.cpu_time(100), Duration::from_secs(154));
        let uptime = Duration::from_secs(882_137);
        assert_eq!(
            stat.age(uptime, 100),
            Duration::from_secs(882_137 - 882_137)
        );
        let later = Duration::from_secs(882_200);
        assert_eq!(stat.age(later, 100), Duration::from_secs(63));
    }

    #[test]
    fn an_age_read_before_its_uptime_saturates_to_zero_rather_than_underflowing() {
        // Two separate reads: a process can be created between them.
        let stat = parse_pid_stat(fixtures::PID_STAT_SIMPLE).expect("valid");
        assert_eq!(stat.age(Duration::from_secs(1), 100), Duration::ZERO);
    }

    #[test]
    fn status_yields_rss_in_bytes_without_needing_a_page_size() {
        let status = parse_pid_status(fixtures::PID_STATUS_TYPICAL).expect("valid");
        assert_eq!(status.name.as_deref(), Some("rustc"));
        assert_eq!(status.uid, Some(1_000));
        assert_eq!(status.rss_bytes, Some(625_664 * 1024));
        assert_eq!(status.virtual_bytes, Some(2_764_644 * 1024));
        assert_eq!(status.swap_bytes, Some(10_240 * 1024));
        assert_eq!(status.threads, Some(9));
        assert_eq!(status.fd_size, Some(256));
        assert_eq!(status.kernel_thread, Some(false));
        assert_eq!(status.voluntary_switches, Some(48_213));
        assert_eq!(status.involuntary_switches, Some(1_204));
    }

    #[test]
    fn a_kernel_thread_status_has_no_memory_fields_at_all() {
        // Absent, not zero: a kernel thread has no user address space to measure.
        let status = parse_pid_status(fixtures::PID_STATUS_KERNEL_THREAD).expect("valid");
        assert_eq!(status.kernel_thread, Some(true));
        assert_eq!(status.rss_bytes, None);
        assert_eq!(status.virtual_bytes, None);
        assert_eq!(status.uid, Some(0));
    }

    #[test]
    fn a_truncated_status_keeps_the_fields_it_did_read() {
        let status = parse_pid_status(fixtures::PID_STATUS_TRUNCATED).expect("valid");
        assert_eq!(status.name.as_deref(), Some("rustc"));
        assert_eq!(status.rss_bytes, None, "the file ended before VmRSS");
        assert_eq!(parse_pid_status(b""), Err(ParseFailure::Empty));
    }

    #[test]
    fn io_counters_parse_and_prefer_block_layer_bytes() {
        let io = parse_pid_io(fixtures::PID_IO_TYPICAL).expect("valid");
        assert_eq!(io.read_chars, 892_173_402);
        assert_eq!(io.read_bytes, 41_230_336);
        assert_eq!(io.written_bytes, 8_388_608);
        assert_eq!(io.cancelled_write_bytes, 4_096);
        assert!(
            io.read_chars > io.read_bytes,
            "rchar counts page-cache reads too, which is why the block-layer \
             counters are the ones the disk columns use"
        );
    }

    #[test]
    fn the_two_tick_io_delta_is_the_real_block_layer_movement() {
        let before = parse_pid_io(fixtures::PID_IO_TYPICAL).expect("valid");
        let after = parse_pid_io(fixtures::PID_IO_NEXT_TICK).expect("valid");
        assert_eq!(after.read_bytes - before.read_bytes, 2 * 1024 * 1024);
        assert_eq!(after.written_bytes - before.written_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn an_empty_or_truncated_io_file_is_a_typed_failure_not_zero_throughput() {
        assert_eq!(
            parse_pid_io(fixtures::PID_IO_EMPTY),
            Err(ParseFailure::Empty)
        );
        assert_eq!(
            parse_pid_io(fixtures::PID_IO_TRUNCATED),
            Err(ParseFailure::Truncated("io.counters"))
        );
    }

    #[test]
    fn a_command_line_is_nul_separated_and_joins_with_single_spaces() {
        assert_eq!(
            &*join_cmdline(fixtures::CMDLINE_TYPICAL),
            "cargo build --release"
        );
        assert_eq!(
            parse_cmdline(fixtures::CMDLINE_TYPICAL),
            vec![
                Box::<str>::from("cargo"),
                Box::from("build"),
                Box::from("--release")
            ]
        );
    }

    #[test]
    fn a_kernel_thread_has_an_empty_command_line_rather_than_a_missing_one() {
        assert_eq!(
            parse_cmdline(fixtures::CMDLINE_EMPTY),
            Vec::<Box<str>>::new()
        );
        assert_eq!(&*join_cmdline(fixtures::CMDLINE_EMPTY), "");
    }

    #[test]
    fn a_self_relabelled_process_keeps_its_whole_argv_string() {
        // `nginx: worker process` has no NUL at all. Discarding it would blank the
        // command column for every nginx and postgres worker on the machine.
        assert_eq!(
            &*join_cmdline(fixtures::CMDLINE_SPACE_SEPARATED),
            "nginx: worker process"
        );
    }

    #[test]
    fn a_missing_trailing_nul_does_not_lose_the_last_argument() {
        assert_eq!(
            &*join_cmdline(fixtures::CMDLINE_NO_TRAILING_NUL),
            "/usr/bin/python3 -c print(1)"
        );
    }

    #[test]
    fn invalid_utf8_in_an_argument_still_produces_a_visible_row() {
        let joined = join_cmdline(fixtures::CMDLINE_INVALID_UTF8);
        assert!(joined.starts_with("weird"));
        assert!(joined.ends_with("arg"));
    }

    #[test]
    fn a_command_line_is_capped_so_one_process_cannot_dominate_the_pass() {
        let huge = vec![b'x'; MAX_CMDLINE_BYTES * 4];
        let joined = join_cmdline(&huge);
        assert_eq!(joined.len(), MAX_CMDLINE_BYTES);
    }

    #[test]
    fn arguments_remain_redactable_because_they_can_contain_secrets() {
        // §14.2 and §15.2: the joined form is what `redacted_command` operates on.
        let joined = join_cmdline(fixtures::CMDLINE_WITH_SECRET);
        assert!(
            joined.contains("hunter2"),
            "the raw value is present in memory"
        );
        let redacted = joined
            .split_once(' ')
            .map_or(&*joined, |(program, _)| program);
        assert_eq!(redacted, "psql");
    }
}

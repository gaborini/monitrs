//! Process metadata from `kern.proc.*` and per-process counters from `libproc`.
//!
//! # Why the enumeration is native rather than delegated
//!
//! The baseline reaches every process through `proc_pidinfo`, which the kernel
//! refuses for a process owned by another user. When that read fails the baseline
//! has no start time, so it keys the process on `(pid, 0)` — and a start key of
//! zero cannot detect PID reuse at all (§26). One `kern.proc.all` sysctl returns a
//! `kinfo_proc` for *every* process including other users', and `p_starttime` in
//! it is a `timeval`, so the identity this module produces is
//! microsecond-resolution and available for the whole table. That is why the
//! process list is rebuilt from the kernel's enumeration and the baseline is
//! merged into it, rather than the other way round.
//!
//! # What a permission failure must not become
//!
//! For roughly a third of the processes on a normal Mac, `proc_pidinfo` and
//! `proc_pid_rusage` return `EPERM`. The frozen baseline turns that into a zeroed
//! structure — 0 bytes resident, 0% CPU — which is exactly the fabrication §26
//! forbids. Every field this module derives from those calls therefore becomes
//! [`MetricState::PermissionDenied`] on `EPERM`, overriding the baseline's zero.
//! An `EPERM` is direct evidence that the OS refused, and evidence of refusal
//! outranks a number that was never measured.
//!
//! # Cost
//!
//! Two syscalls per process per fast tick, plus one sysctl for the whole table.
//! Measured on an M4 Pro with 962 processes: 272 µs for the table, 1.6 ms for the
//! `proc_pidinfo` pass and 1.5 ms for the `proc_pid_rusage` pass, so about 3.4 ms
//! against §16.1's budget. No per-process read is repeated on the slower tiers.

use core::ffi::{c_int, c_void};
use core::mem::{MaybeUninit, size_of};
use core::time::Duration;
use std::collections::HashMap;
use std::time::{Instant, SystemTime};

use monitrs_core::model::{
    AncestorEntry, MetricState, ProcessDetail, ProcessIdentity, ProcessIo, ProcessMemory,
    ProcessSnapshot, ProcessState, UnavailableReason, UserIdentity,
};
use monitrs_core::rates::{CounterWidth, KeyedProcessCpuTrackers, KeyedRateTrackers};
use monitrs_core::units::Percent;

use super::ffi;
use super::sysctl::{self, NativeError};

/// Mach absolute-time units converted to wall duration.
///
/// `proc_pidinfo`'s CPU totals are in absolute time units, not nanoseconds: on
/// Apple Silicon the timebase is 125/3, so a nanosecond reading would be 41 times
/// too small. Verified against a busy loop on an M4 Pro, where one second of
/// spinning moved the counter by 24 000 000 units.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timebase {
    /// Numerator of the absolute-time-to-nanoseconds ratio.
    numer: u64,
    /// Denominator of the same ratio.
    denom: u64,
}

impl Timebase {
    /// Reads the machine's timebase.
    ///
    /// Falls back to 1/1 — absolute time *is* nanoseconds — only if the query
    /// fails, which is the correct answer on Intel and the least wrong one
    /// anywhere else.
    #[must_use]
    pub fn query() -> Self {
        let mut info = MaybeUninit::<ffi::MachTimebaseInfo>::zeroed();
        // SAFETY: `mach_timebase_info` fills the struct it is given and needs
        // nothing but a writable pointer to one.
        let result = unsafe { ffi::mach_timebase_info(info.as_mut_ptr()) };
        if result != 0 {
            return Self { numer: 1, denom: 1 };
        }
        // SAFETY: the call succeeded, so both fields were written; the struct is a
        // pair of `u32`s with no invalid bit patterns in any case.
        let info = unsafe { info.assume_init() };
        let (numer, denom) = (u64::from(info.numer), u64::from(info.denom));
        if numer == 0 || denom == 0 {
            return Self { numer: 1, denom: 1 };
        }
        Self { numer, denom }
    }

    /// Converts absolute time units into a duration.
    ///
    /// Splits into whole seconds first so a long-lived process's CPU total cannot
    /// overflow the intermediate multiplication.
    #[must_use]
    pub fn to_duration(self, absolute: u64) -> Duration {
        let nanos_per_second = 1_000_000_000u64;
        // ticks per second = 1e9 * denom / numer.
        let ticks_per_second = nanos_per_second
            .saturating_mul(self.denom)
            .checked_div(self.numer)
            .unwrap_or(nanos_per_second)
            .max(1);
        let seconds = absolute / ticks_per_second;
        let remainder = absolute % ticks_per_second;
        let nanos = u32::try_from(remainder.saturating_mul(nanos_per_second) / ticks_per_second)
            .unwrap_or(0);
        Duration::new(seconds, nanos)
    }
}

/// One process as the kernel's own enumeration describes it.
///
/// Everything here is readable for *every* process, including other users', which
/// is what makes it the right source for identity and parentage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct KernelProcess {
    /// Identity whose start key has microsecond resolution.
    pub identity: ProcessIdentity,
    /// Parent PID, or `None` for the two processes that have no parent.
    pub parent_pid: Option<u32>,
    /// Effective user id of the owner.
    pub uid: u32,
    /// Scheduling state, including the debugger-attached case.
    pub state: ProcessState,
    /// The kernel's short name for the process, up to `MAXCOMLEN` bytes.
    pub comm: Box<str>,
    /// Scheduling niceness.
    pub nice: i32,
    /// Wall-clock start time, with microsecond resolution.
    pub started_at: SystemTime,
}

/// Builds a start key from a `p_starttime` timeval.
///
/// Microseconds since the epoch. The whole point is sub-second resolution: two
/// processes reusing a PID inside the same second get different keys, which the
/// baseline's whole-second key cannot distinguish (§26).
fn start_key(start: libc::timeval) -> u64 {
    let seconds = u64::try_from(start.tv_sec).unwrap_or(0);
    let micros = u64::try_from(start.tv_usec).unwrap_or(0);
    seconds.saturating_mul(1_000_000).saturating_add(micros)
}

/// Converts a `p_starttime` timeval into wall-clock time.
fn start_time(start: libc::timeval) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_micros(start_key(start))
}

/// Maps `p_stat` and `p_flag` onto the model's scheduling state.
///
/// A traced process reports `SSTOP` while the debugger holds it, so the flag is
/// checked first: §7.2 wants `t` (traced) distinguishable from `T` (job-control
/// stopped).
fn process_state(stat: core::ffi::c_char, flags: c_int) -> ProcessState {
    if flags & ffi::P_TRACED != 0 {
        return ProcessState::Traced;
    }
    match stat {
        // Still being created by `fork`: it exists but has never run.
        ffi::SIDL => ProcessState::Idle,
        ffi::SRUN => ProcessState::Running,
        ffi::SSLEEP => ProcessState::Sleeping,
        ffi::SSTOP => ProcessState::Stopped,
        ffi::SZOMB => ProcessState::Zombie,
        // macOS does not distinguish uninterruptible sleep at process granularity,
        // so `D` never appears here. Guessing would be worse than `?`.
        _ => ProcessState::Unknown,
    }
}

/// Interprets one `kinfo_proc` record.
fn kernel_process(record: &ffi::KinfoProc) -> Option<KernelProcess> {
    let pid = u32::try_from(record.kp_proc.p_pid).ok()?;
    let parent = record.kp_eproc.e_ppid;
    Some(KernelProcess {
        identity: ProcessIdentity::new(pid, start_key(record.kp_proc.p_starttime)),
        // PID 0 is `kernel_task`, which is its own parent in the kernel's records;
        // reporting that would make the process tree cyclic.
        parent_pid: u32::try_from(parent).ok().filter(|ppid| *ppid != pid),
        uid: record.kp_eproc.e_ucred.cr_uid,
        state: process_state(record.kp_proc.p_stat, record.kp_proc.p_flag),
        comm: sysctl::c_char_array_to_text(&record.kp_proc.p_comm),
        nice: c_int::from(record.kp_proc.p_nice),
        started_at: start_time(record.kp_proc.p_starttime),
    })
}

/// Enumerates every process via `kern.proc.all`.
///
/// `buffer` is reused between ticks: the table is several hundred kilobytes and
/// §16.1 leaves no room for an allocation that size on every sample.
pub(super) fn enumerate(buffer: &mut Vec<u8>) -> Result<Vec<KernelProcess>, NativeError> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
    let written = sysctl::read_growable(&mut mib, buffer)?;
    let record_size = size_of::<ffi::KinfoProc>();
    let records = written / record_size;
    let mut processes = Vec::with_capacity(records);
    for index in 0..records {
        let start = index * record_size;
        let Some(bytes) = buffer.get(start..start + record_size) else {
            break;
        };
        // SAFETY: `bytes` is exactly `size_of::<KinfoProc>()` bytes inside a buffer
        // the kernel filled with whole `kinfo_proc` records. `read_unaligned` is
        // required because a `Vec<u8>` has no alignment guarantee, and `KinfoProc`
        // is `Pod`, so every byte pattern is a valid value.
        let record = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<ffi::KinfoProc>()) };
        if let Some(process) = kernel_process(&record) {
            processes.push(process);
        }
    }
    Ok(processes)
}

/// Reads one process's `kinfo_proc`, for revalidating an identity.
///
/// `Ok(None)` means the process has exited: `kern.proc.pid` answers a dead PID by
/// succeeding and writing nothing, which is not an error (§14.1).
pub(super) fn read_one(pid: u32) -> Result<Option<KernelProcess>, NativeError> {
    let pid = c_int::try_from(pid).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_PID, pid];
    let mut bytes = [0u8; size_of::<ffi::KinfoProc>()];
    let written = sysctl::into_buffer(&mut mib, &mut bytes)?;
    if written == 0 {
        return Ok(None);
    }
    if written != bytes.len() {
        return Err(NativeError::ShortRead {
            got: written,
            want: bytes.len(),
        });
    }
    // SAFETY: as in [`enumerate`] — a full record's worth of kernel-written bytes
    // read out of an unaligned buffer into a `Pod` type.
    let record = unsafe { core::ptr::read_unaligned(bytes.as_ptr().cast::<ffi::KinfoProc>()) };
    Ok(kernel_process(&record))
}

/// Per-process CPU, memory, and thread counters from `proc_pidinfo`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct TaskCounters {
    /// Cumulative CPU time, user plus system.
    pub(super) cpu_time: Duration,
    /// Resident set size.
    pub(super) resident_bytes: u64,
    /// Virtual size.
    pub(super) virtual_bytes: u64,
    /// Thread count.
    pub(super) threads: u32,
}

/// Reads `PROC_PIDTASKINFO` for one process.
///
/// `proc_pidinfo` reports failure by returning fewer bytes than requested rather
/// than `-1`, so the byte count is the success test and `errno` — cleared
/// immediately before the call — carries the reason.
pub(super) fn read_task_counters(
    pid: u32,
    timebase: Timebase,
) -> Result<TaskCounters, NativeError> {
    let pid = c_int::try_from(pid).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
    let want = c_int::try_from(size_of::<libc::proc_taskinfo>())
        .map_err(|_| NativeError::Errno(libc::EINVAL))?;
    sysctl::clear_errno();
    // SAFETY: the buffer is a zeroed `proc_taskinfo` and `want` is its real size,
    // so the kernel cannot write past it. The flavour and the buffer type match:
    // `PROC_PIDTASKINFO` fills a `proc_taskinfo`.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            want,
        )
    };
    if written != want {
        return Err(NativeError::last());
    }
    // SAFETY: the call wrote a whole `proc_taskinfo` into the zeroed buffer.
    let info = unsafe { info.assume_init() };
    Ok(TaskCounters {
        cpu_time: timebase.to_duration(info.pti_total_user.saturating_add(info.pti_total_system)),
        resident_bytes: info.pti_resident_size,
        virtual_bytes: info.pti_virtual_size,
        threads: u32::try_from(info.pti_threadnum).unwrap_or(0),
    })
}

/// Cumulative per-process disk I/O.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct DiskIoCounters {
    /// Cumulative bytes read from disk.
    pub(super) read_bytes: u64,
    /// Cumulative bytes written to disk.
    pub(super) written_bytes: u64,
}

/// Reads cumulative per-process disk I/O via `proc_pid_rusage`.
///
/// `proc_pidinfo` has no I/O flavour; `proc_pid_rusage` is the documented
/// `libproc` call that carries `ri_diskio_*`, and `RUSAGE_INFO_V2` is the oldest
/// revision that has those fields.
pub(super) fn read_disk_io(pid: u32) -> Result<DiskIoCounters, NativeError> {
    let pid = c_int::try_from(pid).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    let mut usage = MaybeUninit::<libc::rusage_info_v2>::zeroed();
    sysctl::clear_errno();
    // SAFETY: the C prototype takes `rusage_info_t *`, i.e. a `void **`, but its
    // documented use is a pointer to the flavour's structure cast to that type —
    // which is what this cast expresses. The buffer is a zeroed `rusage_info_v2`
    // and the flavour requested is `RUSAGE_INFO_V2`, so the kernel writes exactly
    // that structure and no more.
    let result = unsafe {
        libc::proc_pid_rusage(
            pid,
            libc::RUSAGE_INFO_V2,
            usage.as_mut_ptr().cast::<libc::rusage_info_t>(),
        )
    };
    if result != 0 {
        return Err(NativeError::last());
    }
    // SAFETY: the call succeeded, so the whole structure was written.
    let usage = unsafe { usage.assume_init() };
    Ok(DiskIoCounters {
        read_bytes: usage.ri_diskio_bytesread,
        written_bytes: usage.ri_diskio_byteswritten,
    })
}

/// The working directory and filesystem root of one process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct VnodePaths {
    /// Current working directory.
    pub(super) working_directory: Box<str>,
    /// Filesystem root.
    pub(super) root: Box<str>,
}

/// Reads a process's working directory and root via `PROC_PIDVNODEPATHINFO`.
///
/// Refused with `EPERM` for another user's process, which is the case §9.3 singles
/// out: the caller turns that into [`MetricState::PermissionDenied`] and never into
/// an empty path.
pub(super) fn read_vnode_paths(pid: u32) -> Result<VnodePaths, NativeError> {
    let pid = c_int::try_from(pid).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let want = c_int::try_from(size_of::<libc::proc_vnodepathinfo>())
        .map_err(|_| NativeError::Errno(libc::EINVAL))?;
    sysctl::clear_errno();
    // SAFETY: the buffer is a zeroed `proc_vnodepathinfo` and `want` is its real
    // size; the flavour fills exactly that structure.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            want,
        )
    };
    if written != want {
        return Err(NativeError::last());
    }
    // SAFETY: the call wrote the whole structure into the zeroed buffer.
    let info = unsafe { info.assume_init() };
    Ok(VnodePaths {
        working_directory: path_from_vnode(&info.pvi_cdir.vip_path),
        root: path_from_vnode(&info.pvi_rdir.vip_path),
    })
}

/// Flattens `libc`'s chunked `vip_path` array into text.
///
/// `libc` declares the `MAXPATHLEN` buffer as `[[c_char; 32]; 32]` to stay
/// compatible with old compilers, so it has to be re-flattened before it can be
/// read as a C string.
fn path_from_vnode(chunks: &[[core::ffi::c_char; 32]; 32]) -> Box<str> {
    let mut flat = Vec::with_capacity(chunks.len() * 32);
    for chunk in chunks {
        flat.extend_from_slice(chunk);
    }
    sysctl::c_char_array_to_text(&flat)
}

/// The open file descriptor count and niceness from `PROC_PIDTASKALLINFO`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct BsdCounters {
    /// Open file descriptors.
    pub(super) open_files: u32,
    /// Scheduling niceness.
    pub(super) nice: i32,
}

/// Reads `PROC_PIDTASKALLINFO`, which carries the BSD half of a process record.
pub(super) fn read_bsd_counters(pid: u32) -> Result<BsdCounters, NativeError> {
    let pid = c_int::try_from(pid).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    let mut info = MaybeUninit::<libc::proc_taskallinfo>::zeroed();
    let want = c_int::try_from(size_of::<libc::proc_taskallinfo>())
        .map_err(|_| NativeError::Errno(libc::EINVAL))?;
    sysctl::clear_errno();
    // SAFETY: the buffer is a zeroed `proc_taskallinfo` and `want` is its real
    // size; `PROC_PIDTASKALLINFO` fills exactly that structure.
    let written = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTASKALLINFO,
            0,
            info.as_mut_ptr().cast::<c_void>(),
            want,
        )
    };
    if written != want {
        return Err(NativeError::last());
    }
    // SAFETY: the call wrote the whole structure into the zeroed buffer.
    let info = unsafe { info.assume_init() };
    Ok(BsdCounters {
        open_files: info.pbsd.pbi_nfiles,
        nice: info.pbsd.pbi_nice,
    })
}

/// Reads a process's argument vector via `kern.procargs2`.
///
/// # Permission behaviour
///
/// For a process owned by another user this fails with `EINVAL`, not `EPERM` —
/// verified against PID 1 on macOS 26. `EINVAL` is also what a PID that never
/// existed returns, so the two are told apart by asking whether the process is
/// still there: a live process that refuses its arguments is
/// [`MetricState::PermissionDenied`], and a dead one is
/// [`UnavailableReason::ProcessExited`]. Neither is ever an empty string (§9.3).
#[must_use]
pub fn read_process_arguments(pid: u32) -> MetricState<Box<str>> {
    let Ok(raw_pid) = c_int::try_from(pid) else {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
    };
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw_pid];
    let mut buffer = Vec::new();
    match sysctl::read_growable(&mut mib, &mut buffer) {
        Ok(0) => MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited),
        Ok(written) => match buffer.get(..written).and_then(parse_procargs2) {
            Some(args) if !args.command.is_empty() => MetricState::Available(args.command),
            // The buffer was readable but held no argv, which happens for a process
            // that has execed nothing. The caller falls back to the kernel's short
            // name rather than showing an empty cell.
            _ => MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
        },
        Err(error) if error.errno() == Some(libc::EINVAL) => match read_one(pid) {
            // Alive but refusing: this is the hidden-command-line case.
            Ok(Some(_)) => MetricState::PermissionDenied,
            Ok(None) => MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited),
            Err(inner) => inner.to_state(),
        },
        Err(error) => error.to_state(),
    }
}

/// What one `KERN_PROCARGS2` blob holds, beyond the command line.
///
/// Three facts, and all three are immutable for the life of the process — a
/// process cannot change its own `argv[0]` or the path it was executed from — which
/// is what makes them worth caching by [`ProcessIdentity`] instead of re-reading
/// every tick.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct ProcArgs {
    /// The executable path, the blob's first field.
    pub(super) exec_path: Option<Box<str>>,
    /// `argv[0]` exactly as the process was invoked.
    pub(super) argv0: Option<Box<str>>,
    /// Every argument, space-joined, which is what the `COMMAND` column shows.
    pub(super) command: Box<str>,
}

/// Parses a `KERN_PROCARGS2` blob.
///
/// The layout is documented by its consumers rather than by a header: a 32-bit
/// `argc`, then the executable path, then NUL padding, then `argc` NUL-terminated
/// arguments, then the environment — which this deliberately stops before, because
/// §15.2 forbids reading environment values at all.
///
/// The executable path used to be skipped. It is the cheapest thing in this blob and
/// the only route to a process's real name that does not cost a second syscall, so it
/// is now returned along with `argv[0]`: `sysinfo`'s process name is `argv[0]`'s file
/// name where argv is readable and the executable's file name otherwise, and
/// reproducing that exactly is what let the baseline's 31 ms-of-CPU process walk be
/// removed from the fast tier.
fn parse_procargs2(blob: &[u8]) -> Option<ProcArgs> {
    let (count, rest) = blob.split_at_checked(size_of::<u32>())?;
    let argc = u32::from_ne_bytes(count.try_into().ok()?);
    // The executable path comes first and is not one of the `argc` arguments.
    let path_end = rest.iter().position(|byte| *byte == 0)?;
    let exec_path: Option<Box<str>> = rest
        .get(..path_end)
        .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
        .filter(|path| !path.is_empty())
        .map(Box::from);
    let mut cursor = path_end + 1;
    // Skip the alignment padding between the path and `argv[0]`.
    while rest.get(cursor).copied() == Some(0) {
        cursor += 1;
    }

    let mut parts: Vec<String> = Vec::new();
    for _ in 0..argc {
        let remaining = rest.get(cursor..)?;
        let end = remaining
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(remaining.len());
        let part = remaining.get(..end)?;
        parts.push(String::from_utf8_lossy(part).into_owned());
        cursor += end + 1;
    }
    let argv0 = parts
        .first()
        .filter(|first| !first.is_empty())
        .map(|first| Box::from(first.as_str()));
    Some(ProcArgs {
        exec_path,
        argv0,
        command: parts.join(" ").into(),
    })
}

/// The file name of a path, as a fresh `Box<str>`.
///
/// Returns `None` for a path that ends in a separator or is empty, so a caller can
/// fall through to the next source rather than showing an empty name.
fn file_name(path: &str) -> Option<Box<str>> {
    let name = path.rsplit('/').next().unwrap_or(path);
    (!name.is_empty()).then(|| Box::from(name))
}

/// `proc_pidpath` for one process.
///
/// The fallback for the third of the process table whose `KERN_PROCARGS2` is refused:
/// unlike that sysctl, this answers for another user's processes. Measured at 1.7 ms
/// of CPU for a thousand processes, which is why it is worth calling once per process
/// and caching, and not worth calling every tick.
fn executable_path(pid: u32) -> Option<Box<str>> {
    // `PROC_PIDPATHINFO_MAXSIZE`, which libc does not expose as a constant.
    const MAX: usize = 4 * libc::PATH_MAX as usize;
    let raw_pid = c_int::try_from(pid).ok()?;
    let mut buffer = vec![0u8; MAX];
    // SAFETY: `buffer` is `MAX` bytes long and `MAX` is the size passed, so the call
    // cannot write past it. A non-positive return means nothing was written.
    let written = unsafe {
        libc::proc_pidpath(
            raw_pid,
            buffer.as_mut_ptr().cast(),
            u32::try_from(MAX).unwrap_or(0),
        )
    };
    let written = usize::try_from(written).ok().filter(|len| *len > 0)?;
    let path = buffer.get(..written)?;
    Some(Box::from(String::from_utf8_lossy(path).into_owned()))
}

/// The parsed `KERN_PROCARGS2` blob, or `None` when it could not be read.
///
/// Deliberately does not distinguish *why*. [`read_process_arguments`] does, because
/// the detail overlay must show a refusal as a refusal (§26); here the caller only
/// needs to know whether to fall through to `proc_pidpath`, and telling a refusal
/// from an exit would cost a second syscall per process to answer a question nothing
/// asks.
fn read_process_arguments_parsed(pid: u32) -> Option<ProcArgs> {
    let raw_pid = c_int::try_from(pid).ok()?;
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw_pid];
    let mut buffer = Vec::new();
    let written = sysctl::read_growable(&mut mib, &mut buffer).ok()?;
    buffer.get(..written).and_then(parse_procargs2)
}

/// Resolved user names, cached because the lookup can consult a directory service.
///
/// One entry per distinct uid on the machine — a handful in practice — so this is
/// bounded by the number of users rather than by the number of processes (§10.3).
#[derive(Debug, Default)]
pub(super) struct UserNames {
    resolved: HashMap<u32, Option<Box<str>>>,
}

impl UserNames {
    /// Looks up a uid's login name, remembering both hits and misses.
    pub(super) fn resolve(&mut self, uid: u32) -> Option<Box<str>> {
        if let Some(cached) = self.resolved.get(&uid) {
            return cached.clone();
        }
        let name = lookup_user_name(uid);
        self.resolved.insert(uid, name.clone());
        name
    }
}

/// The largest `getpwuid_r` scratch buffer worth trying.
const PASSWD_BUFFER_LIMIT: usize = 16 * 1024;

/// Resolves a uid to a login name with `getpwuid_r`.
///
/// The reentrant form is used because the collector runs on its own thread and the
/// non-reentrant `getpwuid` returns a pointer into shared state.
fn lookup_user_name(uid: u32) -> Option<Box<str>> {
    let mut capacity = 1024usize;
    while capacity <= PASSWD_BUFFER_LIMIT {
        let mut passwd = MaybeUninit::<libc::passwd>::zeroed();
        let mut scratch = vec![0i8; capacity];
        let mut found: *mut libc::passwd = core::ptr::null_mut();
        // SAFETY: `passwd` is a writable `libc::passwd`, `scratch` is a unique
        // buffer of exactly `capacity` bytes, and `found` is a writable pointer
        // slot — the three out-parameters `getpwuid_r` documents. Any string
        // pointers it writes into `passwd` point into `scratch`, which outlives the
        // read below.
        let result = unsafe {
            libc::getpwuid_r(
                uid,
                passwd.as_mut_ptr(),
                scratch.as_mut_ptr(),
                scratch.len(),
                &mut found,
            )
        };
        if result == libc::ERANGE {
            capacity = capacity.saturating_mul(2);
            continue;
        }
        if result != 0 || found.is_null() {
            return None;
        }
        // SAFETY: `found` is non-null, so `getpwuid_r` populated the record; it
        // points at `passwd`, which is still alive and now initialised.
        let name = unsafe { (*found).pw_name };
        if name.is_null() {
            return None;
        }
        // SAFETY: `pw_name` points into `scratch`, which is still in scope, and
        // `getpwuid_r` guarantees it is NUL-terminated.
        let text = unsafe { core::ffi::CStr::from_ptr(name) };
        let text = text.to_string_lossy().into_owned();
        return (!text.is_empty()).then(|| text.into());
    }
    None
}

/// Whether a native read failed because the OS refused it.
fn refused<T>(result: &Result<T, NativeError>) -> bool {
    result
        .as_ref()
        .err()
        .is_some_and(|error| error.is_permission_denied())
}

/// Everything the enricher needs to know about one process this tick.
#[derive(Clone, Copy, Debug)]
struct NativeCounters {
    task: Result<TaskCounters, NativeError>,
    io: Result<DiskIoCounters, NativeError>,
}

/// Merges the kernel's process table with the baseline's text fields.
///
/// Holds the CPU and I/O baselines for every live process, so it must outlive
/// individual ticks (§9.1).
#[derive(Debug)]
pub(super) struct ProcessEnricher {
    timebase: Timebase,
    cpu: KeyedProcessCpuTrackers,
    read: KeyedRateTrackers<ProcessIdentity>,
    write: KeyedRateTrackers<ProcessIdentity>,
    users: UserNames,
    /// The name, command line and executable of every live process.
    ///
    /// Read once, when a process is first seen, and then kept: a process cannot
    /// change its own `argv[0]` or the path it was executed from, so re-reading them
    /// every tick was buying the same answer a thousand times a second. Keyed by
    /// [`ProcessIdentity`] rather than by PID so a reused PID gets its own entry
    /// (§26), and pruned by the same pass that prunes the rate trackers, which is
    /// what bounds it (§10.3).
    facts: HashMap<ProcessIdentity, ProcessFacts>,
    /// Whether any process's counters were readable on the last tick.
    ///
    /// Drives the per-process-I/O capability: on a machine where every read is
    /// refused the capability is `PermissionDenied`, not `Available` (§4).
    any_counters_readable: bool,
    /// Whether any process's counters were refused on the last tick.
    any_counters_denied: bool,
}

/// What a process is, as opposed to what it is currently doing.
///
/// Everything here is fixed at `exec` time. The values are `Box<str>` and cloned
/// into each snapshot rather than shared, because a snapshot is handed across a
/// thread boundary and must not borrow from the collector (§10.4).
#[derive(Clone, Debug)]
struct ProcessFacts {
    name: Box<str>,
    command: Box<str>,
    exe: Option<Box<str>>,
}

impl ProcessEnricher {
    /// Builds an enricher with no baselines.
    pub(super) fn new(timebase: Timebase) -> Self {
        Self {
            timebase,
            cpu: KeyedProcessCpuTrackers::new(()),
            // Process I/O counters are 64-bit and reset only when the process
            // exits, which the identity key already catches.
            read: KeyedRateTrackers::new(CounterWidth::Bits64),
            write: KeyedRateTrackers::new(CounterWidth::Bits64),
            users: UserNames::default(),
            facts: HashMap::new(),
            any_counters_readable: false,
            any_counters_denied: false,
        }
    }

    /// Whether per-process counters were readable for at least one process.
    pub(super) const fn counters_readable(&self) -> bool {
        self.any_counters_readable
    }

    /// Whether per-process counters were refused for at least one process.
    pub(super) const fn counters_denied(&self) -> bool {
        self.any_counters_denied
    }

    /// Rebuilds the process table from the kernel's enumeration.
    ///
    /// `baseline` supplies the fields only the cross-platform layer has — the
    /// joined command line, the executable path, the resolved user name — and is
    /// matched by PID. Matching by PID alone is safe here precisely because both
    /// views come from the same tick microseconds apart, and because every
    /// identity-bearing field is recomputed from `kernel` rather than copied.
    pub(super) fn enrich(
        &mut self,
        baseline: Vec<ProcessSnapshot>,
        kernel: &[KernelProcess],
        at: Instant,
        wall_time: SystemTime,
        total_memory: u64,
        can_compute_rates: bool,
    ) -> Vec<ProcessSnapshot> {
        let mut by_pid: HashMap<u32, ProcessSnapshot> = baseline
            .into_iter()
            .map(|process| (process.identity.pid, process))
            .collect();

        self.any_counters_readable = false;
        self.any_counters_denied = false;

        let mut rows = Vec::with_capacity(kernel.len());
        for process in kernel {
            let previous = by_pid.remove(&process.identity.pid);
            let counters = NativeCounters {
                task: read_task_counters(process.identity.pid, self.timebase),
                io: read_disk_io(process.identity.pid),
            };
            if counters.task.is_ok() || counters.io.is_ok() {
                self.any_counters_readable = true;
            }
            if refused(&counters.task) || refused(&counters.io) {
                self.any_counters_denied = true;
            }
            rows.push(self.row(
                process,
                previous,
                counters,
                at,
                wall_time,
                total_memory,
                can_compute_rates,
            ));
        }

        // Drop the baselines of processes the kernel no longer lists, so PID churn
        // cannot grow the trackers without bound (§10.3).
        let live: std::collections::HashSet<ProcessIdentity> =
            kernel.iter().map(|process| process.identity).collect();
        self.cpu.retain(|identity| live.contains(identity));
        self.read.retain(|identity| live.contains(identity));
        self.write.retain(|identity| live.contains(identity));
        // The facts cache is pruned by the same pass, for the same reason: a machine
        // that churns through PIDs would otherwise accumulate one entry per process
        // that has ever existed (§10.3).
        self.facts.retain(|identity, _| live.contains(identity));

        rows
    }

    /// The immutable facts about `process`, read once and then remembered.
    ///
    /// The name is **the executable's file name**, falling back to `p_comm`.
    ///
    /// Three candidates were measured against 1030 live processes before this was
    /// settled, and the reasoning matters more than the ranking:
    ///
    /// * **`p_comm`**, the kernel's short name, is free — it is already in the process
    ///   table — and always present, but `MAXCOMLEN` truncates it to 16 bytes. It
    ///   matched the baseline for 591 processes and was a *prefix* of it for 1023, so
    ///   truncation was the only difference: `Google Chrome Helper (Renderer)` arrives
    ///   as `Google Chrome He`. Unusable as a first choice, correct as a last one.
    /// * **The executable's file name** matched 1022 of 1030 and is never truncated.
    ///   It comes free with the argument blob (its first field) and from
    ///   `proc_pidpath` otherwise — which matters, because `KERN_PROCARGS2` is refused
    ///   for other users' processes: 1 of 333 root-owned ones on the machine this was
    ///   measured on, against 333 of 333 for `proc_pidpath`.
    ///
    ///   The order of those two is not arbitrary. The blob carries the path the process
    ///   was *launched* from and `proc_pidpath` resolves symlinks to the real file, so
    ///   for a versioned install — `~/.local/bin/claude` pointing at
    ///   `.../versions/2.1.220` — the blob says `claude` and `proc_pidpath` says
    ///   `2.1.220`. The launched path is the one a user recognises, and it is also the
    ///   cheaper of the two, so it goes first.
    /// * **`argv[0]`** accounts for the remaining 8, and was tried first — until the
    ///   test that compares the two collectors showed what argv[0] actually contains.
    ///   It is not a name; it is an argument, and a process may write anything into
    ///   it. Measured examples: `Cursor Helper (Plugin): extension-host (agent-exec)
    ///   servicrab [2-14]`, `server-memory@2026.1.26` for a `node` process, and `-zsh`
    ///   for a login shell. A `NAME` column of 32 cells cannot absorb that, and the
    ///   badness is unbounded — whereas the failure mode of the executable's name is
    ///   bounded and dull: a binary whose launch path is itself a version-numbered file
    ///   is called `2.1.220`.
    ///
    /// So this deliberately differs from `sysinfo` for a handful of processes, and it
    /// differs in the predictable direction. The full command line, argv[0] included,
    /// is in the `COMMAND` column either way.
    ///
    /// A name is never empty: an empty row is indistinguishable from a rendering bug,
    /// and every process has at least a `p_comm`.
    fn facts_for(&mut self, process: &KernelProcess) -> ProcessFacts {
        if let Some(known) = self.facts.get(&process.identity) {
            return known.clone();
        }
        let parsed = read_process_arguments_parsed(process.identity.pid);
        // `proc_pidpath` only when the blob could not supply the path, which keeps the
        // common case at one syscall for a process we have never seen before.
        let exe = parsed
            .as_ref()
            .and_then(|args| args.exec_path.clone())
            .or_else(|| executable_path(process.identity.pid));
        let name = exe
            .as_deref()
            .and_then(file_name)
            .unwrap_or_else(|| process.comm.clone());
        let facts = ProcessFacts {
            name,
            command: parsed.map(|args| args.command).unwrap_or_default(),
            exe,
        };
        self.facts.insert(process.identity, facts.clone());
        facts
    }

    /// Builds one enriched row.
    #[allow(
        clippy::too_many_arguments,
        reason = "every argument is a distinct fact about the same tick; bundling them \
                  into a struct would only move the list one line up"
    )]
    fn row(
        &mut self,
        process: &KernelProcess,
        baseline: Option<ProcessSnapshot>,
        counters: NativeCounters,
        at: Instant,
        wall_time: SystemTime,
        total_memory: u64,
        can_compute_rates: bool,
    ) -> ProcessSnapshot {
        let identity = process.identity;
        let cpu = match &counters.task {
            Ok(task) => self.cpu.observe(identity, task.cpu_time, at),
            Err(error) => error.to_state(),
        };
        let memory = match &counters.task {
            Ok(task) => ProcessMemory {
                rss_bytes: MetricState::Available(task.resident_bytes),
                virtual_bytes: MetricState::Available(task.virtual_bytes),
                share_of_total: Percent::ratio(task.resident_bytes, total_memory)
                    .map_or(MetricState::Unsupported, MetricState::Available),
            },
            Err(error) => ProcessMemory {
                rss_bytes: error.to_state(),
                virtual_bytes: error.to_state(),
                share_of_total: error.to_state(),
            },
        };
        let io = match &counters.io {
            Ok(disk) => {
                let read = self.read.observe(identity, disk.read_bytes, at);
                let write = self.write.observe(identity, disk.written_bytes, at);
                ProcessIo {
                    read: if can_compute_rates {
                        read
                    } else {
                        MetricState::WarmingUp
                    },
                    write: if can_compute_rates {
                        write
                    } else {
                        MetricState::WarmingUp
                    },
                    read_total_bytes: MetricState::Available(disk.read_bytes),
                    write_total_bytes: MetricState::Available(disk.written_bytes),
                }
            }
            Err(error) => ProcessIo {
                read: error.to_state(),
                write: error.to_state(),
                read_total_bytes: error.to_state(),
                write_total_bytes: error.to_state(),
            },
        };
        let threads = match &counters.task {
            Ok(task) => MetricState::Available(task.threads),
            Err(error) => error.to_state(),
        };

        let age = wall_time
            .duration_since(process.started_at)
            .map_or(MetricState::WarmingUp, MetricState::Available);
        let user = MetricState::Available(UserIdentity {
            uid: process.uid,
            name: self.users.resolve(process.uid),
        });

        // The identity, name, command and executable all come from this layer now.
        // The baseline row is consulted only for what it alone can still contribute —
        // and on macOS that is nothing, since `is_kernel_thread` is always false here
        // (§7.2: macOS has no per-process kernel threads to hide). It is kept as an
        // argument rather than removed because the Linux enrichment composes the same
        // way, and a `None` here must produce a complete row rather than a stub.
        // An empty command is left empty rather than filled with the name.
        // `ProcessSnapshot::command_or_name` is the one place that decides what a row
        // with no readable command line shows, it is already tested, and a second
        // copy of that decision here would be a second thing to keep in step. The
        // *reason* it is unreadable is not lost either: the detail overlay reads the
        // arguments on demand through `read_process_arguments`, which reports
        // `PermissionDenied` as itself (§26).
        let facts = self.facts_for(process);
        ProcessSnapshot {
            identity,
            parent_pid: process.parent_pid,
            name: facts.name,
            command: facts.command,
            exe: facts.exe,
            user,
            state: process.state,
            cpu,
            memory,
            io,
            threads,
            age,
            started_at: MetricState::Available(process.started_at),
            is_kernel_thread: baseline.is_some_and(|previous| previous.is_kernel_thread),
        }
    }

    /// The number of processes with a live CPU baseline, for the growth test.
    #[cfg(test)]
    pub(super) fn tracked_processes(&self) -> usize {
        self.cpu.len()
    }
}

/// Collects the on-demand detail for one process (§8.6).
///
/// Returns the fields this platform can supply; the caller merges them into the
/// baseline's detail so that ancestry and children — which need the whole table —
/// are not recomputed here.
pub(super) fn detail_for(
    pid: u32,
    collected_at: SystemTime,
    identity: ProcessIdentity,
) -> ProcessDetail {
    let mut detail = ProcessDetail::pending(identity, collected_at);
    match read_vnode_paths(pid) {
        Ok(paths) => {
            detail.working_directory = MetricState::Available(paths.working_directory);
            detail.root = MetricState::Available(paths.root);
        }
        Err(error) => {
            // §9.3's named case: another user's process hides its working
            // directory, and the answer is "permission denied", never "".
            detail.working_directory = error.to_state();
            detail.root = error.to_state();
        }
    }
    match read_bsd_counters(pid) {
        Ok(bsd) => {
            detail.open_files = MetricState::Available(bsd.open_files);
            detail.nice = MetricState::Available(bsd.nice);
        }
        Err(error) => {
            detail.open_files = error.to_state();
            detail.nice = error.to_state();
        }
    }
    // Counting sockets means walking the descriptor table, which needs
    // `PROC_PIDLISTFDS` and one `proc_pidfdinfo` per descriptor. §8.6 puts that on
    // the on-demand tier, but §16.1's budget does not stretch to it for a process
    // with thousands of descriptors, so it stays absent rather than sometimes-slow.
    detail.sockets = MetricState::Unsupported;
    // No cgroups on macOS.
    detail.cgroup = MetricState::Unsupported;
    detail.container = MetricState::Unsupported;
    detail
}

/// Builds the ancestry chain for a process from the kernel's own table.
///
/// Walks parent links to PID 1, with a bound so a corrupted or racing table cannot
/// produce an infinite loop.
pub(super) fn ancestry(pid: u32, table: &[KernelProcess]) -> Vec<AncestorEntry> {
    let by_pid: HashMap<u32, &KernelProcess> = table
        .iter()
        .map(|process| (process.identity.pid, process))
        .collect();
    let mut chain = Vec::new();
    let mut current = by_pid.get(&pid).and_then(|process| process.parent_pid);
    // The deepest plausible process tree is a few dozen entries; anything longer is
    // a cycle introduced by a PID being reused mid-walk.
    for _ in 0..64 {
        let Some(parent_pid) = current else { break };
        let Some(parent) = by_pid.get(&parent_pid) else {
            break;
        };
        chain.push(AncestorEntry {
            identity: parent.identity,
            name: parent.comm.clone(),
        });
        current = parent.parent_pid;
    }
    chain
}

/// The direct children of `pid`, from the kernel's own table.
pub(super) fn children(pid: u32, table: &[KernelProcess]) -> Vec<ProcessIdentity> {
    table
        .iter()
        .filter(|process| process.parent_pid == Some(pid))
        .map(|process| process.identity)
        .collect()
}

/// Counts every descendant of `pid`, not only its direct children (§2.4).
pub(super) fn descendants(pid: u32, table: &[KernelProcess]) -> u32 {
    let mut frontier = vec![pid];
    let mut seen = std::collections::HashSet::new();
    let mut count = 0u32;
    while let Some(parent) = frontier.pop() {
        if !seen.insert(parent) {
            continue;
        }
        for process in table {
            if process.parent_pid == Some(parent) {
                count = count.saturating_add(1);
                frontier.push(process.identity.pid);
            }
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use monitrs_core::rates::ProcessCpuTracker;

    fn timeval(seconds: i64, micros: i32) -> libc::timeval {
        libc::timeval {
            tv_sec: seconds,
            tv_usec: micros,
        }
    }

    fn kernel(pid: u32, parent: Option<u32>, key: u64) -> KernelProcess {
        KernelProcess {
            identity: ProcessIdentity::new(pid, key),
            parent_pid: parent,
            uid: 501,
            state: ProcessState::Sleeping,
            comm: "test".into(),
            nice: 0,
            started_at: SystemTime::UNIX_EPOCH + Duration::from_micros(key),
        }
    }

    #[test]
    fn the_start_key_has_microsecond_resolution_so_reuse_inside_a_second_is_visible() {
        // The whole reason this module exists: the baseline's whole-second key
        // cannot tell these two apart, and signalling the wrong one is the failure
        // §15.1 is written to prevent.
        let first = start_key(timeval(1_785_362_554, 650_814));
        let second = start_key(timeval(1_785_362_554, 650_815));
        assert_ne!(first, second);
        assert_eq!(first, 1_785_362_554_650_814);
        assert!(
            ProcessIdentity::new(4_242, second).is_reuse_of(&ProcessIdentity::new(4_242, first))
        );
    }

    #[test]
    fn a_negative_or_absurd_start_time_does_not_wrap_the_key() {
        assert_eq!(start_key(timeval(-1, -1)), 0);
        assert_eq!(start_key(timeval(i64::MAX, 999_999)), u64::MAX);
    }

    #[test]
    fn the_start_key_and_the_start_time_describe_the_same_instant() {
        let raw = timeval(1_785_362_554, 650_814);
        let expected = SystemTime::UNIX_EPOCH + Duration::from_micros(1_785_362_554_650_814);
        assert_eq!(start_time(raw), expected);
    }

    #[test]
    fn a_traced_process_is_reported_as_traced_and_not_merely_stopped() {
        // §7.2 wants `t` distinguishable from `T`, and the kernel reports SSTOP for
        // both.
        assert_eq!(
            process_state(ffi::SSTOP, ffi::P_TRACED),
            ProcessState::Traced
        );
        assert_eq!(process_state(ffi::SSTOP, 0), ProcessState::Stopped);
    }

    #[test]
    fn every_documented_bsd_status_maps_to_a_distinct_state() {
        assert_eq!(process_state(ffi::SIDL, 0), ProcessState::Idle);
        assert_eq!(process_state(ffi::SRUN, 0), ProcessState::Running);
        assert_eq!(process_state(ffi::SSLEEP, 0), ProcessState::Sleeping);
        assert_eq!(process_state(ffi::SZOMB, 0), ProcessState::Zombie);
        // An unrecognised status renders as `?`, which is honest; guessing
        // "running" would hide a kernel we do not understand.
        assert_eq!(process_state(99, 0), ProcessState::Unknown);
    }

    #[test]
    fn absolute_time_is_converted_through_the_timebase_and_not_read_as_nanoseconds() {
        // Verified on an M4 Pro: one second of CPU moves the counter by 24 000 000
        // absolute units, which is 41.67 times fewer than a nanosecond reading.
        let apple_silicon = Timebase {
            numer: 125,
            denom: 3,
        };
        assert_eq!(
            apple_silicon.to_duration(24_000_000),
            Duration::from_secs(1)
        );
        assert_eq!(
            apple_silicon.to_duration(12_000_000),
            Duration::from_millis(500)
        );
        // On Intel the timebase is 1/1, so absolute time really is nanoseconds.
        let intel = Timebase { numer: 1, denom: 1 };
        assert_eq!(intel.to_duration(1_000_000_000), Duration::from_secs(1));
    }

    #[test]
    fn a_degenerate_timebase_cannot_divide_by_zero() {
        let broken = Timebase { numer: 0, denom: 0 };
        assert_eq!(broken.to_duration(1_000_000_000), Duration::from_secs(1));
    }

    #[test]
    fn a_long_lived_process_cpu_total_does_not_overflow_the_conversion() {
        let apple_silicon = Timebase {
            numer: 125,
            denom: 3,
        };
        // A year of CPU time in 24 MHz units.
        let a_year = 24_000_000u64 * 365 * 24 * 60 * 60;
        assert_eq!(
            apple_silicon.to_duration(a_year),
            Duration::from_secs(365 * 24 * 60 * 60)
        );
        assert!(apple_silicon.to_duration(u64::MAX) > Duration::from_secs(1));
    }

    #[test]
    fn kernel_task_is_not_reported_as_its_own_parent() {
        // The kernel records PID 0's parent as PID 0, and a self-parent makes the
        // process tree cyclic.
        let mut record: ffi::KinfoProc = zeroed_record();
        record.kp_proc.p_pid = 0;
        record.kp_eproc.e_ppid = 0;
        let process = kernel_process(&record).expect("pid 0 is a real process");
        assert_eq!(process.parent_pid, None);
    }

    #[test]
    fn a_normal_record_keeps_its_parent() {
        let mut record: ffi::KinfoProc = zeroed_record();
        record.kp_proc.p_pid = 4_242;
        record.kp_eproc.e_ppid = 1;
        record.kp_proc.p_stat = ffi::SRUN;
        let process = kernel_process(&record).expect("valid record");
        assert_eq!(process.identity.pid, 4_242);
        assert_eq!(process.parent_pid, Some(1));
        assert_eq!(process.state, ProcessState::Running);
    }

    /// A zeroed `kinfo_proc`, which is what a test needs to poke one field of.
    fn zeroed_record() -> ffi::KinfoProc {
        // SAFETY: `KinfoProc` is `Pod`: every bit pattern of its size is a valid
        // value, so an all-zero one is too.
        unsafe { MaybeUninit::<ffi::KinfoProc>::zeroed().assume_init() }
    }

    #[test]
    fn procargs_parsing_joins_the_arguments_and_stops_before_the_environment() {
        let mut blob = Vec::new();
        blob.extend_from_slice(&2u32.to_ne_bytes());
        blob.extend_from_slice(b"/usr/bin/ssh\0");
        blob.extend_from_slice(b"\0\0\0"); // alignment padding
        blob.extend_from_slice(b"ssh\0");
        blob.extend_from_slice(b"host\0");
        blob.extend_from_slice(b"SECRET=hunter2\0");
        let args = parse_procargs2(&blob).expect("two arguments");
        assert_eq!(&*args.command, "ssh host");
        assert!(
            !args.command.contains("hunter2"),
            "§15.2 forbids reading environment values at all"
        );
        // The two fields that used to be discarded. Both are immutable for the life
        // of the process, which is what makes them cacheable rather than per-tick.
        assert_eq!(
            args.exec_path.as_deref(),
            Some("/usr/bin/ssh"),
            "the blob's first field is the executable path"
        );
        assert_eq!(
            args.argv0.as_deref(),
            Some("ssh"),
            "argv[0] is how the process was invoked, which is not the same as its path"
        );
    }

    #[test]
    fn procargs_parsing_of_a_truncated_blob_produces_no_command_rather_than_garbage() {
        assert!(parse_procargs2(&[]).is_none());
        assert!(parse_procargs2(&1u32.to_ne_bytes()).is_none());
        // An argc larger than the payload must not read past the end.
        let mut blob = Vec::new();
        blob.extend_from_slice(&9u32.to_ne_bytes());
        blob.extend_from_slice(b"/bin/sh\0\0sh\0");
        assert!(parse_procargs2(&blob).is_none());
    }

    #[test]
    fn ancestry_walks_to_the_root_without_looping_on_a_cycle() {
        let table = vec![
            kernel(1, None, 10),
            kernel(100, Some(1), 20),
            kernel(200, Some(100), 30),
        ];
        let chain = ancestry(200, &table);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain.first().map(|entry| entry.identity.pid), Some(100));
        assert_eq!(chain.get(1).map(|entry| entry.identity.pid), Some(1));

        // A table that claims two processes are each other's parent must still
        // terminate.
        let cyclic = vec![kernel(7, Some(8), 1), kernel(8, Some(7), 2)];
        assert!(ancestry(7, &cyclic).len() <= 64);
    }

    #[test]
    fn descendants_counts_the_whole_subtree_and_not_only_children() {
        let table = vec![
            kernel(1, None, 10),
            kernel(100, Some(1), 20),
            kernel(200, Some(100), 30),
            kernel(300, Some(200), 40),
            kernel(400, Some(1), 50),
        ];
        assert_eq!(descendants(1, &table), 4);
        assert_eq!(descendants(100, &table), 2);
        assert_eq!(descendants(300, &table), 0);
        assert_eq!(children(1, &table).len(), 2);
    }

    #[test]
    fn descendants_terminates_on_a_cyclic_table() {
        let cyclic = vec![kernel(7, Some(8), 1), kernel(8, Some(7), 2)];
        assert!(descendants(7, &cyclic) <= 2);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_enumeration_includes_our_own_process_with_a_microsecond_key() {
        let mut buffer = Vec::new();
        let table = enumerate(&mut buffer).expect("kern.proc.all is readable");
        assert!(table.len() > 10, "a Mac runs more than ten processes");

        let me = std::process::id();
        let mine = table
            .iter()
            .find(|process| process.identity.pid == me)
            .unwrap_or_else(|| panic!("our own pid {me} is missing"));
        assert!(
            mine.identity.start_key > 1_000_000_000_000_000,
            "a microsecond key since the epoch is a sixteen-digit number, got {}",
            mine.identity.start_key
        );
        assert_eq!(mine.uid, {
            // SAFETY: `getuid` takes no arguments and cannot fail.
            unsafe { libc::getuid() }
        });
        assert!(!mine.comm.is_empty());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_enumeration_sees_processes_owned_by_root() {
        // The gap this module closes: the baseline cannot read these at all.
        let mut buffer = Vec::new();
        let table = enumerate(&mut buffer).expect("readable");
        let root_owned = table.iter().filter(|process| process.uid == 0).count();
        assert!(
            root_owned > 5,
            "macOS always runs several root daemons, saw {root_owned}"
        );
        let launchd = table
            .iter()
            .find(|process| process.identity.pid == 1)
            .expect("pid 1 always exists");
        assert_eq!(launchd.uid, 0);
        assert!(
            launchd.identity.start_key > 0,
            "even pid 1 has a start time"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn our_own_counters_are_readable_and_a_root_process_reports_permission_denied() {
        let timebase = Timebase::query();
        let me = std::process::id();
        let mine = read_task_counters(me, timebase).expect("our own task info");
        assert!(mine.resident_bytes > 0, "we are resident in memory");
        assert!(mine.threads >= 1);

        // PID 1 is root-owned on every Mac. §9.3 requires this to surface as a
        // permission limitation rather than as zeroes.
        let error = read_task_counters(1, timebase).expect_err("launchd must refuse us");
        assert!(
            error.is_permission_denied(),
            "expected EPERM from pid 1, got {error:?}"
        );
        let state: MetricState<u64> = error.to_state();
        assert_eq!(state, MetricState::PermissionDenied);
        assert_ne!(
            state,
            MetricState::Available(0),
            "the whole point: a refused read is not zero"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_root_owned_process_hides_its_command_line_as_permission_denied() {
        // §9.3 names this case explicitly. `kern.procargs2` answers EINVAL rather
        // than EPERM, which is why the mapping consults liveness first.
        let denied = read_process_arguments(1);
        assert_eq!(
            denied,
            MetricState::PermissionDenied,
            "pid 1 must be permission denied, not an empty command line"
        );
        assert_eq!(denied.fresh(), None);
        assert_eq!(denied.placeholder(), Some("permission denied"));

        let mine = read_process_arguments(std::process::id());
        let command = mine.fresh().expect("we can read our own arguments");
        assert!(!command.is_empty());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_root_owned_process_hides_its_working_directory_as_permission_denied() {
        let detail = detail_for(1, SystemTime::now(), ProcessIdentity::new(1, 1));
        assert_eq!(detail.working_directory, MetricState::PermissionDenied);
        assert_eq!(detail.root, MetricState::PermissionDenied);
        assert_eq!(
            detail.working_directory.fresh(),
            None,
            "never an empty string (§9.3)"
        );

        let me = std::process::id();
        let mine = detail_for(me, SystemTime::now(), ProcessIdentity::new(me, 1));
        let cwd = mine
            .working_directory
            .fresh()
            .expect("we can read our own cwd");
        assert!(cwd.starts_with('/'), "got {cwd}");
        assert!(mine.open_files.fresh().is_some_and(|count| *count > 0));
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_dead_pid_is_reported_as_gone_rather_than_as_an_error() {
        // A PID far above `kern.maxproc` cannot be live.
        let phantom = 0x7fff_0000u32;
        assert_eq!(read_one(phantom).expect("the query itself succeeds"), None);
        let error = read_task_counters(phantom, Timebase::query()).expect_err("no such process");
        assert!(error.is_gone(), "expected ESRCH, got {error:?}");
        assert_eq!(
            read_process_arguments(phantom),
            MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited)
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn our_own_disk_io_counters_are_readable() {
        let io = read_disk_io(std::process::id()).expect("our own rusage");
        // Deliberately not asserting a value: a fresh test binary may have read
        // nothing yet, and 0 bytes read is a real measurement here.
        assert!(io.read_bytes < u64::MAX);
        let denied = read_disk_io(1).expect_err("launchd must refuse us");
        assert!(denied.is_permission_denied(), "{denied:?}");
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn our_own_process_cpu_warms_up_before_it_reports_a_percentage() {
        let timebase = Timebase::query();
        let me = std::process::id();
        let mut tracker = ProcessCpuTracker::new();
        let first = read_task_counters(me, timebase).expect("task info");
        let t0 = Instant::now();
        assert!(tracker.observe(first.cpu_time, t0).is_warming_up());

        // Burn a measurable amount of CPU so the second sample is not zero.
        let deadline = Instant::now() + Duration::from_millis(200);
        let mut spin = 0u64;
        while Instant::now() < deadline {
            spin = spin.wrapping_add(1);
        }
        let second = read_task_counters(me, timebase).expect("task info");
        let usage = tracker
            .observe(second.cpu_time, Instant::now())
            .fresh()
            .copied()
            .expect("the second sample is measurable");
        assert!(
            usage.value() > 10.0,
            "a spinning thread should read well above 10% of a core, got {}",
            usage.value()
        );
        assert!(
            usage.value() < 100.0 * 64.0,
            "core normalization cannot exceed the number of cores"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_user_name_cache_resolves_our_own_uid_once() {
        let mut names = UserNames::default();
        // SAFETY: `getuid` takes no arguments and cannot fail.
        let uid = unsafe { libc::getuid() };
        let first = names.resolve(uid);
        assert!(first.is_some(), "our own uid must resolve to a login name");
        assert_eq!(first, names.resolve(uid), "the cache must be stable");
        assert_eq!(names.resolve(0).as_deref(), Some("root"));
        // A uid with no account must be a miss rather than an invented name. Note
        // that macOS *does* have accounts at the top of the range — 4294967294 is
        // `nobody` — so the probe uses a value in the middle of nowhere.
        let unassigned = 900_123_456;
        assert_eq!(names.resolve(unassigned), None);
        assert_eq!(
            names.resolve(unassigned),
            None,
            "a miss must be cached as a miss, not retried forever"
        );
    }

    #[test]
    #[ignore = "platform smoke test: times the per-process enrichment"]
    fn the_per_process_enrichment_stays_inside_its_budget() {
        // §16.1: the whole fast tier has tens of milliseconds. Two syscalls per
        // process is the dominant cost, so it is worth measuring rather than
        // assuming.
        let timebase = Timebase::query();
        let mut buffer = Vec::new();
        let table = enumerate(&mut buffer).expect("readable");
        let start = Instant::now();
        for process in &table {
            let _ = read_task_counters(process.identity.pid, timebase);
            let _ = read_disk_io(process.identity.pid);
        }
        let elapsed = start.elapsed();
        // Measured at 1.9 ms on an M4 Pro with 973 processes.
        assert!(
            elapsed < Duration::from_millis(100),
            "enriching {} processes took {elapsed:?}",
            table.len()
        );
    }
}

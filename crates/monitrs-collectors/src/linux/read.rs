//! The thin layer that turns paths into bytes.
//!
//! Everything else in [`crate::linux`] parses `&[u8]`, which is what makes it
//! testable on any platform (§17.2). This module is the one place that touches the
//! filesystem, and it is deliberately small, dull, and rule-bound:
//!
//! * **Rooted, not hard-coded.** [`ProcRoot`] holds the `/proc` and `/sys`
//!   directories instead of embedding the literals, so the reading layer itself is
//!   exercised by the fixture tree in `fixtures/linux/tree` on this macOS host.
//!   Only `ProcRoot::live` is `cfg`-gated to Linux, which is also why it is not
//!   linkable in a documentation build for another platform.
//! * **Every read is capped.** §9.2 requires capping expensive per-process reads and
//!   §16.1 sets a budget for the whole pass. A file larger than its cap is a typed
//!   [`ReadFailure::Oversized`], never a silently truncated buffer that would parse
//!   into plausible nonsense.
//! * **No recursive walks.** §9.2 forbids scanning unbounded `/proc` subtrees.
//!   [`ProcRoot::list_pids`] reads one directory level and nothing else, and
//!   [`ProcRoot::count_open_files`] and [`ProcRoot::list_open_files`] read one level
//!   of `fd/` with a cap.
//! * **A failed read is a metric state, never an error return.** `EACCES` on
//!   `/proc/<pid>/io` is [`MetricState::PermissionDenied`]; `ENOENT` on
//!   `/proc/<pid>/stat` is [`UnavailableReason::ProcessExited`]. Neither is a
//!   `Result::Err` that could propagate into a failed sample (§9.2, §14.1).
//!
//! # How "no log line per vanished process" is enforced
//!
//! §9.2 forbids logging one error per vanished process, and §14.1 says a vanished
//! process is not an error worth a warning at all. Rather than relying on every call
//! site to remember that, this module makes it structural:
//!
//! * nothing in `crate::linux` calls a logging macro — the module does not import
//!   `tracing` at all, so there is no code path that can emit a line;
//! * the *only* way a failure reaches the user is
//!   [`ReadDiagnostics::record`], which returns `false` and increments a counter
//!   for every [`ReadFailure::is_expected`] failure instead of storing it;
//! * what is stored is bounded and de-duplicated with an occurrence count, so even
//!   an unexpected failure on ten thousand processes is one line.

use core::time::Duration;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use monitrs_core::model::{
    CollectorHealth, MetricState, OpenFileEntry, OpenFileKind, OpenFileList, UnavailableReason,
};

use crate::linux::process::describe_descriptor;
use crate::tier::DueTiers;

/// Cap for whole-system `/proc` files.
///
/// `/proc/diskstats` on a host with a thousand device-mapper volumes is the largest
/// realistic case at roughly 150 kB; half a mebibyte leaves generous headroom while
/// still bounding one tick's allocation.
pub const SYSTEM_FILE_CAP: usize = 512 * 1024;

/// Cap for per-process `/proc/<pid>/*` files.
///
/// `stat` is a few hundred bytes and `status` about 1.5 kB. 64 kB is far more than
/// any of them, and the cap matters because this one is multiplied by the process
/// count on every tick (§16.1).
pub const PROCESS_FILE_CAP: usize = 64 * 1024;

/// Cap for single-value `/sys` attributes.
pub const ATTRIBUTE_CAP: usize = 4 * 1024;

/// The largest number of processes one pass will read detail for.
///
/// §16.2 requires progressively reducing expensive enrichment under load rather than
/// blocking, and §9.2 requires capping per-process reads. 16 384 is above the
/// 10 000-process high-load case §16.2 names, so the cap is a backstop against a
/// fork bomb rather than a limit users meet.
pub const MAX_ENRICHED_PROCESSES: usize = 16_384;

/// The largest number of file descriptors counted for one process.
///
/// A single process can hold a million descriptors; counting them all would blow the
/// on-demand budget for one number (§8.6, §16.1). When the cap is reached the count
/// is a *floor* rather than a total, which is why
/// [`ProcRoot::count_open_files`] returns that fact alongside the number instead of
/// leaving the caller to guess.
pub const MAX_COUNTED_FDS: usize = 65_536;

/// The largest number of `/sys/class/power_supply` entries one pass will read.
///
/// Each entry costs sixteen small attribute reads, and the directory is not only
/// batteries: a docking station full of bluetooth peripherals adds one entry each.
/// Sixteen is far above the two batteries and one charger a real laptop has, so the
/// cap bounds the sensor group's work without reaching a machine anyone owns (§16.1).
pub const MAX_POWER_SUPPLIES: usize = 16;

/// Why a read produced no bytes.
///
/// Deliberately coarse: the distinctions that matter are *is this expected* and
/// *would privileges help*, and both are answerable from these four variants.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ReadFailure {
    /// The path does not exist.
    ///
    /// For a per-process file this is the normal outcome of a process exiting
    /// between enumeration and the read; for a system file it means the kernel
    /// lacks the feature. The two mappings differ, which is why
    /// [`ReadFailure::process_state`] and [`ReadFailure::system_state`] are separate.
    Missing,
    /// The kernel refused the read: `EACCES` or `EPERM`.
    ///
    /// The `/proc/<pid>/io` case §9.2 names: another user's process, and root would
    /// succeed. Never zero.
    Denied,
    /// The file exceeded its cap, so no partial parse was attempted.
    Oversized,
    /// Any other I/O error, including `EIO` from failing hardware and `EINVAL` from
    /// a `/sys` attribute the driver does not implement.
    Failed,
}

impl ReadFailure {
    /// Classifies a standard-library I/O error.
    ///
    /// Falls back to the raw `errno` for `ESRCH`, which `std` does not map to a
    /// kind: reading `/proc/<pid>/*` for a task that exited mid-read can return
    /// `ESRCH` rather than `ENOENT`, and treating it as a generic failure would put
    /// a diagnostic line on screen for a routine event.
    #[must_use]
    pub fn classify(error: &std::io::Error) -> Self {
        use std::io::ErrorKind;
        match error.kind() {
            ErrorKind::NotFound => Self::Missing,
            ErrorKind::PermissionDenied => Self::Denied,
            _ => match error.raw_os_error() {
                // ESRCH: no such process.
                Some(3) => Self::Missing,
                // EPERM and EACCES, in case a platform maps them elsewhere.
                Some(1 | 13) => Self::Denied,
                _ => Self::Failed,
            },
        }
    }

    /// Whether this failure is a routine event rather than something to report.
    ///
    /// `Missing` and `Denied` are both expected on every real system, every tick:
    /// processes exit, and other users' processes are unreadable. §9.2 and §14.1
    /// forbid turning either into a log line or a recurring diagnostic.
    #[must_use]
    pub const fn is_expected(self) -> bool {
        matches!(self, Self::Missing | Self::Denied)
    }

    /// Whether elevated privileges would plausibly make this read succeed.
    #[must_use]
    pub const fn privileges_might_help(self) -> bool {
        matches!(self, Self::Denied)
    }

    /// The metric state for a **per-process** file.
    #[must_use]
    pub const fn process_state<T>(self) -> MetricState<T> {
        match self {
            Self::Missing => MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited),
            Self::Denied => MetricState::PermissionDenied,
            Self::Oversized | Self::Failed => {
                MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
            }
        }
    }

    /// The metric state for a **whole-system** file.
    ///
    /// A missing system file is [`MetricState::Unsupported`] rather than
    /// `ProcessExited`: `/proc/pressure/cpu` is absent because the kernel was built
    /// without PSI, which no amount of retrying or privilege will change (§4).
    #[must_use]
    pub const fn system_state<T>(self) -> MetricState<T> {
        match self {
            Self::Missing => MetricState::Unsupported,
            Self::Denied => MetricState::PermissionDenied,
            Self::Oversized | Self::Failed => {
                MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
            }
        }
    }

    /// A short explanation for the diagnostics panel.
    #[must_use]
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Missing => "not present",
            Self::Denied => "permission denied",
            Self::Oversized => "larger than its read cap",
            Self::Failed => "read failed",
        }
    }
}

/// Bytes read, or why they were not.
pub type SourceBytes = Result<Vec<u8>, ReadFailure>;

/// The `/proc` and `/sys` directories to read from.
///
/// Holding the roots rather than hard-coding the literals is what lets the tests in
/// this file run on a machine with no `/proc` at all.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcRoot {
    proc_dir: PathBuf,
    sys_dir: PathBuf,
}

impl ProcRoot {
    /// The live kernel interfaces.
    ///
    /// The only Linux-gated constructor: everything else in this module works
    /// against any directory pair, which is how the reading layer is tested off
    /// Linux.
    #[cfg(target_os = "linux")]
    #[must_use]
    pub fn live() -> Self {
        Self::new("/proc", "/sys")
    }

    /// A root pointing at arbitrary directories, for tests and for a sandbox that
    /// mounts `/proc` elsewhere.
    pub fn new(proc_dir: impl Into<PathBuf>, sys_dir: impl Into<PathBuf>) -> Self {
        Self {
            proc_dir: proc_dir.into(),
            sys_dir: sys_dir.into(),
        }
    }

    /// The configured `/proc` directory.
    #[must_use]
    pub fn proc_dir(&self) -> &Path {
        &self.proc_dir
    }

    /// The configured `/sys` directory.
    #[must_use]
    pub fn sys_dir(&self) -> &Path {
        &self.sys_dir
    }

    /// Reads a file under `/proc`.
    pub fn read_proc(&self, relative: &str, cap: usize) -> SourceBytes {
        read_capped(&self.proc_dir.join(relative), cap)
    }

    /// Reads a file under `/sys`.
    pub fn read_sys(&self, relative: &str, cap: usize) -> SourceBytes {
        read_capped(&self.sys_dir.join(relative), cap)
    }

    /// Reads one file of one process, e.g. `stat` or `io`.
    pub fn read_pid(&self, pid: u32, file: &str) -> SourceBytes {
        read_capped(
            &self.proc_dir.join(pid.to_string()).join(file),
            PROCESS_FILE_CAP,
        )
    }

    /// Lists the process ids visible in `/proc`.
    ///
    /// Reads exactly one directory level and never descends (§9.2). Entries that are
    /// not all-digits are skipped, which is how `self`, `thread-self`, `net`, and the
    /// rest of `/proc` are excluded without a whitelist. The result is sorted so a
    /// snapshot's process order does not depend on directory iteration order, which
    /// would make the process table jitter between ticks.
    ///
    /// Returns at most `cap` pids and reports whether the list was cut short, so the
    /// caller can surface that rather than silently monitoring part of the machine.
    pub fn list_pids(&self, cap: usize) -> Result<(Vec<u32>, bool), ReadFailure> {
        let entries =
            std::fs::read_dir(&self.proc_dir).map_err(|error| ReadFailure::classify(&error))?;
        let mut pids = Vec::new();
        let mut truncated = false;
        for entry in entries {
            // One unreadable directory entry must not abort the enumeration: on a
            // live `/proc` an entry can disappear between `read_dir` and `next`.
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.is_empty() || !name.bytes().all(|byte| byte.is_ascii_digit()) {
                continue;
            }
            let Ok(pid) = name.parse::<u32>() else {
                continue;
            };
            if pids.len() >= cap {
                truncated = true;
                break;
            }
            pids.push(pid);
        }
        pids.sort_unstable();
        Ok((pids, truncated))
    }

    /// Lists the interfaces in `/sys/class/net`.
    ///
    /// One directory level, like [`ProcRoot::list_pids`]. Interface names are stable
    /// enough to sort, and sorting keeps the Network screen from reordering itself.
    pub fn list_interfaces(&self) -> Result<Vec<Box<str>>, ReadFailure> {
        self.list_class("class/net", usize::MAX)
    }

    /// Lists the power supplies in `/sys/class/power_supply`.
    ///
    /// [`ReadFailure::Missing`] on a kernel built without `CONFIG_POWER_SUPPLY` and on
    /// most virtual machines, which is the honest answer for a host with no battery
    /// rather than an empty list that could be confused with a machine whose battery
    /// vanished. Capped at [`MAX_POWER_SUPPLIES`].
    pub fn list_power_supplies(&self) -> Result<Vec<Box<str>>, ReadFailure> {
        self.list_class("class/power_supply", MAX_POWER_SUPPLIES)
    }

    /// One directory level of a `/sys/class` subdirectory, sorted and capped.
    ///
    /// Sorting matters for the same reason it does in [`ProcRoot::list_pids`]: the
    /// order a screen renders in must not depend on directory iteration order. The cap
    /// applies *after* sorting, so which entries survive it is deterministic too.
    fn list_class(&self, relative: &str, cap: usize) -> Result<Vec<Box<str>>, ReadFailure> {
        let entries = std::fs::read_dir(self.sys_dir.join(relative))
            .map_err(|error| ReadFailure::classify(&error))?;
        let mut names: Vec<Box<str>> = entries
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let name = entry.file_name();
                let name = name.to_str()?;
                (!name.is_empty() && !name.starts_with('.')).then(|| Box::from(name))
            })
            .collect();
        names.sort_unstable();
        names.truncate(cap);
        Ok(names)
    }

    /// Counts one process's open file descriptors.
    ///
    /// On-demand only (§8.6): this is a directory read per call, and doing it for
    /// every process every tick is exactly the cost §2.4 refuses to pay. Stops at
    /// [`MAX_COUNTED_FDS`] and reports whether it stopped early.
    pub fn count_open_files(&self, pid: u32) -> Result<(u32, bool), ReadFailure> {
        let path = self.proc_dir.join(pid.to_string()).join("fd");
        let entries = std::fs::read_dir(&path).map_err(|error| ReadFailure::classify(&error))?;
        let mut count = 0usize;
        let mut capped = false;
        for entry in entries {
            if entry.is_err() {
                // A descriptor closed while we were listing. Not an error.
                continue;
            }
            if count >= MAX_COUNTED_FDS {
                capped = true;
                break;
            }
            count += 1;
        }
        Ok((u32::try_from(count).unwrap_or(u32::MAX), capped))
    }

    /// Lists one process's open descriptors with their paths (§7.2, §8.6).
    ///
    /// On-demand only, and for a stronger reason than
    /// [`ProcRoot::count_open_files`]: counting is one directory read, while naming
    /// is one `readlink` per descriptor. That is why only the first
    /// [`OpenFileList::MAX_LISTED`] descriptors are named and the rest are counted —
    /// [`OpenFileList`] carries how many were left out so the panel can say so
    /// instead of presenting a prefix as the whole table (§4).
    ///
    /// The descriptors are named in *numeric* order rather than in directory order.
    /// `/proc/<pid>/fd` comes back unsorted, so taking the first 256 entries the
    /// kernel happens to return would list an arbitrary subset and a different one
    /// on each read; sorting first means the cap keeps the low descriptors — the
    /// standard streams and the files opened earliest — and keeps the panel stable
    /// between reads.
    pub fn list_open_files(&self, pid: u32) -> Result<OpenFileList, ReadFailure> {
        let dir = self.proc_dir.join(pid.to_string()).join("fd");
        let entries = std::fs::read_dir(&dir).map_err(|error| ReadFailure::classify(&error))?;
        let mut descriptors: Vec<i32> = Vec::new();
        let mut total = 0usize;
        for entry in entries {
            // A descriptor closed while we were listing. Not an error (§14.1).
            let Ok(entry) = entry else { continue };
            let name = entry.file_name();
            let Some(descriptor) = name.to_str().and_then(|name| name.parse::<i32>().ok()) else {
                continue;
            };
            total = total.saturating_add(1);
            if descriptors.len() < MAX_COUNTED_FDS {
                descriptors.push(descriptor);
            }
        }
        descriptors.sort_unstable();
        descriptors.truncate(OpenFileList::MAX_LISTED);
        let named = descriptors
            .into_iter()
            .map(|descriptor| read_one_descriptor(&dir, descriptor))
            .collect();
        Ok(OpenFileList::listed(named, total))
    }
}

/// One descriptor of `dir`, read through `readlink`.
///
/// A failure here is per-descriptor and never aborts the listing: the common case is
/// a descriptor closed between the directory read and this call, which §14.1 treats
/// as routine.
fn read_one_descriptor(dir: &Path, descriptor: i32) -> OpenFileEntry {
    match std::fs::read_link(dir.join(descriptor.to_string())) {
        Ok(target) => {
            // `to_string_lossy` rather than a failure on invalid UTF-8: a path with an
            // undecodable byte in it is still worth showing, and the replacement
            // character says where it was.
            let (kind, path) = describe_descriptor(&target.to_string_lossy());
            OpenFileEntry {
                descriptor,
                kind,
                path,
            }
        }
        Err(error) => OpenFileEntry {
            descriptor,
            // The descriptor exists — it was in the directory — but nothing about it
            // could be read, so its kind is unknown rather than assumed to be a file,
            // and its path is the failure's own state (§4).
            kind: OpenFileKind::Unknown,
            path: ReadFailure::classify(&error).process_state(),
        },
    }
}

/// Reads a whole file, refusing anything past `cap`.
///
/// `/proc` files report a size of zero, so the cap cannot be checked with
/// `metadata()`; the read asks for one byte more than the cap and treats getting it
/// as [`ReadFailure::Oversized`]. Returning a typed failure rather than a truncated
/// buffer matters because a truncated `/proc/stat` parses perfectly well into wrong
/// numbers.
fn read_capped(path: &Path, cap: usize) -> SourceBytes {
    let file = File::open(path).map_err(|error| ReadFailure::classify(&error))?;
    let mut buffer = Vec::new();
    let limit = u64::try_from(cap).unwrap_or(u64::MAX).saturating_add(1);
    file.take(limit)
        .read_to_end(&mut buffer)
        .map_err(|error| ReadFailure::classify(&error))?;
    if buffer.len() > cap {
        return Err(ReadFailure::Oversized);
    }
    Ok(buffer)
}

/// A bounded, de-duplicated record of read failures worth telling the user about.
///
/// See the module documentation: this type is how §9.2's "avoid logging one error
/// per vanished process" is enforced rather than remembered.
#[derive(Clone, Debug, Default)]
pub struct ReadDiagnostics {
    entries: Vec<Recorded>,
    suppressed: u64,
}

/// One distinct failure and how often it happened.
#[derive(Clone, Debug)]
struct Recorded {
    source: Box<str>,
    failure: ReadFailure,
    occurrences: u32,
}

/// The maximum number of distinct failures retained.
///
/// Matches [`monitrs_core::model::MAX_RETAINED_ISSUES`] so that everything recorded
/// here can reach [`CollectorHealth`] without being dropped a second time.
pub const MAX_RECORDED_FAILURES: usize = monitrs_core::model::MAX_RETAINED_ISSUES;

impl ReadDiagnostics {
    /// Records a failure, unless it is one of the expected ones.
    ///
    /// Returns whether it was recorded. Callers do not need to check: the point is
    /// that an expected failure has nowhere to go.
    pub fn record(&mut self, source: &str, failure: ReadFailure) -> bool {
        if failure.is_expected() {
            self.suppressed = self.suppressed.saturating_add(1);
            return false;
        }
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|entry| &*entry.source == source && entry.failure == failure)
        {
            existing.occurrences = existing.occurrences.saturating_add(1);
            return true;
        }
        if self.entries.len() >= MAX_RECORDED_FAILURES {
            return false;
        }
        self.entries.push(Recorded {
            source: source.into(),
            failure,
            occurrences: 1,
        });
        true
    }

    /// How many expected failures were suppressed.
    ///
    /// Surfaced on the Inspect screen as a count rather than as lines: "1 842
    /// processes vanished during sampling" is useful, 1 842 log lines are not.
    #[must_use]
    pub const fn suppressed(&self) -> u64 {
        self.suppressed
    }

    /// How many distinct failures are recorded.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Copies the recorded failures into collector health (§7.5).
    pub fn apply_to(&self, health: &mut CollectorHealth, since_start: Duration) {
        for entry in &self.entries {
            for _ in 0..entry.occurrences {
                health.record_issue(&entry.source, entry.failure.describe(), since_start);
            }
        }
    }

    /// Clears the record, keeping the suppression count.
    ///
    /// Called at the start of each tick: the failures shown must describe the
    /// current sample, while the suppression count is cumulative because PID churn
    /// over a whole run is what makes it interesting.
    pub fn clear_entries(&mut self) {
        self.entries.clear();
    }
}

/// What one pass should read.
#[derive(Clone, Debug)]
pub struct SourceRequest {
    /// Which tiers are due (§8.6). Files outside the due tiers are not read, and
    /// their [`LinuxSources`] fields stay `None` so the caller keeps the previous
    /// value instead of re-reading (§9.1).
    pub tiers: DueTiers,
    /// The processes to enrich, normally from [`ProcRoot::list_pids`].
    pub pids: Vec<u32>,
    /// Whether to read each process's cgroup, which is slow-tier metadata (§8.6).
    pub include_process_cgroups: bool,
}

impl SourceRequest {
    /// A request for every tier and the given processes.
    #[must_use]
    pub fn all_tiers(pids: Vec<u32>) -> Self {
        Self {
            tiers: DueTiers::ALL,
            pids,
            include_process_cgroups: true,
        }
    }
}

/// One interface's `/sys/class/net` attributes.
#[derive(Clone, Debug)]
pub struct InterfaceSources {
    /// Interface name.
    pub name: Box<str>,
    /// `operstate`.
    pub operstate: SourceBytes,
    /// `speed`. Frequently `EINVAL` on wireless and virtual interfaces, which §7.4
    /// turns into "no utilisation percentage" rather than into an error.
    pub speed: SourceBytes,
}

/// One `/sys/class/power_supply/<name>` entry's attributes.
///
/// Every field is optional on some driver, which is why they are all
/// [`SourceBytes`] and why nothing here decides what the absence means: that is
/// [`crate::linux::power`]'s job, and it needs to distinguish "the file is not
/// there" from "the read was refused".
#[derive(Clone, Debug)]
pub struct PowerSupplySources {
    /// Directory name, e.g. `BAT0`.
    pub name: Box<str>,
    /// `type`: `Battery`, `Mains`, `UPS`.
    pub kind: SourceBytes,
    /// `scope`: `System` or `Device`. Absent on most drivers.
    pub scope: SourceBytes,
    /// `status`.
    pub status: SourceBytes,
    /// `capacity`, the charge percentage.
    pub capacity: SourceBytes,
    /// `cycle_count`.
    pub cycle_count: SourceBytes,
    /// `energy_full_design`, in µWh.
    pub energy_full_design: SourceBytes,
    /// `energy_full`, in µWh.
    pub energy_full: SourceBytes,
    /// `charge_full_design`, in µAh.
    pub charge_full_design: SourceBytes,
    /// `charge_full`, in µAh.
    pub charge_full: SourceBytes,
    /// `voltage_min_design`, in µV.
    pub voltage_min_design: SourceBytes,
    /// `power_now`, in µW.
    pub power_now: SourceBytes,
    /// `current_now`, in µA.
    pub current_now: SourceBytes,
    /// `voltage_now`, in µV.
    pub voltage_now: SourceBytes,
    /// `temp`, in tenths of a degree Celsius.
    pub temp: SourceBytes,
    /// `time_to_empty_now`, in seconds.
    pub time_to_empty: SourceBytes,
    /// `time_to_full_now`, in seconds.
    pub time_to_full: SourceBytes,
}

/// The cgroup v2 files that carry this container's limits.
#[derive(Clone, Debug, Default)]
pub struct CgroupSources {
    /// `cgroup.controllers`, whose mere existence proves the unified hierarchy.
    pub controllers: Option<SourceBytes>,
    /// `memory.max`.
    pub memory_max: Option<SourceBytes>,
    /// `memory.current`.
    pub memory_current: Option<SourceBytes>,
    /// `cpu.max`.
    pub cpu_max: Option<SourceBytes>,
}

/// The evidence the environment heuristic needs (§7.5).
#[derive(Clone, Debug)]
pub struct EnvironmentSources {
    /// This process's own cgroup membership.
    pub self_cgroup: SourceBytes,
    /// `/sys/class/dmi/id/sys_vendor`.
    pub dmi_sys_vendor: SourceBytes,
}

/// One process's files.
#[derive(Clone, Debug)]
pub struct ProcessSources {
    /// Which process these bytes came from.
    pub pid: u32,
    /// `stat`, which carries the identity.
    pub stat: SourceBytes,
    /// `status`.
    pub status: SourceBytes,
    /// `io`, often [`ReadFailure::Denied`] for another user's process.
    pub io: SourceBytes,
    /// `cmdline`.
    pub cmdline: SourceBytes,
    /// `cgroup`, read only on the slow tier.
    pub cgroup: Option<SourceBytes>,
}

/// Everything one pass read.
///
/// A `None` field means "not due this tick", which is different from
/// `Some(Err(_))` — "read and failed". Conflating them would make the caller either
/// re-read on every tick or blank a metric that is merely not scheduled (§9.1).
#[derive(Clone, Debug, Default)]
pub struct LinuxSources {
    /// `/proc/stat`.
    pub stat: Option<SourceBytes>,
    /// `/proc/meminfo`.
    pub meminfo: Option<SourceBytes>,
    /// `/proc/loadavg`.
    pub loadavg: Option<SourceBytes>,
    /// `/proc/uptime`.
    pub uptime: Option<SourceBytes>,
    /// `/proc/diskstats`.
    pub diskstats: Option<SourceBytes>,
    /// `/proc/net/dev`.
    pub net_dev: Option<SourceBytes>,
    /// `/proc/pressure/cpu`.
    pub pressure_cpu: Option<SourceBytes>,
    /// `/proc/pressure/memory`.
    pub pressure_memory: Option<SourceBytes>,
    /// `/proc/pressure/io`.
    pub pressure_io: Option<SourceBytes>,
    /// Per-interface `/sys` attributes, medium tier.
    pub interfaces: Option<Vec<InterfaceSources>>,
    /// Per-power-supply `/sys` attributes, read with the sensor group (§8.6).
    ///
    /// `Some(empty)` and `None` are different answers: the first is a kernel that
    /// exports the class and lists nothing under it, the second is a tick on which
    /// the sensor group was not due.
    pub power_supplies: Option<Vec<PowerSupplySources>>,
    /// cgroup limits.
    pub cgroup: CgroupSources,
    /// Container/VM evidence, slow tier.
    pub environment: Option<EnvironmentSources>,
    /// Per-process files.
    pub processes: Vec<ProcessSources>,
    /// Whether the process list was cut short by a cap (§16.2).
    pub processes_truncated: bool,
}

/// Reads everything [`SourceRequest`] asks for.
///
/// Never returns an error: every failure lands in the affected field as a
/// [`ReadFailure`] so the caller can map it to the right [`MetricState`]. A `/proc`
/// that cannot be read at all therefore produces a snapshot full of explicit
/// unavailability rather than no snapshot (§14.1).
#[must_use]
pub fn collect_sources(root: &ProcRoot, request: &SourceRequest) -> LinuxSources {
    use monitrs_core::model::Tier;

    let mut sources = LinuxSources::default();

    if request.tiers.contains(Tier::Fast) {
        sources.stat = Some(root.read_proc("stat", SYSTEM_FILE_CAP));
        sources.meminfo = Some(root.read_proc("meminfo", SYSTEM_FILE_CAP));
        sources.diskstats = Some(root.read_proc("diskstats", SYSTEM_FILE_CAP));
        sources.net_dev = Some(root.read_proc("net/dev", SYSTEM_FILE_CAP));
        sources.pressure_cpu = Some(root.read_proc("pressure/cpu", ATTRIBUTE_CAP));
        sources.pressure_memory = Some(root.read_proc("pressure/memory", ATTRIBUTE_CAP));
        sources.pressure_io = Some(root.read_proc("pressure/io", ATTRIBUTE_CAP));
        // A live counter, so it belongs with the fast tier even though the limit it
        // is compared against does not.
        sources.cgroup.memory_current =
            Some(root.read_sys("fs/cgroup/memory.current", ATTRIBUTE_CAP));

        let capped = request.pids.len().min(MAX_ENRICHED_PROCESSES);
        sources.processes_truncated = request.pids.len() > capped;
        sources.processes = request
            .pids
            .iter()
            .take(capped)
            .map(|&pid| ProcessSources {
                pid,
                stat: root.read_pid(pid, "stat"),
                status: root.read_pid(pid, "status"),
                io: root.read_pid(pid, "io"),
                cmdline: root.read_pid(pid, "cmdline"),
                cgroup: request
                    .include_process_cgroups
                    .then(|| root.read_pid(pid, "cgroup")),
            })
            .collect();
    }

    if request.tiers.contains(Tier::Medium) {
        // §8.6 puts load and static device state on the medium tier.
        sources.loadavg = Some(root.read_proc("loadavg", ATTRIBUTE_CAP));
        sources.uptime = Some(root.read_proc("uptime", ATTRIBUTE_CAP));
        sources.interfaces = Some(
            root.list_interfaces()
                .unwrap_or_default()
                .into_iter()
                .map(|name| InterfaceSources {
                    operstate: root.read_sys(&format!("class/net/{name}/operstate"), ATTRIBUTE_CAP),
                    speed: root.read_sys(&format!("class/net/{name}/speed"), ATTRIBUTE_CAP),
                    name,
                })
                .collect(),
        );
    }

    if request.tiers.sensors() {
        // §8.6 groups the battery with the temperatures, and that group has its own
        // cadence rather than the medium tier's. A pack's charge moves in whole
        // percentage points over minutes, so even five seconds was sixteen attribute
        // opens to watch a number that had not changed (§16.1).
        sources.power_supplies = Some(
            root.list_power_supplies()
                .unwrap_or_default()
                .into_iter()
                .map(|name| power_supply_sources(root, name))
                .collect(),
        );
    }

    if request.tiers.contains(Tier::Slow) {
        // §8.6 puts cgroup metadata and static system facts on the slow tier: a
        // container's limits do not change while it runs.
        sources.cgroup.controllers =
            Some(root.read_sys("fs/cgroup/cgroup.controllers", ATTRIBUTE_CAP));
        sources.cgroup.memory_max = Some(root.read_sys("fs/cgroup/memory.max", ATTRIBUTE_CAP));
        sources.cgroup.cpu_max = Some(root.read_sys("fs/cgroup/cpu.max", ATTRIBUTE_CAP));
        sources.environment = Some(EnvironmentSources {
            self_cgroup: root.read_proc("self/cgroup", ATTRIBUTE_CAP),
            dmi_sys_vendor: root.read_sys("class/dmi/id/sys_vendor", ATTRIBUTE_CAP),
        });
    }

    sources
}

/// Reads one power supply's attributes.
///
/// Every attribute is read unconditionally rather than probed first: a `stat` to
/// decide whether to `open` costs the same syscall it saves, and the absent files
/// return [`ReadFailure::Missing`], which is the information the parser wants.
fn power_supply_sources(root: &ProcRoot, name: Box<str>) -> PowerSupplySources {
    let attribute =
        |file: &str| root.read_sys(&format!("class/power_supply/{name}/{file}"), ATTRIBUTE_CAP);
    PowerSupplySources {
        kind: attribute("type"),
        scope: attribute("scope"),
        status: attribute("status"),
        capacity: attribute("capacity"),
        cycle_count: attribute("cycle_count"),
        energy_full_design: attribute("energy_full_design"),
        energy_full: attribute("energy_full"),
        charge_full_design: attribute("charge_full_design"),
        charge_full: attribute("charge_full"),
        voltage_min_design: attribute("voltage_min_design"),
        power_now: attribute("power_now"),
        current_now: attribute("current_now"),
        voltage_now: attribute("voltage_now"),
        temp: attribute("temp"),
        time_to_empty: attribute("time_to_empty_now"),
        time_to_full: attribute("time_to_full_now"),
        name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::process::parse_pid_stat;
    use monitrs_core::model::Tier;
    use std::io::Error;

    /// The fixture tree standing in for a Linux `/proc` and `/sys`.
    fn tree() -> ProcRoot {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/linux/tree");
        ProcRoot::new(base.join("proc"), base.join("sys"))
    }

    #[test]
    fn a_missing_process_file_is_reported_as_the_process_having_exited() {
        // §9.2: tolerate process disappearance at any point. PID 9999 is absent
        // from the fixture tree on purpose.
        let failure = tree()
            .read_pid(9_999, "stat")
            .expect_err("pid 9999 is absent");
        assert_eq!(failure, ReadFailure::Missing);
        let state: MetricState<u64> = failure.process_state();
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited)
        );
        assert!(state.fresh().is_none(), "a vanished process is not zero");
    }

    #[test]
    fn eacces_on_process_io_is_permission_denied_and_never_zero() {
        // The case §9.2 names explicitly. Constructed from the raw errno rather than
        // by chmod-ing a fixture, so the test is portable and cannot pass by
        // accident when run as root.
        let failure = ReadFailure::classify(&Error::from_raw_os_error(13));
        assert_eq!(failure, ReadFailure::Denied);
        let state: MetricState<u64> = failure.process_state();
        assert_eq!(state, MetricState::PermissionDenied);
        assert!(state.fresh().is_none());
        assert!(failure.privileges_might_help());
        assert_eq!(state.placeholder(), Some("permission denied"));
    }

    #[test]
    fn esrch_is_treated_as_a_vanished_process_not_as_an_unknown_failure() {
        // ESRCH is what a read of a task that exited mid-read can return, and `std`
        // has no ErrorKind for it. Classifying it as a generic failure would put a
        // diagnostic line on screen for a routine event.
        let failure = ReadFailure::classify(&Error::from_raw_os_error(3));
        assert_eq!(failure, ReadFailure::Missing);
        assert!(failure.is_expected());
    }

    #[test]
    fn a_missing_system_file_is_unsupported_rather_than_a_vanished_process() {
        // `/proc/pressure/*` is absent on a kernel built without PSI. Reporting
        // "process exited" there would be nonsense, and reporting 0% would be a
        // fabricated all-clear (§4).
        let state: MetricState<u64> = ReadFailure::Missing.system_state();
        assert_eq!(state, MetricState::Unsupported);
        assert_eq!(state.placeholder(), Some("n/a"));
    }

    #[test]
    fn unexpected_failures_map_to_read_failed_in_both_directions() {
        for failure in [ReadFailure::Failed, ReadFailure::Oversized] {
            let process: MetricState<u64> = failure.process_state();
            let system: MetricState<u64> = failure.system_state();
            assert_eq!(
                process,
                MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
            );
            assert_eq!(process, system);
            assert!(!failure.is_expected());
            assert!(!failure.privileges_might_help());
        }
    }

    #[test]
    fn a_vanished_process_produces_no_diagnostic_line_however_often_it_happens() {
        // §9.2: avoid logging one error per vanished process. Ten thousand exits in
        // one pass must leave nothing behind but a count.
        let mut diagnostics = ReadDiagnostics::default();
        for pid in 0..10_000u32 {
            assert!(!diagnostics.record(&format!("/proc/{pid}/stat"), ReadFailure::Missing));
            assert!(!diagnostics.record(&format!("/proc/{pid}/io"), ReadFailure::Denied));
        }
        assert!(diagnostics.is_empty());
        assert_eq!(diagnostics.suppressed(), 20_000);

        let mut health = CollectorHealth::default();
        diagnostics.apply_to(&mut health, Duration::from_secs(1));
        assert!(
            health.issues.is_empty(),
            "no expected failure may reach collector health"
        );
    }

    #[test]
    fn an_unexpected_failure_is_recorded_once_with_an_occurrence_count() {
        let mut diagnostics = ReadDiagnostics::default();
        for _ in 0..500 {
            assert!(diagnostics.record("/proc/diskstats", ReadFailure::Failed));
        }
        assert_eq!(diagnostics.len(), 1);

        let mut health = CollectorHealth::default();
        diagnostics.apply_to(&mut health, Duration::from_secs(2));
        assert_eq!(health.issues.len(), 1);
        assert_eq!(
            health.issues.first().map(|issue| issue.occurrences),
            Some(500)
        );
        assert_eq!(
            health.issues.first().map(|issue| &*issue.message),
            Some("read failed")
        );
    }

    #[test]
    fn the_diagnostic_record_is_bounded_and_clearable() {
        let mut diagnostics = ReadDiagnostics::default();
        for index in 0..(MAX_RECORDED_FAILURES * 4) {
            diagnostics.record(&format!("/proc/{index}"), ReadFailure::Failed);
        }
        assert_eq!(diagnostics.len(), MAX_RECORDED_FAILURES);
        diagnostics.clear_entries();
        assert!(diagnostics.is_empty());
        assert_eq!(
            diagnostics.suppressed(),
            0,
            "clearing entries keeps the cumulative suppression count meaningful"
        );
    }

    #[test]
    fn the_reading_layer_produces_bytes_the_parsers_accept() {
        let root = tree();
        let stat = root.read_proc("stat", SYSTEM_FILE_CAP).expect("present");
        assert!(crate::linux::stat::parse_proc_stat(&stat).is_ok());
        let meminfo = root.read_proc("meminfo", SYSTEM_FILE_CAP).expect("present");
        assert!(crate::linux::meminfo::parse_meminfo(&meminfo).is_ok());
        let dev = root.read_proc("net/dev", SYSTEM_FILE_CAP).expect("present");
        assert_eq!(
            crate::linux::netdev::parse_net_dev(&dev)
                .expect("valid")
                .len(),
            2
        );
    }

    #[test]
    fn a_file_larger_than_its_cap_is_refused_rather_than_truncated() {
        // A truncated `/proc/stat` parses perfectly well into wrong numbers, which
        // is why the cap is enforced as a typed failure.
        let root = tree();
        assert_eq!(
            root.read_proc("stat", 8),
            Err(ReadFailure::Oversized),
            "an 8-byte cap must refuse the file"
        );
        assert!(root.read_proc("stat", SYSTEM_FILE_CAP).is_ok());
    }

    #[test]
    fn listing_pids_reads_one_level_and_skips_non_numeric_entries() {
        // §9.2: no recursive scans. `net`, `pressure`, and `self` are directories in
        // the fixture tree that must not be mistaken for processes.
        let (pids, truncated) = tree().list_pids(MAX_ENRICHED_PROCESSES).expect("readable");
        assert_eq!(pids, vec![1, 2, 4_242, 9_182]);
        assert!(!truncated);
    }

    #[test]
    fn a_capped_pid_list_reports_that_it_was_cut_short() {
        // §16.2: shedding work is acceptable, hiding that it happened is not.
        let (pids, truncated) = tree().list_pids(2).expect("readable");
        assert_eq!(pids.len(), 2);
        assert!(truncated);
    }

    #[test]
    fn an_unreadable_proc_directory_is_a_typed_failure_not_a_panic() {
        let root = ProcRoot::new("/nonexistent-proc-for-tests", "/nonexistent-sys-for-tests");
        assert_eq!(root.list_pids(16).err(), Some(ReadFailure::Missing));
        assert_eq!(root.list_interfaces().err(), Some(ReadFailure::Missing));
        assert_eq!(root.count_open_files(1).err(), Some(ReadFailure::Missing));
        // A kernel with no `CONFIG_POWER_SUPPLY`, and every VM: a typed absence
        // rather than an empty list, so the caller can tell the two apart.
        assert_eq!(root.list_power_supplies().err(), Some(ReadFailure::Missing));
    }

    #[test]
    fn interfaces_are_listed_from_sys_class_net() {
        let names = tree().list_interfaces().expect("readable");
        assert_eq!(names, vec![Box::<str>::from("eth0"), Box::from("lo")]);
    }

    #[test]
    fn every_power_supply_is_listed_including_the_ones_that_are_not_batteries() {
        // The reading layer lists; deciding which entry is the system battery is
        // `power::classify`'s job and needs the attributes this read fetches.
        let names = tree().list_power_supplies().expect("readable");
        assert_eq!(
            names,
            vec![
                Box::<str>::from("AC"),
                Box::from("BAT0"),
                Box::from("hid-e4-battery")
            ]
        );
    }

    #[test]
    fn a_power_supply_read_reports_each_absent_attribute_separately() {
        // The fixture battery is an energy-reporting ACPI pack: it has no
        // `charge_full` and no `time_to_empty_now`, and those absences are what
        // make the corresponding metrics unsupported rather than zero (§4).
        let sources = collect_sources(&tree(), &SourceRequest::all_tiers(Vec::new()));
        let supplies = sources
            .power_supplies
            .as_ref()
            .expect("the sensor group was due");
        let battery = supplies
            .iter()
            .find(|supply| &*supply.name == "BAT0")
            .expect("the fixture battery");
        assert!(battery.capacity.is_ok());
        assert!(battery.energy_full.is_ok());
        assert_eq!(
            battery.charge_full.as_ref().err(),
            Some(&ReadFailure::Missing)
        );
        assert_eq!(
            battery.time_to_empty.as_ref().err(),
            Some(&ReadFailure::Missing)
        );
    }

    #[test]
    fn the_power_supply_read_follows_the_sensor_group_not_the_medium_tier() {
        // §8.6 groups the battery with the temperatures, and that group has its own
        // cadence: 30 s while nobody is looking at it. A medium tick must therefore
        // read the interface attributes and leave the sixteen battery attributes
        // alone, and `None` is "not read this tick" rather than "no battery" (§9.1).
        let root = tree();
        let medium = collect_sources(
            &root,
            &SourceRequest {
                tiers: DueTiers::fast_and_medium(),
                pids: Vec::new(),
                include_process_cgroups: false,
            },
        );
        assert!(
            medium.interfaces.is_some(),
            "the interface attributes stay on the medium tier"
        );
        assert!(
            medium.power_supplies.is_none(),
            "a medium tick must not read the battery any more"
        );

        // And the sensor group on its own reads the battery without dragging the
        // rest of the medium tier along with it.
        let sensors = collect_sources(
            &root,
            &SourceRequest {
                tiers: DueTiers::NONE.with_sensors(),
                pids: Vec::new(),
                include_process_cgroups: false,
            },
        );
        assert_eq!(sensors.power_supplies.as_ref().map(Vec::len), Some(3));
        assert!(sensors.interfaces.is_none());
        assert!(sensors.loadavg.is_none());
        assert!(sensors.stat.is_none());
    }

    #[test]
    fn collecting_every_tier_fills_every_source() {
        let root = tree();
        let (pids, _) = root.list_pids(MAX_ENRICHED_PROCESSES).expect("readable");
        let sources = collect_sources(&root, &SourceRequest::all_tiers(pids));

        assert!(sources.stat.as_ref().is_some_and(|bytes| bytes.is_ok()));
        assert!(sources.meminfo.as_ref().is_some_and(|bytes| bytes.is_ok()));
        assert!(sources.loadavg.as_ref().is_some_and(|bytes| bytes.is_ok()));
        assert!(sources.uptime.as_ref().is_some_and(|bytes| bytes.is_ok()));
        assert!(
            sources
                .diskstats
                .as_ref()
                .is_some_and(|bytes| bytes.is_ok())
        );
        assert!(sources.net_dev.as_ref().is_some_and(|bytes| bytes.is_ok()));
        assert!(sources.pressure_cpu.as_ref().is_some_and(|b| b.is_ok()));
        assert_eq!(sources.processes.len(), 4);
        assert!(!sources.processes_truncated);
        assert_eq!(sources.interfaces.as_ref().map(Vec::len), Some(2));
        assert_eq!(sources.power_supplies.as_ref().map(Vec::len), Some(3));
        assert!(sources.environment.is_some());
        assert!(
            sources
                .cgroup
                .memory_max
                .as_ref()
                .is_some_and(|b| b.is_ok())
        );
        assert!(sources.cgroup.cpu_max.as_ref().is_some_and(|b| b.is_ok()));
    }

    #[test]
    fn a_fast_only_pass_leaves_the_slower_tiers_untouched() {
        // §9.1: never an all-fields refresh. `None` means "keep what you have",
        // which is different from "read and failed".
        let root = tree();
        let request = SourceRequest {
            tiers: DueTiers::NONE,
            pids: vec![1],
            include_process_cgroups: false,
        };
        let nothing = collect_sources(&root, &request);
        assert!(nothing.stat.is_none());
        assert!(nothing.processes.is_empty());
        // Not read this tick, which the enrichment must not confuse with "the
        // machine stopped having a battery".
        assert!(nothing.power_supplies.is_none());

        let mut fast_only = SourceRequest::all_tiers(vec![1]);
        fast_only.tiers = DueTiers::default();
        assert!(
            !fast_only.tiers.contains(Tier::Fast),
            "the default is nothing due"
        );
    }

    #[test]
    fn a_kernel_thread_with_no_io_file_yields_a_vanished_style_failure_not_a_zero() {
        // PID 2 in the fixture tree has no `io`, exactly as a kernel thread's is
        // unreadable in practice.
        let sources = collect_sources(
            &tree(),
            &SourceRequest {
                tiers: DueTiers::ALL,
                pids: vec![2],
                include_process_cgroups: true,
            },
        );
        let process = sources.processes.first().expect("one process requested");
        assert_eq!(process.pid, 2);
        assert!(process.stat.is_ok());
        let io = process.io.as_ref().expect_err("no io file in the fixture");
        assert!(io.is_expected(), "an unreadable io file is a routine event");
        let state: MetricState<u64> = io.process_state();
        assert!(state.fresh().is_none());
    }

    #[test]
    fn the_weird_process_name_survives_the_whole_read_and_parse_path() {
        // End to end: bytes off the filesystem into the parser that §9.2 singles
        // out, with no path-taking parser in between.
        let bytes = tree().read_pid(9_182, "stat").expect("present");
        let stat = parse_pid_stat(&bytes).expect("valid");
        assert_eq!(&*stat.name, "((weird) name) with spaces");
        assert_eq!(stat.identity().pid, 9_182);
    }

    #[test]
    fn counting_open_files_is_a_typed_failure_when_the_directory_is_absent() {
        // The fixture tree has no `fd` directories: they are symlink farms that a
        // checked-in fixture cannot represent portably.
        assert_eq!(tree().count_open_files(1).err(), Some(ReadFailure::Missing));
        assert_eq!(
            tree().list_open_files(1).err(),
            Some(ReadFailure::Missing),
            "an absent fd directory is not an empty descriptor table"
        );
    }

    /// A scratch `/proc/<pid>/fd` symlink farm, built at run time.
    ///
    /// The checked-in fixture tree cannot hold one: the entries are symlinks whose
    /// targets are frequently not paths at all — a git checkout has no way to store a
    /// link to `socket:[456]` — and the kernel is the only thing that normally
    /// creates them. Building the farm here is what lets the reader itself be tested
    /// on the macOS host as well as on Linux (§17.2), because `readlink` returns a
    /// dangling link's target text on both.
    struct FdFarm {
        base: PathBuf,
    }

    impl FdFarm {
        fn new(label: &str, pid: u32, targets: &[(i32, String)]) -> Self {
            let base = std::env::temp_dir()
                .join(format!("monitrs-fd-{label}-{}-{pid}", std::process::id()));
            // A previous run that was killed before its `Drop` ran must not make this
            // one fail.
            let _ = std::fs::remove_dir_all(&base);
            let fd_dir = base.join("proc").join(pid.to_string()).join("fd");
            std::fs::create_dir_all(&fd_dir).expect("a writable temporary directory");
            for (descriptor, target) in targets {
                std::os::unix::fs::symlink(target, fd_dir.join(descriptor.to_string()))
                    .expect("a symlink in a directory we just created");
            }
            Self { base }
        }

        fn root(&self) -> ProcRoot {
            ProcRoot::new(self.base.join("proc"), self.base.join("sys"))
        }
    }

    impl Drop for FdFarm {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn a_descriptor_listing_names_files_and_labels_the_things_that_have_no_name() {
        // §7.2's list, and §4's rule about what a socket's path is: not zero, not an
        // empty string, and not `permission denied` either.
        let targets = vec![
            (0, "/dev/null".to_owned()),
            (1, "pipe:[9982]".to_owned()),
            (2, "socket:[41231]".to_owned()),
            (7, "/var/log/build.log".to_owned()),
        ];
        let farm = FdFarm::new("mixed", 4_242, &targets);
        let files = farm.root().list_open_files(4_242).expect("readable");

        assert_eq!(files.count(), 4);
        assert!(files.is_complete());
        assert_eq!(files.not_listed(), 0);

        let listed: Vec<(i32, OpenFileKind, Option<String>)> = files
            .entries()
            .iter()
            .map(|entry| {
                (
                    entry.descriptor,
                    entry.kind,
                    entry.path.fresh().map(ToString::to_string),
                )
            })
            .collect();
        assert_eq!(
            listed,
            vec![
                (0, OpenFileKind::File, Some("/dev/null".to_owned())),
                (1, OpenFileKind::Pipe, None),
                (2, OpenFileKind::Socket, None),
                (7, OpenFileKind::File, Some("/var/log/build.log".to_owned())),
            ]
        );
        for entry in files.entries() {
            if entry.path.fresh().is_none() {
                assert_eq!(entry.path, MetricState::Unsupported);
            }
        }
    }

    #[test]
    fn a_descriptor_table_larger_than_the_cap_reports_how_many_it_did_not_name() {
        // §16.1 on the on-demand tier: the cap bounds the syscalls, and the count of
        // what it left out is what keeps the panel honest about it.
        let overshoot = 5;
        let targets: Vec<(i32, String)> = (0..OpenFileList::MAX_LISTED + overshoot)
            .map(|index| {
                let descriptor = i32::try_from(index).expect("small");
                (descriptor, format!("/tmp/file-{descriptor}"))
            })
            .collect();
        let farm = FdFarm::new("capped", 4_243, &targets);
        let files = farm.root().list_open_files(4_243).expect("readable");

        assert_eq!(files.count(), OpenFileList::MAX_LISTED);
        assert_eq!(files.not_listed(), u32::try_from(overshoot).expect("small"));
        assert!(!files.is_complete());
        assert_eq!(
            files.total(),
            u64::try_from(OpenFileList::MAX_LISTED + overshoot).expect("small")
        );
        // The cap keeps the *lowest* descriptors, not whatever order the directory
        // came back in, so two reads of the same process list the same descriptors.
        let numbers: Vec<i32> = files.entries().iter().map(|e| e.descriptor).collect();
        let mut sorted = numbers.clone();
        sorted.sort_unstable();
        assert_eq!(numbers, sorted, "the listing must be in descriptor order");
        assert_eq!(numbers.first().copied(), Some(0));
        assert_eq!(
            numbers.last().copied(),
            Some(i32::try_from(OpenFileList::MAX_LISTED - 1).expect("small"))
        );
    }

    #[test]
    fn a_process_holding_nothing_is_an_empty_listing_and_not_a_failure() {
        // A real state: a zombie's descriptor table is empty, and that is a
        // measurement rather than a refusal.
        let farm = FdFarm::new("empty", 4_244, &[]);
        let files = farm.root().list_open_files(4_244).expect("readable");
        assert_eq!(files.count(), 0);
        assert!(files.is_complete());
        assert_eq!(files.total(), 0);
    }
}

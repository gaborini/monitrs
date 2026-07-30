//! Per-process metrics.
//!
//! Two structures exist on purpose. [`ProcessSnapshot`] is what the fast tier
//! collects for *every* process every tick, so it holds only cheap fields.
//! [`ProcessDetail`] is collected on demand for the *selected* process only,
//! because §2.4 forbids paying for expensive per-process reads across the whole
//! table on every tick.

use core::time::Duration;
use std::time::SystemTime;

use crate::model::{MetricState, ProcessIdentity, UserIdentity};
use crate::units::{Percent, Rate};

/// The kernel scheduling state of a process.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ProcessState {
    /// Running or runnable.
    Running,
    /// Interruptible sleep, the normal idle state.
    Sleeping,
    /// Uninterruptible sleep, usually blocked in the kernel on I/O.
    ///
    /// §7.2 requires this to be visibly distinct: an accumulation of `D`-state
    /// processes is a storage or NFS problem, not idleness.
    UninterruptibleSleep,
    /// Exited but not reaped by its parent.
    ///
    /// §7.2 requires this to be visibly distinct, and §11.2 has a rule for it.
    Zombie,
    /// Stopped by a job-control signal.
    Stopped,
    /// Stopped by a debugger.
    Traced,
    /// Idle kernel thread. Linux `I`, and the state macOS reports for a
    /// suspended process.
    Idle,
    /// Being torn down.
    Dead,
    /// The platform reported something we do not model.
    #[default]
    Unknown,
}

impl ProcessState {
    /// The single-character code shown in the `STATE` column.
    ///
    /// Deliberately the familiar `ps` letters, so existing knowledge transfers.
    #[must_use]
    pub const fn code(self) -> char {
        match self {
            Self::Running => 'R',
            Self::Sleeping => 'S',
            Self::UninterruptibleSleep => 'D',
            Self::Zombie => 'Z',
            Self::Stopped => 'T',
            Self::Traced => 't',
            Self::Idle => 'I',
            Self::Dead => 'X',
            Self::Unknown => '?',
        }
    }

    /// A spelled-out label for the detail overlay and help.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Sleeping => "sleeping",
            Self::UninterruptibleSleep => "uninterruptible sleep",
            Self::Zombie => "zombie",
            Self::Stopped => "stopped",
            Self::Traced => "traced",
            Self::Idle => "idle",
            Self::Dead => "dead",
            Self::Unknown => "unknown",
        }
    }

    /// Whether §7.2 requires this state to be rendered distinctly.
    #[must_use]
    pub const fn is_notable(self) -> bool {
        matches!(self, Self::Zombie | Self::UninterruptibleSleep)
    }

    /// Whether signalling this process can have any effect.
    ///
    /// A zombie has already exited; signalling it is a no-op and the
    /// confirmation dialog says so rather than pretending to act (§15.1).
    #[must_use]
    pub const fn is_signalable(self) -> bool {
        !matches!(self, Self::Zombie | Self::Dead)
    }
}

/// Per-process memory figures.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProcessMemory {
    /// Resident set size: physical memory currently mapped in.
    pub rss_bytes: MetricState<u64>,
    /// Virtual size: address space reserved, most of it never resident.
    ///
    /// Lowest column priority in §7.2 precisely because it is the most
    /// frequently misread number in a process table.
    pub virtual_bytes: MetricState<u64>,
    /// RSS as a share of total physical memory.
    pub share_of_total: MetricState<Percent>,
}

impl ProcessMemory {
    /// A value with nothing measured.
    pub const WARMING_UP: Self = Self {
        rss_bytes: MetricState::WarmingUp,
        virtual_bytes: MetricState::WarmingUp,
        share_of_total: MetricState::WarmingUp,
    };
}

/// Per-process disk I/O.
///
/// Requires `/proc/<pid>/io` on Linux (often permission-restricted for other
/// users' processes) and privileged access on macOS, so it is frequently
/// [`MetricState::PermissionDenied`] rather than zero.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProcessIo {
    /// Read throughput.
    pub read: MetricState<Rate>,
    /// Write throughput.
    pub write: MetricState<Rate>,
    /// Cumulative bytes read since the process started.
    pub read_total_bytes: MetricState<u64>,
    /// Cumulative bytes written since the process started.
    pub write_total_bytes: MetricState<u64>,
}

impl ProcessIo {
    /// A value for a platform that cannot report per-process I/O at all.
    pub const UNSUPPORTED: Self = Self {
        read: MetricState::Unsupported,
        write: MetricState::Unsupported,
        read_total_bytes: MetricState::Unsupported,
        write_total_bytes: MetricState::Unsupported,
    };

    /// A value whose counters exist but whose rates need a second sample.
    pub const WARMING_UP: Self = Self {
        read: MetricState::WarmingUp,
        write: MetricState::WarmingUp,
        read_total_bytes: MetricState::WarmingUp,
        write_total_bytes: MetricState::WarmingUp,
    };
}

/// The cheap per-process fields collected on every fast tick.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProcessSnapshot {
    /// Stable identity, safe to pin and to attach to a pending signal.
    pub identity: ProcessIdentity,
    /// Parent PID.
    ///
    /// A bare PID rather than a [`ProcessIdentity`] because the parent's start
    /// key is not available from the same read; tree construction resolves it
    /// against the rest of the snapshot.
    pub parent_pid: Option<u32>,
    /// Short process name, as the kernel reports it.
    pub name: Box<str>,
    /// Full command line, arguments joined by single spaces.
    ///
    /// Stored pre-joined to avoid a `Vec<String>` per process per tick. May be
    /// empty for kernel threads or another user's process, and §14.2 requires it
    /// to be redacted from logs because arguments can contain secrets.
    pub command: Box<str>,
    /// Executable path, where readable.
    pub exe: Option<Box<str>>,
    /// Owning user.
    pub user: MetricState<UserIdentity>,
    /// Scheduling state.
    pub state: ProcessState,
    /// CPU usage, core-normalized. May exceed 100% (§8.3).
    pub cpu: MetricState<Percent>,
    /// Memory figures.
    pub memory: ProcessMemory,
    /// Disk I/O.
    pub io: ProcessIo,
    /// Thread count, where reported.
    pub threads: MetricState<u32>,
    /// Time since the process started.
    pub age: MetricState<Duration>,
    /// Wall-clock start time, for the confirmation dialog (§6.2).
    pub started_at: MetricState<SystemTime>,
    /// Whether this is a kernel thread, which §7.2 allows hiding on Linux.
    pub is_kernel_thread: bool,
}

impl ProcessSnapshot {
    /// The command line if non-empty, otherwise the process name.
    ///
    /// Kernel threads and processes belonging to other users report no command
    /// line; showing an empty cell would look like a collection bug.
    #[must_use]
    pub fn command_or_name(&self) -> &str {
        if self.command.is_empty() {
            &self.name
        } else {
            &self.command
        }
    }

    /// The command line with arguments removed, keeping only `argv[0]`.
    ///
    /// §15.2 requires JSON export to be able to redact arguments by default,
    /// and §14.2 requires the same for logs.
    #[must_use]
    pub fn redacted_command(&self) -> &str {
        let command = self.command_or_name();
        match command.split_once(' ') {
            Some((program, _)) => program,
            None => command,
        }
    }
}

/// What kind of object a file descriptor refers to.
///
/// The set is the intersection of what both platforms can name from a single read:
/// macOS' `proc_fdtype` values and the prefix of a `/proc/<pid>/fd/<n>` link
/// target. It exists because §4's "unavailable is never an empty string" needs an
/// answer for a descriptor that has no path at all — a socket is not a nameless
/// file, it is a socket, and the kind is what says so.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum OpenFileKind {
    /// A file, directory, or device: something with a path in the filesystem.
    File,
    /// A socket. Has no filesystem path unless it is a bound Unix socket, and
    /// neither platform reports that path through the descriptor table.
    Socket,
    /// A pipe or FIFO.
    Pipe,
    /// A kernel event queue: `kqueue` on macOS, `epoll`/`eventfd`/`timerfd` on
    /// Linux.
    EventQueue,
    /// POSIX shared memory.
    SharedMemory,
    /// A POSIX semaphore.
    Semaphore,
    /// The platform reported something we do not model.
    #[default]
    Unknown,
}

impl OpenFileKind {
    /// A lower-case label, in the same spirit as [`ProcessState::label`].
    ///
    /// Every label is strict 7-bit ASCII so it is legal in both glyph modes (§5.1).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Socket => "socket",
            Self::Pipe => "pipe",
            Self::EventQueue => "event queue",
            Self::SharedMemory => "shared memory",
            Self::Semaphore => "semaphore",
            Self::Unknown => "unknown",
        }
    }

    /// Whether a descriptor of this kind can have a filesystem path at all.
    ///
    /// The one thing that distinguishes "the OS refused the path" from "there is no
    /// path to refuse": a collector that cannot resolve a [`Self::File`]'s path has
    /// hit a permission or read failure, while a [`Self::Socket`]'s path is
    /// [`MetricState::Unsupported`] on every platform and always will be.
    #[must_use]
    pub const fn has_path(self) -> bool {
        matches!(self, Self::File)
    }
}

/// One open file descriptor of a process.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OpenFileEntry {
    /// The descriptor number, as the process itself would use it.
    pub descriptor: i32,
    /// What the descriptor refers to.
    pub kind: OpenFileKind,
    /// Where it points in the filesystem.
    ///
    /// Never an empty string (§4). [`MetricState::Unsupported`] when
    /// [`OpenFileKind::has_path`] is false, or when the kernel resolved the
    /// descriptor but has no name for the object — an unlinked file has no path to
    /// report and never will, so it is not a *temporary* failure either.
    /// [`MetricState::PermissionDenied`] when the OS refused the per-descriptor
    /// read, which is the common case for another user's process.
    ///
    /// Serialized as its state only, never as its text: see the private
    /// `redact_descriptor_path` beside this type, and §15.2.
    #[cfg_attr(feature = "serde", serde(serialize_with = "redact_descriptor_path"))]
    pub path: MetricState<Box<str>>,
}

/// Serializes a descriptor path as its availability state, discarding the path.
///
/// §15.2 and §19 forbid a file path from leaving the process, and the JSON export
/// already strips command arguments for the same reason: an export is something
/// people paste into public issue trackers, and `/Users/someone/Documents/…` is as
/// identifying as an argument list. Doing it in the `Serialize` implementation
/// rather than in the exporter is what makes it unconditional — there is no
/// `--include-paths` to add later by accident, and a future export that starts
/// including [`ProcessDetail`] cannot leak paths by forgetting to.
///
/// The *state* survives, so a reader can still tell `permission denied` from
/// `unsupported` — that is §4's information, not the user's.
#[cfg(feature = "serde")]
fn redact_descriptor_path<S: serde::Serializer>(
    path: &MetricState<Box<str>>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize as _;
    path.as_ref().map(|_| "redacted").serialize(serializer)
}

/// The descriptors of one process, as far as the listing cap allowed.
///
/// The fields are private because the two of them have an invariant: `not_listed`
/// is exactly the number of descriptors the cap left out, and a caller that could
/// set them independently could claim a complete listing of a process it truncated.
/// [`OpenFileList::listed`] is the only constructor, and it enforces both the cap
/// and the arithmetic.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct OpenFileList {
    entries: Vec<OpenFileEntry>,
    not_listed: u32,
}

impl OpenFileList {
    /// The largest number of descriptors any collector will list paths for.
    ///
    /// Resolving a path costs one syscall per descriptor on both platforms — a
    /// `proc_pidfdinfo` on macOS, a `readlink` on Linux — and a process can hold
    /// tens of thousands, so §16.1's "nothing unbounded" applies even on the
    /// on-demand tier of §8.6. 256 was measured at 0.9 µs per descriptor on an M4
    /// Pro (216 µs for the 244 vnodes of a 442-descriptor process), which bounds
    /// the whole walk at well under a millisecond, and it is far more rows than the
    /// overlay can show without scrolling for a while.
    pub const MAX_LISTED: usize = 256;

    /// A listing of `entries` taken from a descriptor table of `total` entries.
    ///
    /// `entries` is truncated to [`OpenFileList::MAX_LISTED`] and everything the cap
    /// left out is counted, so the panel can say how many descriptors it did not
    /// list rather than presenting a partial list as a complete one (§4).
    #[must_use]
    pub fn listed(mut entries: Vec<OpenFileEntry>, total: usize) -> Self {
        entries.truncate(Self::MAX_LISTED);
        let not_listed = total.saturating_sub(entries.len());
        Self {
            entries,
            not_listed: u32::try_from(not_listed).unwrap_or(u32::MAX),
        }
    }

    /// The descriptors that were listed.
    #[must_use]
    pub fn entries(&self) -> &[OpenFileEntry] {
        &self.entries
    }

    /// How many descriptors were listed.
    #[must_use]
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    /// How many descriptors the cap left out.
    #[must_use]
    pub const fn not_listed(&self) -> u32 {
        self.not_listed
    }

    /// How many descriptors the process held when it was walked.
    #[must_use]
    pub fn total(&self) -> u64 {
        u64::try_from(self.entries.len()).unwrap_or(u64::MAX) + u64::from(self.not_listed)
    }

    /// Whether every descriptor was listed.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.not_listed == 0
    }
}

/// One entry in a process's ancestry chain.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct AncestorEntry {
    /// The ancestor's identity.
    pub identity: ProcessIdentity,
    /// The ancestor's short name, for the breadcrumb (§2.4).
    pub name: Box<str>,
}

/// The expensive per-process fields, collected on demand for the selected
/// process only (§8.6).
///
/// Environment variables are deliberately absent from this type, not merely
/// hidden by default: §7.5 forbids showing their values, and §15.2 forbids
/// logging them, so the safest design is never to read them at all.
///
/// The paths in [`ProcessDetail::open_file_list`] are user data of the same kind,
/// and they *are* read because §7.2 asks for them on screen. What §15.2 and §19
/// forbid is letting them leave the process, so they are redacted in the
/// `Serialize` implementation itself rather than by whatever serializes them — see
/// [`OpenFileEntry::path`].
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct ProcessDetail {
    /// Which process this describes.
    ///
    /// Checked against the current selection before rendering, so a late reply
    /// for a process the user has moved off is discarded rather than shown
    /// against the wrong row.
    pub identity: ProcessIdentity,
    /// Current working directory.
    pub working_directory: MetricState<Box<str>>,
    /// Filesystem root, which differs from `/` inside a container.
    pub root: MetricState<Box<str>>,
    /// Open file descriptor count.
    pub open_files: MetricState<u32>,
    /// Open socket count.
    pub sockets: MetricState<u32>,
    /// The descriptors themselves, bounded by [`OpenFileList::MAX_LISTED`].
    ///
    /// Separate from [`ProcessDetail::open_files`] because the two cost different
    /// things: the count is one cheap read on both platforms, while the list is one
    /// syscall per descriptor. A platform that can count but not list therefore
    /// reports a count and an [`MetricState::Unsupported`] list rather than
    /// withholding both.
    pub open_file_list: MetricState<OpenFileList>,
    /// Ancestry from the immediate parent up towards PID 1.
    pub ancestry: MetricState<Vec<AncestorEntry>>,
    /// Direct children.
    pub children: MetricState<Vec<ProcessIdentity>>,
    /// Total descendants, including indirect ones (§2.4).
    pub descendants: MetricState<u32>,
    /// Scheduling niceness.
    pub nice: MetricState<i32>,
    /// cgroup path. Linux only.
    pub cgroup: MetricState<Box<str>>,
    /// Container identity, where derivable from the cgroup path.
    pub container: MetricState<Box<str>>,
    /// When this detail was collected, so the UI can age it.
    pub collected_at: SystemTime,
}

impl ProcessDetail {
    /// An empty detail record for `identity`, with every field unmeasured.
    #[must_use]
    pub fn pending(identity: ProcessIdentity, collected_at: SystemTime) -> Self {
        Self {
            identity,
            working_directory: MetricState::WarmingUp,
            root: MetricState::WarmingUp,
            open_files: MetricState::WarmingUp,
            sockets: MetricState::WarmingUp,
            open_file_list: MetricState::WarmingUp,
            ancestry: MetricState::WarmingUp,
            children: MetricState::WarmingUp,
            descendants: MetricState::WarmingUp,
            nice: MetricState::WarmingUp,
            cgroup: MetricState::WarmingUp,
            container: MetricState::WarmingUp,
            collected_at,
        }
    }
}

/// The outcome of a detail lookup, which may fail because the process exited.
#[derive(Clone, Debug, PartialEq)]
pub enum ProcessDetailResult {
    /// The lookup succeeded, at least partially.
    Loaded(Box<ProcessDetail>),
    /// The process no longer exists.
    ///
    /// Expected and not an error (§14.1).
    Vanished(ProcessIdentity),
    /// The PID now belongs to a different process.
    Reused {
        /// What was requested.
        requested: ProcessIdentity,
        /// What the PID refers to now.
        found: ProcessIdentity,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(command: &str, name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(31_842, 900_100),
            parent_pid: Some(1),
            name: name.into(),
            command: command.into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::WarmingUp,
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    #[test]
    fn notable_states_are_exactly_zombie_and_uninterruptible_sleep() {
        assert!(ProcessState::Zombie.is_notable());
        assert!(ProcessState::UninterruptibleSleep.is_notable());
        for state in [
            ProcessState::Running,
            ProcessState::Sleeping,
            ProcessState::Stopped,
            ProcessState::Traced,
            ProcessState::Idle,
            ProcessState::Dead,
            ProcessState::Unknown,
        ] {
            assert!(!state.is_notable(), "{state:?}");
        }
    }

    #[test]
    fn state_codes_are_unique_so_the_column_is_unambiguous() {
        let states = [
            ProcessState::Running,
            ProcessState::Sleeping,
            ProcessState::UninterruptibleSleep,
            ProcessState::Zombie,
            ProcessState::Stopped,
            ProcessState::Traced,
            ProcessState::Idle,
            ProcessState::Dead,
            ProcessState::Unknown,
        ];
        let mut codes: Vec<char> = states.iter().map(|s| s.code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), states.len());
    }

    #[test]
    fn already_exited_processes_are_not_signalable() {
        assert!(!ProcessState::Zombie.is_signalable());
        assert!(!ProcessState::Dead.is_signalable());
        assert!(ProcessState::Running.is_signalable());
        assert!(ProcessState::UninterruptibleSleep.is_signalable());
    }

    #[test]
    fn an_empty_command_falls_back_to_the_process_name() {
        let kernel_thread = sample("", "kworker/2:1");
        assert_eq!(kernel_thread.command_or_name(), "kworker/2:1");
        let normal = sample("cargo build --release", "cargo");
        assert_eq!(normal.command_or_name(), "cargo build --release");
    }

    #[test]
    fn redaction_strips_arguments_which_may_contain_secrets() {
        let process = sample("psql postgres://user:hunter2@db/prod", "psql");
        assert_eq!(process.redacted_command(), "psql");
        assert!(!process.redacted_command().contains("hunter2"));
    }

    #[test]
    fn redaction_of_a_bare_program_keeps_the_program() {
        assert_eq!(sample("rustc", "rustc").redacted_command(), "rustc");
        assert_eq!(sample("", "kworker/2:1").redacted_command(), "kworker/2:1");
    }

    fn descriptor(
        descriptor: i32,
        kind: OpenFileKind,
        path: MetricState<Box<str>>,
    ) -> OpenFileEntry {
        OpenFileEntry {
            descriptor,
            kind,
            path,
        }
    }

    #[test]
    fn only_a_file_descriptor_can_have_a_path_to_refuse() {
        // The distinction the collectors depend on: a socket's missing path is a fact
        // about sockets, so failing to resolve one is not a read failure to report.
        assert!(OpenFileKind::File.has_path());
        for kind in [
            OpenFileKind::Socket,
            OpenFileKind::Pipe,
            OpenFileKind::EventQueue,
            OpenFileKind::SharedMemory,
            OpenFileKind::Semaphore,
            OpenFileKind::Unknown,
        ] {
            assert!(!kind.has_path(), "{kind:?}");
        }
    }

    #[test]
    fn every_descriptor_kind_has_a_distinct_ascii_label() {
        // The label is the only thing on screen that tells a socket from a pipe when
        // neither has a path, so two kinds sharing one label would erase §4's answer.
        let kinds = [
            OpenFileKind::File,
            OpenFileKind::Socket,
            OpenFileKind::Pipe,
            OpenFileKind::EventQueue,
            OpenFileKind::SharedMemory,
            OpenFileKind::Semaphore,
            OpenFileKind::Unknown,
        ];
        let mut labels: Vec<&str> = kinds.iter().map(|kind| kind.label()).collect();
        for label in &labels {
            assert!(label.is_ascii(), "{label} is not strict ASCII");
        }
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), kinds.len());
    }

    #[test]
    fn the_listing_cap_is_enforced_by_the_constructor_rather_than_by_its_callers() {
        // If a collector could hand over more entries than the cap, the cap would be
        // a convention rather than a bound, and §16.1 asks for a bound.
        let entries: Vec<OpenFileEntry> = (0..OpenFileList::MAX_LISTED + 50)
            .map(|index| {
                descriptor(
                    i32::try_from(index).unwrap_or(i32::MAX),
                    OpenFileKind::File,
                    MetricState::Available("/tmp/x".into()),
                )
            })
            .collect();
        let list = OpenFileList::listed(entries, OpenFileList::MAX_LISTED + 50);
        assert_eq!(list.count(), OpenFileList::MAX_LISTED);
        assert_eq!(list.not_listed(), 50);
        assert!(!list.is_complete());
    }

    #[test]
    fn a_complete_listing_says_nothing_was_left_out() {
        let list = OpenFileList::listed(
            vec![descriptor(
                3,
                OpenFileKind::Socket,
                MetricState::Unsupported,
            )],
            1,
        );
        assert!(list.is_complete());
        assert_eq!(list.not_listed(), 0);
        assert_eq!(list.total(), 1);
    }

    #[test]
    fn a_total_below_the_listed_count_cannot_produce_a_negative_remainder() {
        // A descriptor table that shrank between the two reads must not underflow into
        // a claim that four billion descriptors were omitted.
        let list = OpenFileList::listed(
            vec![
                descriptor(3, OpenFileKind::File, MetricState::PermissionDenied),
                descriptor(4, OpenFileKind::Pipe, MetricState::Unsupported),
            ],
            1,
        );
        assert_eq!(list.not_listed(), 0);
        assert_eq!(list.total(), 2);
    }

    #[test]
    fn an_unreadable_descriptor_path_is_a_state_and_never_an_empty_string() {
        let list = OpenFileList::listed(
            vec![
                descriptor(3, OpenFileKind::File, MetricState::PermissionDenied),
                descriptor(7, OpenFileKind::Socket, MetricState::Unsupported),
            ],
            2,
        );
        for entry in list.entries() {
            assert!(entry.path.fresh().is_none_or(|path| !path.is_empty()));
            assert!(entry.path.placeholder().is_some() || entry.path.fresh().is_some());
        }
    }

    #[test]
    fn a_pending_detail_reports_nothing_as_measured() {
        let identity = ProcessIdentity::new(1, 2);
        let detail = ProcessDetail::pending(identity, SystemTime::UNIX_EPOCH);
        assert_eq!(detail.identity, identity);
        assert!(detail.working_directory.is_warming_up());
        assert!(detail.open_files.fresh().is_none());
        assert!(
            detail.open_file_list.is_warming_up(),
            "an unread descriptor list is not an empty one"
        );
    }
}

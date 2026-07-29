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

    #[test]
    fn a_pending_detail_reports_nothing_as_measured() {
        let identity = ProcessIdentity::new(1, 2);
        let detail = ProcessDetail::pending(identity, SystemTime::UNIX_EPOCH);
        assert_eq!(detail.identity, identity);
        assert!(detail.working_directory.is_warming_up());
        assert!(detail.open_files.fresh().is_none());
    }
}

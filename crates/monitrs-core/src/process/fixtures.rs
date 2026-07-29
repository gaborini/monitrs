//! Test-only builders for [`ProcessSnapshot`] rows.
//!
//! Every optional metric starts *unmeasured* rather than zero, which is both the
//! §26 invariant and the state a sorting test has to exercise most: the fixtures
//! make "no value" the default so a test that cares about a value has to say so.

use core::time::Duration;
use std::time::{Instant, SystemTime};

use crate::model::{
    MetricState, ProcessIdentity, ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState,
    SystemSnapshot, UserIdentity,
};
use crate::units::{Percent, Rate};

/// Starts a fixture for the process identified by `pid` and `start_key`.
pub(crate) fn process(pid: u32, start_key: u64) -> Fixture {
    Fixture {
        process: ProcessSnapshot {
            identity: ProcessIdentity::new(pid, start_key),
            parent_pid: None,
            name: format!("p{pid}").into(),
            command: String::new().into(),
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
        },
    }
}

/// A snapshot whose process table is `processes` and whose other metrics are
/// warming up, for exercising the snapshot-shaped entry points.
pub(crate) fn snapshot(processes: Vec<ProcessSnapshot>) -> SystemSnapshot {
    let mut snapshot = SystemSnapshot::warming_up(Instant::now(), SystemTime::UNIX_EPOCH, 8);
    snapshot.processes = processes;
    snapshot
}

/// A partially built [`ProcessSnapshot`].
pub(crate) struct Fixture {
    process: ProcessSnapshot,
}

impl Fixture {
    /// Sets the parent PID, which tree construction resolves against the table.
    pub(crate) fn parent(mut self, pid: u32) -> Self {
        self.process.parent_pid = Some(pid);
        self
    }

    /// Sets the short process name.
    pub(crate) fn name(mut self, name: &str) -> Self {
        self.process.name = name.into();
        self
    }

    /// Sets the full command line.
    pub(crate) fn command(mut self, command: &str) -> Self {
        self.process.command = command.into();
        self
    }

    /// Sets a measured CPU percentage.
    pub(crate) fn cpu(mut self, percent: f32) -> Self {
        self.process.cpu = MetricState::Available(Percent::new(percent).expect("valid percent"));
        self
    }

    /// Sets an arbitrary CPU metric state, for the unavailable-value cases.
    pub(crate) fn cpu_state(mut self, state: MetricState<Percent>) -> Self {
        self.process.cpu = state;
        self
    }

    /// Sets a measured resident set size.
    pub(crate) fn rss(mut self, bytes: u64) -> Self {
        self.process.memory.rss_bytes = MetricState::Available(bytes);
        self
    }

    /// Sets a measured virtual size.
    pub(crate) fn virtual_bytes(mut self, bytes: u64) -> Self {
        self.process.memory.virtual_bytes = MetricState::Available(bytes);
        self
    }

    /// Sets a measured read rate in bytes per second.
    pub(crate) fn read(mut self, per_second: f64) -> Self {
        self.process.io.read = MetricState::Available(Rate::new(per_second).expect("valid rate"));
        self
    }

    /// Sets a measured write rate in bytes per second.
    pub(crate) fn write(mut self, per_second: f64) -> Self {
        self.process.io.write = MetricState::Available(Rate::new(per_second).expect("valid rate"));
        self
    }

    /// Sets a measured thread count.
    pub(crate) fn threads(mut self, count: u32) -> Self {
        self.process.threads = MetricState::Available(count);
        self
    }

    /// Sets a measured age.
    pub(crate) fn age(mut self, seconds: u64) -> Self {
        self.process.age = MetricState::Available(Duration::from_secs(seconds));
        self
    }

    /// Sets a resolved owner.
    pub(crate) fn user(mut self, uid: u32, name: Option<&str>) -> Self {
        self.process.user = MetricState::Available(UserIdentity {
            uid,
            name: name.map(Into::into),
        });
        self
    }

    /// Sets an arbitrary owner metric state, for the unattributable cases.
    pub(crate) fn user_state(mut self, state: MetricState<UserIdentity>) -> Self {
        self.process.user = state;
        self
    }

    /// Sets the scheduling state.
    pub(crate) fn state(mut self, state: ProcessState) -> Self {
        self.process.state = state;
        self
    }

    /// Marks the process as a kernel thread (§7.2 hide toggle).
    pub(crate) fn kernel_thread(mut self) -> Self {
        self.process.is_kernel_thread = true;
        self
    }

    /// Finishes the fixture.
    pub(crate) fn build(self) -> ProcessSnapshot {
        self.process
    }
}

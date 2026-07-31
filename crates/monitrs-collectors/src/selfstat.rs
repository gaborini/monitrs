//! What monitrs itself costs: our own resident memory, descriptor count, and the
//! CPU time we burn.
//!
//! §26 ends with "a system monitor must measure and expose its own overhead", and
//! §16.1 sets two budgets that cannot be checked any other way:
//!
//! * resident memory below 50 MiB in the default configuration, with **no
//!   unbounded growth over a twelve-hour run**;
//! * **no unbounded file-descriptor growth**.
//!
//! Both are *trend* budgets. A single reading proves nothing — an allocator that
//! has not yet returned freed pages looks identical to a leak — so the only useful
//! form of this measurement is a series of them compared against each other. The
//! soak harness (`crates/monitrs/tests/soak.rs`, `docs/soak-testing.md`) samples
//! both figures periodically and compares the last quartile of a run against the
//! first.
//!
//! [`thread_cpu_time`] and [`process_cpu_time`] are readings of a different kind.
//! §16.1's idle self-CPU budget is not a trend but an instrument problem: the
//! budget is about CPU, and the only figure the measurement runs had was
//! wall-clock, which for a blocking read is a different number entirely. They
//! answer that, and `docs/benchmarks.md` records what they answered.
//!
//! # Why this lives here rather than in the collectors' sampling path
//!
//! Nothing in this module is called from [`crate::SnapshotSource::sample`]. It
//! describes *this* process, not the machine, so putting it in a
//! [`monitrs_core::SystemSnapshot`] would mix two different subjects. It lives in
//! this crate because this is the crate that is allowed to talk to the OS and that
//! already owns the platform `cfg` predicates and the unsafe policy of §15.3.
//!
//! # What each platform can answer
//!
//! | Platform | Resident bytes | Open descriptors | CPU time, thread and process |
//! |---|---|---|---|
//! | Linux | `VmRSS` from `/proc/self/status` | entries in `/proc/self/fd` | `clock_gettime` and `getrusage`, and only with `linux-native` |
//! | macOS, `macos-native` | `pti_resident_size` from `proc_pidinfo` | `PROC_PIDLISTFDS` byte count | `clock_gettime` and `getrusage` |
//! | anything else | `Unsupported` | `Unsupported` | `Unsupported` |
//!
//! Unsupported rather than zero, for the reason the whole codebase repeats: a
//! figure nobody measured is not a figure of nought (§4, §26).
//!
//! The last column is the one that needs `libc` on Linux as well as on macOS,
//! which is why it has its own predicate ([`CPU_TIME_COMPILED`]) rather than
//! sharing [`SELF_MEASUREMENT_COMPILED`]: the first two columns deliberately avoid
//! `libc` there, and there is no documented way to read either CPU clock without
//! it.
//!
//! # Three caveats worth knowing before trusting a number from here
//!
//! * **Resident size is a whole-process figure.** In a test binary it includes the
//!   test harness and every other test thread. Comparing two readings from the
//!   same process is meaningful; comparing an absolute reading against §16.1's
//!   50 MiB budget is only meaningful for the real binary.
//! * **On Linux the descriptor count includes the handle used to enumerate.**
//!   `read_dir` holds a descriptor open on `/proc/self/fd` while it lists it, and
//!   that descriptor appears in its own listing. The offset is a constant one, so
//!   it cannot manufacture or hide a trend, and it is not silently corrected here
//!   because a corrected count that was wrong by one in the other direction would
//!   be harder to explain than the raw one.
//! * **Thread CPU time belongs to the calling thread and to no other.** Two
//!   readings taken on the same thread subtract into the CPU that thread spent
//!   between them; two readings taken on different threads subtract into nothing,
//!   because each thread's clock starts near zero when it does. That narrowness is
//!   the feature and also the trap: work a call pushes onto a helper thread is real
//!   CPU that [`thread_cpu_time`] cannot see, which is why [`process_cpu_time`]
//!   sits beside it. Read them as a pair. Measured, the two agree closely for most
//!   of the sampling tick shapes and diverge materially for the one that reads the
//!   sensors, so the gap is not hypothetical; `docs/benchmarks.md` has the figures.

use std::time::Duration;

use monitrs_core::model::MetricState;

/// Whether this build can measure its own resident size and descriptor count.
///
/// False on platforms with no implementation, and false on macOS built without
/// `macos-native`, because the descriptor and task-info calls come from `libc`
/// which that feature gates. Exposed so a caller can say "not measured here"
/// without duplicating the `cfg` predicate and getting it subtly wrong.
pub const SELF_MEASUREMENT_COMPILED: bool = cfg!(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-native")
));

/// One reading of this process's own footprint.
///
/// Both fields are sampled together because a soak run wants them on the same
/// time axis: a descriptor leak and a memory leak look alike in a graph of only
/// one of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SelfUsage {
    /// Resident set size in bytes: physical memory currently mapped in.
    ///
    /// The same definition as the `RSS` column of the process table
    /// (`docs/metrics.md`), applied to ourselves.
    pub resident_bytes: MetricState<u64>,
    /// How many file descriptors this process holds open.
    pub open_descriptors: MetricState<u32>,
}

impl SelfUsage {
    /// Takes one reading.
    ///
    /// Cheap by construction — one small file read plus one directory listing on
    /// Linux, two `libproc` calls on macOS — because a measurement that perturbs
    /// what it measures is worse than none. It is still not free, so a soak run
    /// samples it on its own slow cadence rather than once per frame.
    #[must_use]
    pub fn sample() -> Self {
        Self {
            resident_bytes: resident_bytes(),
            open_descriptors: open_descriptors(),
        }
    }
}

/// This process's resident set size in bytes.
///
/// See [`SelfUsage`] for what the number does and does not mean.
#[must_use]
pub fn resident_bytes() -> MetricState<u64> {
    imp::resident_bytes()
}

/// How many file descriptors this process holds open.
///
/// The count a descriptor leak shows up in. §16.1 forbids unbounded growth here
/// specifically because the sampling loop opens and closes `/proc` files and
/// sockets constantly, and one missed close per sample is invisible for hours and
/// then fatal.
#[must_use]
pub fn open_descriptors() -> MetricState<u32> {
    imp::open_descriptors()
}

/// Whether this build can read either CPU-time figure.
///
/// Narrower than [`SELF_MEASUREMENT_COMPILED`] and deliberately so: the two CPU
/// readings need `libc` on Linux too, so they need `linux-native` there as well as
/// `macos-native` on macOS. Both are default features, so a default build on
/// either platform can answer. Exposed for the same reason as its sibling — a
/// caller should be able to say "not measured here" without restating a `cfg`
/// predicate and getting it subtly wrong.
pub const CPU_TIME_COMPILED: bool = cfg!(any(
    all(target_os = "linux", feature = "linux-native"),
    all(target_os = "macos", feature = "macos-native")
));

/// How much CPU time the calling thread has consumed since it started.
///
/// The quantity §16.1's idle self-CPU budget is actually about, and the one a
/// stopwatch cannot take. A read that *blocks* — `getfsstat`, a `CFURL` capacity
/// query — costs wall-clock time and very little CPU, so an `Instant::elapsed()`
/// around it measures something the budget does not budget; `docs/benchmarks.md`
/// ("Where the idle CPU goes") records this project making exactly that mistake
/// once already. Two of these readings, subtracted, give what a piece of work
/// really cost on the meter.
///
/// *Thread* rather than process, because that is what makes the subtraction
/// attributable: the difference across one call on this thread contains that
/// call's CPU and no other thread's, so it can be charged to the call.
///
/// The price of that narrowness is the reason [`process_cpu_time`] exists beside
/// it: work the call causes on *another* thread is real CPU that this reading
/// cannot see. Read the pair, not either alone.
///
/// Monotonic within a thread. Across threads it is not a clock at all — see the
/// third caveat in the module docs.
#[must_use]
pub fn thread_cpu_time() -> MetricState<Duration> {
    cpu::thread_cpu_time()
}

/// How much CPU time this whole process has consumed, on every thread.
///
/// The companion [`thread_cpu_time`] needs to be trustworthy rather than merely
/// narrow. A thread clock charges a call only for what the calling thread ran, so
/// a call that hands work to a helper thread — an IOKit or CoreFoundation reply,
/// a framework's own worker — reads as cheaper than it is, and there is no way to
/// tell that from the thread reading alone. The difference between the two figures
/// across the same call is how much of its CPU went somewhere else.
///
/// That is not a theoretical worry: measured per tick shape, the two agree to a
/// few tens of microseconds for a fast or a fast-plus-medium tick and diverge by
/// several milliseconds for a tick that reads the sensors, which is exactly the
/// case where the thread figure alone would have understated the cost. See
/// `docs/benchmarks.md` for the numbers and the machine they came from.
///
/// `ru_utime + ru_stime` from `getrusage(RUSAGE_SELF)`: POSIX, documented on both
/// platforms, and reported in whole microseconds rather than nanoseconds — ample
/// for a millisecond-scale tick, and worth knowing before quoting it for anything
/// finer.
#[must_use]
pub fn process_cpu_time() -> MetricState<Duration> {
    cpu::process_cpu_time()
}

/// Maps an I/O failure onto the state the affected metric takes.
///
/// A refusal becomes [`MetricState::PermissionDenied`] rather than a generic read
/// failure because the two have different remedies: one is fixed by privileges,
/// the other is not (§4). Neither can happen for our *own* process today, which is
/// exactly why the distinction is made here rather than assumed away — a future
/// sandbox that hides `/proc/self` should read as "refused", not as "broken".
#[cfg(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-native")
))]
fn unavailable_from<T>(kind: std::io::ErrorKind) -> MetricState<T> {
    match kind {
        std::io::ErrorKind::PermissionDenied => MetricState::PermissionDenied,
        // Fully qualified rather than imported: the import would be unused on a
        // build that compiles neither platform implementation.
        _ => {
            MetricState::TemporarilyUnavailable(monitrs_core::model::UnavailableReason::ReadFailed)
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    //! Linux: `/proc/self`, with no `libc` dependency at all.
    //!
    //! `/proc/self/status` rather than `/proc/self/statm` because `statm` reports
    //! pages and converting them needs the page size, which needs `libc` — a
    //! dependency this crate only takes on macOS. `status` reports kibibytes
    //! directly.

    use monitrs_core::model::{MetricState, UnavailableReason};

    /// Reads `VmRSS` from `/proc/self/status`.
    pub(super) fn resident_bytes() -> MetricState<u64> {
        let status = match std::fs::read_to_string("/proc/self/status") {
            Ok(status) => status,
            Err(error) => return super::unavailable_from(error.kind()),
        };
        parse_vm_rss(&status)
    }

    /// Extracts `VmRSS: <n> kB` from `/proc/self/status`.
    ///
    /// Split out so the parser is testable on every platform: the fixture is a
    /// string, not a machine in a particular state (§17.2).
    pub(super) fn parse_vm_rss(status: &str) -> MetricState<u64> {
        for line in status.lines() {
            let Some(rest) = line.strip_prefix("VmRSS:") else {
                continue;
            };
            let mut fields = rest.split_whitespace();
            let Some(value) = fields.next().and_then(|value| value.parse::<u64>().ok()) else {
                return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
            };
            // The kernel writes "kB" and means kibibytes. Anything else is a
            // format we do not recognise, and guessing the unit would be worse
            // than reporting that we could not read it.
            return match fields.next() {
                Some("kB") => MetricState::Available(value.saturating_mul(1024)),
                _ => MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            };
        }
        // Present for every user process; absent for a kernel thread, which this
        // can never be.
        MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed)
    }

    /// Counts the entries of `/proc/self/fd`.
    ///
    /// Includes the descriptor `read_dir` itself holds; see the module docs for
    /// why that is left in.
    pub(super) fn open_descriptors() -> MetricState<u32> {
        let entries = match std::fs::read_dir("/proc/self/fd") {
            Ok(entries) => entries,
            Err(error) => return super::unavailable_from(error.kind()),
        };
        let mut count: u32 = 0;
        for entry in entries {
            // A descriptor closed by another thread mid-listing yields an error
            // for that entry alone. Skipping it undercounts by one at worst,
            // which beats abandoning the whole reading.
            if entry.is_ok() {
                count = count.saturating_add(1);
            }
        }
        MetricState::Available(count)
    }
}

#[cfg(all(target_os = "macos", feature = "macos-native"))]
mod imp {
    //! macOS: `libproc`, the same public interface the process collector uses.
    //!
    //! There is no `/proc` here and no documented way to ask for either figure
    //! without a `libc` call, so both functions below contain one `unsafe` block
    //! with its invariant named (§15.3). Neither opens a file, so neither needs
    //! Full Disk Access and neither perturbs the descriptor count it reports.

    use core::ffi::{c_int, c_void};
    use core::mem::MaybeUninit;

    use monitrs_core::model::{MetricState, UnavailableReason};

    /// This process's PID as the `c_int` `libproc` wants.
    ///
    /// `std::process::id` rather than `libc::getpid` keeps one more call out of
    /// `unsafe`; it cannot fail and cannot exceed `c_int` on any supported target,
    /// but the conversion is still checked rather than cast.
    fn own_pid() -> Option<c_int> {
        c_int::try_from(std::process::id()).ok()
    }

    /// Reads `pti_resident_size` from `PROC_PIDTASKINFO`.
    ///
    /// `proc_pidinfo` signals failure by returning fewer bytes than asked for
    /// rather than `-1`, so the byte count is the success test — the same
    /// convention the process collector documents.
    pub(super) fn resident_bytes() -> MetricState<u64> {
        let Some(pid) = own_pid() else {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
        };
        let Ok(want) = c_int::try_from(size_of::<libc::proc_taskinfo>()) else {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
        };
        let mut info = MaybeUninit::<libc::proc_taskinfo>::zeroed();
        // SAFETY: the buffer is a zeroed `proc_taskinfo` and `want` is its real
        // size, so the kernel cannot write past it. The flavour matches the buffer
        // type: `PROC_PIDTASKINFO` fills a `proc_taskinfo`.
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
            return super::unavailable_from(std::io::Error::last_os_error().kind());
        }
        // SAFETY: the call wrote a whole `proc_taskinfo` into the zeroed buffer.
        let info = unsafe { info.assume_init() };
        MetricState::Available(info.pti_resident_size)
    }

    /// The largest descriptor table this will enumerate, in entries.
    ///
    /// A million descriptors is far beyond any `RLIMIT_NOFILE` a monitor should
    /// meet, and the value only bounds the allocation the size enquiry can talk us
    /// into: a nonsensical reply must not become an eight-megabyte allocation.
    const MAX_DESCRIPTORS: usize = 1 << 20;

    /// Counts descriptors by enumerating `PROC_PIDLISTFDS`.
    ///
    /// Two calls, both documented in `libproc.h`: a null buffer of size zero asks
    /// how large a buffer would be needed, then a real buffer is filled and the
    /// **returned** byte count — not the requested one — gives the number of live
    /// entries.
    ///
    /// The buffer is not optional, tempting though it looks. The size enquiry
    /// answers with the extent of the descriptor *table*, which does not change
    /// when a descriptor lands in an existing gap: measured on macOS 26, opening a
    /// file left the enquiry's answer identical while the real count rose by one.
    /// A leak detector that could not see one descriptor appear is not a leak
    /// detector, so the extra call and the short-lived allocation are the price.
    pub(super) fn open_descriptors() -> MetricState<u32> {
        let Some(pid) = own_pid() else {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
        };
        let stride = size_of::<libc::proc_fdinfo>();
        if stride == 0 {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
        }

        // SAFETY: a null buffer with a zero size is the documented size-enquiry
        // form of this call, so the kernel writes nothing and there is no buffer
        // for it to overrun.
        let needed = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                core::ptr::null_mut::<c_void>(),
                0,
            )
        };
        if needed < 0 {
            return super::unavailable_from(std::io::Error::last_os_error().kind());
        }
        let entries = usize::try_from(needed).unwrap_or(0) / stride;
        if entries == 0 || entries > MAX_DESCRIPTORS {
            // Zero is impossible for a live process — we hold at least stdout —
            // and anything past the cap is not a number to allocate against.
            return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
        }

        // Zero-initialised rather than uninitialised: it costs one memset of a few
        // kilobytes and removes the need to reason about a partially written
        // buffer at all.
        let mut table = vec![
            libc::proc_fdinfo {
                proc_fd: 0,
                proc_fdtype: 0,
            };
            entries
        ];
        let Ok(size) = c_int::try_from(entries.saturating_mul(stride)) else {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
        };
        // SAFETY: the buffer holds `entries` initialised `proc_fdinfo` values and
        // `size` is exactly their extent in bytes, so the kernel cannot write past
        // it. The flavour matches the buffer type: `PROC_PIDLISTFDS` fills an array
        // of `proc_fdinfo`.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                table.as_mut_ptr().cast::<c_void>(),
                size,
            )
        };
        if written < 0 {
            return super::unavailable_from(std::io::Error::last_os_error().kind());
        }
        // A remainder would mean the kernel described a partial structure, which is
        // not a number to round off and report.
        let written = usize::try_from(written).unwrap_or(0);
        if !written.is_multiple_of(stride) {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
        }
        u32::try_from(written / stride).map_or(
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            MetricState::Available,
        )
    }
}

#[cfg(not(any(
    target_os = "linux",
    all(target_os = "macos", feature = "macos-native")
)))]
mod imp {
    //! Everywhere else: honestly unsupported.
    //!
    //! Reached on Windows and the BSDs, and on macOS built without
    //! `macos-native`. Returning `Unsupported` rather than a plausible zero is
    //! what stops a soak run on such a build from "proving" a flat memory curve
    //! it never measured.

    use monitrs_core::model::MetricState;

    /// No documented reading is compiled into this build.
    pub(super) const fn resident_bytes() -> MetricState<u64> {
        MetricState::Unsupported
    }

    /// No documented reading is compiled into this build.
    pub(super) const fn open_descriptors() -> MetricState<u32> {
        MetricState::Unsupported
    }
}

#[cfg(any(
    all(target_os = "linux", feature = "linux-native"),
    all(target_os = "macos", feature = "macos-native")
))]
mod cpu {
    //! Two POSIX clocks, the same calls on both targets: `clock_gettime` with
    //! `CLOCK_THREAD_CPUTIME_ID` for the thread, `getrusage(RUSAGE_SELF)` for the
    //! process.
    //!
    //! One documented interface per reading rather than two apiece. The clock id is
    //! POSIX (`clock_getcpuclockid`'s per-thread sibling), present on Linux since
    //! 2.6.12 and on macOS since 10.12, and `libc` exposes both the function and the
    //! constant for Linux and Apple alike — so §9.3's ban on private and
    //! undocumented interfaces is satisfied without a per-platform branch.
    //!
    //! The Mach alternative, `thread_info(mach_thread_self(), THREAD_BASIC_INFO, …)`
    //! with `user_time` and `system_time` summed, is equally documented and equally
    //! permitted. It was not used because it answers on one of the two platforms,
    //! would need its own code path and its own `time_value_t` conversion, and
    //! reports microseconds where the portable call reports nanoseconds. There is no
    //! reading the portable call cannot give us that would justify either cost.

    use core::mem::MaybeUninit;
    use std::time::Duration;

    use monitrs_core::model::{MetricState, UnavailableReason};

    /// Reads the CPU time consumed by the thread that calls it.
    ///
    /// `clock_gettime` signals failure with `-1` and `errno`, so unlike the
    /// `proc_pidinfo` calls above the return value is the success test directly.
    pub(super) fn thread_cpu_time() -> MetricState<Duration> {
        // Zeroed rather than uninitialised, and via `MaybeUninit` rather than a
        // struct literal: `libc::timespec` carries `cfg`-gated padding fields on
        // some targets, so naming its fields would compile here and break there.
        let mut when = MaybeUninit::<libc::timespec>::zeroed();
        // SAFETY: the buffer is a whole zeroed `timespec` and the pointer is valid
        // for the single `timespec`-sized write the call makes, so the kernel
        // cannot write past it. `CLOCK_THREAD_CPUTIME_ID` is a documented clock id
        // on both targets and needs no further arguments.
        let outcome =
            unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, when.as_mut_ptr()) };
        if outcome != 0 {
            return super::unavailable_from(std::io::Error::last_os_error().kind());
        }
        // SAFETY: the call reported success, so it wrote a whole `timespec`; the
        // buffer was zeroed beforehand in any case, and `timespec` is two integers
        // with no invalid bit patterns.
        let when = unsafe { when.assume_init() };
        // A negative field cannot be a consumed duration. §26's rule applies to our
        // own instrument as much as to a sensor: a figure that does not parse says
        // so rather than being clamped into something plausible.
        let (Ok(seconds), Ok(nanoseconds)) =
            (u64::try_from(when.tv_sec), u64::try_from(when.tv_nsec))
        else {
            return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
        };
        Duration::from_secs(seconds)
            .checked_add(Duration::from_nanos(nanoseconds))
            .map_or(
                MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
                MetricState::Available,
            )
    }

    /// Sums `ru_utime` and `ru_stime` from `getrusage(RUSAGE_SELF)`.
    ///
    /// User and system time added together because the budget is about CPU burned,
    /// and a syscall's time in the kernel on our behalf is our cost as much as our
    /// own arithmetic is. `RUSAGE_SELF` covers every thread of this process,
    /// including ones a framework started, which is the whole point of having this
    /// beside the thread clock.
    pub(super) fn process_cpu_time() -> MetricState<Duration> {
        let mut usage = MaybeUninit::<libc::rusage>::zeroed();
        // SAFETY: the buffer is a whole zeroed `rusage` and the pointer is valid for
        // the single `rusage`-sized write the call makes, so the kernel cannot write
        // past it. `RUSAGE_SELF` is the documented constant for this process.
        let outcome = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if outcome != 0 {
            return super::unavailable_from(std::io::Error::last_os_error().kind());
        }
        // SAFETY: the call reported success, so it wrote a whole `rusage`; the buffer
        // was zeroed beforehand in any case, and every field is an integer with no
        // invalid bit patterns.
        let usage = unsafe { usage.assume_init() };
        match (timeval(usage.ru_utime), timeval(usage.ru_stime)) {
            (Some(user), Some(system)) => user.checked_add(system).map_or(
                MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
                MetricState::Available,
            ),
            _ => MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
        }
    }

    /// One `timeval` as a duration, or `None` if it cannot be one.
    ///
    /// Negative in either field means the reading is not a consumed duration, and
    /// §26's rule applies to our own instrument too: say so rather than clamp it
    /// into something that looks like a measurement.
    fn timeval(value: libc::timeval) -> Option<Duration> {
        let seconds = u64::try_from(value.tv_sec).ok()?;
        let microseconds = u64::try_from(value.tv_usec).ok()?;
        Duration::from_secs(seconds).checked_add(Duration::from_micros(microseconds))
    }
}

#[cfg(not(any(
    all(target_os = "linux", feature = "linux-native"),
    all(target_os = "macos", feature = "macos-native")
)))]
mod cpu {
    //! No CPU clock in this build: Windows, the BSDs, and either platform built
    //! without its `libc` feature.
    //!
    //! `Unsupported` rather than a zero, because a tick that was never timed must
    //! not read as a tick that cost nothing.

    use std::time::Duration;

    use monitrs_core::model::MetricState;

    /// No documented reading is compiled into this build.
    pub(super) const fn thread_cpu_time() -> MetricState<Duration> {
        MetricState::Unsupported
    }

    /// No documented reading is compiled into this build.
    pub(super) const fn process_cpu_time() -> MetricState<Duration> {
        MetricState::Unsupported
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The state a malformed `/proc/self/status` must produce.
    ///
    /// A helper rather than an import because [`super::unavailable_from`] is the only
    /// other user of `UnavailableReason` and it is `cfg`-gated; an unconditional
    /// import here would be unused on a build with no platform implementation.
    #[cfg(target_os = "linux")]
    fn unparsable<T>() -> MetricState<T> {
        MetricState::TemporarilyUnavailable(monitrs_core::model::UnavailableReason::ParseFailed)
    }

    /// The two figures a soak run trends. Panics rather than returning an option
    /// because a platform that compiled the measurement in must be able to take it.
    fn measured() -> Option<(u64, u32)> {
        if !SELF_MEASUREMENT_COMPILED {
            return None;
        }
        let usage = SelfUsage::sample();
        let resident = *usage
            .resident_bytes
            .fresh()
            .expect("a build that compiled self-measurement must produce a resident size");
        let descriptors = *usage
            .open_descriptors
            .fresh()
            .expect("a build that compiled self-measurement must produce a descriptor count");
        Some((resident, descriptors))
    }

    #[test]
    fn an_unsupported_build_says_so_rather_than_reporting_zero() {
        if SELF_MEASUREMENT_COMPILED {
            return;
        }
        // §26: unavailable is not zero. A soak run on such a build must fail to
        // measure rather than appear to measure a flat curve.
        assert_eq!(resident_bytes(), MetricState::Unsupported);
        assert_eq!(open_descriptors(), MetricState::Unsupported);
    }

    #[test]
    fn our_own_resident_size_is_plausible() {
        let Some((resident, _)) = measured() else {
            return;
        };
        // A running test binary occupies more than a megabyte and less than a
        // hundred gibibytes. Deliberately loose: the point is to catch a unit
        // error — kibibytes reported as bytes, or pages as bytes — not to pin a
        // figure that legitimately varies with the platform and the test harness.
        assert!(
            resident > 1024 * 1024,
            "{resident} bytes is too small to be a real process"
        );
        assert!(
            resident < 100 * 1024 * 1024 * 1024,
            "{resident} bytes is too large to be a unit-correct reading"
        );
    }

    #[test]
    fn we_hold_at_least_the_three_standard_descriptors() {
        let Some((_, descriptors)) = measured() else {
            return;
        };
        assert!(
            descriptors >= 3,
            "stdin, stdout and stderr alone make three, got {descriptors}"
        );
    }

    /// Descriptors another test thread may open or close while we are counting.
    ///
    /// The harness runs tests in parallel inside one process, so the descriptor
    /// count is genuinely shared state and an exact assertion is not merely strict,
    /// it is wrong: it fails when a sibling test opens a fixture. Both assertions
    /// below are written so that this much concurrent movement cannot decide them,
    /// while the effect each one measures is several times larger.
    const CONCURRENT_SLACK: u32 = 8;
    /// How many files the visibility test opens at once, chosen to dwarf the slack.
    const HELD_FILES: u32 = 32;

    /// Serialises the two tests whose subject *is* the descriptor table.
    ///
    /// They perturb the very thing they measure, by tens of descriptors, so run
    /// concurrently each one's noise is the other's failure. Poisoning is recovered
    /// from rather than propagated so that one failing test does not fail the other
    /// as well and hide which is which.
    static DESCRIPTOR_TABLE: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Takes the descriptor-table lock, ignoring a previous panic.
    fn descriptor_table_lock() -> std::sync::MutexGuard<'static, ()> {
        DESCRIPTOR_TABLE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    #[test]
    fn opening_files_is_visible_in_the_descriptor_count() {
        let _serialised = descriptor_table_lock();
        let Some((_, before)) = measured() else {
            return;
        };
        // The behaviour the soak test depends on: an instrument that could not see
        // thirty-two descriptors appear could not see a leak of thousands either.
        let path = std::env::current_exe().expect("our own path");
        let held: Vec<std::fs::File> = (0..HELD_FILES)
            .map(|_| std::fs::File::open(&path).expect("our own executable is readable"))
            .collect();
        let during = *open_descriptors()
            .fresh()
            .expect("a second reading must also succeed");
        drop(held);
        let after = *open_descriptors()
            .fresh()
            .expect("a third reading must also succeed");

        let expected = HELD_FILES - CONCURRENT_SLACK;
        assert!(
            during >= before.saturating_add(expected),
            "{HELD_FILES} open files must raise the count by at least {expected}: \
             {before} -> {during}"
        );
        assert!(
            after.saturating_add(expected) <= during,
            "closing them must give the descriptors back: {during} -> {after}"
        );
    }

    #[test]
    fn measuring_repeatedly_leaks_no_descriptors() {
        let _serialised = descriptor_table_lock();
        let Some((_, before)) = measured() else {
            return;
        };
        // The measurement is the instrument the soak test trusts. An instrument
        // that leaked a descriptor per reading would fabricate exactly the failure
        // §16.1 asks us to look for — and would show up here as a rise of two
        // hundred, which no amount of concurrent noise can imitate.
        let readings = 200;
        for _ in 0..readings {
            let _ = SelfUsage::sample();
        }
        let after = *open_descriptors()
            .fresh()
            .expect("still measurable after two hundred readings");
        assert!(
            after <= before.saturating_add(CONCURRENT_SLACK),
            "{readings} readings raised the descriptor count from {before} to {after}"
        );
    }

    #[test]
    fn a_build_without_a_cpu_clock_says_so_rather_than_reporting_zero() {
        if CPU_TIME_COMPILED {
            return;
        }
        // The same §26 rule the resident-size test states: a tick that was never
        // timed must not read as a tick that cost nothing.
        assert_eq!(thread_cpu_time(), MetricState::Unsupported);
        assert_eq!(process_cpu_time(), MetricState::Unsupported);
    }

    #[test]
    fn the_process_figure_is_never_below_this_threads_own() {
        let Some(thread) = thread_cpu_time().fresh().copied() else {
            return;
        };
        let process = process_cpu_time()
            .fresh()
            .copied()
            .expect("the same platform answered a moment ago");
        // The invariant that makes the pair readable: the process total contains
        // this thread's share, so it cannot be smaller. If it ever were, one of the
        // two is in the wrong unit — the failure mode that matters here, since
        // `getrusage` reports microseconds and `clock_gettime` nanoseconds, and the
        // measurement they feed subtracts one class of reading from the other.
        assert!(
            process >= thread,
            "the process total must include this thread's: {thread:?} vs {process:?}"
        );
    }

    #[test]
    fn process_cpu_time_advances_when_this_thread_burns_cpu() {
        let Some(before) = process_cpu_time().fresh().copied() else {
            return;
        };
        // Spinning on this thread is CPU this process spent, so the process figure
        // must move too. A `getrusage` wired to the wrong field — maximum resident
        // size, say — would sit still through this.
        let mut sink = 0u64;
        let spin_until = std::time::Instant::now() + Duration::from_millis(50);
        while std::time::Instant::now() < spin_until {
            sink = sink.wrapping_add(1);
        }
        assert!(sink > 0, "the spin must not be optimised away");
        let after = process_cpu_time()
            .fresh()
            .copied()
            .expect("the same platform answered a moment ago");
        assert!(
            after > before,
            "50 ms of spinning must show as CPU time: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn thread_cpu_time_advances_when_this_thread_burns_cpu() {
        let before = thread_cpu_time();
        let Some(before) = before.fresh().copied() else {
            // A platform that cannot answer says so, and this test has nothing to
            // prove there — the same shape `resident_bytes`'s tests use.
            return;
        };
        // Deliberately not a sleep: sleeping is precisely what this must NOT count.
        let mut sink = 0u64;
        let spin_until = std::time::Instant::now() + Duration::from_millis(50);
        while std::time::Instant::now() < spin_until {
            sink = sink.wrapping_add(1);
        }
        assert!(sink > 0, "the spin must not be optimised away");
        let after = thread_cpu_time()
            .fresh()
            .copied()
            .expect("the same platform answered a moment ago");
        assert!(
            after > before,
            "50 ms of spinning must show as CPU time: {before:?} -> {after:?}"
        );
    }

    #[test]
    fn thread_cpu_time_does_not_count_sleeping() {
        let Some(before) = thread_cpu_time().fresh().copied() else {
            return;
        };
        std::thread::sleep(Duration::from_millis(100));
        let after = thread_cpu_time()
            .fresh()
            .copied()
            .expect("the same platform answered a moment ago");
        // The whole point of this instrument: a blocking read costs wall-clock and
        // not CPU, and telling those apart is what Task 7 could not do.
        assert!(
            after.saturating_sub(before) < Duration::from_millis(20),
            "sleeping must not read as CPU time: {before:?} -> {after:?}"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn vm_rss_is_read_as_kibibytes() {
        let status = "Name:\tmonitrs\nVmSize:\t  123456 kB\nVmRSS:\t   12345 kB\nThreads:\t4\n";
        assert_eq!(
            imp::parse_vm_rss(status),
            MetricState::Available(12_345 * 1024)
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_status_file_without_vm_rss_is_unparsable_rather_than_zero() {
        assert_eq!(
            imp::parse_vm_rss("Name:\tmonitrs\nThreads:\t4\n"),
            unparsable()
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn an_unexpected_unit_is_refused_rather_than_guessed() {
        // If a future kernel reported mebibytes, silently treating them as
        // kibibytes would understate our own footprint by a factor of 1024.
        assert_eq!(imp::parse_vm_rss("VmRSS:\t12345 MB\n"), unparsable());
        assert_eq!(imp::parse_vm_rss("VmRSS:\tnot-a-number kB\n"), unparsable());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_truncated_status_line_does_not_panic() {
        for fixture in ["VmRSS:", "VmRSS:\n", "VmRSS:\t\n", "VmRSS:\t123\n", ""] {
            let _ = imp::parse_vm_rss(fixture);
        }
    }
}

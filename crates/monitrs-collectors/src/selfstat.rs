//! What monitrs itself costs: our own resident memory and descriptor count.
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
//! | Platform | Resident bytes | Open descriptors |
//! |---|---|---|
//! | Linux | `VmRSS` from `/proc/self/status` | entries in `/proc/self/fd` |
//! | macOS, `macos-native` | `pti_resident_size` from `proc_pidinfo` | `PROC_PIDLISTFDS` byte count |
//! | anything else | `Unsupported` | `Unsupported` |
//!
//! Unsupported rather than zero, for the reason the whole codebase repeats: a
//! figure nobody measured is not a figure of nought (§4, §26).
//!
//! # Two caveats worth knowing before trusting a number from here
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

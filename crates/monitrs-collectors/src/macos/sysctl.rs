//! Safe wrappers over `sysctl(3)` and the errno vocabulary the model needs.
//!
//! Two ideas carry the whole file.
//!
//! * **A failed read is a [`MetricState`], not an error.** [`NativeError`] exists
//!   only to carry an errno far enough to be classified by
//!   [`NativeError::to_state`]: `EPERM` becomes
//!   [`MetricState::PermissionDenied`], `ESRCH` becomes
//!   [`UnavailableReason::ProcessExited`], and a missing MIB becomes
//!   [`MetricState::Unsupported`]. Nothing here can turn a failure into a zero
//!   (§4, §26).
//! * **`errno` is only meaningful after a failure, and only if nothing else has
//!   run.** Several of the interfaces this collector uses report failure by
//!   returning a short byte count rather than `-1`, so the wrappers clear errno
//!   immediately before the call. Without that, a stale errno from an unrelated
//!   earlier syscall would be read as this call's reason.

use core::ffi::{CStr, c_int, c_void};
use core::mem::{MaybeUninit, size_of};

use monitrs_core::model::{MetricState, UnavailableReason};

use super::ffi::Pod;

/// Why a native read did not produce a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NativeError {
    /// The call failed and set `errno`.
    Errno(c_int),
    /// A mach routine returned a non-`KERN_SUCCESS` code.
    Mach(libc::kern_return_t),
    /// The call succeeded but wrote a different number of bytes than the
    /// structure we asked for needs, so the bytes cannot be interpreted.
    ShortRead {
        /// What the kernel wrote.
        got: usize,
        /// What the layout requires.
        want: usize,
    },
}

impl NativeError {
    /// Captures the current `errno`.
    pub(super) fn last() -> Self {
        Self::Errno(errno())
    }

    /// Whether the OS refused the read at our privilege level.
    ///
    /// Kept separate from [`NativeError::to_state`] because the process path has
    /// to branch on this to decide whether to keep a baseline value (§9.3: show
    /// permission limitations rather than substituting a number).
    pub(super) const fn is_permission_denied(self) -> bool {
        matches!(self, Self::Errno(libc::EPERM | libc::EACCES))
    }

    /// Whether the process the read named no longer exists.
    pub(super) const fn is_gone(self) -> bool {
        matches!(self, Self::Errno(libc::ESRCH))
    }

    /// The errno, when there is one.
    pub(super) const fn errno(self) -> Option<c_int> {
        match self {
            Self::Errno(code) => Some(code),
            Self::Mach(_) | Self::ShortRead { .. } => None,
        }
    }

    /// Classifies the failure as a metric availability state.
    pub(super) const fn to_state<T>(self) -> MetricState<T> {
        match self {
            Self::Errno(libc::EPERM | libc::EACCES) => MetricState::PermissionDenied,
            Self::Errno(libc::ESRCH) => {
                MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited)
            }
            // A MIB that does not exist on this kernel is a permanent absence, not
            // a transient failure: an older or newer macOS simply does not have it.
            Self::Errno(libc::ENOENT) => MetricState::Unsupported,
            Self::ShortRead { .. } => {
                MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed)
            }
            Self::Errno(_) | Self::Mach(_) => {
                MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
            }
        }
    }
}

/// The current thread's `errno`.
pub(super) fn errno() -> c_int {
    // SAFETY: `__error()` returns a pointer to this thread's errno slot, which
    // libSystem guarantees is valid for the life of the thread. The read is of a
    // plain `c_int` through a pointer we do not retain.
    unsafe { *libc::__error() }
}

/// Clears `errno` so that the next failure's reason is unambiguous.
pub(super) fn clear_errno() {
    // SAFETY: as [`errno`]; the slot is writable and owned by this thread.
    unsafe { *libc::__error() = 0 }
}

/// Reads a fixed-size sysctl node.
///
/// Fails with [`NativeError::ShortRead`] rather than returning a partially
/// initialised value when the kernel writes fewer bytes than `T` needs — which is
/// how `kern.proc.pid` reports a process that has exited.
pub(super) fn scalar<T: Pod>(mib: &mut [c_int]) -> Result<T, NativeError> {
    let mut value = MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>();
    let name_len = u32::try_from(mib.len()).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    clear_errno();
    // SAFETY: `mib` is a unique borrow of `name_len` `c_int`s, matching the
    // `name`/`namelen` contract. `oldp` points at `len` writable bytes because
    // `len` starts as `size_of::<T>()` and the kernel never writes more than the
    // value it is given. `newp`/`newlen` are null/0, which is the documented way
    // to request a read-only query.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            name_len,
            value.as_mut_ptr().cast::<c_void>(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeError::last());
    }
    if len != size_of::<T>() {
        return Err(NativeError::ShortRead {
            got: len,
            want: size_of::<T>(),
        });
    }
    // SAFETY: the buffer was zeroed, the call succeeded, and the kernel wrote
    // exactly `size_of::<T>()` bytes. `T: Pod` guarantees every bit pattern of
    // that size is a valid `T`.
    Ok(unsafe { value.assume_init() })
}

/// Reads a fixed-size sysctl node addressed by name.
pub(super) fn scalar_by_name<T: Pod>(name: &CStr) -> Result<T, NativeError> {
    let mut value = MaybeUninit::<T>::zeroed();
    let mut len = size_of::<T>();
    clear_errno();
    // SAFETY: `name` is a valid NUL-terminated C string for the duration of the
    // call. The `oldp`/`oldlenp` and `newp`/`newlen` arguments follow the same
    // contract as in [`scalar`].
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast::<c_void>(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeError::last());
    }
    if len != size_of::<T>() {
        return Err(NativeError::ShortRead {
            got: len,
            want: size_of::<T>(),
        });
    }
    // SAFETY: as in [`scalar`].
    Ok(unsafe { value.assume_init() })
}

/// Reads a NUL-terminated string sysctl node addressed by name.
///
/// Returns [`NativeError::ShortRead`] for an empty value: a sysctl that exists
/// but holds nothing is not a name, and an empty string in the UI reads as a
/// collection bug (§4).
pub(super) fn string_by_name(name: &CStr) -> Result<Box<str>, NativeError> {
    let mut len = 0usize;
    clear_errno();
    // SAFETY: a null `oldp` with a valid `oldlenp` is the documented way to ask
    // how large the value is.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            core::ptr::null_mut(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeError::last());
    }
    if len == 0 {
        return Err(NativeError::ShortRead { got: 0, want: 1 });
    }
    let mut buffer = vec![0u8; len];
    let mut got = buffer.len();
    clear_errno();
    // SAFETY: `buffer` is a unique allocation of `got` bytes, and `got` is its
    // real length, so the kernel cannot write past it.
    let rc = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<c_void>(),
            &mut got,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeError::last());
    }
    buffer.truncate(got);
    Ok(c_string_to_text(&buffer))
}

/// Reads a sysctl node into `buffer`, returning how many bytes were written.
///
/// A return of `Ok(0)` means the node exists but has no value right now, which is
/// how `kern.proc.pid` answers for a PID that has exited.
pub(super) fn into_buffer(mib: &mut [c_int], buffer: &mut [u8]) -> Result<usize, NativeError> {
    let mut len = buffer.len();
    let name_len = u32::try_from(mib.len()).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    clear_errno();
    // SAFETY: `buffer` is a unique borrow of `len` bytes and `len` is its real
    // length, so the kernel cannot write past the end. `mib` matches
    // `name`/`namelen`.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            name_len,
            buffer.as_mut_ptr().cast::<c_void>(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeError::last());
    }
    Ok(len)
}

/// Asks how many bytes a variable-size sysctl node currently needs.
pub(super) fn probe_len(mib: &mut [c_int]) -> Result<usize, NativeError> {
    let mut len = 0usize;
    let name_len = u32::try_from(mib.len()).map_err(|_| NativeError::Errno(libc::EINVAL))?;
    clear_errno();
    // SAFETY: a null `oldp` with a valid `oldlenp` is the documented sizing query.
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            name_len,
            core::ptr::null_mut(),
            &mut len,
            core::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 {
        return Err(NativeError::last());
    }
    Ok(len)
}

/// How much headroom a sizing query gets before the fetch, as a fraction.
///
/// The process table can grow between the two calls. Asking for a slice of extra
/// space costs one over-large `read` and avoids a retry; the retry loop in
/// [`read_growable`] is still there for the case where it is not enough.
const SIZING_SLACK_DIVISOR: usize = 8;

/// A floor on the headroom, so a tiny table still gets room for a few entries.
const SIZING_SLACK_FLOOR: usize = 4096;

/// How many times [`read_growable`] re-sizes before giving up.
const MAX_SIZING_ATTEMPTS: usize = 4;

/// Reads a variable-size sysctl node into a reused buffer.
///
/// The buffer is an argument rather than a return value because the process table
/// is read on every fast tick and is hundreds of kilobytes: allocating it afresh
/// each time would be a per-tick allocation §16.1 has no room for.
///
/// Returns the number of valid bytes at the front of `buffer`.
pub(super) fn read_growable(mib: &mut [c_int], buffer: &mut Vec<u8>) -> Result<usize, NativeError> {
    let mut last = NativeError::Errno(libc::ENOMEM);
    for _ in 0..MAX_SIZING_ATTEMPTS {
        let needed = probe_len(mib)?;
        if needed == 0 {
            return Ok(0);
        }
        let capacity = needed
            .saturating_add(needed / SIZING_SLACK_DIVISOR)
            .saturating_add(SIZING_SLACK_FLOOR);
        if buffer.len() < capacity {
            buffer.resize(capacity, 0);
        }
        match into_buffer(mib, buffer) {
            Ok(written) => return Ok(written),
            // The table outgrew our buffer between the two calls. That is the one
            // failure worth retrying, and only a bounded number of times.
            Err(error) if error == NativeError::Errno(libc::ENOMEM) => last = error,
            Err(error) => return Err(error),
        }
    }
    Err(last)
}

/// Interprets NUL-terminated kernel bytes as text.
///
/// Kernel strings are almost always ASCII but nothing guarantees it, and a
/// process name is attacker-influenced, so this is lossy rather than fallible.
pub(super) fn c_string_to_text(bytes: &[u8]) -> Box<str> {
    let end = bytes
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned().into()
}

/// Interprets a fixed-size `c_char` array from a kernel struct as text.
pub(super) fn c_char_array_to_text(chars: &[core::ffi::c_char]) -> Box<str> {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|byte| **byte != 0)
        // `to_ne_bytes` reinterprets rather than converting, so byte 0xFF stays
        // 0xFF instead of becoming 1 the way an absolute value would.
        .map(|byte| u8::from_ne_bytes(byte.to_ne_bytes()))
        .collect();
    String::from_utf8_lossy(&bytes).into_owned().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_denial_is_a_state_and_never_a_value() {
        let state: MetricState<u64> = NativeError::Errno(libc::EPERM).to_state();
        assert_eq!(state, MetricState::PermissionDenied);
        assert_eq!(
            state.fresh(),
            None,
            "a denied read must not become a number"
        );
        assert!(NativeError::Errno(libc::EPERM).is_permission_denied());
        assert!(NativeError::Errno(libc::EACCES).is_permission_denied());
        assert!(!NativeError::Errno(libc::ESRCH).is_permission_denied());
    }

    #[test]
    fn a_vanished_process_is_reported_as_exited_rather_than_as_a_failure() {
        // §14.1: a process disappearing during sampling is expected, so it gets
        // its own reason and never a generic read failure.
        let state: MetricState<u64> = NativeError::Errno(libc::ESRCH).to_state();
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited)
        );
        assert!(NativeError::Errno(libc::ESRCH).is_gone());
    }

    #[test]
    fn a_missing_mib_is_unsupported_because_no_privilege_would_conjure_it() {
        let state: MetricState<u64> = NativeError::Errno(libc::ENOENT).to_state();
        assert_eq!(state, MetricState::Unsupported);
    }

    #[test]
    fn a_short_read_is_a_parse_failure_not_a_truncated_value() {
        let state: MetricState<u64> = NativeError::ShortRead { got: 3, want: 8 }.to_state();
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed)
        );
        assert_eq!(NativeError::ShortRead { got: 3, want: 8 }.errno(), None);
    }

    #[test]
    fn a_mach_failure_is_a_read_failure_with_no_errno_to_report() {
        let error = NativeError::Mach(5);
        assert_eq!(error.errno(), None);
        let state: MetricState<u8> = error.to_state();
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
        );
    }

    #[test]
    fn kernel_text_stops_at_the_first_nul_and_survives_invalid_utf8() {
        assert_eq!(&*c_string_to_text(b"launchd\0\0\0"), "launchd");
        assert_eq!(&*c_string_to_text(b"no-terminator"), "no-terminator");
        assert_eq!(&*c_string_to_text(b"\0ignored"), "");
        // A process can name itself with arbitrary bytes; lossy is the only
        // option that cannot fail in a sampling loop.
        let text = c_string_to_text(&[b'a', 0xff, 0xfe, b'z', 0]);
        assert!(text.starts_with('a') && text.ends_with('z'));
    }

    #[test]
    fn a_c_char_array_is_read_up_to_its_terminator() {
        let mut buffer = [0i8; 17];
        for (slot, byte) in buffer.iter_mut().zip(b"kernel_task".iter()) {
            *slot = i8::try_from(*byte).expect("ascii fits an i8");
        }
        assert_eq!(&*c_char_array_to_text(&buffer), "kernel_task");
        assert_eq!(&*c_char_array_to_text(&[0i8; 4]), "");
    }

    #[test]
    fn clearing_errno_makes_the_next_failure_unambiguous() {
        // The reason the wrappers clear errno: without this, a short read would
        // be blamed on whatever failed last.
        clear_errno();
        assert_eq!(errno(), 0);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_scalar_sysctl_reads_the_physical_memory_size() {
        let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
        let bytes: u64 = scalar(&mut mib).expect("hw.memsize is always readable");
        assert!(bytes > 0, "a machine with no memory cannot be monitored");
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_named_string_sysctl_reads_the_hardware_model() {
        let model = string_by_name(c"hw.model").expect("hw.model is always readable");
        assert!(!model.is_empty());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_missing_mib_reports_enoent_rather_than_a_zero() {
        let error = string_by_name(c"monitrs.no.such.node").expect_err("this node cannot exist");
        assert_eq!(error.errno(), Some(libc::ENOENT));
        let state: MetricState<u8> = error.to_state();
        assert!(state.is_unsupported());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn a_growable_read_returns_the_whole_process_table_in_whole_records() {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROC, libc::KERN_PROC_ALL, 0];
        let mut buffer = Vec::new();
        let written = read_growable(&mut mib, &mut buffer).expect("kern.proc.all is readable");
        let record = size_of::<super::super::ffi::KinfoProc>();
        assert!(written >= record, "at least our own process must be listed");
        assert_eq!(
            written % record,
            0,
            "a partial record would mean the layout is wrong"
        );
    }
}

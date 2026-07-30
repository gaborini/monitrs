//! Inode counts per mount point, from `statfs(2)`.
//!
//! # Why this file is gated to Linux, unlike the rest of the module
//!
//! Everything else here parses bytes and is therefore testable from a checked-in
//! `/proc` tree on any platform (§17.2). Inode counts are not in `/proc` at all —
//! `/proc/mounts` names the filesystems but nothing under `/proc` reports `f_files` —
//! so this is a syscall against a path, and a syscall cannot be fed from a fixture.
//! The file is kept to the syscall and its errno vocabulary for that reason: it is
//! the un-exercised part, so it is the small part.
//!
//! # Why `libc` and not a hand-written declaration
//!
//! `crate::renice` and [`crate::linux::signal`] declare their calls by hand because
//! what varies between C libraries there is the *signature*. Here what varies is the
//! `struct statfs` layout — field widths differ between 32- and 64-bit targets and
//! the padding differs between glibc and musl — and getting that right for every
//! target is exactly what `libc` is for. A transcription of it would be a
//! transcription of four layouts, three of them untested by this workspace's CI.
//!
//! # Tier and bounds
//!
//! Medium (§8.6), beside the byte capacity, because `statfs` on a stalled network
//! mount can block for seconds; the baseline's own capacity read already pays that
//! cost on the same tier. The mount list is capped at `MAX_MOUNTS` so a machine
//! with a pathological mount table cannot turn one tick into thousands of syscalls
//! (§16.1).

use core::ffi::c_int;
use std::ffi::CString;

use monitrs_core::model::{InodeUsage, MetricState, UnavailableReason};

use crate::inodes::MountInodes;

/// The most mount points one refresh will `statfs`.
///
/// A container host in the low hundreds fits; beyond it the remaining mounts keep
/// the baseline's state, which says "not measured" rather than inventing a number.
const MAX_MOUNTS: usize = 512;

/// Reads the inode occupancy of each mount point in `mount_points`.
///
/// One `statfs` per mount, in the order given, stopping at `MAX_MOUNTS`. A mount
/// that cannot be read contributes the *reason* rather than being dropped: a mount
/// the kernel refuses is something the Storage screen should say out loud, and a
/// dropped reading would leave the baseline's `Unsupported` in its place — which
/// claims the platform has no inode counts at all.
pub(super) fn read_inode_usage<'a>(
    mount_points: impl IntoIterator<Item = &'a str>,
) -> Vec<MountInodes> {
    mount_points
        .into_iter()
        .take(MAX_MOUNTS)
        .filter_map(|mount_point| {
            // A path containing an interior NUL cannot be passed to a syscall, and it
            // cannot have come from the kernel either; there is nothing to report.
            let path = CString::new(mount_point).ok()?;
            Some(MountInodes::new(mount_point, read_one(&path)))
        })
        .collect()
}

/// `statfs` one path, as a metric state.
fn read_one(path: &CString) -> MetricState<InodeUsage> {
    let mut buffer = zeroed_statfs();
    // SAFETY: `path` is a NUL-terminated C string that outlives the call, and
    // `buffer` is a live, writable `statfs` the kernel fills in completely. Neither
    // pointer is retained.
    let result = unsafe { libc::statfs(path.as_ptr(), &raw mut buffer) };
    if result != 0 {
        return errno_state(errno());
    }
    // `fsfilcnt_t` is 64 bits wide on every target monitrs supports, and these
    // annotations are where that is *checked* rather than assumed: a narrower target
    // fails to compile here instead of silently truncating a count. A conversion would
    // be the wrong tool — it would compile everywhere and be a no-op on the platforms
    // that matter, which is how a truncation gets shipped.
    let total: u64 = buffer.f_files;
    let free: u64 = buffer.f_ffree;
    InodeUsage::from_counts(total, free)
}

/// A zeroed `statfs` for the kernel to fill in.
fn zeroed_statfs() -> libc::statfs {
    // SAFETY: every field is an integer or an array of integers, none of which has
    // an invalid bit pattern, so the all-zero value is inhabited. It is overwritten
    // by a successful call and never read after a failed one.
    unsafe { core::mem::zeroed() }
}

/// The current thread's `errno`.
fn errno() -> c_int {
    // SAFETY: `__errno_location()` returns this thread's errno slot, which the C
    // library guarantees is valid for the life of the thread. Only a `c_int` is read
    // and the pointer is not retained.
    unsafe { *libc::__errno_location() }
}

/// Classifies a `statfs` failure as an availability state (§4).
///
/// Nothing here can produce a number, which is the point: every branch is a reason.
const fn errno_state(code: c_int) -> MetricState<InodeUsage> {
    match code {
        libc::EACCES | libc::EPERM => MetricState::PermissionDenied,
        // The mount was in the table when it was enumerated and is not there now.
        // Expected on a machine with automounts, and not an error (§14.1).
        libc::ENOENT | libc::ENOTDIR | libc::ESTALE => {
            MetricState::TemporarilyUnavailable(UnavailableReason::DeviceDisappeared)
        }
        // A kernel or a sandbox that does not implement the call at all.
        libc::ENOSYS => MetricState::Unsupported,
        _ => MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_failure_is_a_reason_and_never_a_count() {
        // §4, and the reason this classifier exists at all: there is no errno that
        // produces `0 of 0` inodes.
        for code in [
            libc::EACCES,
            libc::EPERM,
            libc::ENOENT,
            libc::ESTALE,
            libc::ENOSYS,
            libc::EIO,
        ] {
            let state = errno_state(code);
            assert!(state.fresh().is_none(), "errno {code} produced a value");
            assert!(state.placeholder().is_some(), "errno {code} has no reason");
        }
        assert_eq!(errno_state(libc::EACCES), MetricState::PermissionDenied);
    }

    #[test]
    fn the_root_filesystem_reports_a_state_for_its_inodes() {
        // Every Linux filesystem worth mounting has an inode table, but the assertion
        // is deliberately about the *state*: a tmpfs-only sandbox is allowed to say
        // `Unsupported`, and what matters is that it says something.
        let readings = read_inode_usage(["/"]);
        assert_eq!(readings.len(), 1);
        let reading = &readings[0];
        assert_eq!(&*reading.mount_point, "/");
        match reading.inodes.fresh() {
            Some(inodes) => {
                assert!(inodes.total() > 0);
                assert!(inodes.free() <= inodes.total());
            }
            None => assert!(reading.inodes.placeholder().is_some()),
        }
    }

    #[test]
    fn a_mount_that_is_not_there_reports_why_rather_than_vanishing() {
        let readings = read_inode_usage(["/monitrs-does-not-mount-this"]);
        assert_eq!(readings.len(), 1, "the reading must not be dropped");
        assert_eq!(
            readings[0].inodes,
            MetricState::TemporarilyUnavailable(UnavailableReason::DeviceDisappeared)
        );
    }

    #[test]
    fn the_mount_list_is_capped() {
        // §16.1: one tick's syscall count cannot be set by the mount table's length.
        let many: Vec<String> = (0..(MAX_MOUNTS * 2))
            .map(|index| format!("/m{index}"))
            .collect();
        let readings = read_inode_usage(many.iter().map(String::as_str));
        assert_eq!(readings.len(), MAX_MOUNTS);
    }
}

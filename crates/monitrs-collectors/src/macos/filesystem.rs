//! Inode counts for every mounted filesystem, from `getfsstat`.
//!
//! The baseline has byte capacity and nothing else: `sysinfo` exposes no `f_files`,
//! so without this module every mount reports [`MetricState::Unsupported`](monitrs_core::model::MetricState::Unsupported) inodes
//! forever. It matters because inode exhaustion is invisible in the byte figures — a
//! filesystem with 200 GB free can still refuse to create a file — and it is the one
//! filesystem failure the Storage screen could not previously catch.
//!
//! # Why `getfsstat` and not `statfs` per mount
//!
//! One call answers for every mount. The per-mount form would be one syscall per
//! mount *and* would have to name a path, which on a stalled network mount is where
//! `statfs` blocks; `getfsstat` with [`libc::MNT_NOWAIT`] explicitly asks the kernel
//! for its cached values rather than making it go and ask each filesystem. It is
//! still a medium-tier read (§8.6), for the same reason the byte capacity is.
//!
//! # Why the buffer is sized by asking first
//!
//! `struct statfs` is over 2 KiB on this platform — two 1024-byte path buffers — so
//! a machine with a hundred mounts needs a quarter of a megabyte of scratch. Calling
//! `getfsstat` with a null buffer returns the current mount count, which is what
//! §16.1's "nothing unbounded" means here: the allocation is bounded by what the
//! kernel just said it needs, and it is re-used across ticks by the caller holding
//! the [`Vec`].
//!
//! # What is deliberately not read
//!
//! Nothing else. `getfsstat` also carries block counts, the filesystem type name and
//! the mount flags, and the baseline already has all three; taking them from here as
//! well would mean two sources for one field and no rule for which wins (§9.1).

use core::ffi::{c_char, c_int};

use monitrs_core::model::InodeUsage;

use crate::inodes::MountInodes;

use super::sysctl::{self, NativeError, clear_errno};

/// The most mounts one read will describe.
///
/// A generous bound — a busy container host is in the low hundreds — that exists so
/// a kernel answering with a nonsense count cannot turn into a multi-gigabyte
/// allocation (§16.1). Mounts beyond it keep the baseline's state, which is a
/// disclosed absence rather than a wrong number.
const MAX_MOUNTS: usize = 512;

/// Reads the inode occupancy of every mounted filesystem.
///
/// `scratch` is the caller's re-used buffer, so a steady-state tick allocates
/// nothing. Its contents are meaningless after the call returns and are not read.
///
/// # Errors
///
/// [`NativeError`] when `getfsstat` itself fails. An individual mount that reports
/// no inode table is not a failure — it comes back as
/// [`MetricState::Unsupported`](monitrs_core::model::MetricState::Unsupported) on that mount alone (§4).
pub(super) fn read_inode_usage(
    scratch: &mut Vec<libc::statfs>,
) -> Result<Vec<MountInodes>, NativeError> {
    let wanted = mount_count()?;
    if wanted == 0 {
        return Ok(Vec::new());
    }
    // One spare slot, because a filesystem can be mounted between the counting call
    // and the reading one; without it that mount is silently dropped.
    let capacity = wanted.saturating_add(1).min(MAX_MOUNTS);
    scratch.clear();
    scratch.resize(capacity, zeroed_statfs());

    let bytes = c_int::try_from(
        capacity
            .checked_mul(size_of::<libc::statfs>())
            .ok_or(NativeError::Errno(libc::EINVAL))?,
    )
    .map_err(|_| NativeError::Errno(libc::EINVAL))?;
    clear_errno();
    // SAFETY: `scratch` owns `capacity` contiguous `statfs` values and `bytes` is
    // exactly their size, which is the `mntbufp`/`bufsize` contract. `MNT_NOWAIT`
    // is one of the two documented flag values. The kernel writes whole `statfs`
    // structures into the buffer and returns how many; nothing partially written is
    // read, because the slice below is cut to that count.
    let written = unsafe { libc::getfsstat(scratch.as_mut_ptr(), bytes, libc::MNT_NOWAIT) };
    if written < 0 {
        return Err(NativeError::last());
    }
    let written = usize::try_from(written).unwrap_or(0).min(capacity);

    Ok(scratch
        .get(..written)
        .unwrap_or_default()
        .iter()
        .filter_map(|mount| {
            mount_point(&mount.f_mntonname).map(|path| {
                MountInodes::new(path, InodeUsage::from_counts(mount.f_files, mount.f_ffree))
            })
        })
        .collect())
}

/// How many filesystems are mounted right now.
fn mount_count() -> Result<usize, NativeError> {
    clear_errno();
    // SAFETY: a null `mntbufp` with a zero `bufsize` is the documented way to ask
    // `getfsstat` for the mount count without it writing anything.
    let count = unsafe { libc::getfsstat(core::ptr::null_mut(), 0, libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(NativeError::last());
    }
    Ok(usize::try_from(count).unwrap_or(0))
}

/// A zeroed `statfs`, for filling the scratch buffer.
///
/// `statfs` is a `#[repr(C)]` aggregate of integers and `c_char` arrays, so an
/// all-zero value is a valid one — an empty path and zero counters — and the kernel
/// overwrites every slot it reports.
fn zeroed_statfs() -> libc::statfs {
    // SAFETY: every field is an integer or an array of them, none of which has an
    // invalid bit pattern, so the all-zero value is inhabited.
    unsafe { core::mem::zeroed() }
}

/// The mount path out of an `f_mntonname` field, or `None` when it is empty.
///
/// An empty path cannot be matched against a mount point, so a reading for one
/// would be a reading that silently applies to nothing.
fn mount_point(raw: &[c_char]) -> Option<Box<str>> {
    // The shared helper, because `f_mntonname` is the same kind of NUL-padded
    // fixed-size kernel string as `p_comm` and the padding must be stripped the
    // same way for both.
    let path = sysctl::c_char_array_to_text(raw);
    (!path.is_empty()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nul_padded_path_field_becomes_the_path_without_its_padding() {
        let mut raw = [0i8; 1024];
        for (slot, byte) in raw.iter_mut().zip(b"/System/Volumes/Data") {
            *slot = i8::try_from(*byte).expect("ascii fits an i8");
        }
        assert_eq!(
            mount_point(&raw).as_deref(),
            Some("/System/Volumes/Data"),
            "the trailing NUL padding must not reach the mount point"
        );
    }

    #[test]
    fn an_empty_path_field_yields_no_reading_at_all() {
        // A reading keyed by an empty mount point matches no filesystem, so it would
        // be indistinguishable from a read that never happened — except that it
        // would still be carried around and compared against every mount.
        let raw = [0i8; 1024];
        assert_eq!(mount_point(&raw), None);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine's mount table"]
    fn every_mounted_filesystem_reports_a_path_and_an_inode_state() {
        let mut scratch = Vec::new();
        let readings = read_inode_usage(&mut scratch).expect("getfsstat must answer");
        assert!(
            !readings.is_empty(),
            "at least the root filesystem is mounted"
        );
        assert!(
            readings.iter().any(|reading| &*reading.mount_point == "/"),
            "the root mount must be among {:?}",
            readings
                .iter()
                .map(|reading| &reading.mount_point)
                .collect::<Vec<_>>()
        );
        for reading in &readings {
            assert!(!reading.mount_point.is_empty());
            // §4: whichever way it went, it is a state and never a fabricated zero.
            if let Some(inodes) = reading.inodes.fresh() {
                assert!(inodes.total() > 0);
                assert!(inodes.free() <= inodes.total());
            } else {
                assert!(reading.inodes.placeholder().is_some());
            }
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live machine's mount table"]
    fn a_second_read_reuses_the_buffer_rather_than_growing_it() {
        // §16.1: a medium-tier read that grew its scratch every tick would be an
        // unbounded allocation with a slow fuse.
        let mut scratch = Vec::new();
        let _ = read_inode_usage(&mut scratch).expect("getfsstat must answer");
        let first = scratch.capacity();
        for _ in 0..8 {
            let _ = read_inode_usage(&mut scratch).expect("getfsstat must answer");
        }
        assert!(
            scratch.capacity() <= first.saturating_mul(2),
            "the scratch buffer grew from {first} to {}",
            scratch.capacity()
        );
    }
}

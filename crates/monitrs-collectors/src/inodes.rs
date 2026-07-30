//! Inode counts per mount point, and the merge that puts them on a snapshot.
//!
//! # Why this is not in either platform module
//!
//! The *read* is platform-specific and lives where the rest of that platform's FFI
//! lives: `getfsstat` in [`crate::macos`], `statfs` in [`crate::linux`]. What both
//! produce is the same list — one `(mount point, inode state)` pair per mount — and
//! what both then do with it is the same merge. Writing that merge twice would mean
//! two chances to make the mistake §4 forbids, which here would be quiet: a mount
//! the read did not cover must keep whatever state it already had, and *not* borrow
//! the neighbouring mount's numbers because the mount points happened to be
//! iterated in a different order.
//!
//! # Why the readings are keyed by mount point
//!
//! Not by device: on an APFS Mac several mounts share one container, and on Linux a
//! bind mount and its origin are the same device with different inode-relevant
//! roots. The mount point is what `statfs` was asked about, so it is what the answer
//! belongs to.
//!
//! # Tier
//!
//! Medium, with the byte capacity, and for the same reason: `statfs` on a stalled
//! network mount can block for seconds, which §16.1 does not allow in the fast tier
//! (§8.6).

use monitrs_core::model::{FilesystemSnapshot, InodeUsage, MetricState};

/// One mount's inode occupancy, as a platform read produced it.
#[derive(Clone, Debug, PartialEq)]
pub struct MountInodes {
    /// The mount point the read was performed against.
    pub mount_point: Box<str>,
    /// What the read found, or why it found nothing (§4).
    pub inodes: MetricState<InodeUsage>,
}

impl MountInodes {
    /// A reading for `mount_point`.
    #[must_use]
    pub fn new(mount_point: impl Into<Box<str>>, inodes: MetricState<InodeUsage>) -> Self {
        Self {
            mount_point: mount_point.into(),
            inodes,
        }
    }
}

/// Writes `readings` onto the filesystems they belong to, matching by mount point.
///
/// A filesystem with no matching reading is left exactly as it was, which for the
/// `sysinfo` baseline means [`MetricState::Unsupported`]. That is the whole
/// discipline of this function: an unmatched mount is one nothing was learned about,
/// and inventing a state for it — or worse, taking the next reading in the list —
/// would be the fabrication §4 exists to prevent.
pub fn merge_into(filesystems: &mut [FilesystemSnapshot], readings: &[MountInodes]) {
    for filesystem in filesystems {
        if let Some(reading) = readings
            .iter()
            .find(|reading| *reading.mount_point == *filesystem.mount_point)
        {
            filesystem.inodes = reading.inodes;
        }
    }
}

#[cfg(test)]
mod tests {
    use monitrs_core::model::FilesystemKind;

    use super::*;

    fn filesystem(mount: &str) -> FilesystemSnapshot {
        FilesystemSnapshot {
            mount_point: mount.into(),
            device: Some("disk0s1".into()),
            fs_type: Some("apfs".into()),
            total_bytes: 1_000,
            available_bytes: MetricState::Available(250),
            used_bytes: MetricState::Available(750),
            usage: MetricState::Unsupported,
            inodes: MetricState::Unsupported,
            kind: FilesystemKind::Physical,
            read_only: false,
        }
    }

    #[test]
    fn a_reading_lands_on_the_mount_it_was_taken_from() {
        let mut filesystems = vec![filesystem("/"), filesystem("/System/Volumes/Data")];
        let readings = vec![
            MountInodes::new("/System/Volumes/Data", InodeUsage::from_counts(1_000, 400)),
            MountInodes::new("/", InodeUsage::from_counts(10, 1)),
        ];
        merge_into(&mut filesystems, &readings);
        assert_eq!(
            filesystems[0].inodes.fresh().map(|inodes| inodes.total()),
            Some(10),
            "the readings are matched by mount point, not by position"
        );
        assert_eq!(
            filesystems[1].inodes.fresh().map(|inodes| inodes.total()),
            Some(1_000)
        );
    }

    #[test]
    fn a_mount_the_read_did_not_cover_keeps_its_own_state() {
        // If this borrowed the neighbouring mount's numbers, a filesystem would be
        // shown occupancy it never reported — and there would be nothing on screen
        // to say so (§4).
        let mut filesystems = vec![filesystem("/"), filesystem("/private/var/vm")];
        merge_into(
            &mut filesystems,
            &[MountInodes::new("/", InodeUsage::from_counts(64, 8))],
        );
        assert!(filesystems[0].inodes.is_available());
        assert!(filesystems[1].inodes.is_unsupported());
    }

    #[test]
    fn a_refused_read_is_merged_as_the_refusal() {
        // A `statfs` that failed with EACCES is information, and overwriting the
        // baseline's `Unsupported` with it is an upgrade: "the OS refused" is a
        // stronger statement than "this platform cannot".
        let mut filesystems = vec![filesystem("/secret")];
        merge_into(
            &mut filesystems,
            &[MountInodes::new("/secret", MetricState::PermissionDenied)],
        );
        assert_eq!(filesystems[0].inodes, MetricState::PermissionDenied);
    }
}

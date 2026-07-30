//! Filesystem capacity and block-device throughput.
//!
//! §7.3 and §26 both insist these are *different metrics*. A filesystem that is
//! 95% full is not busy, and a device saturated at 100% utilization may sit on a
//! nearly empty filesystem. They are therefore separate types, and no code path
//! can accidentally render both as one unlabelled percentage.

use crate::model::MetricState;
use crate::units::{Percent, Rate};

/// What kind of filesystem a mount point is backed by.
///
/// Used by the removable/virtual filter in §7.3, not for styling.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum FilesystemKind {
    /// A local block device.
    Physical,
    /// A local device that can be unplugged.
    Removable,
    /// A network mount such as NFS or SMB.
    ///
    /// Capacity reads on these can block for seconds, which is why filesystem
    /// capacity lives in the medium tier rather than the fast one (§8.6).
    Network,
    /// A kernel pseudo-filesystem such as `tmpfs`, `devfs`, or `overlay`.
    Virtual,
    /// Not classifiable from the available information.
    #[default]
    Unknown,
}

impl FilesystemKind {
    /// Whether this mount is hidden by default in the Storage screen.
    #[must_use]
    pub const fn hidden_by_default(self) -> bool {
        matches!(self, Self::Virtual)
    }
}

/// Inode occupancy of one filesystem.
///
/// A separate type from the byte figures because it answers a different question,
/// and a classic operational surprise: a filesystem can refuse a `create` with
/// `ENOSPC` while `df` shows plenty of free space, because what ran out was the
/// inode table. Nothing else in this model can express that.
///
/// The fields are private so that the two invariants a reader relies on hold by
/// construction: `total` is never zero — a filesystem with no inode table reports
/// [`MetricState::Unsupported`] instead, never `0 of 0` (§4) — and `free` never
/// exceeds `total`, so [`InodeUsage::used`] cannot underflow into a huge number.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct InodeUsage {
    total: u64,
    free: u64,
}

impl InodeUsage {
    /// The state a `statfs`/`statvfs` `(f_files, f_ffree)` pair describes.
    ///
    /// A zero `total` is what a filesystem with no fixed inode table reports, and
    /// many do: it is [`MetricState::Unsupported`], because "this filesystem has
    /// no inode limit" and "0 inodes exist" are opposite claims and only the first
    /// is true (§4, §26).
    #[must_use]
    pub const fn from_counts(total: u64, free: u64) -> MetricState<Self> {
        if total == 0 {
            return MetricState::Unsupported;
        }
        MetricState::Available(Self {
            total,
            // A kernel that reports more free inodes than it has is reporting
            // something we cannot interpret; clamping keeps `used` meaningful
            // rather than wrapping it round to near `u64::MAX`.
            free: if free > total { total } else { free },
        })
    }

    /// Size of the inode table.
    #[must_use]
    pub const fn total(self) -> u64 {
        self.total
    }

    /// Inodes still allocatable.
    #[must_use]
    pub const fn free(self) -> u64 {
        self.free
    }

    /// Inodes in use: one per file, directory, symlink, and device node.
    #[must_use]
    pub const fn used(self) -> u64 {
        self.total.saturating_sub(self.free)
    }

    /// Share of the inode table in use.
    ///
    /// Infallible, unlike the byte-capacity percentage, because [`Self::from_counts`]
    /// has already rejected the zero-total case. The fallback exists only because
    /// [`Percent::ratio`] cannot see that invariant, and it is the pessimistic
    /// reading on purpose: a table whose size somehow could not be divided by is
    /// reported as full, never as empty, since "empty" is the reassuring answer and
    /// an uninterpretable count is not a reassuring situation.
    #[must_use]
    pub fn usage(self) -> Percent {
        Percent::ratio(self.used(), self.total).unwrap_or(Percent::FULL)
    }
}

/// Capacity of one mounted filesystem.
///
/// Contains no throughput fields at all; that is [`DiskSnapshot`]'s job.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct FilesystemSnapshot {
    /// Where it is mounted.
    pub mount_point: Box<str>,
    /// The backing device, where the platform maps it.
    pub device: Option<Box<str>>,
    /// Filesystem type, e.g. `apfs`, `ext4`, `overlay`.
    pub fs_type: Option<Box<str>>,
    /// Total capacity.
    pub total_bytes: u64,
    /// Space available to the current user.
    ///
    /// Usually smaller than `total - used` because of reserved blocks.
    pub available_bytes: MetricState<u64>,
    /// Space in use.
    pub used_bytes: MetricState<u64>,
    /// Share of capacity used. Never mixed with device utilization (§7.3).
    pub usage: MetricState<Percent>,
    /// Inode occupancy, where the filesystem has an inode table to report.
    ///
    /// A *medium*-tier read like the byte capacity, and from the same `statfs`
    /// call — `sysinfo` does not expose `f_files`, so it is the native layers that
    /// fill this in and the baseline that leaves it [`MetricState::Unsupported`].
    pub inodes: MetricState<InodeUsage>,
    /// How the mount is classified.
    pub kind: FilesystemKind,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl FilesystemSnapshot {
    /// Share of the inode table in use, carrying the inode read's own availability.
    ///
    /// The percentage a display wants: a refused or absent inode count produces a
    /// state and never a number, and a retained count produces a percentage that is
    /// marked stale exactly as the count was (§4).
    #[must_use]
    pub fn inode_usage(&self) -> MetricState<Percent> {
        self.inodes.as_ref().map(|inodes| inodes.usage())
    }
}

/// Cumulative device counters, kept alongside rates so the Inspect screen can
/// show totals as well as throughput.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DiskTotals {
    /// Bytes read since boot.
    pub read_bytes: u64,
    /// Bytes written since boot.
    pub write_bytes: u64,
}

/// Throughput of one block device.
///
/// Contains no capacity fields at all; that is [`FilesystemSnapshot`]'s job.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct DiskSnapshot {
    /// Kernel device name, e.g. `nvme0n1` or `disk0`.
    pub device: Box<str>,
    /// Hardware model, where reported.
    pub model: Option<Box<str>>,
    /// Read throughput.
    pub read: MetricState<Rate>,
    /// Write throughput.
    pub write: MetricState<Rate>,
    /// Read operations per second.
    pub read_ops: MetricState<Rate>,
    /// Write operations per second.
    pub write_ops: MetricState<Rate>,
    /// Fraction of wall time the device had at least one request in flight.
    ///
    /// §7.3 limits this to platforms where it is *semantically correct*: it is
    /// derived from `/proc/diskstats` field 10 on Linux and is
    /// [`MetricState::Unsupported`] elsewhere, because a queue-depth-based
    /// approximation on an NVMe device is misleading rather than merely
    /// imprecise.
    pub busy: MetricState<Percent>,
    /// Average in-flight request count.
    pub queue_length: MetricState<f32>,
    /// Cumulative counters.
    pub totals: MetricState<DiskTotals>,
    /// Mount points backed by this device, where the mapping is available.
    ///
    /// §8.6 puts this expensive mapping in the on-demand tier, so it is often
    /// empty in a fast-tier snapshot.
    pub mount_points: Vec<Box<str>>,
}

impl DiskSnapshot {
    /// A device whose counters exist but whose rates need a second sample.
    #[must_use]
    pub fn warming_up(device: Box<str>) -> Self {
        Self {
            device,
            model: None,
            read: MetricState::WarmingUp,
            write: MetricState::WarmingUp,
            read_ops: MetricState::WarmingUp,
            write_ops: MetricState::WarmingUp,
            busy: MetricState::WarmingUp,
            queue_length: MetricState::WarmingUp,
            totals: MetricState::WarmingUp,
            mount_points: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_filesystems_are_hidden_by_default_and_real_ones_are_not() {
        assert!(FilesystemKind::Virtual.hidden_by_default());
        for kind in [
            FilesystemKind::Physical,
            FilesystemKind::Removable,
            FilesystemKind::Network,
            FilesystemKind::Unknown,
        ] {
            assert!(!kind.hidden_by_default(), "{kind:?}");
        }
    }

    #[test]
    fn a_warming_up_device_reports_no_throughput_and_no_busy_percentage() {
        let disk = DiskSnapshot::warming_up("nvme0n1".into());
        assert!(disk.read.fresh().is_none());
        assert!(disk.busy.fresh().is_none());
        assert!(disk.mount_points.is_empty());
    }

    /// The type system is what keeps §7.3 honest: neither struct can express the
    /// other's metric, so no widget can conflate them by accident.
    #[test]
    fn capacity_and_throughput_live_in_separate_types() {
        fn assert_fields<T>(_: &T) {}
        let fs = FilesystemSnapshot {
            mount_point: "/".into(),
            device: Some("disk3s1s1".into()),
            fs_type: Some("apfs".into()),
            total_bytes: 494_384_795_648,
            available_bytes: MetricState::Available(120_000_000_000),
            used_bytes: MetricState::Available(374_384_795_648),
            usage: Percent::ratio(374_384_795_648, 494_384_795_648)
                .map_or(MetricState::Unsupported, MetricState::Available),
            inodes: InodeUsage::from_counts(4_882_812_499, 4_395_698_642),
            kind: FilesystemKind::Physical,
            read_only: false,
        };
        assert_fields(&fs);
        assert!(fs.usage.fresh().is_some());
        let disk = DiskSnapshot::warming_up("disk0".into());
        assert_fields(&disk);
    }

    #[test]
    fn a_filesystem_with_no_inode_table_is_unsupported_and_never_zero_of_zero() {
        // The property §4 exists for. `f_files == 0` is what a filesystem without a
        // fixed inode table reports, and rendering it as `0 of 0` would say the
        // table is exhausted — the opposite of the truth.
        assert_eq!(InodeUsage::from_counts(0, 0), MetricState::Unsupported);
        assert_eq!(InodeUsage::from_counts(0, 12), MetricState::Unsupported);
    }

    #[test]
    fn inode_usage_is_a_share_of_the_table_and_cannot_underflow() {
        let inodes = InodeUsage::from_counts(1_000, 250)
            .fresh()
            .copied()
            .expect("a thousand inodes is a table");
        assert_eq!(inodes.used(), 750);
        assert_eq!(inodes.free(), 250);
        assert_eq!(inodes.usage(), Percent::new(75.0).expect("finite"));

        // More free than total cannot be interpreted, and must not wrap `used`
        // round to near u64::MAX.
        let nonsense = InodeUsage::from_counts(10, 99)
            .fresh()
            .copied()
            .expect("the table size is still known");
        assert_eq!(nonsense.used(), 0);
        assert_eq!(nonsense.free(), 10);
    }

    #[test]
    fn the_inode_percentage_carries_the_counts_availability() {
        // §4: a refused count produces a state, never a number, and a stale count
        // produces a percentage that is still marked stale.
        let mut fs = FilesystemSnapshot {
            mount_point: "/".into(),
            device: None,
            fs_type: None,
            total_bytes: 1,
            available_bytes: MetricState::Unsupported,
            used_bytes: MetricState::Unsupported,
            usage: MetricState::Unsupported,
            inodes: MetricState::PermissionDenied,
            kind: FilesystemKind::Physical,
            read_only: false,
        };
        assert_eq!(fs.inode_usage(), MetricState::PermissionDenied);

        fs.inodes = InodeUsage::from_counts(4, 1);
        assert_eq!(
            fs.inode_usage().fresh().map(|percent| percent.value()),
            Some(75.0)
        );

        fs.inodes = fs.inodes.into_stale(core::time::Duration::from_secs(9));
        assert!(fs.inode_usage().is_stale());
    }
}

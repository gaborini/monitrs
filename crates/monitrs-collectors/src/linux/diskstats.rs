//! `/proc/diskstats`: block-device counters, including the one busy percentage
//! §7.3 allows.
//!
//! Field 10 of each line — *milliseconds spent doing I/O* — is wall-clock time
//! during which the device had at least one request in flight. That makes
//! `delta_field10 / elapsed` a genuine utilisation figure rather than an
//! approximation, which is why §7.3 permits a busy percentage here and nowhere
//! else. A queue-depth guess on an NVMe device would be misleading rather than
//! merely imprecise, so [`DiskStats::busy_since`] is the only place in this
//! codebase that produces one.
//!
//! Field counts vary by kernel: 4 stat fields on very old kernels and for
//! partitions, 11 since 2.6, 15 since 4.18 (discards), 17 since 5.5 (flush). The
//! parser accepts all of them and reports the missing ones as absent rather than
//! zero, because a device with no discard support has not performed zero discards
//! — the kernel simply never counted (§4).

use core::time::Duration;

use monitrs_core::units::Percent;

use crate::linux::parse::{ParseFailure, ParseResult, fields, lines, parse_u64, to_text};

/// The kernel always reports `/proc/diskstats` sectors in 512-byte units,
/// regardless of the device's physical or logical block size.
pub const SECTOR_BYTES: u64 = 512;

/// One line of `/proc/diskstats`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiskStats {
    /// Device major number.
    pub major: u64,
    /// Device minor number.
    pub minor: u64,
    /// Kernel device name, e.g. `nvme0n1`.
    pub device: Box<str>,
    /// Completed read requests.
    pub reads_completed: u64,
    /// Read requests merged before reaching the device.
    pub reads_merged: Option<u64>,
    /// Sectors read.
    pub sectors_read: u64,
    /// Milliseconds spent on reads, summed over requests.
    pub read_time_ms: Option<u64>,
    /// Completed write requests.
    pub writes_completed: u64,
    /// Write requests merged.
    pub writes_merged: Option<u64>,
    /// Sectors written.
    pub sectors_written: u64,
    /// Milliseconds spent on writes, summed over requests.
    pub write_time_ms: Option<u64>,
    /// Requests currently in flight.
    pub in_flight: Option<u64>,
    /// Field 10: wall-clock milliseconds with at least one request in flight.
    pub io_time_ms: Option<u64>,
    /// Field 11: request-weighted milliseconds, from which average queue depth
    /// can be derived.
    pub weighted_io_time_ms: Option<u64>,
    /// Completed discard requests. Linux 4.18 and later.
    pub discards_completed: Option<u64>,
    /// Sectors discarded. Linux 4.18 and later.
    pub sectors_discarded: Option<u64>,
    /// Completed flush requests. Linux 5.5 and later.
    pub flushes_completed: Option<u64>,
}

impl DiskStats {
    /// Cumulative bytes read.
    #[must_use]
    pub fn read_bytes(&self) -> u64 {
        self.sectors_read.saturating_mul(SECTOR_BYTES)
    }

    /// Cumulative bytes written.
    #[must_use]
    pub fn written_bytes(&self) -> u64 {
        self.sectors_written.saturating_mul(SECTOR_BYTES)
    }

    /// The fraction of `elapsed` during which the device was busy (§7.3).
    ///
    /// Returns `None` when either reading lacks field 10, when the counter moved
    /// backwards (the device was re-created, so §8.2 forbids a number), or when no
    /// time elapsed. Clamped to 100%: field 10 is wall-clock time and so cannot
    /// legitimately exceed the interval, but a small clock disagreement between the
    /// kernel's millisecond counter and our monotonic clock can put the ratio a
    /// hair above, and 101% busy would read as a bug rather than as saturation.
    #[must_use]
    pub fn busy_since(&self, previous: &Self, elapsed: Duration) -> Option<Percent> {
        let current = self.io_time_ms?;
        let earlier = previous.io_time_ms?;
        let delta_ms = current.checked_sub(earlier)?;
        let elapsed_ms = u64::try_from(elapsed.as_millis()).ok()?;
        Percent::ratio(delta_ms, elapsed_ms).map(Percent::clamped_to_100)
    }

    /// Average in-flight request count over the interval.
    ///
    /// Derived from field 11 the way `iostat` derives `aqu-sz`: weighted
    /// milliseconds divided by elapsed milliseconds. Reported separately from
    /// `busy` because a device can be 100% busy with one request outstanding
    /// (saturated latency) or with sixty (saturated throughput), and the two call
    /// for different responses.
    #[must_use]
    pub fn queue_length_since(&self, previous: &Self, elapsed: Duration) -> Option<f32> {
        let current = self.weighted_io_time_ms?;
        let earlier = previous.weighted_io_time_ms?;
        let delta_ms = current.checked_sub(earlier)?;
        let elapsed_ms = u64::try_from(elapsed.as_millis()).ok()?;
        if elapsed_ms == 0 {
            return None;
        }
        // Narrowing to f32 is deliberate: a queue depth is displayed with one
        // decimal, and f32 has far more precision than the metric has meaning.
        #[allow(clippy::cast_possible_truncation)]
        let length = (delta_ms as f64 / elapsed_ms as f64) as f32;
        length.is_finite().then_some(length)
    }

    /// Whether this line looks like a partition rather than a whole device.
    ///
    /// A **heuristic** on the device name, because the authoritative answer needs
    /// `/sys/block` and a directory walk this layer refuses to do every tick
    /// (§9.2: do not recursively scan unbounded subtrees). It is used only to
    /// decide what to show by default, never to drop data: a misjudged name still
    /// appears in the device list.
    #[must_use]
    pub fn looks_like_partition(&self) -> bool {
        let name = self.device.as_bytes();
        let ends_with_digit = name.last().is_some_and(u8::is_ascii_digit);
        if !ends_with_digit {
            return false;
        }
        // `nvme0n1p3` and `mmcblk0p1` mark partitions with `p`; `nvme0n1` and
        // `mmcblk0` are whole devices whose names also end in a digit.
        if self.device.starts_with("nvme") || self.device.starts_with("mmcblk") {
            return match name.iter().rposition(|byte| *byte == b'p') {
                Some(position) => name
                    .get(position + 1..)
                    .is_some_and(|tail| !tail.is_empty() && tail.iter().all(u8::is_ascii_digit)),
                None => false,
            };
        }
        // `dm-0` and `md0` are whole (virtual) devices even though they end in a
        // digit; `sda1`, `vdb2`, and `xvda1` are partitions.
        !(self.device.starts_with("dm-") || self.device.starts_with("md"))
    }

    /// Whether this device is a kernel pseudo-device with no physical media.
    ///
    /// `loop`, `ram`, and `zram` devices exist on almost every system and dominate
    /// a device list by count while carrying no I/O anyone is looking for.
    #[must_use]
    pub fn is_pseudo_device(&self) -> bool {
        self.device.starts_with("loop")
            || self.device.starts_with("ram")
            || self.device.starts_with("zram")
    }
}

/// Parses `/proc/diskstats`.
///
/// A malformed line is skipped rather than failing the file: `/proc/diskstats` is
/// a list of independent devices, and one unreadable line must not blank the
/// Storage screen. An empty file yields an empty list — a container with no block
/// devices visible is a real state, not a parse error.
pub fn parse_diskstats(bytes: &[u8]) -> ParseResult<Vec<DiskStats>> {
    let mut devices = Vec::new();
    for line in lines(bytes) {
        if let Ok(entry) = parse_line(line) {
            devices.push(entry);
        }
    }
    Ok(devices)
}

/// Parses one device line.
fn parse_line(line: &[u8]) -> ParseResult<DiskStats> {
    let mut parts = fields(line);
    let major = parse_u64(
        parts.next().ok_or(ParseFailure::Truncated("major"))?,
        "major",
    )?;
    let minor = parse_u64(
        parts.next().ok_or(ParseFailure::Truncated("minor"))?,
        "minor",
    )?;
    let device = to_text(parts.next().ok_or(ParseFailure::Truncated("device"))?);

    let mut counters: Vec<u64> = Vec::with_capacity(20);
    for field in parts {
        counters.push(parse_u64(field, "diskstats.counter")?);
    }
    let at = |index: usize| counters.get(index).copied();

    let mut stats = DiskStats {
        major,
        minor,
        device,
        ..DiskStats::default()
    };
    match counters.len() {
        // The reduced form used by pre-2.6 kernels and by partitions on kernels
        // where `CONFIG_BLK_DEV_IO_TRACE` is off: reads, sectors, writes,
        // sectors. There is no timing information at all, so no busy percentage
        // can be derived and §7.3 forbids inventing one.
        4 => {
            stats.reads_completed = at(0).unwrap_or(0);
            stats.sectors_read = at(1).unwrap_or(0);
            stats.writes_completed = at(2).unwrap_or(0);
            stats.sectors_written = at(3).unwrap_or(0);
        }
        len if len >= 11 => {
            stats.reads_completed = at(0).unwrap_or(0);
            stats.reads_merged = at(1);
            stats.sectors_read = at(2).unwrap_or(0);
            stats.read_time_ms = at(3);
            stats.writes_completed = at(4).unwrap_or(0);
            stats.writes_merged = at(5);
            stats.sectors_written = at(6).unwrap_or(0);
            stats.write_time_ms = at(7);
            stats.in_flight = at(8);
            stats.io_time_ms = at(9);
            stats.weighted_io_time_ms = at(10);
            stats.discards_completed = at(11);
            stats.sectors_discarded = at(13);
            stats.flushes_completed = at(15);
        }
        _ => return Err(ParseFailure::Truncated("diskstats.counters")),
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    fn find<'a>(devices: &'a [DiskStats], name: &str) -> &'a DiskStats {
        devices
            .iter()
            .find(|device| &*device.device == name)
            .unwrap_or_else(|| panic!("{name} is missing"))
    }

    #[test]
    fn a_typical_file_yields_every_device_with_all_eighteen_fields() {
        let devices = parse_diskstats(fixtures::DISKSTATS_TYPICAL).expect("valid");
        assert_eq!(devices.len(), 6);
        let nvme = find(&devices, "nvme0n1");
        assert_eq!(nvme.major, 259);
        assert_eq!(nvme.reads_completed, 3_000_000);
        assert_eq!(nvme.sectors_read, 200_000_000);
        assert_eq!(nvme.io_time_ms, Some(1_000_000));
        assert_eq!(nvme.weighted_io_time_ms, Some(2_700_000));
        assert_eq!(nvme.discards_completed, Some(12_000));
        assert_eq!(nvme.sectors_discarded, Some(240_000));
        assert_eq!(nvme.flushes_completed, Some(500));
    }

    #[test]
    fn sectors_are_always_five_hundred_and_twelve_bytes() {
        let devices = parse_diskstats(fixtures::DISKSTATS_TYPICAL).expect("valid");
        let nvme = find(&devices, "nvme0n1");
        assert_eq!(nvme.read_bytes(), 200_000_000 * 512);
        assert_eq!(nvme.written_bytes(), 100_000_000 * 512);
    }

    #[test]
    fn busy_time_comes_from_field_ten_and_nothing_else() {
        // §7.3: this is the only semantically correct busy percentage available.
        let before = parse_diskstats(fixtures::DISKSTATS_TYPICAL).expect("valid");
        let after = parse_diskstats(fixtures::DISKSTATS_NEXT_TICK).expect("valid");
        // 800 ms of device time inside a 2 s interval is 40% busy.
        let busy = find(&after, "nvme0n1")
            .busy_since(find(&before, "nvme0n1"), Duration::from_secs(2))
            .expect("field 10 present in both readings");
        assert!((busy.value() - 40.0).abs() < 0.01, "got {busy}");
    }

    #[test]
    fn a_busy_percentage_is_capped_at_one_hundred_rather_than_reading_as_a_bug() {
        let before = DiskStats {
            io_time_ms: Some(0),
            ..DiskStats::default()
        };
        let after = DiskStats {
            io_time_ms: Some(1_100),
            ..DiskStats::default()
        };
        let busy = after
            .busy_since(&before, Duration::from_secs(1))
            .expect("both present");
        assert!((busy.value() - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_device_without_field_ten_reports_no_busy_percentage_at_all() {
        // The 4-field reduced form. §7.3 forbids approximating from throughput.
        let devices = parse_diskstats(fixtures::DISKSTATS_SHORT_FIELDS).expect("valid");
        let sda1 = find(&devices, "sda1");
        assert_eq!(sda1.reads_completed, 1_234);
        assert_eq!(sda1.sectors_read, 5_678);
        assert_eq!(sda1.writes_completed, 91_011);
        assert_eq!(sda1.sectors_written, 121_314);
        assert_eq!(sda1.io_time_ms, None);
        assert_eq!(sda1.busy_since(sda1, Duration::from_secs(1)), None);
        assert_eq!(sda1.queue_length_since(sda1, Duration::from_secs(1)), None);
    }

    #[test]
    fn a_counter_reset_yields_no_busy_percentage_rather_than_a_huge_one() {
        // §8.2: the device was re-created, so the delta describes two different
        // counters. Subtracting would produce a wildly negative number.
        let before = parse_diskstats(fixtures::DISKSTATS_TYPICAL).expect("valid");
        let after = parse_diskstats(fixtures::DISKSTATS_AFTER_RESET).expect("valid");
        let reset = find(&after, "nvme0n1");
        assert_eq!(
            reset.busy_since(find(&before, "nvme0n1"), Duration::from_secs(2)),
            None
        );
        assert_eq!(
            reset.queue_length_since(find(&before, "nvme0n1"), Duration::from_secs(2)),
            None
        );
    }

    #[test]
    fn a_near_u64_max_counter_parses_and_converts_without_wrapping() {
        let devices = parse_diskstats(fixtures::DISKSTATS_HUGE).expect("valid");
        let nvme = find(&devices, "nvme0n1");
        assert_eq!(nvme.sectors_read, u64::MAX);
        // Saturating rather than wrapping: 512 x u64::MAX does not fit, and a
        // wrapped byte count would read as a tiny transfer.
        assert_eq!(nvme.read_bytes(), u64::MAX);
    }

    #[test]
    fn a_zero_length_interval_yields_no_busy_percentage() {
        let before = parse_diskstats(fixtures::DISKSTATS_TYPICAL).expect("valid");
        let after = parse_diskstats(fixtures::DISKSTATS_NEXT_TICK).expect("valid");
        assert_eq!(
            find(&after, "nvme0n1").busy_since(find(&before, "nvme0n1"), Duration::ZERO),
            None
        );
    }

    #[test]
    fn queue_length_is_derived_from_field_eleven() {
        let before = parse_diskstats(fixtures::DISKSTATS_TYPICAL).expect("valid");
        let after = parse_diskstats(fixtures::DISKSTATS_NEXT_TICK).expect("valid");
        // 900 weighted ms over a 2 s interval is an average depth of 0.45.
        let depth = find(&after, "nvme0n1")
            .queue_length_since(find(&before, "nvme0n1"), Duration::from_secs(2))
            .expect("field 11 present");
        assert!((depth - 0.45).abs() < 0.001, "got {depth}");
    }

    #[test]
    fn malformed_lines_are_skipped_without_costing_the_readable_ones() {
        let devices = parse_diskstats(fixtures::DISKSTATS_MALFORMED).expect("valid");
        assert_eq!(devices.len(), 1, "only the one intact line survives");
        assert_eq!(&*devices[0].device, "nvme0n1");
    }

    #[test]
    fn an_empty_file_is_an_empty_device_list_not_a_failure() {
        // A container can legitimately see no block devices at all.
        assert!(
            parse_diskstats(fixtures::DISKSTATS_EMPTY)
                .expect("valid")
                .is_empty()
        );
        assert!(parse_diskstats(b"\n\n").expect("valid").is_empty());
    }

    #[test]
    fn partition_detection_handles_nvme_mmc_and_device_mapper_names() {
        let partition = |name: &str| {
            DiskStats {
                device: name.into(),
                ..DiskStats::default()
            }
            .looks_like_partition()
        };

        assert!(partition("sda1"));
        assert!(partition("vdb2"));
        assert!(partition("xvda1"));
        assert!(partition("nvme0n1p3"));
        assert!(partition("mmcblk0p1"));

        assert!(!partition("sda"), "whole SCSI disk");
        assert!(!partition("nvme0n1"), "whole NVMe namespace");
        assert!(!partition("mmcblk0"));
        assert!(!partition("dm-0"), "a mapper device is not a partition");
        assert!(!partition("md0"), "a RAID device is not a partition");
    }

    #[test]
    fn pseudo_devices_are_identifiable_so_they_can_be_filtered_not_dropped() {
        let pseudo = |name: &str| {
            DiskStats {
                device: name.into(),
                ..DiskStats::default()
            }
            .is_pseudo_device()
        };
        assert!(pseudo("loop0"));
        assert!(pseudo("zram0"));
        assert!(pseudo("ram3"));
        assert!(!pseudo("nvme0n1"));
        assert!(!pseudo("dm-0"));
    }
}

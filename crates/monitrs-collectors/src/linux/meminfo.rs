//! `/proc/meminfo`: the Linux memory definitions of §8.4.
//!
//! Two things in this file are easy to get wrong and both are called out by the
//! specification.
//!
//! **`kB` in `/proc/meminfo` means kibibytes.** The kernel prints `kB` but divides
//! by 1024, so the multiplier here is 1024 and not 1000. Getting this wrong
//! understates memory by 2.4%, which is small enough to look plausible and wrong
//! enough to make a threshold fire late.
//!
//! **`used` is `MemTotal - MemAvailable`, not `MemTotal - MemFree`.** §8.4 requires
//! `MemAvailable` semantics precisely so that page cache is not reported as
//! application use. When the kernel is too old to publish `MemAvailable` this
//! module refuses to substitute a different formula: [`MemInfo::available_bytes`]
//! stays `None`, and the caller keeps the cross-platform baseline's numbers rather
//! than silently changing what `used` means (§8.4, §26).

use monitrs_core::model::{
    MemoryDetail, MemorySemantics, MemorySnapshot, MetricState, SwapSnapshot,
};
use monitrs_core::units::Percent;

use crate::linux::parse::{
    ParseFailure, ParseResult, fields, lines, parse_u64, split_key_value, trim_ascii,
};

/// `/proc/meminfo` prints kibibytes under the label `kB`.
const KIB: u64 = 1024;

/// The `/proc/meminfo` fields a monitor needs.
///
/// Every field except `total_bytes` is optional because the set has grown across
/// kernel versions and shrinks inside some containers. `total_bytes` is the one
/// value without which there is nothing to report a percentage against.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemInfo {
    /// `MemTotal`: physical memory the kernel manages.
    pub total_bytes: u64,
    /// `MemFree`: completely unused. Usually far smaller than `MemAvailable`.
    pub free_bytes: Option<u64>,
    /// `MemAvailable`: the kernel's own estimate of what a new allocation can get
    /// without swapping. Absent before Linux 3.14.
    pub available_bytes: Option<u64>,
    /// `Buffers`: block-device buffers.
    pub buffers_bytes: Option<u64>,
    /// `Cached`: page cache, excluding swap cache.
    pub cached_bytes: Option<u64>,
    /// `SReclaimable`: reclaimable slab, which `free(1)` counts as cache.
    pub slab_reclaimable_bytes: Option<u64>,
    /// `Shmem`: shared memory and tmpfs pages.
    pub shared_bytes: Option<u64>,
    /// `Active`: recently touched pages.
    pub active_bytes: Option<u64>,
    /// `Inactive`: reclaim candidates.
    pub inactive_bytes: Option<u64>,
    /// `Dirty`: pages awaiting writeback.
    pub dirty_bytes: Option<u64>,
    /// `SwapTotal`: configured swap. Zero means swap is off, which is a fact.
    pub swap_total_bytes: Option<u64>,
    /// `SwapFree`: unused swap.
    pub swap_free_bytes: Option<u64>,
}

impl MemInfo {
    /// `MemTotal - MemAvailable`, or `None` when the kernel has no
    /// `MemAvailable`.
    ///
    /// Saturating: a kernel that reports `MemAvailable` above `MemTotal` (seen with
    /// aggressive memory ballooning) means zero used rather than a negative.
    #[must_use]
    pub fn used_bytes(&self) -> Option<u64> {
        Some(self.total_bytes.saturating_sub(self.available_bytes?))
    }

    /// What `free(1)` calls "buff/cache": page cache plus reclaimable slab.
    ///
    /// Reported as one figure because that is the quantity a user can reason about
    /// — memory that will be handed back under pressure — while the raw `Cached`
    /// and `SReclaimable` fields remain separately readable on this struct.
    #[must_use]
    pub fn cache_and_slab_bytes(&self) -> Option<u64> {
        match (self.cached_bytes, self.slab_reclaimable_bytes) {
            (Some(cached), Some(slab)) => Some(cached.saturating_add(slab)),
            (Some(cached), None) => Some(cached),
            (None, _) => None,
        }
    }

    /// Swap in use, or `None` when the swap fields are absent.
    #[must_use]
    pub fn swap_used_bytes(&self) -> Option<u64> {
        Some(self.swap_total_bytes?.saturating_sub(self.swap_free_bytes?))
    }

    /// Builds the platform-neutral memory snapshot.
    ///
    /// `cgroup_limit` is passed in rather than derived here because §9.2 requires
    /// the container limit to be *separate* from the host total: this function's
    /// `total_bytes` is always the host figure, and the limit travels alongside it
    /// so both stay observable.
    ///
    /// Returns `None` when `MemAvailable` is absent, which is the signal to the
    /// caller that native enrichment cannot improve on the baseline for this
    /// kernel (§8.4).
    #[must_use]
    pub fn to_snapshot(&self, cgroup_limit: MetricState<u64>) -> Option<MemorySnapshot> {
        let available = self.available_bytes?;
        let used = self.used_bytes()?;
        let swap_total = self.swap_total_bytes.unwrap_or(0);
        let optional = |value: Option<u64>| match value {
            Some(bytes) => MetricState::Available(bytes),
            None => MetricState::Unsupported,
        };

        Some(MemorySnapshot {
            total_bytes: self.total_bytes,
            available: MetricState::Available(available),
            used: MetricState::Available(used),
            free: optional(self.free_bytes),
            usage: Percent::ratio(used, self.total_bytes)
                .map_or(MetricState::Unsupported, MetricState::Available),
            detail: MemoryDetail {
                cached: optional(self.cache_and_slab_bytes()),
                buffers: optional(self.buffers_bytes),
                shared: optional(self.shared_bytes),
                active: optional(self.active_bytes),
                inactive: optional(self.inactive_bytes),
                // Neither concept exists on Linux; §4 forbids answering an
                // inapplicable metric with a number.
                wired: MetricState::Unsupported,
                compressed: MetricState::Unsupported,
                dirty: optional(self.dirty_bytes),
            },
            swap: if swap_total == 0 {
                SwapSnapshot::disabled()
            } else {
                let used_swap = self.swap_used_bytes();
                SwapSnapshot {
                    total_bytes: swap_total,
                    used: optional(used_swap),
                    usage: used_swap
                        .and_then(|used| Percent::ratio(used, swap_total))
                        .map_or(MetricState::Unsupported, MetricState::Available),
                    // Swap-in and swap-out rates come from `/proc/vmstat`, which
                    // this layer does not read. Capacity without activity is the
                    // less useful half (§8.4), and saying so beats reporting zero.
                    in_rate: MetricState::Unsupported,
                    out_rate: MetricState::Unsupported,
                }
            },
            semantics: MemorySemantics::LinuxMemAvailable,
            cgroup_limit_bytes: cgroup_limit,
        })
    }
}

/// Parses one `NAME:   value kB` line into bytes.
///
/// An unrecognised unit is a failure rather than an assumption: `/proc/meminfo`
/// writes `kB` for every size field, and a line reading `MB` means the file is not
/// the one this parser understands.
fn parse_kib_value(value: &[u8], field: &'static str) -> ParseResult<u64> {
    let mut parts = fields(value);
    let Some(number) = parts.next() else {
        return Err(ParseFailure::Missing(field));
    };
    let scale = match parts.next() {
        None => 1,
        Some(b"kB") => KIB,
        Some(_) => return Err(ParseFailure::Malformed(field)),
    };
    parse_u64(number, field)?
        .checked_mul(scale)
        .ok_or(ParseFailure::Malformed(field))
}

/// Parses `/proc/meminfo`.
///
/// Unknown keys are skipped: the file has over fifty fields and gains more with
/// each kernel release, and none of the ones this monitor does not read is worth
/// the cost of parsing every second (§16.1).
pub fn parse_meminfo(bytes: &[u8]) -> ParseResult<MemInfo> {
    if trim_ascii(bytes).is_empty() {
        return Err(ParseFailure::Empty);
    }
    let mut info = MemInfo::default();
    let mut seen_total = false;

    for line in lines(bytes) {
        let Some((key, value)) = split_key_value(line) else {
            // A line without a colon is the truncated tail of a file that was
            // being read while the kernel wrote it. Skipping it is right; failing
            // the whole parse would blank the memory panel for one tick.
            continue;
        };
        let target: &mut Option<u64> = match key {
            b"MemTotal" => {
                info.total_bytes = parse_kib_value(value, "MemTotal")?;
                seen_total = true;
                continue;
            }
            b"MemFree" => &mut info.free_bytes,
            b"MemAvailable" => &mut info.available_bytes,
            b"Buffers" => &mut info.buffers_bytes,
            b"Cached" => &mut info.cached_bytes,
            b"SReclaimable" => &mut info.slab_reclaimable_bytes,
            b"Shmem" => &mut info.shared_bytes,
            b"Active" => &mut info.active_bytes,
            b"Inactive" => &mut info.inactive_bytes,
            b"Dirty" => &mut info.dirty_bytes,
            b"SwapTotal" => &mut info.swap_total_bytes,
            b"SwapFree" => &mut info.swap_free_bytes,
            _ => continue,
        };
        // A malformed *optional* field leaves that field absent rather than
        // failing the file: one unreadable `Dirty` line must not cost the whole
        // memory panel.
        *target = parse_kib_value(value, "meminfo").ok();
    }

    if !seen_total {
        return Err(ParseFailure::Missing("MemTotal"));
    }
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    fn typical() -> MemInfo {
        parse_meminfo(fixtures::MEMINFO_TYPICAL).expect("valid")
    }

    #[test]
    fn kb_in_meminfo_is_kibibytes() {
        // 32784156 kB is 33 570 975 744 bytes, not 32 784 156 000.
        let info = typical();
        assert_eq!(info.total_bytes, 32_784_156 * 1024);
        assert_eq!(info.available_bytes, Some(9_531_072 * 1024));
    }

    #[test]
    fn used_is_total_minus_available_so_page_cache_is_not_application_use() {
        // §8.4: the whole point of MemAvailable semantics. Using MemFree instead
        // would report 30 GiB used on a machine with 8 GiB of page cache.
        let info = typical();
        let used = info.used_bytes().expect("MemAvailable is present");
        assert_eq!(used, (32_784_156 - 9_531_072) * 1024);
        let free_based = info.total_bytes - info.free_bytes.expect("present");
        assert!(
            used < free_based,
            "MemAvailable semantics must report less used memory than MemFree does"
        );
    }

    #[test]
    fn cache_includes_reclaimable_slab_as_free_does() {
        let info = typical();
        assert_eq!(
            info.cache_and_slab_bytes(),
            Some((8_123_408 + 512_000) * 1024)
        );
    }

    #[test]
    fn swap_used_is_total_minus_free() {
        let info = typical();
        assert_eq!(info.swap_used_bytes(), Some((2_097_152 - 1_835_008) * 1024));
    }

    #[test]
    fn the_snapshot_declares_linux_semantics_and_keeps_the_cgroup_limit_separate() {
        // §9.2: the container limit is exposed *alongside* the host total, never
        // in place of it.
        let info = typical();
        let snapshot = info
            .to_snapshot(MetricState::Available(2 * 1024 * 1024 * 1024))
            .expect("MemAvailable is present");
        assert_eq!(snapshot.semantics, MemorySemantics::LinuxMemAvailable);
        assert_eq!(snapshot.total_bytes, 32_784_156 * 1024);
        assert_eq!(
            snapshot.cgroup_limit_bytes.fresh(),
            Some(&(2 * 1024 * 1024 * 1024))
        );
        assert_eq!(snapshot.effective_limit_bytes(), 2 * 1024 * 1024 * 1024);
        // Both remain readable: the host total did not move.
        assert!(snapshot.total_bytes > snapshot.effective_limit_bytes());
    }

    #[test]
    fn macos_only_fields_stay_unsupported_on_linux() {
        let snapshot = typical()
            .to_snapshot(MetricState::Unsupported)
            .expect("valid");
        assert!(snapshot.detail.wired.is_unsupported());
        assert!(snapshot.detail.compressed.is_unsupported());
        assert!(
            snapshot.detail.dirty.fresh().is_some(),
            "dirty is Linux-only"
        );
    }

    #[test]
    fn a_kernel_without_memavailable_produces_no_snapshot_rather_than_a_new_definition() {
        // §8.4 permits `total - available` only when the platform has a meaningful
        // available estimate. Substituting `free + buffers + cached` here would
        // silently change what the headline number means.
        let info = parse_meminfo(fixtures::MEMINFO_NO_MEMAVAILABLE).expect("valid");
        assert_eq!(info.available_bytes, None);
        assert_eq!(info.used_bytes(), None);
        assert!(info.to_snapshot(MetricState::Unsupported).is_none());
        // The rest of the file is still usable as detail.
        assert_eq!(info.total_bytes, 2_048_000 * 1024);
        assert_eq!(info.cached_bytes, Some(512_000 * 1024));
    }

    #[test]
    fn disabled_swap_is_reported_as_a_fact_not_as_unavailable() {
        let info = parse_meminfo(fixtures::MEMINFO_NO_MEMAVAILABLE).expect("valid");
        assert_eq!(info.swap_total_bytes, Some(0));
        assert_eq!(info.swap_used_bytes(), Some(0));
    }

    #[test]
    fn a_truncated_final_line_is_skipped_rather_than_failing_the_file() {
        // The realistic race: the kernel was writing while we read.
        let info = parse_meminfo(fixtures::MEMINFO_TRUNCATED).expect("MemTotal is present");
        assert_eq!(info.total_bytes, 32_784_156 * 1024);
        assert_eq!(
            info.available_bytes, None,
            "the half-written MemAvailable line must not be guessed at"
        );
        assert!(info.to_snapshot(MetricState::Unsupported).is_none());
    }

    #[test]
    fn an_unexpected_unit_on_memtotal_fails_instead_of_understating_memory_by_a_thousandfold() {
        assert_eq!(
            parse_meminfo(fixtures::MEMINFO_MALFORMED_UNITS),
            Err(ParseFailure::Malformed("MemTotal"))
        );
    }

    #[test]
    fn one_malformed_optional_field_does_not_cost_the_whole_panel() {
        let info = parse_meminfo(
            b"MemTotal:       1024 kB\nMemFree:  nonsense kB\nMemAvailable:  512 kB\n",
        )
        .expect("MemTotal parsed");
        assert_eq!(info.free_bytes, None);
        assert_eq!(info.available_bytes, Some(512 * 1024));
    }

    #[test]
    fn an_empty_or_headerless_file_is_a_typed_failure() {
        assert_eq!(parse_meminfo(b""), Err(ParseFailure::Empty));
        assert_eq!(
            parse_meminfo(b"MemFree: 12 kB\n"),
            Err(ParseFailure::Missing("MemTotal"))
        );
    }

    #[test]
    fn available_above_total_yields_zero_used_rather_than_underflowing() {
        let info = parse_meminfo(b"MemTotal: 100 kB\nMemAvailable: 200 kB\n").expect("valid");
        assert_eq!(info.used_bytes(), Some(0));
    }
}

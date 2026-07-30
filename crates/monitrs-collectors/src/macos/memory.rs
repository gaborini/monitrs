//! The `host_statistics64` memory breakdown §8.4 asks for.
//!
//! # Why wired and compressed are separate fields
//!
//! On Linux, `total - MemAvailable` is a defensible "in use" figure because page
//! cache is reclaimable on demand. macOS has two large classes that are *not*:
//! **wired** pages cannot be paged out at all, and **compressed** pages are
//! already the compacted form of anonymous memory, so reclaiming them means
//! swapping. Folding either into a single "used" bar would tell the user memory is
//! recoverable when it is not, so [`MemoryDetail`] reports them separately and
//! [`MemorySemantics::MacosVmStatistics`] records which definition produced the
//! headline numbers (§8.4, §26).
//!
//! # The page-size trap
//!
//! Every figure here is a page count, and the page size is 4 KiB on Intel and
//! 16 KiB on Apple Silicon — and 4 KiB in a *translated* process running on Apple
//! Silicon, whose user-space `vm_page_size` disagrees with the kernel's. The size
//! is therefore taken from `host_page_size` on the same host port that produced
//! the counts, and the result is cross-checked against `hw.memsize` before it is
//! published: an implausible total means the enrichment is silently dropped in
//! favour of the baseline rather than published as a wrong number (§4).

use core::mem::{MaybeUninit, offset_of, size_of};

use monitrs_core::model::{
    MemoryDetail, MemorySemantics, MemorySnapshot, MetricState, SwapSnapshot, UnavailableReason,
};
use monitrs_core::units::{Percent, Rate};

use super::ffi;
use super::sysctl::{self, NativeError};

/// The page counts `host_statistics64` reports, widened to `u64`.
///
/// The optional fields are the ones a kernel older than the SDK we compiled
/// against may not have written. They are `None` rather than `0` because "the
/// compressor holds nothing" and "this kernel does not report the compressor" are
/// different facts (§4).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct VmStatistics {
    /// Pages on the free list. Includes speculative (prefetched) pages.
    pub(super) free_pages: u64,
    /// Pages recently used.
    pub(super) active_pages: u64,
    /// Pages not recently used, and therefore reclaimable.
    pub(super) inactive_pages: u64,
    /// Pages that cannot be paged out.
    pub(super) wired_pages: u64,
    /// Pages an application has marked as discardable.
    pub(super) purgeable_pages: u64,
    /// Pages read ahead speculatively, counted inside `free_pages`.
    pub(super) speculative_pages: u64,
    /// File-backed pages: the closest macOS analogue of the Linux page cache.
    pub(super) external_pages: u64,
    /// Pages held by the compressor in compacted form.
    pub(super) compressor_pages: Option<u64>,
    /// Cumulative pages read back from swap.
    pub(super) swapins: Option<u64>,
    /// Cumulative pages written to swap.
    pub(super) swapouts: Option<u64>,
}

/// Swap capacity as `vm.swapusage` reports it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct SwapUsage {
    /// Total configured swap. Zero means macOS has not created a swap file, which
    /// is a fact about the machine rather than a missing metric.
    pub(super) total_bytes: u64,
    /// Swap currently in use.
    pub(super) used_bytes: u64,
}

/// Swap activity, which is the half of swap that actually indicates distress
/// (§11.2).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SwapActivity {
    /// Pages read back from swap per second.
    pub(super) in_rate: MetricState<Rate>,
    /// Pages written to swap per second.
    pub(super) out_rate: MetricState<Rate>,
}

impl SwapActivity {
    /// Activity on a kernel that does not report the swap counters.
    pub(super) const UNSUPPORTED: Self = Self {
        in_rate: MetricState::Unsupported,
        out_rate: MetricState::Unsupported,
    };
}

/// Whether `bytes_written` covers a field at `offset` of `size` bytes.
///
/// `host_statistics64` reports how much of the structure it filled in. Reading
/// past that would report a zero the kernel never wrote, which §4 forbids.
const fn field_written(bytes_written: usize, offset: usize, size: usize) -> bool {
    bytes_written >= offset + size
}

/// Reads the virtual-memory statistics.
pub(super) fn read_vm_statistics() -> Result<VmStatistics, NativeError> {
    let mut raw = MaybeUninit::<libc::vm_statistics64>::zeroed();
    let mut count = libc::HOST_VM_INFO64_COUNT;
    // SAFETY: `host_info64_out` points at a zeroed `vm_statistics64` and `count`
    // is that structure's size in `integer_t` units, so the kernel cannot write
    // past it. `mach_host_self` returns a host-port name that needs no release.
    let result = unsafe {
        libc::host_statistics64(
            ffi::mach_host_self(),
            libc::HOST_VM_INFO64,
            raw.as_mut_ptr().cast::<libc::integer_t>(),
            &mut count,
        )
    };
    if result != 0 {
        return Err(NativeError::Mach(result));
    }
    // SAFETY: the buffer was zeroed before the call and `vm_statistics64` is a
    // `#[repr(C, packed(8))]` aggregate of integers, so every byte pattern is a
    // valid value whether the kernel wrote it or not. Which fields it *did* write
    // is decided below, not by reading uninitialised memory.
    let raw = unsafe { raw.assume_init() };

    let written = usize::try_from(count)
        .unwrap_or(0)
        .saturating_mul(size_of::<libc::integer_t>());
    let required =
        offset_of!(libc::vm_statistics64, speculative_count) + size_of::<libc::natural_t>();
    if !field_written(written, 0, required) {
        return Err(NativeError::ShortRead {
            got: written,
            want: required,
        });
    }

    let optional = |offset: usize, size: usize, value: u64| -> Option<u64> {
        field_written(written, offset, size).then_some(value)
    };
    Ok(VmStatistics {
        free_pages: u64::from(raw.free_count),
        active_pages: u64::from(raw.active_count),
        inactive_pages: u64::from(raw.inactive_count),
        wired_pages: u64::from(raw.wire_count),
        purgeable_pages: u64::from(raw.purgeable_count),
        speculative_pages: u64::from(raw.speculative_count),
        external_pages: optional(
            offset_of!(libc::vm_statistics64, external_page_count),
            size_of::<libc::natural_t>(),
            u64::from(raw.external_page_count),
        )
        .unwrap_or(0),
        compressor_pages: optional(
            offset_of!(libc::vm_statistics64, compressor_page_count),
            size_of::<libc::natural_t>(),
            u64::from(raw.compressor_page_count),
        ),
        swapins: optional(
            offset_of!(libc::vm_statistics64, swapins),
            size_of::<u64>(),
            raw.swapins,
        ),
        swapouts: optional(
            offset_of!(libc::vm_statistics64, swapouts),
            size_of::<u64>(),
            raw.swapouts,
        ),
    })
}

/// The page size the memory statistics are counted in.
///
/// Taken from the mach host rather than from `sysconf`, so the size and the counts
/// cannot come from two different views of the machine.
pub(super) fn read_page_size() -> Result<u64, NativeError> {
    let mut size: libc::vm_size_t = 0;
    // SAFETY: `out_page_size` is a unique borrow of a correctly typed local, which
    // is all `host_page_size` requires.
    let result = unsafe { ffi::host_page_size(ffi::mach_host_self(), &mut size) };
    if result != 0 {
        return Err(NativeError::Mach(result));
    }
    let size = u64::try_from(size).unwrap_or(0);
    if size == 0 || !size.is_power_of_two() {
        // A page size that is not a power of two is not a page size.
        return Err(NativeError::ShortRead {
            got: 0,
            want: size_of::<libc::vm_size_t>(),
        });
    }
    Ok(size)
}

/// Total physical memory from `hw.memsize`.
pub(super) fn read_memory_size() -> Result<u64, NativeError> {
    let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
    sysctl::scalar::<u64>(&mut mib)
}

/// Swap capacity from `vm.swapusage`.
pub(super) fn read_swap_usage() -> Result<SwapUsage, NativeError> {
    let mut mib = [ffi::CTL_VM, ffi::VM_SWAPUSAGE];
    let usage = sysctl::scalar::<libc::xsw_usage>(&mut mib)?;
    Ok(SwapUsage {
        total_bytes: usage.xsu_total,
        used_bytes: usage.xsu_used,
    })
}

/// The window of `pages * page_size / hw.memsize` that counts as consistent.
///
/// The classes summed below do not account for every physical page — a live
/// machine leaves one to two percent in lists this collector does not read — so
/// the check is deliberately loose. It exists to catch the failure that actually
/// matters: a page size four times too small or too large, which lands two
/// hundred percent away from these bounds.
const PLAUSIBLE_RATIO_MIN: f64 = 0.5;

/// The upper bound of the same window; see [`PLAUSIBLE_RATIO_MIN`].
const PLAUSIBLE_RATIO_MAX: f64 = 1.10;

/// Whether the page counts and the page size agree with the physical memory size.
fn counts_are_plausible(total_bytes: u64, page_size: u64, vm: &VmStatistics) -> bool {
    if total_bytes == 0 || page_size == 0 {
        return false;
    }
    let pages = vm
        .free_pages
        .saturating_add(vm.active_pages)
        .saturating_add(vm.inactive_pages)
        .saturating_add(vm.wired_pages)
        .saturating_add(vm.compressor_pages.unwrap_or(0));
    let accounted = pages.saturating_mul(page_size);
    let ratio = accounted as f64 / total_bytes as f64;
    (PLAUSIBLE_RATIO_MIN..=PLAUSIBLE_RATIO_MAX).contains(&ratio)
}

/// Builds the macOS memory snapshot, or `None` when the inputs are inconsistent.
///
/// `None` is not a failure to report — the caller keeps the baseline's snapshot,
/// which is coarser but not wrong. Publishing a breakdown derived from a page size
/// that does not match the counts would be.
pub(super) fn memory_snapshot(
    total_bytes: u64,
    page_size: u64,
    vm: &VmStatistics,
    swap: Option<SwapUsage>,
    activity: SwapActivity,
) -> Option<MemorySnapshot> {
    if !counts_are_plausible(total_bytes, page_size, vm) {
        return None;
    }
    let bytes = |pages: u64| pages.saturating_mul(page_size);

    // The definition [`MemorySemantics::MacosVmStatistics`] documents, and the one
    // the Inspect screen shows the user: free plus inactive plus purgeable. The sum
    // is clamped because purgeable pages can also be counted on the active and
    // inactive lists, and "more memory available than the machine has" would be a
    // visibly broken number.
    let available = bytes(
        vm.free_pages
            .saturating_add(vm.inactive_pages)
            .saturating_add(vm.purgeable_pages),
    )
    .min(total_bytes);
    let used = total_bytes.saturating_sub(available);

    Some(MemorySnapshot {
        total_bytes,
        available: MetricState::Available(available),
        used: MetricState::Available(used),
        // `free_count` includes pages holding speculatively read-ahead file data,
        // which is not "completely unused"; `vm_stat` subtracts it the same way.
        free: MetricState::Available(bytes(vm.free_pages.saturating_sub(vm.speculative_pages))),
        usage: Percent::ratio(used, total_bytes)
            .map_or(MetricState::Unsupported, MetricState::Available),
        detail: MemoryDetail {
            cached: MetricState::Available(bytes(vm.external_pages)),
            // Both are Linux concepts. Zero would claim this kernel has no dirty
            // pages awaiting writeback, which is not what "no such metric" means.
            buffers: MetricState::Unsupported,
            dirty: MetricState::Unsupported,
            // `host_statistics64` reports no shared-memory total.
            shared: MetricState::Unsupported,
            active: MetricState::Available(bytes(vm.active_pages)),
            inactive: MetricState::Available(bytes(vm.inactive_pages)),
            wired: MetricState::Available(bytes(vm.wired_pages)),
            compressed: vm
                .compressor_pages
                .map_or(MetricState::Unsupported, |pages| {
                    MetricState::Available(bytes(pages))
                }),
        },
        swap: match swap {
            Some(usage) if usage.total_bytes > 0 => SwapSnapshot {
                total_bytes: usage.total_bytes,
                used: MetricState::Available(usage.used_bytes),
                usage: Percent::ratio(usage.used_bytes, usage.total_bytes)
                    .map_or(MetricState::Unsupported, MetricState::Available),
                in_rate: activity.in_rate,
                out_rate: activity.out_rate,
            },
            // macOS creates swap files on demand, so "no swap" is normal and is a
            // measured zero rather than an unavailable metric.
            Some(_) => SwapSnapshot::disabled(),
            None => SwapSnapshot {
                total_bytes: 0,
                used: MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
                usage: MetricState::Unsupported,
                in_rate: activity.in_rate,
                out_rate: activity.out_rate,
            },
        },
        semantics: MemorySemantics::MacosVmStatistics,
        // There are no cgroups on macOS, so there is no second limit to show
        // alongside the host total (§9.2).
        cgroup_limit_bytes: MetricState::Unsupported,
        // No cgroups on macOS, so `used` above is already the effective figure.
        cgroup_used_bytes: MetricState::Unsupported,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16 GiB machine with 16 KiB pages, which is one million pages.
    const PAGE: u64 = 16_384;
    const TOTAL: u64 = 1_000_000 * PAGE;

    fn statistics() -> VmStatistics {
        VmStatistics {
            free_pages: 100_000,
            active_pages: 400_000,
            inactive_pages: 300_000,
            wired_pages: 150_000,
            purgeable_pages: 20_000,
            speculative_pages: 30_000,
            external_pages: 250_000,
            compressor_pages: Some(50_000),
            swapins: Some(0),
            swapouts: Some(0),
        }
    }

    #[test]
    fn available_is_free_plus_inactive_plus_purgeable_as_the_semantics_promise() {
        // The Inspect screen renders `MemorySemantics::description()` verbatim, so
        // this formula is a contract with the user, not an implementation detail.
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &statistics(),
            Some(SwapUsage::default()),
            SwapActivity::UNSUPPORTED,
        )
        .expect("plausible inputs");
        assert_eq!(
            memory.available.fresh().copied(),
            Some((100_000 + 300_000 + 20_000) * PAGE)
        );
        assert_eq!(
            memory.used.fresh().copied(),
            Some(TOTAL - (100_000 + 300_000 + 20_000) * PAGE)
        );
        assert_eq!(memory.semantics, MemorySemantics::MacosVmStatistics);
    }

    #[test]
    fn wired_and_compressed_are_reported_separately_from_the_headline() {
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &statistics(),
            Some(SwapUsage::default()),
            SwapActivity::UNSUPPORTED,
        )
        .expect("plausible inputs");
        // §8.4: neither is reclaimable the way Linux page cache is, so each has to
        // be visible on its own rather than folded into "used".
        assert_eq!(memory.detail.wired.fresh().copied(), Some(150_000 * PAGE));
        assert_eq!(
            memory.detail.compressed.fresh().copied(),
            Some(50_000 * PAGE)
        );
        assert_eq!(memory.detail.active.fresh().copied(), Some(400_000 * PAGE));
        assert_eq!(
            memory.detail.inactive.fresh().copied(),
            Some(300_000 * PAGE)
        );
    }

    #[test]
    fn the_linux_only_detail_fields_stay_unsupported_rather_than_zero() {
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &statistics(),
            Some(SwapUsage::default()),
            SwapActivity::UNSUPPORTED,
        )
        .expect("plausible inputs");
        assert!(memory.detail.buffers.is_unsupported());
        assert!(memory.detail.dirty.is_unsupported());
        assert!(memory.detail.shared.is_unsupported());
        assert!(memory.cgroup_limit_bytes.is_unsupported());
    }

    #[test]
    fn free_excludes_the_speculative_pages_counted_inside_it() {
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &statistics(),
            Some(SwapUsage::default()),
            SwapActivity::UNSUPPORTED,
        )
        .expect("plausible inputs");
        assert_eq!(
            memory.free.fresh().copied(),
            Some((100_000 - 30_000) * PAGE),
            "speculative pages hold prefetched file data and are not unused"
        );
    }

    #[test]
    fn a_kernel_that_reports_no_compressor_says_so_instead_of_claiming_zero() {
        let mut vm = statistics();
        vm.compressor_pages = None;
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &vm,
            Some(SwapUsage::default()),
            SwapActivity::UNSUPPORTED,
        )
        .expect("plausible even without the compressor");
        assert!(memory.detail.compressed.is_unsupported());
        assert_ne!(memory.detail.compressed, MetricState::Available(0));
    }

    #[test]
    fn a_page_size_that_disagrees_with_the_machine_is_rejected_outright() {
        // The failure this guards against: a translated process whose user-space
        // page size is 4 KiB reading counts the kernel produced in 16 KiB pages.
        let vm = statistics();
        assert!(memory_snapshot(TOTAL, 4_096, &vm, None, SwapActivity::UNSUPPORTED).is_none());
        assert!(memory_snapshot(TOTAL, 65_536, &vm, None, SwapActivity::UNSUPPORTED).is_none());
        assert!(memory_snapshot(0, PAGE, &vm, None, SwapActivity::UNSUPPORTED).is_none());
        assert!(memory_snapshot(TOTAL, 0, &vm, None, SwapActivity::UNSUPPORTED).is_none());
    }

    #[test]
    fn available_cannot_exceed_the_physical_total_even_when_purgeable_overlaps() {
        // Purgeable pages can also sit on the active and inactive lists, so the sum
        // the semantics prescribe can overshoot on a heavily purgeable workload.
        let vm = VmStatistics {
            free_pages: 500_000,
            inactive_pages: 400_000,
            purgeable_pages: 400_000,
            active_pages: 50_000,
            wired_pages: 50_000,
            compressor_pages: Some(0),
            ..statistics()
        };
        let memory = memory_snapshot(TOTAL, PAGE, &vm, None, SwapActivity::UNSUPPORTED)
            .expect("plausible inputs");
        assert_eq!(memory.available.fresh().copied(), Some(TOTAL));
        assert_eq!(memory.used.fresh().copied(), Some(0));
    }

    #[test]
    fn swap_that_macos_has_not_created_is_a_measured_zero_not_an_absence() {
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &statistics(),
            Some(SwapUsage {
                total_bytes: 0,
                used_bytes: 0,
            }),
            SwapActivity::UNSUPPORTED,
        )
        .expect("plausible inputs");
        assert!(!memory.swap.is_enabled());
        assert_eq!(memory.swap.used.fresh().copied(), Some(0));
        assert!(
            memory.swap.usage.fresh().is_none(),
            "a percentage of zero capacity is undefined"
        );
    }

    #[test]
    fn configured_swap_reports_capacity_and_activity_together() {
        let rate = Rate::new(12.0).expect("valid rate");
        let memory = memory_snapshot(
            TOTAL,
            PAGE,
            &statistics(),
            Some(SwapUsage {
                total_bytes: 2 * 1024 * 1024 * 1024,
                used_bytes: 512 * 1024 * 1024,
            }),
            SwapActivity {
                in_rate: MetricState::Available(rate),
                out_rate: MetricState::WarmingUp,
            },
        )
        .expect("plausible inputs");
        assert!(memory.swap.is_enabled());
        assert_eq!(memory.swap.used.fresh().copied(), Some(512 * 1024 * 1024));
        assert!((memory.swap.usage.fresh().expect("usage").value() - 25.0).abs() < 0.001);
        assert_eq!(memory.swap.in_rate.fresh().copied(), Some(rate));
        assert!(memory.swap.out_rate.is_warming_up());
    }

    #[test]
    fn an_unreadable_swap_node_leaves_usage_unavailable_rather_than_zero() {
        let memory = memory_snapshot(TOTAL, PAGE, &statistics(), None, SwapActivity::UNSUPPORTED)
            .expect("plausible inputs");
        assert_eq!(memory.swap.used.fresh(), None);
        assert!(!memory.swap.is_enabled());
    }

    #[test]
    fn field_presence_follows_the_byte_count_the_kernel_reported() {
        assert!(field_written(8, 0, 8));
        assert!(!field_written(7, 0, 8));
        assert!(field_written(132, 128, 4));
        assert!(!field_written(131, 128, 4));
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_page_size_is_one_of_the_two_apple_page_sizes() {
        let page_size = read_page_size().expect("host_page_size always answers");
        assert!(
            page_size == 4_096 || page_size == 16_384,
            "unexpected page size {page_size}"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_page_counts_account_for_the_physical_memory_size() {
        // The check that proves the page size and the counts come from the same
        // view of the machine.
        let total = read_memory_size().expect("hw.memsize");
        let page_size = read_page_size().expect("host_page_size");
        let vm = read_vm_statistics().expect("host_statistics64");
        assert!(
            counts_are_plausible(total, page_size, &vm),
            "page counts do not account for {total} bytes at {page_size} bytes per page: {vm:?}"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_breakdown_reports_wired_and_compressed_separately() {
        let total = read_memory_size().expect("hw.memsize");
        let page_size = read_page_size().expect("host_page_size");
        let vm = read_vm_statistics().expect("host_statistics64");
        let memory = memory_snapshot(
            total,
            page_size,
            &vm,
            read_swap_usage().ok(),
            SwapActivity::UNSUPPORTED,
        )
        .expect("a live machine is self-consistent");

        let wired = *memory.detail.wired.fresh().expect("macOS always has wired");
        assert!(wired > 0, "a running kernel always wires some memory");
        assert!(wired < total);
        assert!(
            memory.detail.compressed.fresh().is_some(),
            "macOS 10.9 and later always report the compressor"
        );
        let available = *memory.available.fresh().expect("available");
        let used = *memory.used.fresh().expect("used");
        assert_eq!(available + used, total);
        assert_eq!(memory.semantics, MemorySemantics::MacosVmStatistics);
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_swap_node_is_readable_and_self_consistent() {
        let swap = read_swap_usage().expect("vm.swapusage is always readable");
        assert!(swap.used_bytes <= swap.total_bytes);
    }
}

//! Per-CPU time counters from `host_processor_info`, turned into percentages.
//!
//! # The tick-to-`Duration` boundary
//!
//! `processor_cpu_load_info` reports four cumulative counters per logical CPU, in
//! units of the statistics clock. The frozen rate engine deliberately holds no
//! platform constants — [`CpuTimeTotals`] is expressed in [`Duration`] — so
//! converting ticks is this module's job. The tick rate comes from
//! `kern.clockrate`, falling back to `sysconf(_SC_CLK_TCK)`, because §9.3 forbids
//! hard-coding it: both are 100 Hz on current Apple hardware, but neither is a
//! constant of the architecture.
//!
//! # Why the counters are laundered through [`CounterTracker`] first
//!
//! The kernel's counters are `natural_t`, i.e. 32 bits, so a CPU that has been
//! busy for roughly 497 days wraps. §8.2 requires a *known* width to be treated
//! as a wrap rather than a reset, and that logic already exists and is tested in
//! [`CounterTracker`] with [`CounterWidth::Bits32`]. Each state of each CPU
//! therefore gets a tracker whose validated forward deltas are accumulated into a
//! 64-bit monotonic total, and it is those totals that reach
//! [`SystemCpuTracker`]. Handing raw 32-bit values to the rate engine instead
//! would blank every CPU for one sample on wrap.

use core::ffi::c_int;
use core::mem::size_of;
use core::time::Duration;
use std::time::Instant;

use monitrs_core::model::{CpuBreakdown, CpuSnapshot, CpuUsage, MetricState, UnavailableReason};
use monitrs_core::rates::{
    CounterDelta, CounterTracker, CounterWidth, CpuTimeTotals, SystemCpuTracker,
};
use monitrs_core::units::Percent;

use super::ffi;
use super::sysctl::{self, NativeError};

/// The number of CPU states macOS accounts for: `CPU_STATE_MAX`.
///
/// There is no `iowait`, `irq`, `softirq`, or `steal` among them, which is why
/// [`CpuBreakdown`]'s Linux-only fields stay [`MetricState::Unsupported`] rather
/// than becoming zero (§4).
pub(super) const CPU_STATES: usize = 4;

/// Cumulative tick counters for one logical CPU.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct CpuTicks {
    /// Ticks in each `CPU_STATE_*` slot, indexed by the `libc` constants.
    pub(super) states: [u32; CPU_STATES],
}

/// Turns a `CPU_STATE_*` constant into an array index.
///
/// The constants are non-negative in every SDK; falling back to 0 keeps this
/// total rather than panicking inside the sampling loop (§14.3).
fn state_index(state: c_int) -> usize {
    usize::try_from(state).unwrap_or(0)
}

/// Reads one state's slot out of a per-state array.
fn slot(values: &[u64; CPU_STATES], state: c_int) -> u64 {
    values.get(state_index(state)).copied().unwrap_or(0)
}

/// An owned `processor_info_array_t`, released on drop.
///
/// `host_processor_info` allocates the array in our address space and hands over
/// ownership; without this guard the sampling loop would leak a few hundred bytes
/// per tick, which is the sort of slow leak §16.1 exists to prevent.
#[derive(Debug)]
struct ProcessorInfoArray {
    address: libc::processor_info_array_t,
    count: libc::mach_msg_type_number_t,
}

impl Drop for ProcessorInfoArray {
    fn drop(&mut self) {
        if self.address.is_null() {
            return;
        }
        let bytes = usize::try_from(self.count)
            .unwrap_or(0)
            .saturating_mul(size_of::<libc::integer_t>());
        // SAFETY: `address` and `count` are exactly what `host_processor_info`
        // returned and neither has been modified, so this is the deallocation that
        // call's contract requires. The pointer is not used again because `self` is
        // being dropped.
        let _ = unsafe {
            ffi::vm_deallocate(
                ffi::mach_task_self(),
                self.address.expose_provenance(),
                bytes,
            )
        };
    }
}

/// Reads the cumulative tick counters for every logical CPU.
pub(super) fn read_processor_ticks() -> Result<Vec<CpuTicks>, NativeError> {
    let mut processors: libc::natural_t = 0;
    let mut info: libc::processor_info_array_t = core::ptr::null_mut();
    let mut info_count: libc::mach_msg_type_number_t = 0;
    // SAFETY: the three out-parameters are unique borrows of correctly typed
    // locals, which is what `host_processor_info` requires. `mach_host_self`
    // returns a name for the host port that needs no explicit release.
    let result = unsafe {
        libc::host_processor_info(
            ffi::mach_host_self(),
            libc::PROCESSOR_CPU_LOAD_INFO,
            &mut processors,
            &mut info,
            &mut info_count,
        )
    };
    if result != 0 {
        return Err(NativeError::Mach(result));
    }
    // Taken before any further early return so the array is released on every path.
    let owned = ProcessorInfoArray {
        address: info,
        count: info_count,
    };
    if owned.address.is_null() {
        return Err(NativeError::ShortRead { got: 0, want: 1 });
    }

    let processors = usize::try_from(processors).unwrap_or(0);
    let available = usize::try_from(owned.count).unwrap_or(0);
    let wanted = processors.saturating_mul(CPU_STATES);
    // The array holds `processors * CPU_STATE_MAX` integers. Trusting the CPU
    // count over the count of integers actually returned is how an out-of-bounds
    // read gets written.
    if available < wanted {
        return Err(NativeError::ShortRead {
            got: available,
            want: wanted,
        });
    }

    let mut ticks = Vec::with_capacity(processors);
    for cpu in 0..processors {
        let mut states = [0u32; CPU_STATES];
        for (state, slot) in states.iter_mut().enumerate() {
            let index = cpu * CPU_STATES + state;
            // SAFETY: `index < wanted <= available`, and `available` is the number
            // of `integer_t`s `host_processor_info` reported for this very
            // allocation, so the read is inside the array.
            let raw = unsafe { *owned.address.add(index) };
            // Reinterpreting rather than clamping keeps a counter whose high bit is
            // set monotonic, which is what the wrap handling downstream expects.
            *slot = u32::from_ne_bytes(raw.to_ne_bytes());
        }
        ticks.push(CpuTicks { states });
    }
    Ok(ticks)
}

/// The statistics-clock frequency the tick counters are expressed in.
///
/// Queried, never assumed (§9.3). `kern.clockrate` is the authoritative source and
/// `sysconf(_SC_CLK_TCK)` the POSIX fallback for a kernel that does not export the
/// MIB. `None` means CPU percentages must be withheld rather than computed against
/// a guessed rate.
pub(super) fn ticks_per_second() -> Option<u32> {
    let mut mib = [libc::CTL_KERN, ffi::KERN_CLOCKRATE];
    if let Ok(clock) = sysctl::scalar::<ffi::Clockinfo>(&mut mib)
        && let Ok(hz) = u32::try_from(clock.hz)
        && hz > 0
    {
        return Some(hz);
    }
    // SAFETY: `sysconf` takes an integer name and returns a `c_long`. No pointers
    // are involved and the only requirement is a valid name constant.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    u32::try_from(ticks).ok().filter(|hz| *hz > 0)
}

/// Converts a tick count into a duration at `hz` ticks per second.
///
/// Split into whole seconds plus a nanosecond remainder so the arithmetic stays
/// exact and cannot overflow: `ticks * 1_000_000_000` exceeds `u64` for a counter
/// that has been running a few years.
fn ticks_to_duration(ticks: u64, hz: u32) -> Duration {
    let hz = u64::from(hz.max(1));
    let nanos = u32::try_from((ticks % hz).saturating_mul(1_000_000_000) / hz).unwrap_or(0);
    Duration::new(ticks / hz, nanos)
}

/// Re-labels one CPU's unavailability as an unavailability of a whole row.
///
/// Returns `None` for a measured value, which is what makes the caller's
/// "collapse to the first unavailable state" loop total.
fn collapse<T>(state: &MetricState<CpuUsage>) -> Option<MetricState<T>> {
    match state {
        MetricState::Available(_) => None,
        // A per-CPU tracker never produces a stale value; if one ever did, the row
        // needs another sample before it can be drawn.
        MetricState::Stale { .. } => Some(MetricState::TemporarilyUnavailable(
            UnavailableReason::NeedsSecondSample,
        )),
        MetricState::WarmingUp => Some(MetricState::WarmingUp),
        MetricState::PermissionDenied => Some(MetricState::PermissionDenied),
        MetricState::Unsupported => Some(MetricState::Unsupported),
        MetricState::TemporarilyUnavailable(reason) => {
            Some(MetricState::TemporarilyUnavailable(*reason))
        }
    }
}

/// Builds the four-way split from one interval's per-state deltas.
///
/// Returns [`MetricState::WarmingUp`] when no CPU time passed at all: §8.2 prefers
/// warming up over four fabricated zeroes.
fn breakdown_from(deltas: &[u64; CPU_STATES]) -> MetricState<CpuBreakdown> {
    let total = deltas.iter().copied().fold(0u64, u64::saturating_add);
    let share = |state: c_int| Percent::ratio(slot(deltas, state), total);
    let (Some(user), Some(system), Some(nice), Some(idle)) = (
        share(libc::CPU_STATE_USER),
        share(libc::CPU_STATE_SYSTEM),
        share(libc::CPU_STATE_NICE),
        share(libc::CPU_STATE_IDLE),
    ) else {
        return MetricState::WarmingUp;
    };
    MetricState::Available(CpuBreakdown {
        user,
        system,
        nice,
        idle,
        // macOS accounts for no other states. Zero would claim this machine never
        // waits on I/O and is never stolen from, neither of which is knowable here.
        iowait: MetricState::Unsupported,
        irq: MetricState::Unsupported,
        softirq: MetricState::Unsupported,
        steal: MetricState::Unsupported,
    })
}

/// One logical CPU's wrap-corrected totals and its usage tracker.
#[derive(Debug)]
struct CoreTracker {
    /// One counter tracker per `CPU_STATE_*` slot, for 32-bit wrap correction.
    counters: [CounterTracker; CPU_STATES],
    /// Monotonic 64-bit tick totals accumulated from validated deltas.
    totals: [u64; CPU_STATES],
    /// The most recent validated per-state delta, the basis of the split.
    last_delta: Option<[u64; CPU_STATES]>,
    /// Busy-share tracker, fed the accumulated totals.
    usage: SystemCpuTracker,
}

impl CoreTracker {
    fn new() -> Self {
        Self {
            counters: [CounterTracker::new(CounterWidth::Bits32); CPU_STATES],
            totals: [0; CPU_STATES],
            last_delta: None,
            usage: SystemCpuTracker::new(),
        }
    }

    /// Folds one reading in and returns this CPU's usage.
    fn observe(&mut self, ticks: CpuTicks, hz: u32, at: Instant) -> MetricState<CpuUsage> {
        let mut deltas = [0u64; CPU_STATES];
        let mut first_sample = false;
        let mut reset = false;
        for (state, tracker) in self.counters.iter_mut().enumerate() {
            let raw = u64::from(ticks.states.get(state).copied().unwrap_or(0));
            match tracker.observe(raw, at) {
                CounterDelta::FirstSample => first_sample = true,
                CounterDelta::Advanced { delta, .. } => {
                    if let Some(slot) = deltas.get_mut(state) {
                        *slot = delta;
                    }
                    if let Some(total) = self.totals.get_mut(state) {
                        *total = total.saturating_add(delta);
                    }
                }
                CounterDelta::Reset => reset = true,
            }
        }

        if reset {
            // The counters have already re-baselined, so the *next* sample is
            // valid. The usage baseline has to go too: a delta measured across a
            // reset describes two different counters (§8.2).
            self.usage.forget_baseline();
            self.last_delta = None;
            return MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        }
        self.last_delta = (!first_sample).then_some(deltas);

        let busy = slot(&self.totals, libc::CPU_STATE_USER)
            .saturating_add(slot(&self.totals, libc::CPU_STATE_SYSTEM))
            .saturating_add(slot(&self.totals, libc::CPU_STATE_NICE));
        let idle = slot(&self.totals, libc::CPU_STATE_IDLE);
        let totals = CpuTimeTotals::new(ticks_to_duration(busy, hz), ticks_to_duration(idle, hz));

        let usage = self.usage.observe(totals, at);
        let breakdown = self
            .last_delta
            .as_ref()
            .map_or(MetricState::WarmingUp, breakdown_from);
        usage.map(|busy| CpuUsage { busy, breakdown })
    }
}

/// Machine and per-CPU utilization derived from `host_processor_info`.
#[derive(Debug)]
pub(super) struct CpuTracker {
    /// Statistics-clock frequency, or `None` when it could not be established.
    hz: Option<u32>,
    /// One tracker per logical CPU, rebuilt if the CPU count changes.
    cores: Vec<CoreTracker>,
    /// Machine aggregate, fed the sum of the per-CPU totals.
    machine: SystemCpuTracker,
}

impl CpuTracker {
    /// Builds a tracker, querying the tick rate once.
    pub(super) fn new() -> Self {
        Self {
            hz: ticks_per_second(),
            cores: Vec::new(),
            machine: SystemCpuTracker::new(),
        }
    }

    /// The tick rate this tracker resolved, for the capability report.
    pub(super) const fn resolved_tick_rate(&self) -> Option<u32> {
        self.hz
    }

    /// Folds one reading of every CPU in and produces the CPU part of a snapshot.
    ///
    /// `None` means no CPU figure can be produced at all — an unknown tick rate or
    /// an empty CPU list — and the caller must keep the baseline's values rather
    /// than substitute zeroes.
    pub(super) fn observe(&mut self, ticks: &[CpuTicks], at: Instant) -> Option<ObservedCpu> {
        let hz = self.hz?;
        if ticks.is_empty() {
            return None;
        }
        if self.cores.len() != ticks.len() {
            // A CPU appeared or disappeared, so every baseline now belongs to a
            // different set of counters. Start over rather than diff across the
            // change (§8.2).
            self.cores = (0..ticks.len()).map(|_| CoreTracker::new()).collect();
            self.machine.forget_baseline();
        }

        let mut per_core = Vec::with_capacity(ticks.len());
        for (core, reading) in self.cores.iter_mut().zip(ticks.iter()) {
            per_core.push(core.observe(*reading, hz, at));
        }

        let mut busy = 0u64;
        let mut idle = 0u64;
        let mut machine_deltas = [0u64; CPU_STATES];
        let mut every_core_measured = true;
        for core in &self.cores {
            for (state, total) in core.totals.iter().enumerate() {
                if state == state_index(libc::CPU_STATE_IDLE) {
                    idle = idle.saturating_add(*total);
                } else {
                    busy = busy.saturating_add(*total);
                }
            }
            match core.last_delta {
                Some(deltas) => {
                    for (state, delta) in deltas.iter().enumerate() {
                        if let Some(slot) = machine_deltas.get_mut(state) {
                            *slot = slot.saturating_add(*delta);
                        }
                    }
                }
                None => every_core_measured = false,
            }
        }
        let machine = self.machine.observe(
            CpuTimeTotals::new(ticks_to_duration(busy, hz), ticks_to_duration(idle, hz)),
            at,
        );
        let breakdown = if every_core_measured {
            breakdown_from(&machine_deltas)
        } else {
            MetricState::WarmingUp
        };

        Some(ObservedCpu {
            total: machine.map(|busy| CpuUsage { busy, breakdown }),
            per_core,
        })
    }
}

/// What one CPU observation produced.
#[derive(Clone, Debug)]
pub(super) struct ObservedCpu {
    /// Aggregate machine utilization, `0..=100` (§8.3).
    pub(super) total: MetricState<CpuUsage>,
    /// Per-logical-CPU utilization in stable index order.
    pub(super) per_core: Vec<MetricState<CpuUsage>>,
}

impl ObservedCpu {
    /// Merges this observation into the baseline's CPU snapshot.
    ///
    /// Per-core usage collapses to the first unavailable state present: a
    /// [`CpuSnapshot`] carries one state for the whole vector, so publishing a
    /// short vector would render a bar chart with missing CPUs and no explanation.
    pub(super) fn merge_into(self, baseline: CpuSnapshot) -> CpuSnapshot {
        let CpuSnapshot {
            logical_count,
            physical_count,
            total: baseline_total,
            per_core: baseline_per_core,
            frequency_mhz,
        } = baseline;

        // An enrichment with nothing to say must not overwrite a baseline that has
        // something; §9.2's rule that unavailability is a state rather than a value
        // cuts both ways.
        let per_core = if self.per_core.is_empty() {
            baseline_per_core
        } else {
            match self.per_core.iter().find_map(collapse) {
                // "Unsupported" from a CPU tracker means the tick rate is unknown,
                // so there is nothing to say and the baseline keeps the field.
                Some(MetricState::Unsupported) => baseline_per_core,
                // Any other unavailability is a measurement about this tick and is
                // more informative than whatever the baseline had.
                Some(unavailable) => unavailable,
                None => MetricState::Available(
                    self.per_core
                        .iter()
                        .filter_map(|state| state.fresh().cloned())
                        .collect(),
                ),
            }
        };
        CpuSnapshot {
            logical_count,
            physical_count,
            total: match self.total {
                MetricState::Unsupported => baseline_total,
                measured => measured,
            },
            per_core,
            frequency_mhz,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticks(user: u32, system: u32, idle: u32, nice: u32) -> CpuTicks {
        let mut states = [0u32; CPU_STATES];
        states[state_index(libc::CPU_STATE_USER)] = user;
        states[state_index(libc::CPU_STATE_SYSTEM)] = system;
        states[state_index(libc::CPU_STATE_IDLE)] = idle;
        states[state_index(libc::CPU_STATE_NICE)] = nice;
        CpuTicks { states }
    }

    fn tracker_at(hz: u32) -> CpuTracker {
        CpuTracker {
            hz: Some(hz),
            cores: Vec::new(),
            machine: SystemCpuTracker::new(),
        }
    }

    #[test]
    fn tick_conversion_is_exact_at_one_hundred_hertz() {
        assert_eq!(ticks_to_duration(100, 100), Duration::from_secs(1));
        assert_eq!(ticks_to_duration(1, 100), Duration::from_millis(10));
        assert_eq!(ticks_to_duration(250, 100), Duration::from_millis(2_500));
        assert_eq!(ticks_to_duration(0, 100), Duration::ZERO);
    }

    #[test]
    fn tick_conversion_survives_a_counter_running_for_decades() {
        // `ticks * 1_000_000_000` would have overflowed a u64 long before this.
        let ten_years = 10 * 365 * 24 * 60 * 60 * 100;
        assert_eq!(
            ticks_to_duration(ten_years, 100),
            Duration::from_secs(10 * 365 * 24 * 60 * 60)
        );
        assert!(ticks_to_duration(u64::MAX, 100) > Duration::from_secs(1));
    }

    #[test]
    fn a_zero_tick_rate_cannot_divide_by_zero() {
        // `ticks_per_second` never returns zero, but this is the last line of
        // defence and §14.3 forbids a panic in the sampling loop.
        assert_eq!(ticks_to_duration(5, 0), Duration::from_secs(5));
    }

    #[test]
    fn the_first_reading_of_a_cpu_is_warming_up_and_not_zero_percent() {
        let mut tracker = tracker_at(100);
        let observed = tracker
            .observe(&[ticks(0, 0, 0, 0)], Instant::now())
            .expect("one cpu was offered");
        assert!(observed.total.is_warming_up());
        assert!(
            observed
                .per_core
                .first()
                .is_some_and(MetricState::is_warming_up),
            "the first sample must not report a percentage"
        );
    }

    #[test]
    fn a_half_busy_cpu_reads_fifty_percent_on_the_second_sample() {
        let mut tracker = tracker_at(100);
        let t0 = Instant::now();
        tracker.observe(&[ticks(1_000, 0, 1_000, 0)], t0);
        let observed = tracker
            .observe(&[ticks(1_040, 10, 1_050, 0)], t0 + Duration::from_secs(1))
            .expect("one cpu");
        let usage = observed
            .per_core
            .first()
            .and_then(MetricState::fresh)
            .expect("the second sample is measurable");
        assert!((usage.busy.value() - 50.0).abs() < 0.001, "{usage:?}");

        let breakdown = usage.breakdown.fresh().expect("macOS reports a split");
        assert!((breakdown.user.value() - 40.0).abs() < 0.001);
        assert!((breakdown.system.value() - 10.0).abs() < 0.001);
        assert!((breakdown.idle.value() - 50.0).abs() < 0.001);
        assert!(
            breakdown.iowait.is_unsupported(),
            "macOS has no iowait accounting, and zero would be a claim"
        );
        assert!(breakdown.steal.is_unsupported());
    }

    #[test]
    fn nice_time_counts_as_busy_but_is_reported_separately() {
        let mut tracker = tracker_at(100);
        let t0 = Instant::now();
        tracker.observe(&[ticks(0, 0, 0, 0)], t0);
        let observed = tracker
            .observe(&[ticks(0, 0, 50, 50)], t0 + Duration::from_secs(1))
            .expect("one cpu");
        let usage = observed
            .per_core
            .first()
            .and_then(MetricState::fresh)
            .expect("measurable");
        assert!((usage.busy.value() - 50.0).abs() < 0.001);
        let breakdown = usage.breakdown.fresh().expect("split");
        assert!((breakdown.nice.value() - 50.0).abs() < 0.001);
        assert!(breakdown.user.value().abs() < f32::EPSILON);
    }

    #[test]
    fn the_machine_aggregate_never_exceeds_one_hundred_percent() {
        let mut tracker = tracker_at(100);
        let t0 = Instant::now();
        tracker.observe(&[ticks(0, 0, 0, 0); 4], t0);
        // Every CPU fully busy for one second: 400% of a core, 100% of the machine.
        let observed = tracker
            .observe(&[ticks(100, 0, 0, 0); 4], t0 + Duration::from_secs(1))
            .expect("four cpus");
        let total = observed.total.fresh().expect("measurable");
        assert!(
            (total.busy.value() - 100.0).abs() < f32::EPSILON,
            "{total:?}"
        );
        assert!(
            total.breakdown.fresh().is_some(),
            "the aggregate split is derived from the same deltas"
        );
    }

    #[test]
    fn a_thirty_two_bit_wrap_is_reconstructed_rather_than_blanking_the_cpu() {
        // §8.2: a known counter width makes a backwards move a wrap. At 100 Hz a
        // per-CPU counter wraps after about 497 days of uptime, which real servers
        // reach.
        let mut tracker = tracker_at(100);
        let t0 = Instant::now();
        tracker.observe(&[ticks(u32::MAX - 10, 0, 0, 0)], t0);
        let observed = tracker
            .observe(&[ticks(9, 0, 11, 0)], t0 + Duration::from_secs(1))
            .expect("one cpu");
        let usage = observed
            .per_core
            .first()
            .and_then(MetricState::fresh)
            .expect("a wrap must not blank the CPU");
        // 20 user ticks across the wrap against 11 idle: 20/31 busy.
        assert!((usage.busy.value() - 64.516).abs() < 0.01, "{usage:?}");
    }

    #[test]
    fn an_unexplainable_backwards_move_is_a_reset_and_recovers_next_sample() {
        let mut tracker = tracker_at(100);
        let t0 = Instant::now();
        tracker.observe(&[ticks(1_000_000, 0, 1_000_000, 0)], t0);
        // A drop of more than half the counter range is not a wrap.
        let reset = tracker
            .observe(&[ticks(10, 0, 10, 0)], t0 + Duration::from_secs(1))
            .expect("one cpu");
        assert_eq!(
            reset.per_core.first(),
            Some(&MetricState::TemporarilyUnavailable(
                UnavailableReason::CounterReset
            ))
        );

        tracker.observe(&[ticks(20, 0, 20, 0)], t0 + Duration::from_secs(2));
        let recovered = tracker
            .observe(&[ticks(30, 0, 20, 0)], t0 + Duration::from_secs(3))
            .expect("one cpu");
        assert!(
            recovered
                .per_core
                .first()
                .is_some_and(MetricState::is_available),
            "got {:?}",
            recovered.per_core.first()
        );
    }

    #[test]
    fn a_cpu_appearing_restarts_every_baseline_instead_of_diffing_across_it() {
        let mut tracker = tracker_at(100);
        let t0 = Instant::now();
        tracker.observe(&[ticks(0, 0, 0, 0)], t0);
        tracker.observe(&[ticks(50, 0, 50, 0)], t0 + Duration::from_secs(1));
        let after_hotplug = tracker
            .observe(
                &[ticks(50, 0, 50, 0), ticks(0, 0, 0, 0)],
                t0 + Duration::from_secs(2),
            )
            .expect("two cpus");
        assert!(after_hotplug.total.is_warming_up());
        assert_eq!(after_hotplug.per_core.len(), 2);
        assert!(
            after_hotplug
                .per_core
                .iter()
                .all(MetricState::is_warming_up)
        );
    }

    #[test]
    fn an_unknown_tick_rate_produces_no_cpu_observation_at_all() {
        // Better no CPU figure than one divided by a guessed constant (§9.3).
        let mut tracker = CpuTracker {
            hz: None,
            cores: Vec::new(),
            machine: SystemCpuTracker::new(),
        };
        assert!(
            tracker
                .observe(&[ticks(1, 2, 3, 4)], Instant::now())
                .is_none()
        );
        assert!(tracker.resolved_tick_rate().is_none());
    }

    #[test]
    fn a_partly_warming_row_does_not_publish_a_short_per_core_vector() {
        let observed = ObservedCpu {
            total: MetricState::WarmingUp,
            per_core: vec![
                MetricState::Available(CpuUsage::plain(Percent::ZERO)),
                MetricState::WarmingUp,
            ],
        };
        let merged = observed.merge_into(CpuSnapshot::warming_up(2));
        assert!(
            merged.per_core.fresh().is_none(),
            "half a bar chart is worse than none"
        );
        assert!(merged.per_core.is_warming_up());
    }

    #[test]
    fn a_denied_cpu_row_stays_denied_rather_than_becoming_warming_up() {
        let observed = ObservedCpu {
            total: MetricState::PermissionDenied,
            per_core: vec![MetricState::PermissionDenied],
        };
        let merged = observed.merge_into(CpuSnapshot::warming_up(1));
        assert_eq!(merged.per_core, MetricState::PermissionDenied);
        assert_eq!(merged.total, MetricState::PermissionDenied);
    }

    #[test]
    fn merging_keeps_the_baselines_counts_and_frequency() {
        let mut baseline = CpuSnapshot::warming_up(12);
        baseline.physical_count = MetricState::Available(10);
        baseline.frequency_mhz = MetricState::Unsupported;
        let observed = ObservedCpu {
            total: MetricState::Available(CpuUsage::plain(
                Percent::new(12.5).expect("valid percentage"),
            )),
            per_core: vec![MetricState::Available(CpuUsage::plain(Percent::ZERO))],
        };
        let merged = observed.merge_into(baseline);
        assert_eq!(merged.logical_count, 12);
        assert_eq!(merged.physical_count, MetricState::Available(10));
        assert!(merged.frequency_mhz.is_unsupported());
        assert!(merged.total.is_available());
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_tick_rate_is_a_plausible_statistics_clock() {
        let hz = ticks_per_second().expect("macOS always reports a clock rate");
        assert!(
            (10..=10_000).contains(&hz),
            "implausible statistics clock: {hz} Hz"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn the_live_counters_advance_at_about_the_reported_tick_rate() {
        // The check that validates the whole tick-to-Duration conversion: over a
        // measured interval every CPU accrues close to `hz` ticks in total,
        // whatever state it spends them in.
        let hz = ticks_per_second().expect("clock rate");
        let before = read_processor_ticks().expect("host_processor_info");
        let start = Instant::now();
        std::thread::sleep(Duration::from_millis(500));
        let after = read_processor_ticks().expect("host_processor_info");
        let elapsed = start.elapsed().as_secs_f64();

        assert_eq!(before.len(), after.len(), "CPU count changed mid-test");
        let mut total = 0u64;
        for (a, b) in before.iter().zip(after.iter()) {
            for state in 0..CPU_STATES {
                let old = a.states.get(state).copied().unwrap_or(0);
                let new = b.states.get(state).copied().unwrap_or(0);
                total += u64::from(new.wrapping_sub(old));
            }
        }
        let expected = f64::from(hz) * elapsed * before.len() as f64;
        let ratio = total as f64 / expected;
        assert!(
            (0.8..=1.2).contains(&ratio),
            "counters advanced {total} ticks, expected about {expected:.0} at {hz} Hz"
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn live_per_cpu_usage_lands_in_range_on_the_second_sample() {
        let mut tracker = CpuTracker::new();
        let first = read_processor_ticks().expect("read");
        assert!(
            tracker
                .observe(&first, Instant::now())
                .expect("cpus")
                .per_core
                .iter()
                .all(MetricState::is_warming_up)
        );
        std::thread::sleep(Duration::from_millis(300));
        let second = read_processor_ticks().expect("read");
        let observed = tracker.observe(&second, Instant::now()).expect("cpus");

        let total = observed.total.fresh().expect("machine usage");
        assert!((0.0..=100.0).contains(&total.busy.value()));
        for (index, core) in observed.per_core.iter().enumerate() {
            let usage = core
                .fresh()
                .unwrap_or_else(|| panic!("cpu {index}: {core:?}"));
            assert!(
                (0.0..=100.0).contains(&usage.busy.value()),
                "cpu {index} reported {}%",
                usage.busy.value()
            );
            let breakdown = usage.breakdown.fresh().expect("cpu breakdown");
            let sum = breakdown.user.value()
                + breakdown.system.value()
                + breakdown.nice.value()
                + breakdown.idle.value();
            assert!((sum - 100.0).abs() < 0.5, "cpu {index} split sums to {sum}");
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live kernel"]
    fn repeated_reads_release_the_processor_info_array() {
        // Without the Drop guard this loop leaks a few megabytes; the assertion is
        // that two thousand reads still succeed, since a leaked mach allocation
        // eventually fails to allocate.
        for _ in 0..2_000 {
            assert!(!read_processor_ticks().expect("read").is_empty());
        }
    }
}

//! The counter and rate engine: cumulative OS counters turned into validated
//! rates and percentages.
//!
//! Every OS metric that matters in a monitor is a *cumulative* counter — bytes
//! since boot, CPU jiffies since boot, I/O operations since the device appeared.
//! Turning those into "per second" numbers is the single most bug-prone part of a
//! system monitor, so the rules live here once instead of in each collector.
//!
//! # The contract
//!
//! ```text
//! rate = (current_counter - previous_counter) / actual_elapsed_seconds
//! ```
//!
//! and never with an assumed one-second interval: suspend/resume, system load,
//! and scheduler delay all make the real interval variable (§8.1).
//!
//! # What "invalid" means, and what it must never become
//!
//! §8.2 lists the ways a delta goes wrong, and §26 states the consequence in one
//! line: *unavailable is not zero*. Each case has a typed answer, and none of
//! them is a number:
//!
//! | Situation | Result |
//! |---|---|
//! | First reading of a counter | [`WarmingUp`](crate::model::MetricState::WarmingUp) |
//! | Interval of zero length | [`WarmingUp`](crate::model::MetricState::WarmingUp) |
//! | Counter moved backwards, width unknown | [`CounterReset`](crate::model::UnavailableReason::CounterReset) |
//! | Counter moved backwards, consistent with a single wrap of a known width | a real rate, from the wrapped delta |
//! | Key absent and back again | [`DeviceDisappeared`](crate::model::UnavailableReason::DeviceDisappeared) |
//! | Set at its size cap | [`SkippedUnderLoad`](crate::model::UnavailableReason::SkippedUnderLoad) |
//! | Machine-normalized CPU with no CPU count | [`ReadFailed`](crate::model::UnavailableReason::ReadFailed) |
//!
//! A reset always re-baselines on the offending reading, so the sample *after* a
//! reset is a normal measurement rather than a second reset.
//!
//! # Monotonic time is the caller's responsibility
//!
//! Every `observe`/`rate` method takes an [`Instant`](std::time::Instant) and
//! derives the interval itself with `saturating_duration_since`. Callers must
//! pass the snapshot's monotonic `captured_at` and never a value derived from
//! `SystemTime`: §8.1 requires that a wall-clock change cannot make history move
//! backwards or produce a negative rate, and the only way to guarantee that is to
//! keep wall time out of the arithmetic entirely. Because the engine never
//! subtracts wall-clock stamps, a reversed pair of instants degrades to a
//! zero-length interval — warming up — instead of panicking or overflowing.
//!
//! # What lives where
//!
//! * [`CounterTracker`] — one counter, with [`CounterWidth`] deciding wrap from
//!   reset.
//! * [`SystemCpuTracker`] and [`ProcessCpuTracker`] — CPU-time deltas under the
//!   two conventions of §8.3.
//! * [`KeyedTrackers`] — a size-capped set of trackers for devices, interfaces,
//!   and processes, which is what keeps §10.3's no-unbounded-growth rule true as
//!   PIDs churn.

mod counter;
mod cpu;
mod keyed;

pub use counter::{CounterDelta, CounterTracker, CounterWidth};
pub use cpu::{CpuTimeTotals, ProcessCpuTracker, SystemCpuTracker};
pub use keyed::{
    DEFAULT_MAX_TRACKED, DeltaTracker, KeyedProcessCpuTrackers, KeyedRateTrackers, KeyedTrackers,
};

#[cfg(test)]
mod tests {
    use core::time::Duration;
    use std::time::Instant;

    use super::*;
    use crate::model::{CpuNormalization, MetricState, UnavailableReason};
    use crate::units::Percent;

    /// The end-to-end shape of one sampling cycle, as a collector would run it:
    /// a warming-up first pass, then real rates on the second, with the interval
    /// deliberately not one second.
    #[test]
    fn a_full_sampling_cycle_warms_up_then_measures() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(1_400);

        let mut cpu = SystemCpuTracker::new();
        let mut rx: KeyedRateTrackers<&'static str> = KeyedRateTrackers::new(CounterWidth::Bits64);
        let mut process = ProcessCpuTracker::new();

        assert!(
            cpu.observe(CpuTimeTotals::new(Duration::ZERO, Duration::ZERO), t0)
                .is_warming_up()
        );
        assert!(rx.observe("eth0", 0, t0).is_warming_up());
        assert!(process.observe(Duration::ZERO, t0).is_warming_up());

        // 1.4 s later: 1.4 s of busy CPU out of 11.2 s of CPU time on 8 CPUs,
        // 14 000 bytes received, and 700 ms of process CPU.
        let cpu_state = cpu.observe(
            CpuTimeTotals::new(Duration::from_millis(1_400), Duration::from_millis(9_800)),
            t1,
        );
        let rx_state = rx.observe("eth0", 14_000, t1);
        let process_state =
            process.observe_normalized(Duration::from_millis(700), t1, CpuNormalization::Core, 8);

        assert!(
            (cpu_state
                .fresh()
                .copied()
                .map(Percent::value)
                .expect("measured")
                - 12.5)
                .abs()
                < f32::EPSILON
        );
        let rx_per_second = rx_state
            .fresh()
            .map(|rate| rate.per_second())
            .expect("measured");
        assert!(
            (rx_per_second - 10_000.0).abs() < 1e-6,
            "the rate must use the real 1.4 s interval, not an assumed second, got {rx_per_second}"
        );
        assert!(
            (process_state
                .fresh()
                .copied()
                .map(Percent::value)
                .expect("measured")
                - 50.0)
                .abs()
                < f32::EPSILON
        );
    }

    /// Nothing in the engine can hand back a number for an unavailable metric.
    #[test]
    fn no_unavailable_state_in_the_engine_exposes_a_value() {
        let t0 = Instant::now();

        let mut counter = CounterTracker::new(CounterWidth::Unknown);
        counter.rate(1_000_000, t0);
        let reset = counter.rate(1, t0 + Duration::from_secs(1));

        let mut set: KeyedRateTrackers<&'static str> =
            KeyedRateTrackers::new(CounterWidth::Unknown).with_max_tracked(0);
        let skipped = set.observe("eth0", 1, t0);

        let mut process = ProcessCpuTracker::new();
        process.observe(Duration::from_secs(1), t0);
        let denied = process.observe_normalized(
            Duration::from_secs(2),
            t0 + Duration::from_secs(1),
            CpuNormalization::Machine,
            0,
        );

        assert_eq!(reset.fresh(), None);
        assert_eq!(reset.displayable(), None);
        assert_eq!(skipped.fresh(), None);
        assert_eq!(denied.fresh(), None);
        for placeholder in [
            reset.placeholder(),
            skipped.placeholder(),
            denied.placeholder(),
        ] {
            assert!(
                placeholder.is_some(),
                "every unavailable state must explain itself"
            );
        }
    }

    /// The reasons this module publishes are the ones §8.2 names, and each has a
    /// message the UI can render without inventing text.
    #[test]
    fn every_reason_the_engine_publishes_has_a_message() {
        for reason in [
            UnavailableReason::CounterReset,
            UnavailableReason::DeviceDisappeared,
            UnavailableReason::SkippedUnderLoad,
            UnavailableReason::ReadFailed,
        ] {
            let state: MetricState<u64> = MetricState::TemporarilyUnavailable(reason);
            assert_eq!(state.placeholder(), Some(reason.message()));
        }
    }
}

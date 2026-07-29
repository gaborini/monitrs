//! The contract every collector implements.
//!
//! The clock lives in the runtime, not in the collector. A collector is handed a
//! [`SampleTick`] describing *when* it is being asked and *which tiers are due*,
//! and returns one complete snapshot. Three things follow from that shape:
//!
//! * The collector cannot invent an interval, so §8.1's rule against assuming one
//!   second is structural rather than a convention.
//! * Tier scheduling is testable without a real collector, and a collector is
//!   testable without a real clock.
//! * A collector returns a *whole* snapshot, so the UI can never see CPU from one
//!   tick beside memory from another (§10.4).

use core::time::Duration;
use std::time::{Instant, SystemTime};

use monitrs_core::SystemSnapshot;
use monitrs_core::model::{CapabilitySnapshot, ProcessDetailResult, ProcessIdentity};

use crate::error::CollectorError;
use crate::tier::DueTiers;

/// Everything a collector needs to know about the moment it is sampling.
#[derive(Clone, Copy, Debug)]
pub struct SampleTick {
    /// Monotonically increasing snapshot number, starting at 0.
    pub sequence: u64,
    /// Monotonic capture time. The basis for all rate arithmetic (§8.1).
    pub captured_at: Instant,
    /// Wall-clock capture time. For display and export only.
    pub wall_time: SystemTime,
    /// The **measured** interval since the previous fast collection.
    ///
    /// [`Duration::ZERO`] on the first tick, which is what makes the first
    /// snapshot warming up rather than zero (§8.2, §26).
    pub elapsed: Duration,
    /// Which tiers to refresh. Data groups outside this set must be carried over
    /// from the previous snapshot, not re-read (§9.1: never an all-fields refresh).
    pub due: DueTiers,
}

impl SampleTick {
    /// The first tick: every tier due, no elapsed interval.
    #[must_use]
    pub fn first(captured_at: Instant, wall_time: SystemTime) -> Self {
        Self {
            sequence: 0,
            captured_at,
            wall_time,
            elapsed: Duration::ZERO,
            due: DueTiers::ALL,
        }
    }

    /// Whether rates can be computed from this tick.
    ///
    /// False on the first tick and on any tick where the clock did not advance,
    /// both of which must yield [`monitrs_core::MetricState::WarmingUp`] rather than a
    /// division by zero (§8.2).
    #[must_use]
    pub const fn can_compute_rates(&self) -> bool {
        self.sequence > 0 && !self.elapsed.is_zero()
    }

    /// The next tick after this one, measuring the real gap to `captured_at`.
    #[must_use]
    pub fn advance(&self, captured_at: Instant, wall_time: SystemTime, due: DueTiers) -> Self {
        Self {
            sequence: self.sequence.saturating_add(1),
            captured_at,
            wall_time,
            elapsed: captured_at.saturating_duration_since(self.captured_at),
            due,
        }
    }
}

/// A source of system snapshots.
///
/// Implemented by the live platform collector and by
/// [`crate::fake::FakeCollector`]. Everything downstream — the reducer, the
/// history ring, every UI test — is written against this trait, so no test needs
/// a real machine in a particular state.
pub trait SnapshotSource: Send {
    /// A short name for health reporting and error messages.
    fn name(&self) -> &'static str;

    /// What this source can and cannot report on this machine.
    ///
    /// Probed once and then cheap to call: the UI consults it every frame to
    /// decide whether to reserve space for an optional panel (§4).
    fn capabilities(&self) -> CapabilitySnapshot;

    /// Produces one complete, internally consistent snapshot.
    ///
    /// A metric that could not be read is **not** an error: it is reported as the
    /// appropriate [`monitrs_core::MetricState`] on the affected field. An `Err` here
    /// means a whole data group failed, and even then the runtime publishes a
    /// snapshot with those fields marked unavailable rather than showing nothing.
    fn sample(&mut self, tick: &SampleTick) -> Result<SystemSnapshot, CollectorError>;

    /// Collects the expensive per-process detail for one process (§8.6).
    ///
    /// Called from a dedicated worker thread, never from the sampler, because a
    /// slow detail read must not delay regular sampling (§10.3). Returns
    /// [`ProcessDetailResult::Vanished`] or `Reused` rather than an error when the
    /// process is gone — both are expected during normal operation (§14.1).
    fn process_detail(&mut self, identity: ProcessIdentity) -> ProcessDetailResult;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_tick_cannot_compute_rates() {
        let tick = SampleTick::first(Instant::now(), SystemTime::UNIX_EPOCH);
        assert_eq!(tick.sequence, 0);
        assert_eq!(tick.elapsed, Duration::ZERO);
        assert!(!tick.can_compute_rates());
        assert_eq!(tick.due, DueTiers::ALL, "the first frame needs every tier");
    }

    #[test]
    fn advancing_measures_the_real_gap_rather_than_assuming_an_interval() {
        let start = Instant::now();
        let first = SampleTick::first(start, SystemTime::UNIX_EPOCH);

        // The scheduler asked for 1s; the OS delivered 1.4s.
        let late = start + Duration::from_millis(1_400);
        let second = first.advance(late, SystemTime::UNIX_EPOCH, DueTiers::ALL);

        assert_eq!(second.sequence, 1);
        assert_eq!(second.elapsed, Duration::from_millis(1_400));
        assert!(second.can_compute_rates());
    }

    #[test]
    fn a_tick_with_no_elapsed_time_cannot_compute_rates_even_at_a_later_sequence() {
        // Two collections inside the same clock granule, or a forced refresh.
        let now = Instant::now();
        let first = SampleTick::first(now, SystemTime::UNIX_EPOCH);
        let immediate = first.advance(now, SystemTime::UNIX_EPOCH, DueTiers::ALL);

        assert_eq!(immediate.sequence, 1);
        assert_eq!(immediate.elapsed, Duration::ZERO);
        assert!(
            !immediate.can_compute_rates(),
            "a zero interval must warm up, not divide by zero"
        );
    }

    #[test]
    fn sequence_numbers_are_monotonic_across_many_ticks() {
        let start = Instant::now();
        let mut tick = SampleTick::first(start, SystemTime::UNIX_EPOCH);
        for expected in 1..=100u64 {
            let now = start + Duration::from_secs(expected);
            tick = tick.advance(now, SystemTime::UNIX_EPOCH, DueTiers::ALL);
            assert_eq!(tick.sequence, expected);
            assert_eq!(tick.elapsed, Duration::from_secs(1));
        }
    }
}

//! A bounded, keyed set of delta trackers: one per disk, interface, or process.
//!
//! Two failure modes motivate this file, and both are named in the
//! specification:
//!
//! * A device, interface, or PID that disappears and comes back must not produce
//!   a delta spanning the gap (§8.2). The set forgets a key on request and can
//!   also notice a suspicious gap on its own.
//! * A tracker map keyed on processes grows forever as PIDs churn, which §10.3
//!   forbids. The set has a hard size cap, explicit removal, and idle pruning.

use core::fmt;
use core::hash::Hash;
use core::time::Duration;
use std::collections::HashMap;
use std::time::Instant;

use crate::model::{MetricState, ProcessIdentity, UnavailableReason};
use crate::rates::counter::CounterTracker;
use crate::rates::cpu::ProcessCpuTracker;

/// Default cap on the number of keys a single set will track.
///
/// §10.3 forbids unbounded growth, and the process-keyed set is the dangerous
/// case: without a cap, every short-lived PID leaves a baseline behind forever.
/// 16 384 comfortably exceeds the 10 000-process high-load case in §16.2 while
/// bounding the set to well under a mebibyte.
pub const DEFAULT_MAX_TRACKED: usize = 16_384;

/// A single-key delta tracker that a [`KeyedTrackers`] set can manage.
///
/// The trait exists so the bounded-growth, gap-detection, and re-baselining
/// rules are written once and shared by counter rates and per-process CPU rather
/// than duplicated per metric. Every observation yields a
/// [`MetricState`], which is what lets the set report a re-appearance after a gap
/// without knowing what kind of value the tracker produces.
pub trait DeltaTracker {
    /// Construction parameters shared by every tracker in one set.
    ///
    /// `Copy` so the set can hand a fresh copy to each new tracker without
    /// allocating, and `Debug` so the set itself can derive `Debug`.
    type Config: Copy + fmt::Debug;
    /// The cumulative reading folded in on each observation.
    type Reading;
    /// The value published when an observation succeeds.
    type Value;

    /// Builds a tracker with no baseline yet.
    fn with_config(config: Self::Config) -> Self;

    /// Folds one cumulative reading in, at monotonic time `at`.
    fn observe_reading(&mut self, reading: Self::Reading, at: Instant) -> MetricState<Self::Value>;

    /// When this tracker last accepted a reading, or `None` while warming up.
    fn last_observed_at(&self) -> Option<Instant>;

    /// Drops the baseline so the next reading warms up instead of producing a
    /// delta across a gap (§8.2).
    fn forget_baseline(&mut self);
}

/// A keyed set of delta trackers with a hard bound on its size.
///
/// # Per-cycle usage
///
/// A collector observes every key the OS still reports, then drops the rest:
///
/// ```
/// use core::time::Duration;
/// use std::time::Instant;
///
/// use monitrs_core::rates::{CounterWidth, KeyedRateTrackers};
///
/// let mut rx: KeyedRateTrackers<String> = KeyedRateTrackers::new(CounterWidth::Bits64);
/// let t0 = Instant::now();
///
/// // First cycle: two interfaces, both warming up.
/// assert!(rx.observe("eth0".to_owned(), 1_000, t0).is_warming_up());
/// assert!(rx.observe("wlan0".to_owned(), 500, t0).is_warming_up());
///
/// // Second cycle: wlan0 is gone, so it is dropped rather than left to accrue.
/// let t1 = t0 + Duration::from_secs(1);
/// let eth0 = rx.observe("eth0".to_owned(), 3_000, t1);
/// rx.retain(|name| name == "eth0");
///
/// assert_eq!(eth0.fresh().map(|rate| rate.per_second()), Some(2_000.0));
/// assert_eq!(rx.len(), 1);
///
/// // wlan0 comes back with a counter that restarted: it re-baselines instead of
/// // reporting the whole counter as one second of traffic.
/// let t2 = t1 + Duration::from_secs(1);
/// assert!(rx.observe("wlan0".to_owned(), 90_000, t2).is_warming_up());
/// ```
#[derive(Debug)]
pub struct KeyedTrackers<K, T: DeltaTracker> {
    config: T::Config,
    max_tracked: usize,
    max_gap: Option<Duration>,
    evictions: u64,
    entries: HashMap<K, T>,
}

impl<K, T> KeyedTrackers<K, T>
where
    K: Clone + Eq + Hash,
    T: DeltaTracker,
{
    /// Builds an empty set with [`DEFAULT_MAX_TRACKED`] and no gap guard.
    #[must_use]
    pub fn new(config: T::Config) -> Self {
        Self {
            config,
            max_tracked: DEFAULT_MAX_TRACKED,
            max_gap: None,
            evictions: 0,
            entries: HashMap::new(),
        }
    }

    /// Overrides the hard size cap (§10.3).
    ///
    /// A cap of zero tracks nothing and reports every key as skipped, which is a
    /// branch-free way to disable an expensive metric under load (§16.2).
    #[must_use]
    pub fn with_max_tracked(mut self, max_tracked: usize) -> Self {
        self.max_tracked = max_tracked;
        self
    }

    /// Treats a gap longer than `max_gap` between two readings of one key as the
    /// key having disappeared and come back (§8.2).
    ///
    /// This is a safety net, not the primary mechanism: a collector that calls
    /// [`KeyedTrackers::retain`] or [`KeyedTrackers::forget`] each cycle never
    /// needs it. Set it to a small multiple of the sampling interval so ordinary
    /// jitter does not trip it, and remember that suspend/resume looks exactly
    /// like a disappearance from in here — reporting it as one is the honest
    /// answer, because the counter advanced during a period this sample cannot
    /// account for.
    #[must_use]
    pub fn with_max_gap(mut self, max_gap: Duration) -> Self {
        self.max_gap = Some(max_gap);
        self
    }

    /// Folds one reading for `key` in and publishes the result.
    ///
    /// A key seen for the first time warms up rather than reporting zero (§8.2).
    /// `at` must be monotonic.
    pub fn observe(&mut self, key: K, reading: T::Reading, at: Instant) -> MetricState<T::Value> {
        if let Some(tracker) = self.entries.get_mut(&key) {
            let gapped = match (self.max_gap, DeltaTracker::last_observed_at(tracker)) {
                (Some(max_gap), Some(previous)) => at.saturating_duration_since(previous) > max_gap,
                _ => false,
            };
            if !gapped {
                return tracker.observe_reading(reading, at);
            }
            // The key went unobserved for longer than sampling allows, so it was
            // absent. Fold the reading in *after* dropping the baseline so this
            // sample is honest and the next one is valid (§8.2).
            tracker.forget_baseline();
            let _ = tracker.observe_reading(reading, at);
            return MetricState::TemporarilyUnavailable(UnavailableReason::DeviceDisappeared);
        }

        if self.entries.len() >= self.max_tracked && !self.evict_oldest() {
            // Only reachable with a cap of zero: the caller has switched this
            // metric off. Saying so beats reporting zero (§4).
            return MetricState::TemporarilyUnavailable(UnavailableReason::SkippedUnderLoad);
        }
        let config = self.config;
        self.entries
            .entry(key)
            .or_insert_with(|| T::with_config(config))
            .observe_reading(reading, at)
    }

    /// Drops `key` entirely, so a later re-appearance re-baselines.
    ///
    /// This is the explicit answer to every identity change §8.2 lists: a device
    /// that vanished, a renamed interface, an exited PID. Returns whether the key
    /// was being tracked. The caller publishes the matching
    /// [`UnavailableReason`] — `DeviceDisappeared`, `InterfaceRenamed`, or
    /// `ProcessExited` — for the sample in which it noticed.
    pub fn forget(&mut self, key: &K) -> bool {
        self.entries.remove(key).is_some()
    }

    /// Keeps only the keys `keep` accepts, returning how many were dropped.
    ///
    /// The cheap per-cycle way to stay bounded: call it with the set of keys the
    /// OS still reports. Deliberately not counted as an eviction, because it is
    /// the caller acting on knowledge rather than the set defending its budget.
    pub fn retain(&mut self, mut keep: impl FnMut(&K) -> bool) -> usize {
        let before = self.entries.len();
        self.entries.retain(|key, _| keep(key));
        before.saturating_sub(self.entries.len())
    }

    /// Drops trackers that have not seen a reading within `max_idle`.
    ///
    /// The backstop for PID churn: a process that exits is never observed again,
    /// so it ages out even if the collector never says it is gone (§10.3). A
    /// tracker that never completed a reading holds no baseline worth keeping and
    /// is dropped too.
    pub fn prune_idle(&mut self, now: Instant, max_idle: Duration) -> usize {
        let before = self.entries.len();
        self.entries.retain(|_, tracker| {
            DeltaTracker::last_observed_at(tracker)
                .is_some_and(|at| now.saturating_duration_since(at) <= max_idle)
        });
        let dropped = before.saturating_sub(self.entries.len());
        self.evictions = self
            .evictions
            .saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
        dropped
    }

    /// Drops every tracker.
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// How many keys are currently tracked.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is currently tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether `key` currently has a tracker.
    #[must_use]
    pub fn contains_key(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }

    /// The tracker for `key`, for callers that need its raw baseline.
    #[must_use]
    pub fn tracker(&self, key: &K) -> Option<&T> {
        self.entries.get(key)
    }

    /// The hard size cap this set enforces.
    #[must_use]
    pub const fn max_tracked(&self) -> usize {
        self.max_tracked
    }

    /// How many trackers this set has dropped to stay inside its budget.
    ///
    /// Worth surfacing through [`crate::model::CollectorHealth`]: a non-zero and
    /// rising count means the cap is too low for the workload, and rates for the
    /// churning keys are being restarted rather than measured.
    #[must_use]
    pub const fn evictions(&self) -> u64 {
        self.evictions
    }

    /// Drops the least-recently-observed tracker, returning whether one went.
    ///
    /// Linear in the number of keys, but only ever called on insertion while at
    /// the cap — the situation where the set is already refusing to grow.
    /// Trackers that never completed a reading sort first (`None` before `Some`)
    /// because they hold no baseline to lose.
    fn evict_oldest(&mut self) -> bool {
        let victim = self
            .entries
            .iter()
            .min_by_key(|(_, tracker)| DeltaTracker::last_observed_at(*tracker))
            .map(|(key, _)| key.clone());
        let Some(key) = victim else {
            return false;
        };
        self.entries.remove(&key);
        self.evictions = self.evictions.saturating_add(1);
        true
    }
}

impl<K, T> Default for KeyedTrackers<K, T>
where
    K: Clone + Eq + Hash,
    T: DeltaTracker,
    T::Config: Default,
{
    fn default() -> Self {
        Self::new(T::Config::default())
    }
}

/// A keyed set of cumulative-counter rate trackers: one per disk, interface, or
/// mount point.
///
/// Every tracker in the set shares one [`CounterWidth`](super::CounterWidth),
/// which is correct because a set holds counters of one kind from one source.
pub type KeyedRateTrackers<K> = KeyedTrackers<K, CounterTracker>;

/// A keyed set of per-process CPU trackers.
///
/// Keyed on [`ProcessIdentity`] rather than a bare PID, so a reused PID gets a
/// fresh baseline instead of inheriting the dead process's CPU time (§26). That
/// choice is baked into the alias precisely so it cannot be got wrong at a call
/// site.
pub type KeyedProcessCpuTrackers = KeyedTrackers<ProcessIdentity, ProcessCpuTracker>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rates::counter::CounterWidth;
    use crate::rates::cpu::{CpuTimeTotals, SystemCpuTracker};
    use crate::units::{Percent, Rate};

    fn origin() -> Instant {
        Instant::now()
    }

    fn secs(seconds: u64) -> Duration {
        Duration::from_secs(seconds)
    }

    /// See the note on `assert_rate` in `counter.rs`: an exact float comparison
    /// is denied by `clippy::float_cmp` and unreliable for a derived rate.
    fn assert_rate(state: &MetricState<Rate>, expected: f64) {
        let actual = state
            .fresh()
            .expect("expected a measured rate")
            .per_second();
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}/s, got {actual}/s"
        );
    }

    fn bytes() -> KeyedRateTrackers<&'static str> {
        KeyedRateTrackers::new(CounterWidth::Bits64)
    }

    #[test]
    fn an_unseen_key_warms_up_instead_of_reporting_zero() {
        let mut set = bytes();
        let state = set.observe("eth0", 4_096, origin());
        assert!(state.is_warming_up());
        assert_eq!(set.len(), 1);
        assert!(set.contains_key(&"eth0"));
    }

    #[test]
    fn a_second_reading_for_a_key_yields_a_rate_over_the_real_interval() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("eth0", 1_000, t0);
        let state = set.observe("eth0", 2_000, t0 + Duration::from_millis(500));
        assert_rate(&state, 2_000.0);
    }

    #[test]
    fn keys_keep_independent_baselines() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("eth0", 0, t0);
        set.observe("wlan0", 1_000_000, t0);

        let eth0 = set.observe("eth0", 100, t0 + secs(1));
        let wlan0 = set.observe("wlan0", 1_000_300, t0 + secs(1));
        assert_rate(&eth0, 100.0);
        assert_rate(&wlan0, 300.0);
    }

    #[test]
    fn a_forgotten_key_rebaselines_when_it_reappears() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("sdb", 900_000, t0);
        assert!(set.forget(&"sdb"));
        assert!(!set.forget(&"sdb"), "forgetting twice is not an error");
        assert!(set.is_empty());

        // The device came back with a counter that restarted from zero. Without
        // the re-baseline this would announce 40 kB/s that never happened.
        let state = set.observe("sdb", 40_000, t0 + secs(1));
        assert!(state.is_warming_up());
    }

    #[test]
    fn retain_drops_absent_keys_so_a_reappearance_cannot_produce_a_bogus_delta() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("eth0", 1_000, t0);
        set.observe("tun0", 5_000, t0);

        // tun0 went away; the interface list no longer contains it.
        assert_eq!(set.retain(|key| *key == "eth0"), 1);
        assert_eq!(set.len(), 1);
        assert_eq!(set.evictions(), 0, "deliberate removal is not an eviction");

        // It returns much later with a much larger counter.
        let state = set.observe("tun0", 9_000_000, t0 + secs(300));
        assert!(state.is_warming_up());
        let recovered = set.observe("tun0", 9_000_100, t0 + secs(301));
        assert_rate(&recovered, 100.0);
    }

    #[test]
    fn a_gap_longer_than_the_guard_is_reported_as_a_disappearance() {
        let t0 = origin();
        let mut set = bytes().with_max_gap(secs(3));
        set.observe("eth0", 1_000, t0);

        // Ten seconds of silence: the caller never said the interface went away,
        // but a delta across that gap would be attributed to this one sample.
        let gapped = set.observe("eth0", 9_000_000, t0 + secs(10));
        assert_eq!(
            gapped,
            MetricState::TemporarilyUnavailable(UnavailableReason::DeviceDisappeared)
        );

        // Re-baselined on the gapped reading, so the next sample is valid.
        let recovered = set.observe("eth0", 9_000_500, t0 + secs(11));
        assert_rate(&recovered, 500.0);
    }

    #[test]
    fn a_gap_inside_the_guard_is_an_ordinary_sample() {
        let t0 = origin();
        let mut set = bytes().with_max_gap(secs(3));
        set.observe("eth0", 1_000, t0);
        let state = set.observe("eth0", 3_000, t0 + secs(2));
        assert_rate(&state, 1_000.0);
    }

    #[test]
    fn without_a_guard_no_gap_is_ever_treated_as_a_disappearance() {
        // The guard is opt-in: the default set trusts the caller's own removal.
        let t0 = origin();
        let mut set = bytes();
        set.observe("eth0", 1_000, t0);
        let state = set.observe("eth0", 3_000, t0 + secs(600));
        assert!(state.is_available());
    }

    #[test]
    fn the_set_never_grows_past_its_cap_as_keys_churn() {
        // §10.3: a process-keyed map must not grow without bound. Ten thousand
        // distinct short-lived keys must leave the set at its cap, not at 10 000.
        let t0 = origin();
        let mut set: KeyedRateTrackers<u64> =
            KeyedRateTrackers::new(CounterWidth::Bits64).with_max_tracked(64);
        for pid in 0..10_000u64 {
            set.observe(pid, pid, t0 + Duration::from_millis(pid));
        }
        assert_eq!(set.max_tracked(), 64);
        assert!(set.len() <= 64, "len was {}", set.len());
        assert!(set.evictions() > 0, "eviction must be reported");
    }

    #[test]
    fn the_least_recently_observed_key_is_evicted_first() {
        let t0 = origin();
        let mut set = bytes().with_max_tracked(2);
        set.observe("oldest", 1, t0);
        set.observe("newer", 1, t0 + secs(5));

        set.observe("newest", 1, t0 + secs(10));
        assert!(!set.contains_key(&"oldest"));
        assert!(set.contains_key(&"newer"));
        assert!(set.contains_key(&"newest"));
        assert_eq!(set.evictions(), 1);
    }

    #[test]
    fn a_zero_cap_reports_skipped_rather_than_zero() {
        let mut set = bytes().with_max_tracked(0);
        let state = set.observe("eth0", 1_000, origin());
        assert_eq!(
            state,
            MetricState::TemporarilyUnavailable(UnavailableReason::SkippedUnderLoad)
        );
        assert!(set.is_empty());
    }

    #[test]
    fn prune_idle_drops_keys_that_stopped_reporting() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("alive", 0, t0);
        set.observe("exited", 0, t0);

        // "alive" keeps reporting; "exited" does not.
        set.observe("alive", 100, t0 + secs(30));
        assert_eq!(set.prune_idle(t0 + secs(30), secs(5)), 1);
        assert!(set.contains_key(&"alive"));
        assert!(!set.contains_key(&"exited"));
        assert_eq!(set.evictions(), 1);
    }

    #[test]
    fn pruning_an_empty_set_is_a_no_op() {
        let mut set = bytes();
        assert_eq!(set.prune_idle(origin(), secs(1)), 0);
        assert!(set.is_empty());
    }

    #[test]
    fn pruning_keeps_a_key_observed_exactly_at_the_idle_limit() {
        // The boundary matters: at a 1 s interval and a 1 s limit, an on-time key
        // must survive or every rate restarts every cycle.
        let t0 = origin();
        let mut set = bytes();
        set.observe("eth0", 0, t0);
        assert_eq!(set.prune_idle(t0 + secs(1), secs(1)), 0);
        assert!(set.contains_key(&"eth0"));
        assert_eq!(
            set.prune_idle(t0 + secs(1) + Duration::from_nanos(1), secs(1)),
            1
        );
    }

    #[test]
    fn clearing_drops_every_baseline() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("a", 1, t0);
        set.observe("b", 1, t0);
        set.clear();
        assert!(set.is_empty());
        assert!(set.observe("a", 1_000_000, t0 + secs(1)).is_warming_up());
    }

    #[test]
    fn the_tracker_behind_a_key_is_inspectable() {
        let t0 = origin();
        let mut set = bytes();
        set.observe("eth0", 4_096, t0);
        let tracker = set.tracker(&"eth0").expect("tracked");
        assert_eq!(tracker.last_value(), Some(4_096));
        assert_eq!(tracker.width(), CounterWidth::Bits64);
        assert!(set.tracker(&"missing").is_none());
    }

    #[test]
    fn a_default_set_uses_the_default_counter_width_and_cap() {
        let set: KeyedRateTrackers<&'static str> = KeyedRateTrackers::default();
        assert_eq!(set.max_tracked(), DEFAULT_MAX_TRACKED);
        assert!(set.is_empty());
    }

    #[test]
    fn a_known_width_wrap_still_works_through_the_keyed_set() {
        let t0 = origin();
        let mut set: KeyedRateTrackers<&'static str> = KeyedRateTrackers::new(CounterWidth::Bits32);
        let previous = u64::from(u32::MAX) - 99;
        set.observe("eth0", previous, t0);
        let state = set.observe("eth0", 400, t0 + secs(1));
        assert_rate(&state, 500.0);
    }

    #[test]
    fn process_cpu_trackers_are_keyed_on_identity_so_a_reused_pid_rebaselines() {
        // §26 and rule 4: a PID alone is not an identity. The recycled PID must
        // not inherit 30 s of CPU time from the process that exited.
        let t0 = origin();
        let original = ProcessIdentity::new(4_242, 900_100);
        let recycled = ProcessIdentity::new(4_242, 977_400);

        let mut set = KeyedProcessCpuTrackers::default();
        set.observe(original, secs(30), t0);
        let measured = set.observe(original, secs(31), t0 + secs(1));
        assert!(
            (measured
                .fresh()
                .copied()
                .map(Percent::value)
                .expect("measured")
                - 100.0)
                .abs()
                < f32::EPSILON
        );

        let reused = set.observe(recycled, Duration::from_millis(10), t0 + secs(2));
        assert!(
            reused.is_warming_up(),
            "a recycled PID must warm up, not report a negative or reset delta"
        );
        assert_eq!(set.len(), 2, "the two identities are distinct keys");
    }

    #[test]
    fn exited_processes_are_prunable_so_the_set_stays_bounded() {
        let t0 = origin();
        let mut set = KeyedProcessCpuTrackers::default();
        for pid in 0..500u32 {
            set.observe(ProcessIdentity::new(pid, 1), Duration::ZERO, t0);
        }
        let survivor = ProcessIdentity::new(1, 1);
        set.observe(survivor, Duration::from_millis(1), t0 + secs(10));

        assert_eq!(set.prune_idle(t0 + secs(10), secs(2)), 499);
        assert_eq!(set.len(), 1);
        assert!(set.contains_key(&survivor));
    }

    #[test]
    fn per_core_cpu_trackers_share_the_set_without_a_counter_width() {
        // The `()` configuration path: per-core trackers keyed on the core index,
        // which change when a CPU is hotplugged.
        let t0 = origin();
        let mut set: KeyedTrackers<u16, SystemCpuTracker> = KeyedTrackers::default();
        for core in 0..4u16 {
            assert!(
                set.observe(core, CpuTimeTotals::new(secs(0), secs(0)), t0)
                    .is_warming_up()
            );
        }
        let busy = set.observe(0, CpuTimeTotals::new(secs(1), secs(3)), t0 + secs(4));
        assert!(
            (busy.fresh().copied().map(Percent::value).expect("measured") - 25.0).abs()
                < f32::EPSILON
        );
        assert_eq!(set.len(), 4);
    }
}

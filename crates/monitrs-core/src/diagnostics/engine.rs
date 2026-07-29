//! The Pressure Radar engine (§2.3).
//!
//! # Ownership boundary
//!
//! Collectors **do not** derive pressure. Every collector in this workspace emits
//! [`PressureSnapshot::warming_up`] and, on Linux, fills in the raw
//! [`PsiSnapshot`](crate::model::PsiSnapshot) — a measurement. Deciding that 84%
//! busy for eleven of the last fifteen samples means `watch` is policy, and policy
//! belongs to one place so that two platforms cannot disagree about it. The runtime
//! therefore runs this engine over each published snapshot and replaces
//! `snapshot.pressure` with the result:
//!
//! ```text
//! collector.sample()      -> SystemSnapshot { pressure: warming_up, psi: raw }
//! ring.record(&snapshot)  -> history gains the sample
//! engine.observe(&snapshot) -> PressureSnapshot { signals: derived, psi: carried }
//! rules.evaluate(...)     -> findings, which may read the derived signals
//! ```
//!
//! The raw PSI figures are carried through untouched: they are the collector's
//! measurement, and the engine has no business rewriting them.
//!
//! # Why an unmeasurable signal is not a healthy one
//!
//! Each signal's state is derived from its own input. When that input is
//! unavailable the signal reports the unavailability, never `normal`, and the
//! hysteresis state behind it is discarded so the samples either side of the gap
//! cannot be stitched into a sustained condition (§11.3).

use core::time::Duration;

use crate::model::{
    MetricState, PressureId, PressureSignal, PressureSnapshot, PressureState, SystemSnapshot,
    UnavailableReason,
};

use super::{Hysteresis, SignalReading, Thresholds, signals};

/// The rule text used for every signal while diagnostics are switched off.
const DISABLED_RULE: &str = "diagnostics are disabled in configuration";

/// Derives the Pressure Radar from a stream of snapshots (§2.3).
///
/// Stateful on purpose: hysteresis is memory, and §11.3 requires it. The state is
/// bounded — one fixed-length observation window per signal — so a twelve-hour run
/// occupies exactly as much as the first minute (§16.1).
#[derive(Clone, Debug)]
pub struct PressureEngine {
    thresholds: Thresholds,
    /// One tracker per [`PressureId::DISPLAY_ORDER`] entry, in that order.
    trackers: Vec<Hysteresis>,
    /// The smallest non-zero interval seen so far, used as the reference for
    /// detecting a sleep/wake gap.
    ///
    /// The *smallest* rather than the latest, because a stall must not be able to
    /// inflate the reference and thereby hide the next stall (§8.1).
    reference_interval: Option<Duration>,
    observations: u64,
    discontinuities: u64,
}

impl PressureEngine {
    /// Builds an engine from configuration, sanitizing it first.
    #[must_use]
    pub fn new(thresholds: Thresholds) -> Self {
        let thresholds = thresholds.sanitized();
        Self {
            trackers: PressureId::DISPLAY_ORDER
                .iter()
                .map(|_| Hysteresis::new(&thresholds))
                .collect(),
            thresholds,
            reference_interval: None,
            observations: 0,
            discontinuities: 0,
        }
    }

    /// The sanitized thresholds in effect.
    #[must_use]
    pub const fn thresholds(&self) -> &Thresholds {
        &self.thresholds
    }

    /// Replaces the thresholds, discarding hysteresis state.
    ///
    /// §12 makes configuration reload atomic. The observations already collected
    /// were judged against the *old* thresholds, so keeping them would let a
    /// reload produce a state that neither configuration justifies; a reset is the
    /// honest answer and costs one warm-up period.
    pub fn set_thresholds(&mut self, thresholds: Thresholds) {
        self.thresholds = thresholds.sanitized();
        self.trackers = PressureId::DISPLAY_ORDER
            .iter()
            .map(|_| Hysteresis::new(&self.thresholds))
            .collect();
        self.observations = 0;
    }

    /// Discards every signal's hysteresis state (§11.3).
    ///
    /// The runtime calls this after anything that breaks the continuity of the
    /// measurement stream and that the engine cannot see for itself — a collector
    /// restart, for instance. Sleep/wake gaps are detected automatically by
    /// [`Self::observe`].
    pub fn reset(&mut self) {
        for tracker in &mut self.trackers {
            tracker.reset();
        }
        self.observations = 0;
    }

    /// How many snapshots have been folded into the current state.
    #[must_use]
    pub const fn observations(&self) -> u64 {
        self.observations
    }

    /// How many measurement discontinuities have been absorbed (§11.3).
    ///
    /// Surfaced on the Inspect screen so a user who closed their laptop can see
    /// why the radar went back to warming up (§7.5).
    #[must_use]
    pub const fn discontinuities(&self) -> u64 {
        self.discontinuities
    }

    /// Folds one snapshot in and returns the radar the UI should render.
    ///
    /// The returned snapshot's `psi` is the one from `snapshot`: raw PSI is the
    /// collector's measurement and is passed through unchanged.
    #[must_use]
    pub fn observe(&mut self, snapshot: &SystemSnapshot) -> PressureSnapshot {
        if !self.thresholds.enabled {
            return PressureSnapshot {
                signals: PressureId::DISPLAY_ORDER
                    .iter()
                    .map(|&id| PressureSignal::unsupported(id, DISABLED_RULE))
                    .collect(),
                psi: snapshot.pressure.psi,
            };
        }

        // §8.2: without a measured interval there is nothing to sustain, and the
        // metrics themselves are warming up anyway. Deliberately does not touch
        // the trackers: a re-delivered snapshot is not an observation.
        if !snapshot.has_valid_interval() {
            return self.warming_up_snapshot(snapshot);
        }

        if self.is_discontinuity(snapshot.elapsed) {
            self.discontinuities = self.discontinuities.saturating_add(1);
            self.reset();
        }
        self.reference_interval = Some(match self.reference_interval {
            Some(reference) => reference.min(snapshot.elapsed),
            None => snapshot.elapsed,
        });
        self.observations = self.observations.saturating_add(1);

        let signals = PressureId::DISPLAY_ORDER
            .iter()
            .map(|&id| self.signal(id, snapshot))
            .collect();
        PressureSnapshot {
            signals,
            psi: snapshot.pressure.psi,
        }
    }

    /// Whether `elapsed` is so much larger than the reference interval that it
    /// must be a sleep/wake gap rather than a measurement (§11.3).
    fn is_discontinuity(&self, elapsed: Duration) -> bool {
        let Some(reference) = self.reference_interval else {
            return false;
        };
        let limit =
            Thresholds::intervals_as_seconds(reference, self.thresholds.discontinuity_intervals);
        elapsed.as_secs_f64() > limit
    }

    /// Derives one signal, feeding or resetting its tracker as appropriate.
    fn signal(&mut self, id: PressureId, snapshot: &SystemSnapshot) -> PressureSignal {
        let reading = signals::read(id, snapshot, &self.thresholds);
        let slot = Self::slot(id);
        let Some(tracker) = self.trackers.get_mut(slot) else {
            // Unreachable: one tracker exists per display-order entry. Reporting
            // the reading without hysteresis is still honest, and §14.3 forbids
            // panicking here.
            return Self::signal_from(id, &reading, MetricState::WarmingUp, None);
        };

        let Some(&candidate) = reading.state.fresh() else {
            // §11.3: an unavailable input is not an event. Drop the window so the
            // samples either side of the gap cannot be counted together.
            tracker.reset();
            return Self::signal_from(id, &reading, reading.state, None);
        };

        let state = tracker.observe(candidate, snapshot.elapsed);
        let held_for = tracker.held_for();
        Self::signal_from(id, &reading, state, held_for)
    }

    /// Assembles a signal, keeping severity consistent with the resolved state.
    ///
    /// The raw metric is preserved whatever the state, because §2.3 requires the
    /// raw metric to be shown; the normalized severity is only reported alongside
    /// an actual state, so the UI never draws a full bar under the word
    /// "warming up".
    fn signal_from(
        id: PressureId,
        reading: &SignalReading,
        state: MetricState<PressureState>,
        held_for: Option<Duration>,
    ) -> PressureSignal {
        let severity = match state {
            MetricState::Available(_) => reading.severity,
            MetricState::WarmingUp => MetricState::WarmingUp,
            MetricState::PermissionDenied => MetricState::PermissionDenied,
            MetricState::Unsupported => MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(reason) => {
                MetricState::TemporarilyUnavailable(reason)
            }
            // The engine never derives a stale state: a state is a conclusion, and
            // a conclusion drawn from a retained value is not current (§4).
            MetricState::Stale { .. } => {
                MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample)
            }
        };
        PressureSignal {
            id,
            state,
            severity,
            raw: reading.raw,
            rule: reading.rule,
            held_for,
        }
    }

    /// The radar for a snapshot that cannot yet support any state.
    fn warming_up_snapshot(&self, snapshot: &SystemSnapshot) -> PressureSnapshot {
        PressureSnapshot {
            signals: PressureId::DISPLAY_ORDER
                .iter()
                .map(|&id| {
                    let reading = signals::read(id, snapshot, &self.thresholds);
                    // Keep an unavailability that is *stronger* than warming up:
                    // "this platform has no PSI" stays true on the first tick.
                    let state = match reading.state {
                        MetricState::Available(_) | MetricState::Stale { .. } => {
                            MetricState::WarmingUp
                        }
                        other => other,
                    };
                    Self::signal_from(id, &reading, state, None)
                })
                .collect(),
            psi: snapshot.pressure.psi,
        }
    }

    /// The tracker slot for a signal id.
    fn slot(id: PressureId) -> usize {
        PressureId::DISPLAY_ORDER
            .iter()
            .position(|candidate| *candidate == id)
            .unwrap_or(0)
    }
}

impl Default for PressureEngine {
    /// An engine with the §12 default thresholds.
    fn default() -> Self {
        Self::new(Thresholds::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{
        Timeline, set_cpu, set_disk_busy, set_memory, set_psi, snapshot,
    };
    use crate::model::UnavailableReason;

    const TOTAL: u64 = 32 * 1024 * 1024 * 1024;

    fn engine() -> PressureEngine {
        PressureEngine::default()
    }

    fn cpu_state(radar: &PressureSnapshot) -> MetricState<PressureState> {
        radar
            .signal(PressureId::Cpu)
            .map_or(MetricState::Unsupported, |signal| signal.state)
    }

    /// Feeds `count` snapshots with a constant CPU busy percentage.
    fn feed_cpu(engine: &mut PressureEngine, busy: f32, count: usize) -> PressureSnapshot {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut radar = PressureSnapshot::warming_up();
        for _ in 0..=count {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, busy));
            radar = engine.observe(&snapshot);
        }
        radar
    }

    #[test]
    fn the_radar_always_contains_every_signal_in_display_order() {
        let radar = engine().observe(&snapshot());
        assert_eq!(radar.signals.len(), PressureId::DISPLAY_ORDER.len());
        for (signal, expected) in radar.signals.iter().zip(PressureId::DISPLAY_ORDER) {
            assert_eq!(signal.id, expected);
            assert!(!signal.rule.is_empty(), "§2.3 requires the rule text");
        }
    }

    #[test]
    fn the_first_snapshot_produces_no_state_at_all() {
        let mut engine = engine();
        let radar = engine.observe(&snapshot());
        assert!(
            radar.worst_state().is_warming_up(),
            "an unmeasured system must not read as healthy"
        );
        assert_eq!(engine.observations(), 0, "a zero interval is not a sample");
    }

    #[test]
    fn a_signal_warms_up_until_the_minimum_sample_count_is_reached() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for index in 0..10 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
            let radar = engine.observe(&snapshot);
            let state = cpu_state(&radar);
            if index == 0 {
                assert!(state.is_warming_up(), "the first sample has no interval");
                continue;
            }
            assert!(
                state.is_warming_up(),
                "sample {index} must not support a sustained claim"
            );
        }
        let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        assert_eq!(
            cpu_state(&engine.observe(&snapshot)),
            MetricState::Available(PressureState::Critical)
        );
    }

    #[test]
    fn a_sustained_condition_escalates_and_reports_how_long_it_has_held() {
        let mut engine = engine();
        let radar = feed_cpu(&mut engine, 99.0, 20);
        let signal = radar.signal(PressureId::Cpu).expect("cpu signal exists");

        assert_eq!(
            signal.state,
            MetricState::Available(PressureState::Critical)
        );
        assert_eq!(signal.symbol(), 'X', "§2.3's redundant cue");
        assert!(signal.severity.fresh().is_some());
        assert!(signal.raw.is_some(), "§2.3 requires the raw metric");
        assert!(
            signal
                .held_for
                .is_some_and(|held| held >= Duration::from_secs(10)),
            "held_for {:?}",
            signal.held_for
        );
    }

    #[test]
    fn an_alternating_metric_does_not_flap_the_radar() {
        // The §11.3 requirement, end to end: a CPU sitting on its threshold must
        // not raise and clear the radar once per second.
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut states = Vec::new();
        for index in 0..60 {
            let busy = if index % 2 == 0 { 99.0 } else { 1.0 };
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, busy));
            states.push(cpu_state(&engine.observe(&snapshot)));
        }

        let derived: Vec<PressureState> = states
            .iter()
            .filter_map(|state| state.fresh().copied())
            .collect();
        assert!(!derived.is_empty(), "the signal must settle eventually");
        assert!(
            derived.iter().all(|state| *state == PressureState::Normal),
            "the radar flapped: {derived:?}"
        );
    }

    #[test]
    fn an_unavailable_input_leaves_the_signal_unavailable_rather_than_normal() {
        let mut engine = engine();
        feed_cpu(&mut engine, 99.0, 20);

        let mut timeline = Timeline::new(Duration::from_secs(1));
        let snapshot = timeline.push(|snapshot| {
            snapshot.cpu.total = MetricState::PermissionDenied;
        });
        let radar = engine.observe(&snapshot);
        assert_eq!(cpu_state(&radar), MetricState::PermissionDenied);
        let signal = radar.signal(PressureId::Cpu).expect("cpu signal exists");
        assert_eq!(signal.symbol(), '!');
        assert!(signal.held_for.is_none());
    }

    #[test]
    fn a_counter_reset_clears_the_window_instead_of_counting_as_an_event() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));

        // Nine loud samples, then a reset, then one more loud sample.
        for _ in 0..10 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
            let _ = engine.observe(&snapshot);
        }
        let reset = timeline.push(|snapshot| {
            snapshot.cpu.total =
                MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        });
        assert_eq!(
            cpu_state(&engine.observe(&reset)),
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset)
        );

        let after = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        assert!(
            cpu_state(&engine.observe(&after)).is_warming_up(),
            "§11.3: a reset must not be readable as an event"
        );
    }

    #[test]
    fn a_sleep_wake_gap_resets_every_signal() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..15 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
            let _ = engine.observe(&snapshot);
        }
        assert_eq!(
            cpu_state(&engine.observe(&timeline.push(|s| set_cpu(s, 99.0)))),
            MetricState::Available(PressureState::Critical)
        );

        // The machine slept for two hours; the next interval is enormous.
        let mut woken = timeline.build(|snapshot| set_cpu(snapshot, 99.0));
        woken.elapsed = Duration::from_secs(7_200);
        let radar = engine.observe(&woken);

        assert!(
            cpu_state(&radar).is_warming_up(),
            "the gap must not be read as fifteen saturated samples"
        );
        assert_eq!(engine.discontinuities(), 1);
    }

    #[test]
    fn a_shorter_than_usual_interval_is_not_a_discontinuity() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..12 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
            let _ = engine.observe(&snapshot);
        }
        let mut jittered = timeline.build(|snapshot| set_cpu(snapshot, 99.0));
        jittered.elapsed = Duration::from_millis(600);
        let _ = engine.observe(&jittered);
        assert_eq!(engine.discontinuities(), 0);
    }

    #[test]
    fn the_reference_interval_cannot_be_inflated_by_a_stall() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..3 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 10.0));
            let _ = engine.observe(&snapshot);
        }
        // A 5s stall is within ten intervals, so it is a measurement...
        let mut stalled = timeline.build(|snapshot| set_cpu(snapshot, 10.0));
        stalled.elapsed = Duration::from_secs(5);
        let _ = engine.observe(&stalled);
        assert_eq!(engine.discontinuities(), 0);

        // ...and it must not raise the bar for what counts as a gap.
        let mut gap = timeline.build(|snapshot| set_cpu(snapshot, 10.0));
        gap.elapsed = Duration::from_secs(30);
        let _ = engine.observe(&gap);
        assert_eq!(engine.discontinuities(), 1);
    }

    #[test]
    fn raw_psi_is_carried_through_untouched() {
        let mut engine = engine();
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 1.0, 2.0, 3.0);
        let radar = engine.observe(&snapshot);
        assert_eq!(
            radar.psi, snapshot.pressure.psi,
            "psi is the collector's measurement, not the engine's"
        );
    }

    #[test]
    fn signals_the_platform_cannot_measure_stay_unsupported_forever() {
        // A collector on a platform without PSI reports the metric as unsupported;
        // the engine must never turn that into a state, however long it runs.
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..20 {
            let snapshot = timeline.push(|snapshot| {
                set_cpu(snapshot, 10.0);
                snapshot.pressure.psi = MetricState::Unsupported;
            });
            let radar = engine.observe(&snapshot);
            for id in [PressureId::PsiCpu, PressureId::PsiMemory, PressureId::PsiIo] {
                let signal = radar.signal(id).expect("signal exists");
                assert!(
                    signal.state.is_unsupported(),
                    "{id:?} became {:?} without PSI data",
                    signal.state
                );
                assert_eq!(signal.symbol(), '-');
            }
        }
    }

    #[test]
    fn a_metric_the_collector_has_not_reported_yet_stays_warming_up() {
        // `PressureSnapshot::warming_up` leaves psi warming up rather than
        // unsupported: "not measured yet" and "never measurable" are different
        // claims, and neither is `normal` (§4).
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..20 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 10.0));
            let radar = engine.observe(&snapshot);
            let signal = radar.signal(PressureId::PsiMemory).expect("signal exists");
            assert!(signal.state.is_warming_up(), "{:?}", signal.state);
            assert!(signal.state.fresh().is_none());
        }
    }

    #[test]
    fn independent_signals_do_not_share_hysteresis_state() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..12 {
            let snapshot = timeline.push(|snapshot| {
                set_cpu(snapshot, 99.0);
                set_memory(snapshot, TOTAL, TOTAL / 2);
                set_disk_busy(snapshot, "nvme0n1", 5.0);
            });
            let radar = engine.observe(&snapshot);
            let _ = radar;
        }
        let snapshot = timeline.push(|snapshot| {
            set_cpu(snapshot, 99.0);
            set_memory(snapshot, TOTAL, TOTAL / 2);
            set_disk_busy(snapshot, "nvme0n1", 5.0);
        });
        let radar = engine.observe(&snapshot);

        assert_eq!(
            cpu_state(&radar),
            MetricState::Available(PressureState::Critical)
        );
        assert_eq!(
            radar.signal(PressureId::Memory).map(|signal| signal.state),
            Some(MetricState::Available(PressureState::Normal))
        );
        assert_eq!(
            radar.signal(PressureId::Disk).map(|signal| signal.state),
            Some(MetricState::Available(PressureState::Normal))
        );
    }

    #[test]
    fn disabling_diagnostics_reports_unsupported_rather_than_healthy() {
        let mut engine = PressureEngine::new(Thresholds {
            enabled: false,
            ..Thresholds::default()
        });
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..20 {
            let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
            let radar = engine.observe(&snapshot);
            for signal in &radar.signals {
                assert!(signal.state.is_unsupported());
                assert_eq!(signal.rule, DISABLED_RULE);
            }
            assert!(radar.worst_state().is_warming_up());
        }
    }

    #[test]
    fn changing_thresholds_restarts_the_evidence_rather_than_reusing_it() {
        let mut engine = engine();
        feed_cpu(&mut engine, 99.0, 20);

        engine.set_thresholds(Thresholds {
            cpu_watch_percent: 20.0,
            ..Thresholds::default()
        });
        assert_eq!(engine.observations(), 0);

        let mut timeline = Timeline::new(Duration::from_secs(1));
        let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        assert!(
            cpu_state(&engine.observe(&snapshot)).is_warming_up(),
            "observations made under other thresholds must not be reused"
        );
    }

    #[test]
    fn an_explicit_reset_returns_every_signal_to_warming_up() {
        let mut engine = engine();
        feed_cpu(&mut engine, 99.0, 20);
        engine.reset();

        let mut timeline = Timeline::new(Duration::from_secs(1));
        // The first sample of a timeline has no interval, so it is not an
        // observation; the second one is (§8.2).
        timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        let snapshot = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        let radar = engine.observe(&snapshot);
        assert!(cpu_state(&radar).is_warming_up());
        assert_eq!(engine.observations(), 1);
    }

    #[test]
    fn the_worst_state_across_the_radar_is_what_the_header_shows() {
        let mut engine = engine();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        for _ in 0..12 {
            let snapshot = timeline.push(|snapshot| {
                set_cpu(snapshot, 10.0);
                set_memory(snapshot, TOTAL, TOTAL / 100);
            });
            let _ = engine.observe(&snapshot);
        }
        let snapshot = timeline.push(|snapshot| {
            set_cpu(snapshot, 10.0);
            set_memory(snapshot, TOTAL, TOTAL / 100);
        });
        let radar = engine.observe(&snapshot);
        assert_eq!(
            radar.worst_state(),
            MetricState::Available(PressureState::Critical),
            "one critical signal makes the system critical"
        );
    }
}

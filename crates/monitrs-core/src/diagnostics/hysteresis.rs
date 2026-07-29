//! Explicit hysteresis state: the reason the radar does not flap (§11.3).
//!
//! # The rule
//!
//! A signal escalates only once the higher state has been observed in
//! `sustained_samples` of the last `sustained_window` observations, and it
//! de-escalates only once the state it currently holds has **completely cleared**
//! from that window. Both halves are needed: the first stops one noisy tick from
//! raising an alarm, the second stops one quiet tick from clearing it. An input
//! that alternates between quiet and loud therefore produces no transition at all,
//! which is the property [`Hysteresis`]'s tests pin down.
//!
//! # Resetting
//!
//! §11.3 also requires the engine to reset cleanly after a counter reset or a
//! sleep/wake cycle. [`Hysteresis::reset`] is that reset: it clears the
//! observation window, so the samples from before the gap can never be stitched
//! together with the samples after it to manufacture a sustained condition. After
//! a reset the signal is warming up again — a reset must never look like an event.

use core::time::Duration;
use std::collections::VecDeque;

use crate::model::{MetricState, PressureState};

use super::Thresholds;

/// The hysteresis state of one pressure signal.
///
/// Deliberately a concrete, inspectable struct rather than a closure or a counter
/// hidden inside the engine: §11.3 calls for hysteresis as behaviour that can be
/// tested, and the tests in this file are what guarantee the radar is stable.
#[derive(Clone, Debug)]
pub struct Hysteresis {
    /// The most recent candidate states, oldest first, bounded by `window`.
    observations: VecDeque<PressureState>,
    /// How many observations are retained.
    window: usize,
    /// How many observations at or above a state are needed to reach it.
    required: usize,
    /// The state currently held.
    state: PressureState,
    /// How long `state` has been held, accumulated from measured intervals.
    held_for: Duration,
    /// Whether enough observations have accumulated for `state` to mean anything.
    settled: bool,
}

impl Hysteresis {
    /// Builds tracker state from sanitized thresholds.
    ///
    /// `thresholds` must already be [`Thresholds::sanitized`]; the engine does that
    /// once at construction so every tracker inherits a window at least as wide as
    /// the sample count it needs.
    #[must_use]
    pub fn new(thresholds: &Thresholds) -> Self {
        let window = thresholds.sustained_window.max(1);
        Self {
            observations: VecDeque::with_capacity(window),
            window,
            required: thresholds.sustained_samples.clamp(1, window),
            state: PressureState::Normal,
            held_for: Duration::ZERO,
            settled: false,
        }
    }

    /// Feeds one candidate state derived from the current sample.
    ///
    /// `elapsed` is the *measured* interval since the previous sample (§8.1); it is
    /// only used to accumulate [`Self::held_for`], never to decide a state.
    ///
    /// Returns [`MetricState::WarmingUp`] until `sustained_samples` observations
    /// exist, because below that no sustained claim is possible (§11.3).
    pub fn observe(
        &mut self,
        candidate: PressureState,
        elapsed: Duration,
    ) -> MetricState<PressureState> {
        if self.observations.len() >= self.window {
            self.observations.pop_front();
        }
        self.observations.push_back(candidate);

        if self.observations.len() < self.required {
            self.held_for = Duration::ZERO;
            self.settled = false;
            return MetricState::WarmingUp;
        }

        let target = self.escalation_target();
        let changed = if target > self.state {
            // Escalation: the higher state has been observed often enough.
            self.state = target;
            true
        } else if target < self.state && self.count_at_least(self.state) == 0 {
            // De-escalation: only once the held state has left the window
            // entirely. This is what stops a single quiet sample from clearing a
            // real problem (§11.3).
            self.state = target;
            true
        } else {
            false
        };

        if changed || !self.settled {
            self.held_for = Duration::ZERO;
        }
        self.settled = true;
        self.held_for = self.held_for.saturating_add(elapsed);
        MetricState::Available(self.state)
    }

    /// Discards every observation (§11.3: counter reset, sleep/wake).
    pub fn reset(&mut self) {
        self.observations.clear();
        self.state = PressureState::Normal;
        self.held_for = Duration::ZERO;
        self.settled = false;
    }

    /// How long the current state has been held, once it means anything.
    ///
    /// `None` while warming up: a duration for a state that has not been
    /// established would be a fabricated number.
    #[must_use]
    pub const fn held_for(&self) -> Option<Duration> {
        if self.settled {
            Some(self.held_for)
        } else {
            None
        }
    }

    /// The state currently held, or `None` while warming up.
    #[must_use]
    pub const fn state(&self) -> Option<PressureState> {
        if self.settled { Some(self.state) } else { None }
    }

    /// How many observations are retained.
    #[must_use]
    pub fn observations(&self) -> usize {
        self.observations.len()
    }

    /// How many observations are still needed before a state can be derived.
    #[must_use]
    pub fn remaining_samples(&self) -> usize {
        self.required.saturating_sub(self.observations.len())
    }

    /// The highest state observed often enough to be reached.
    fn escalation_target(&self) -> PressureState {
        if self.count_at_least(PressureState::Critical) >= self.required {
            PressureState::Critical
        } else if self.count_at_least(PressureState::Watch) >= self.required {
            PressureState::Watch
        } else {
            PressureState::Normal
        }
    }

    /// How many retained observations are at or above `state`.
    fn count_at_least(&self, state: PressureState) -> usize {
        self.observations
            .iter()
            .filter(|observed| **observed >= state)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TICK: Duration = Duration::from_secs(1);

    fn tracker() -> Hysteresis {
        Hysteresis::new(&Thresholds::default())
    }

    fn feed(tracker: &mut Hysteresis, states: &[PressureState]) -> MetricState<PressureState> {
        let mut last = MetricState::WarmingUp;
        for state in states {
            last = tracker.observe(*state, TICK);
        }
        last
    }

    #[test]
    fn a_signal_warms_up_until_it_has_the_minimum_number_of_samples() {
        let mut tracker = tracker();
        for index in 1..10 {
            assert!(
                tracker
                    .observe(PressureState::Critical, TICK)
                    .is_warming_up(),
                "observation {index} must not support a sustained claim"
            );
            assert!(tracker.state().is_none());
            assert!(tracker.held_for().is_none());
        }
        assert_eq!(tracker.remaining_samples(), 1);
        assert_eq!(
            tracker.observe(PressureState::Critical, TICK),
            MetricState::Available(PressureState::Critical)
        );
        assert_eq!(tracker.remaining_samples(), 0);
    }

    #[test]
    fn an_alternating_input_never_escalates() {
        // The flapping case §11.3 exists to prevent: a metric sitting exactly on
        // its threshold must not raise and clear an alarm once per second.
        let mut tracker = tracker();
        let mut states = Vec::new();
        for index in 0..100 {
            let candidate = if index % 2 == 0 {
                PressureState::Watch
            } else {
                PressureState::Normal
            };
            states.push(tracker.observe(candidate, TICK));
        }

        let derived: Vec<PressureState> =
            states.iter().filter_map(|s| s.fresh().copied()).collect();
        assert!(
            derived.iter().all(|state| *state == PressureState::Normal),
            "alternating input produced {derived:?}"
        );
    }

    #[test]
    fn an_alternating_input_does_not_flap_once_a_state_is_established() {
        let mut tracker = tracker();
        feed(&mut tracker, &[PressureState::Watch; 10]);
        assert_eq!(tracker.state(), Some(PressureState::Watch));

        // Now alternate. The watch state must hold, because it has not cleared.
        for index in 0..40 {
            let candidate = if index % 2 == 0 {
                PressureState::Normal
            } else {
                PressureState::Watch
            };
            assert_eq!(
                tracker.observe(candidate, TICK),
                MetricState::Available(PressureState::Watch),
                "flapped on observation {index}"
            );
        }
    }

    #[test]
    fn escalation_needs_the_required_count_inside_the_window() {
        let mut tracker = tracker();
        // Nine critical observations inside a fifteen-sample window are not ten.
        feed(&mut tracker, &[PressureState::Critical; 9]);
        feed(&mut tracker, &[PressureState::Watch; 6]);
        assert_eq!(
            tracker.state(),
            Some(PressureState::Watch),
            "watch is sustained (15 of 15 at or above watch), critical is not"
        );
    }

    #[test]
    fn a_state_clears_only_once_it_has_left_the_window_entirely() {
        let mut tracker = tracker();
        feed(&mut tracker, &[PressureState::Critical; 10]);
        assert_eq!(tracker.state(), Some(PressureState::Critical));

        // Fourteen quiet samples still leave one critical observation in a
        // fifteen-sample window, so the signal must not clear yet.
        feed(&mut tracker, &[PressureState::Normal; 14]);
        assert_eq!(tracker.state(), Some(PressureState::Critical));

        feed(&mut tracker, &[PressureState::Normal]);
        assert_eq!(tracker.state(), Some(PressureState::Normal));
    }

    #[test]
    fn de_escalation_stops_at_the_state_that_is_still_sustained() {
        let mut tracker = tracker();
        feed(&mut tracker, &[PressureState::Critical; 15]);
        assert_eq!(tracker.state(), Some(PressureState::Critical));

        // Critical leaves the window but watch remains sustained.
        feed(&mut tracker, &[PressureState::Watch; 15]);
        assert_eq!(tracker.state(), Some(PressureState::Watch));
    }

    #[test]
    fn held_for_accumulates_measured_intervals_and_restarts_on_a_transition() {
        let mut tracker = tracker();
        feed(&mut tracker, &[PressureState::Normal; 10]);
        assert_eq!(tracker.held_for(), Some(TICK));

        for _ in 0..4 {
            tracker.observe(PressureState::Normal, Duration::from_millis(500));
        }
        assert_eq!(
            tracker.held_for(),
            Some(TICK + Duration::from_millis(2_000)),
            "held_for must use the measured interval, not an assumed second"
        );

        feed(&mut tracker, &[PressureState::Watch; 10]);
        assert_eq!(
            tracker.held_for(),
            Some(TICK),
            "a transition restarts the held duration"
        );
    }

    #[test]
    fn a_reset_discards_the_window_so_a_gap_cannot_become_a_sustained_condition() {
        let mut tracker = tracker();
        feed(&mut tracker, &[PressureState::Critical; 9]);
        tracker.reset();

        assert_eq!(tracker.observations(), 0);
        assert!(tracker.state().is_none());
        assert!(tracker.held_for().is_none());

        // One critical observation after the reset must not join the nine from
        // before it.
        assert!(
            tracker
                .observe(PressureState::Critical, TICK)
                .is_warming_up(),
            "§11.3: a reset must not be readable as an event"
        );
    }

    #[test]
    fn the_window_is_bounded_however_long_the_engine_runs() {
        let mut tracker = tracker();
        for _ in 0..10_000 {
            tracker.observe(PressureState::Watch, TICK);
        }
        assert_eq!(
            tracker.observations(),
            Thresholds::default().sustained_window
        );
    }

    #[test]
    fn a_single_sample_configuration_still_applies_hysteresis_downwards() {
        let thresholds = Thresholds {
            sustained_samples: 1,
            sustained_window: 1,
            ..Thresholds::default()
        }
        .sanitized();
        let mut tracker = Hysteresis::new(&thresholds);
        assert_eq!(
            tracker.observe(PressureState::Critical, TICK),
            MetricState::Available(PressureState::Critical)
        );
        assert_eq!(
            tracker.observe(PressureState::Normal, TICK),
            MetricState::Available(PressureState::Normal),
            "with a one-sample window the previous state has left it"
        );
    }
}

//! Pressure Radar signals (§2.3).
//!
//! Every signal must show four things: the raw metric, its normalized severity,
//! **the rule used to derive the state**, and an explicit unavailable state. The
//! rule text is part of the data, not documentation, so a user can always see
//! why a signal turned amber without reading the source.

use core::time::Duration;

use crate::model::{Measurement, MetricState, Severity};
use crate::units::Percent;

/// Which resource a signal describes.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PressureId {
    /// CPU saturation.
    Cpu,
    /// Memory availability.
    Memory,
    /// Disk device pressure.
    Disk,
    /// Network saturation. Only meaningful with a known link speed (§2.3).
    Network,
    /// Swap activity.
    Swap,
    /// Sustained run-queue or load pressure.
    Load,
    /// Linux PSI, CPU resource.
    PsiCpu,
    /// Linux PSI, memory resource.
    PsiMemory,
    /// Linux PSI, I/O resource.
    PsiIo,
}

impl PressureId {
    /// The short, fixed-width label used in the radar panel (§5.5).
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Memory => "MEM",
            Self::Disk => "DISK",
            Self::Network => "NET",
            Self::Swap => "SWAP",
            Self::Load => "LOAD",
            Self::PsiCpu => "PSI-CPU",
            Self::PsiMemory => "PSI-MEM",
            Self::PsiIo => "PSI-IO",
        }
    }

    /// The order signals appear in the radar, most important first.
    pub const DISPLAY_ORDER: [Self; 9] = [
        Self::Cpu,
        Self::Memory,
        Self::Disk,
        Self::Network,
        Self::Swap,
        Self::Load,
        Self::PsiCpu,
        Self::PsiMemory,
        Self::PsiIo,
    ];
}

/// The three states a pressure signal can be in (§2.3).
#[derive(Clone, Copy, Debug, Default, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PressureState {
    /// Nothing to act on.
    #[default]
    Normal,
    /// Elevated; worth watching.
    Watch,
    /// Actively degrading the system.
    Critical,
}

impl PressureState {
    /// The redundant ASCII cue. §2.3 names these exact characters, and §5.2
    /// forbids color from being the only indicator.
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Normal => '.',
            Self::Watch => '!',
            Self::Critical => 'X',
        }
    }

    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Watch => "watch",
            Self::Critical => "critical",
        }
    }

    /// The equivalent diagnostic severity.
    #[must_use]
    pub const fn severity(self) -> Severity {
        match self {
            Self::Normal => Severity::Info,
            Self::Watch => Severity::Watch,
            Self::Critical => Severity::Critical,
        }
    }
}

/// One radar signal.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PressureSignal {
    /// Which resource.
    pub id: PressureId,
    /// The derived state, or why it could not be derived.
    pub state: MetricState<PressureState>,
    /// Normalized `0..=100` severity, for sorting and bar length.
    ///
    /// Separate from `state` because two signals can both be `Watch` while one
    /// is far closer to critical.
    pub severity: MetricState<Percent>,
    /// The raw metric the state was derived from (§2.3).
    pub raw: Option<Measurement>,
    /// Human-readable statement of the rule that produced `state` (§2.3).
    ///
    /// For example: `"available < 15% of total for 10 of 15 samples"`.
    pub rule: &'static str,
    /// How long the signal has held its current state, for hysteresis display.
    pub held_for: Option<Duration>,
}

impl PressureSignal {
    /// A signal this platform cannot produce at all.
    #[must_use]
    pub const fn unsupported(id: PressureId, rule: &'static str) -> Self {
        Self {
            id,
            state: MetricState::Unsupported,
            severity: MetricState::Unsupported,
            raw: None,
            rule,
            held_for: None,
        }
    }

    /// A signal awaiting the samples its rule requires.
    #[must_use]
    pub const fn warming_up(id: PressureId, rule: &'static str) -> Self {
        Self {
            id,
            state: MetricState::WarmingUp,
            severity: MetricState::WarmingUp,
            raw: None,
            rule,
            held_for: None,
        }
    }

    /// The character shown in the radar's leftmost column.
    ///
    /// Falls back to the availability symbol when no state could be derived, so
    /// an unknown signal reads as `?` rather than as `normal` (§5.5).
    #[must_use]
    pub fn symbol(&self) -> char {
        match self.state.displayable() {
            Some((state, _)) => state.symbol(),
            None => self.state.symbol(),
        }
    }
}

/// The Linux PSI figures for one resource.
///
/// `some` is the share of time at least one task was stalled; `full` is the
/// share where *every* runnable task was stalled. `full` is absent for the CPU
/// resource on many kernels, which is why it is a [`MetricState`].
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PsiResource {
    /// 10-second `some` average.
    pub some_avg10: Percent,
    /// 60-second `some` average.
    pub some_avg60: Percent,
    /// 300-second `some` average.
    pub some_avg300: Percent,
    /// 10-second `full` average.
    pub full_avg10: MetricState<Percent>,
    /// 60-second `full` average.
    pub full_avg60: MetricState<Percent>,
    /// 300-second `full` average.
    pub full_avg300: MetricState<Percent>,
    /// Cumulative stall time, useful as a monotonic counter.
    pub total_stalled: Duration,
}

/// All three Linux PSI resources.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PsiSnapshot {
    /// `/proc/pressure/cpu`.
    pub cpu: PsiResource,
    /// `/proc/pressure/memory`.
    pub memory: PsiResource,
    /// `/proc/pressure/io`.
    pub io: PsiResource,
}

/// The whole radar.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct PressureSnapshot {
    /// Signals in [`PressureId::DISPLAY_ORDER`].
    pub signals: Vec<PressureSignal>,
    /// Raw PSI figures, Linux only.
    pub psi: MetricState<PsiSnapshot>,
}

impl PressureSnapshot {
    /// Looks up one signal.
    #[must_use]
    pub fn signal(&self, id: PressureId) -> Option<&PressureSignal> {
        self.signals.iter().find(|signal| signal.id == id)
    }

    /// The most severe state any signal reports.
    ///
    /// Unavailable signals are skipped rather than counted as normal, so a
    /// system whose pressure cannot be measured does not read as healthy.
    #[must_use]
    pub fn worst_state(&self) -> MetricState<PressureState> {
        let worst = self
            .signals
            .iter()
            .filter_map(|signal| signal.state.fresh().copied())
            .max();
        match worst {
            Some(state) => MetricState::Available(state),
            None => MetricState::WarmingUp,
        }
    }

    /// A radar with every signal warming up, for the first frame.
    #[must_use]
    pub fn warming_up() -> Self {
        Self {
            signals: PressureId::DISPLAY_ORDER
                .iter()
                .map(|&id| PressureSignal::warming_up(id, "awaiting samples"))
                .collect(),
            psi: MetricState::WarmingUp,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{MeasuredValue, UnavailableReason};

    #[test]
    fn state_symbols_are_exactly_the_specified_characters() {
        assert_eq!(PressureState::Normal.symbol(), '.');
        assert_eq!(PressureState::Watch.symbol(), '!');
        assert_eq!(PressureState::Critical.symbol(), 'X');
    }

    #[test]
    fn an_unavailable_signal_shows_a_question_mark_not_normal() {
        // This is the `? NET unknown` row in the §5.5 mockup.
        let signal = PressureSignal {
            id: PressureId::Network,
            state: MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
            severity: MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
            raw: Some(Measurement::new(
                "throughput",
                MeasuredValue::Count(18_000_000),
            )),
            rule: "utilization requires a known link speed",
            held_for: None,
        };
        assert_eq!(signal.symbol(), '?');
        assert_ne!(signal.symbol(), PressureState::Normal.symbol());
    }

    #[test]
    fn an_unsupported_signal_is_distinguishable_from_a_normal_one() {
        let signal = PressureSignal::unsupported(PressureId::PsiIo, "Linux only");
        assert_eq!(signal.symbol(), '-');
        assert!(signal.state.is_unsupported());
    }

    #[test]
    fn worst_state_ignores_unavailable_signals_rather_than_treating_them_as_healthy() {
        let mut snapshot = PressureSnapshot::warming_up();
        // Everything warming up: the system is not "normal", it is unmeasured.
        assert!(snapshot.worst_state().is_warming_up());

        if let Some(signal) = snapshot.signals.first_mut() {
            signal.state = MetricState::Available(PressureState::Watch);
        }
        assert_eq!(
            snapshot.worst_state(),
            MetricState::Available(PressureState::Watch)
        );

        if let Some(signal) = snapshot.signals.get_mut(1) {
            signal.state = MetricState::Available(PressureState::Critical);
        }
        assert_eq!(
            snapshot.worst_state(),
            MetricState::Available(PressureState::Critical)
        );
    }

    #[test]
    fn warming_up_radar_contains_every_signal_in_display_order() {
        let snapshot = PressureSnapshot::warming_up();
        assert_eq!(snapshot.signals.len(), PressureId::DISPLAY_ORDER.len());
        for (signal, expected) in snapshot.signals.iter().zip(PressureId::DISPLAY_ORDER) {
            assert_eq!(signal.id, expected);
        }
        assert!(snapshot.signal(PressureId::Memory).is_some());
    }

    #[test]
    fn every_signal_carries_the_rule_that_derived_it() {
        let snapshot = PressureSnapshot::warming_up();
        for signal in &snapshot.signals {
            assert!(!signal.rule.is_empty(), "{:?} has no rule text", signal.id);
        }
    }

    #[test]
    fn labels_are_short_enough_for_the_radar_column() {
        for id in PressureId::DISPLAY_ORDER {
            assert!(id.label().len() <= 8, "{id:?} label is too wide");
            assert!(id.label().is_ascii());
        }
    }
}

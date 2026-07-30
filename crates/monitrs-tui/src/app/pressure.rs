//! Announcing a pressure signal that crossed a threshold (§2.3, §11.3, §14.1).
//!
//! The Pressure Radar already decides *whether* a signal is in `watch` or
//! `critical`, and [`monitrs_core::diagnostics::PressureEngine`] applies the §11.3
//! hysteresis to that decision. The moment the decision changes is the one worth
//! telling the user about: it is the only thing in the radar they could not have
//! read off the previous frame, and it is the reason they do not have to watch the
//! screen.
//!
//! Three properties are deliberate:
//!
//! * **One notice per transition, not one per sample.** The reducer sees a snapshot
//!   a second and a saturated CPU reports `critical` in every one of them, so the
//!   previous state is held per signal and compared. Without that the notice log
//!   would fill with the same line sixty times a minute — and
//!   [`crate::app::NoticeLog`] would merge them into a count, which is the wrong
//!   shape for an event that happened once.
//! * **The engine is quoted rather than paraphrased.** The rule text and the held
//!   duration are taken off [`PressureSignal`] unchanged (§2.3 puts the rule *in*
//!   the data for exactly this reason), so a notice cannot claim something the radar
//!   panel would contradict.
//! * **An unavailable signal is not good news.** A signal whose input the OS refused
//!   has no state at all (§4), so it neither escalates nor de-escalates: the
//!   remembered state is dropped instead. That is also what stops the samples either
//!   side of a gap being stitched into a transition, the same rule §11.3 imposes on
//!   the hysteresis window itself.

use monitrs_core::model::{PressureId, PressureSignal, PressureSnapshot, PressureState, Severity};
use monitrs_core::units::format_duration;

use super::notice::{Notice, NoticeKind};

/// How many signals the radar carries, and therefore how much the watch holds.
///
/// Fixed at the display order's length: §10.3 forbids unbounded accumulation, and
/// one slot per signal is the whole of this module's memory however long monitrs
/// runs.
const SIGNALS: usize = PressureId::DISPLAY_ORDER.len();

/// One radar transition, ready to be recorded.
#[derive(Clone, Debug, PartialEq)]
pub struct PressureAlert {
    /// The notice describing the transition.
    pub notice: Notice,
    /// Whether this transition *entered* [`PressureState::Critical`].
    ///
    /// The only thing the optional terminal bell reacts to (§12
    /// `diagnostics.bell_on_critical`). A signal that was already critical and is
    /// still critical produces no alert at all, so this cannot fire twice for one
    /// episode; a de-escalation never sets it, however severe the state it left.
    pub reached_critical: bool,
}

/// The previous state of every radar signal, so a change can be recognised.
///
/// Indexed by position in [`PressureId::DISPLAY_ORDER`] rather than keyed by id: the
/// order is fixed and total, so an array makes "a signal with no slot" unrepresentable
/// instead of something to handle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PressureWatch {
    /// The last state each signal was seen in. `None` means no state is known —
    /// either nothing has been derived yet, or the last sample was unavailable.
    last: [Option<PressureState>; SIGNALS],
}

impl PressureWatch {
    /// A watch that has seen nothing.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: [None; SIGNALS],
        }
    }

    /// The state `id` was last seen in, if any.
    ///
    /// Exposed for the tests that pin the "unavailable is not a de-escalation" rule:
    /// the interesting half of that rule is what the watch *forgets*.
    #[must_use]
    pub fn last_state(&self, id: PressureId) -> Option<PressureState> {
        self.last.get(slot(id)).copied().flatten()
    }

    /// Folds one radar in and returns the transitions worth announcing.
    ///
    /// At most one alert per signal, so the returned list is bounded by
    /// [`PressureId::DISPLAY_ORDER`] — this runs on the one-second path (§16.1).
    pub fn observe(&mut self, pressure: &PressureSnapshot) -> Vec<PressureAlert> {
        let mut alerts = Vec::new();
        for (index, id) in PressureId::DISPLAY_ORDER.iter().enumerate() {
            let Some(remembered) = self.last.get_mut(index) else {
                continue;
            };
            let Some(signal) = pressure.signal(*id) else {
                // A radar missing one of its own signals is a bug elsewhere, not a
                // reason to invent a transition. Leave what is remembered alone so a
                // one-sample gap does not read as an event.
                continue;
            };
            let Some(&state) = signal.state.fresh() else {
                // §4: refused, warming up, or unsupported. No state means no
                // transition, and forgetting the old one is what keeps the next
                // available sample from being reported as a change that never
                // happened (§11.3).
                *remembered = None;
                continue;
            };
            let previous = remembered.replace(state);
            if let Some(alert) = alert_for(signal, previous, state) {
                alerts.push(alert);
            }
        }
        alerts
    }
}

impl Default for PressureWatch {
    fn default() -> Self {
        Self::new()
    }
}

/// The alert for a signal now in `state`, or `None` if nothing changed.
///
/// A signal whose previous state is unknown is compared against
/// [`PressureState::Normal`]: coming up already critical — which happens once the
/// hysteresis window fills, and again after any reset — is news, while coming up
/// normal is not.
fn alert_for(
    signal: &PressureSignal,
    previous: Option<PressureState>,
    state: PressureState,
) -> Option<PressureAlert> {
    let baseline = previous.unwrap_or(PressureState::Normal);
    if state == baseline {
        return None;
    }
    let escalated = state > baseline;
    Some(PressureAlert {
        notice: Notice::new(
            NoticeKind::Pressure,
            // §14.1: a de-escalation is not a problem, so it is recorded at the
            // severity of good news — including `critical` to `watch`, where the
            // signal is still elevated but the news is the improvement. The state
            // itself is named in the text, so an informational `.` beside the word
            // `watch` is a statement about the notice, not about the signal; the
            // radar is where the signal's own cue lives. An escalation does take the
            // severity of the state it reached, which is what puts an `X` in front of
            // a critical line and a `!` in front of a watch (§5.2).
            if escalated {
                state.severity()
            } else {
                Severity::Info
            },
            message(signal, previous, state),
        ),
        reached_critical: escalated && state == PressureState::Critical,
    })
}

/// The sentence a transition is reported as.
///
/// The state it came from is named only when it is known, because "was normal" would
/// be a guess after an unavailable sample rather than a report.
///
/// `held_for` is the engine's own accumulator, which resets when the state changes —
/// so at the moment of a transition it is the first interval spent in the new state,
/// not the length of the episode being announced. It is quoted anyway, and quoted
/// rather than recomputed here, because it is the same number the radar panel shows:
/// a second, independent duration would eventually disagree with the first.
fn message(
    signal: &PressureSignal,
    previous: Option<PressureState>,
    state: PressureState,
) -> String {
    let detail = match (previous, signal.held_for) {
        (Some(previous), Some(held)) => {
            format!(
                " (was {}, held {})",
                previous.label(),
                format_duration(held)
            )
        }
        (Some(previous), None) => format!(" (was {})", previous.label()),
        (None, Some(held)) => format!(" (held {})", format_duration(held)),
        (None, None) => String::new(),
    };
    format!(
        "{} is now {}{detail}: {}",
        signal.id.label(),
        state.label(),
        signal.rule
    )
}

/// The array slot a signal id occupies.
///
/// Falls back to the first slot for an id that is somehow not in the display order,
/// which cannot happen — the order is exhaustive — and would at worst mis-attribute
/// one notice rather than panic in a render path (§14.3).
fn slot(id: PressureId) -> usize {
    PressureId::DISPLAY_ORDER
        .iter()
        .position(|candidate| *candidate == id)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::diagnostics::signal_rule_text;
    use monitrs_core::model::{MetricState, UnavailableReason};
    use monitrs_core::units::Percent;

    use super::*;

    /// A radar in which `id` holds `state`, and every other signal is warming up.
    fn radar(id: PressureId, state: PressureState, held: Duration) -> PressureSnapshot {
        let mut snapshot = PressureSnapshot::warming_up();
        if let Some(signal) = snapshot.signals.iter_mut().find(|signal| signal.id == id) {
            signal.state = MetricState::Available(state);
            signal.severity =
                Percent::new(50.0).map_or(MetricState::WarmingUp, MetricState::Available);
            signal.held_for = Some(held);
            signal.rule = signal_rule_text(id);
        }
        snapshot
    }

    /// A radar in which `id` cannot be derived at all (§4).
    fn refused(id: PressureId) -> PressureSnapshot {
        let mut snapshot = PressureSnapshot::warming_up();
        if let Some(signal) = snapshot.signals.iter_mut().find(|signal| signal.id == id) {
            signal.state = MetricState::PermissionDenied;
            signal.severity = MetricState::PermissionDenied;
            signal.held_for = None;
        }
        snapshot
    }

    fn one_second() -> Duration {
        Duration::from_secs(1)
    }

    #[test]
    fn a_transition_is_announced_once_and_not_once_per_sample() {
        // The property that makes this feature usable rather than noise: the radar
        // reports `critical` in every snapshot for as long as the condition lasts.
        let mut watch = PressureWatch::new();
        let escalation = watch.observe(&radar(
            PressureId::Cpu,
            PressureState::Critical,
            one_second(),
        ));
        assert_eq!(escalation.len(), 1, "{escalation:?}");

        for _ in 0..60 {
            assert!(
                watch
                    .observe(&radar(
                        PressureId::Cpu,
                        PressureState::Critical,
                        one_second()
                    ))
                    .is_empty(),
                "an unchanged state is not an event"
            );
        }
    }

    #[test]
    fn an_escalation_quotes_the_engines_rule_and_held_duration() {
        let mut watch = PressureWatch::new();
        let alerts = watch.observe(&radar(
            PressureId::Memory,
            PressureState::Watch,
            Duration::from_secs(12),
        ));

        let alert = alerts.first().expect("memory escalated");
        assert_eq!(alert.notice.severity, Severity::Watch);
        assert_eq!(alert.notice.kind, NoticeKind::Pressure);
        assert!(
            alert.notice.message.starts_with("MEM is now watch"),
            "{}",
            alert.notice.message
        );
        assert!(
            alert.notice.message.contains("held 12s"),
            "{}",
            alert.notice.message
        );
        assert!(
            alert
                .notice
                .message
                .contains(signal_rule_text(PressureId::Memory)),
            "§2.3's rule text is what explains the notice: {}",
            alert.notice.message
        );
        assert!(!alert.reached_critical, "watch is not critical");
    }

    #[test]
    fn a_de_escalation_is_reported_more_quietly_and_never_rings() {
        let mut watch = PressureWatch::new();
        let _ = watch.observe(&radar(
            PressureId::Cpu,
            PressureState::Critical,
            one_second(),
        ));

        let alerts = watch.observe(&radar(PressureId::Cpu, PressureState::Normal, one_second()));

        let alert = alerts.first().expect("cpu recovered");
        assert_eq!(
            alert.notice.severity,
            Severity::Info,
            "recovery is not a problem"
        );
        assert!(
            alert.notice.message.contains("was critical"),
            "{}",
            alert.notice.message
        );
        assert!(
            !alert.reached_critical,
            "leaving critical must never ring the bell"
        );
    }

    #[test]
    fn a_signal_that_goes_unavailable_is_not_a_de_escalation() {
        // §4/§26: a refused read is not "no pressure". Reporting recovery here would
        // tell the user the machine is fine on the strength of a metric nobody could
        // read.
        let mut watch = PressureWatch::new();
        let _ = watch.observe(&radar(
            PressureId::Cpu,
            PressureState::Critical,
            one_second(),
        ));

        assert!(
            watch.observe(&refused(PressureId::Cpu)).is_empty(),
            "an unavailable signal must not announce anything"
        );
        assert_eq!(watch.last_state(PressureId::Cpu), None);

        // And the state either side of the gap is not a transition either.
        let resumed = watch.observe(&radar(
            PressureId::Cpu,
            PressureState::Critical,
            one_second(),
        ));
        assert_eq!(
            resumed.len(),
            1,
            "coming back critical is news again, because the state was forgotten"
        );
    }

    #[test]
    fn a_signal_that_first_appears_normal_says_nothing() {
        // Every session starts with the whole radar warming up, so the first derived
        // `normal` is not a recovery from anything.
        let mut watch = PressureWatch::new();
        assert!(
            watch
                .observe(&radar(
                    PressureId::Load,
                    PressureState::Normal,
                    one_second()
                ))
                .is_empty()
        );
        assert_eq!(
            watch.last_state(PressureId::Load),
            Some(PressureState::Normal)
        );
    }

    #[test]
    fn a_signal_that_first_appears_critical_still_announces_itself() {
        // The hysteresis window can fill straight into `critical` — the machine was
        // already busy when monitrs started — and silence would be worse than a line
        // that names no previous state.
        let mut watch = PressureWatch::new();
        let alerts = watch.observe(&radar(
            PressureId::Cpu,
            PressureState::Critical,
            one_second(),
        ));

        let alert = alerts.first().expect("cpu escalated");
        assert!(alert.reached_critical);
        assert!(
            !alert.notice.message.contains("was "),
            "there is no previous state to name: {}",
            alert.notice.message
        );
    }

    #[test]
    fn only_an_escalation_into_critical_asks_for_a_bell() {
        let mut watch = PressureWatch::new();
        let watch_alerts =
            watch.observe(&radar(PressureId::Cpu, PressureState::Watch, one_second()));
        assert!(watch_alerts.iter().all(|alert| !alert.reached_critical));

        let critical = watch.observe(&radar(
            PressureId::Cpu,
            PressureState::Critical,
            one_second(),
        ));
        assert!(critical.iter().any(|alert| alert.reached_critical));

        // Still critical the next second: no second bell for one episode.
        assert!(
            watch
                .observe(&radar(
                    PressureId::Cpu,
                    PressureState::Critical,
                    one_second()
                ))
                .is_empty()
        );
    }

    #[test]
    fn signals_are_tracked_independently_of_each_other() {
        let mut watch = PressureWatch::new();
        let mut both = PressureSnapshot::warming_up();
        for (id, state) in [
            (PressureId::Cpu, PressureState::Critical),
            (PressureId::Swap, PressureState::Watch),
        ] {
            if let Some(signal) = both.signals.iter_mut().find(|signal| signal.id == id) {
                signal.state = MetricState::Available(state);
                signal.held_for = Some(one_second());
            }
        }

        let alerts = watch.observe(&both);

        assert_eq!(alerts.len(), 2, "{alerts:?}");
        assert_eq!(
            watch.last_state(PressureId::Cpu),
            Some(PressureState::Critical)
        );
        assert_eq!(
            watch.last_state(PressureId::Swap),
            Some(PressureState::Watch)
        );
        assert_eq!(
            watch.last_state(PressureId::Memory),
            None,
            "a warming-up signal has no remembered state"
        );
    }

    #[test]
    fn an_unsupported_signal_never_announces_anything_however_long_it_runs() {
        // A Linux-only signal on macOS: it is `Unsupported` in every sample, and
        // `Unsupported` is not a state (§4).
        let mut watch = PressureWatch::new();
        for _ in 0..30 {
            let mut snapshot = PressureSnapshot::warming_up();
            if let Some(signal) = snapshot
                .signals
                .iter_mut()
                .find(|signal| signal.id == PressureId::PsiIo)
            {
                signal.state = MetricState::Unsupported;
            }
            assert!(watch.observe(&snapshot).is_empty());
        }
    }

    #[test]
    fn a_temporarily_unavailable_signal_is_treated_the_same_as_a_refused_one() {
        let mut watch = PressureWatch::new();
        let _ = watch.observe(&radar(
            PressureId::Network,
            PressureState::Watch,
            one_second(),
        ));

        let mut snapshot = PressureSnapshot::warming_up();
        if let Some(signal) = snapshot
            .signals
            .iter_mut()
            .find(|signal| signal.id == PressureId::Network)
        {
            signal.state = MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown);
        }

        assert!(watch.observe(&snapshot).is_empty());
        assert_eq!(watch.last_state(PressureId::Network), None);
    }

    #[test]
    fn every_alert_carries_a_symbol_so_colour_is_never_the_only_cue() {
        // §5.2, through the notice log: the severity a transition is recorded at has
        // to bring its own character.
        let mut watch = PressureWatch::new();
        for state in [PressureState::Watch, PressureState::Critical] {
            let alerts = watch.observe(&radar(PressureId::Cpu, state, one_second()));
            let alert = alerts.first().expect("a transition happened");
            assert_eq!(alert.notice.symbol(), state.symbol());
        }
    }
}

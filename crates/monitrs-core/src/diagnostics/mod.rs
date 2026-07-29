//! Deterministic rules over collected evidence: the Pressure Radar (§2.3) and the
//! diagnostic findings of §11.
//!
//! **This is not an AI subsystem.** §11 opens with that sentence and the design
//! follows it literally: every conclusion here is a threshold comparison over
//! measurements the collectors already took, with the counts, the window, and the
//! confidence attached so a reader can check the arithmetic.
//!
//! # The two halves
//!
//! | | produces | state | consumer |
//! |---|---|---|---|
//! | [`PressureEngine`] | [`PressureSnapshot`](crate::model::PressureSnapshot) | hysteresis per signal | the radar panel (§5.5) |
//! | [`RuleSet`] | [`Finding`]s | none; pure functions | the Inspect diagnostics list (§7.5) |
//!
//! The engine answers "how is the system doing, stably enough to put on screen".
//! The rules answer "what should we say about it, with what evidence". Keeping them
//! apart is what makes the rules deterministic — a rule is a pure function of
//! `(snapshot, history)` — while still letting the radar refuse to flap (§11.3).
//!
//! # Ownership boundary with the collectors
//!
//! Collectors deliberately emit
//! [`PressureSnapshot::warming_up`](crate::model::PressureSnapshot::warming_up) and,
//! where the platform has it, the raw
//! [`PsiSnapshot`](crate::model::PsiSnapshot). They never derive a
//! [`PressureState`](crate::model::PressureState): that is policy, and policy in two
//! platform collectors is policy that will eventually disagree with itself. The
//! runtime owns the sequence:
//!
//! ```no_run
//! use monitrs_core::diagnostics::{HistoryWindow, PressureEngine, RuleSet};
//! use monitrs_core::history::HistoryRing;
//! use monitrs_core::model::SystemSnapshot;
//!
//! # fn tick(mut snapshot: SystemSnapshot, ring: &mut HistoryRing,
//! #         engine: &mut PressureEngine, rules: &RuleSet) {
//! let _ = ring.record(&snapshot);                 // history first
//! snapshot.pressure = engine.observe(&snapshot);  // then the radar
//! let window = HistoryWindow::live(ring);
//! let findings = rules.evaluate(&snapshot, &window); // then the findings
//! # let _ = findings;
//! # }
//! ```
//!
//! Recording before observing means the sustained rules can count the sample they
//! are being asked about. Observing before evaluating means the one rule that needs
//! hysteresis-confirmed evidence — [`rules::DiskBusyRule`], see its documentation —
//! can read it from the snapshot instead of keeping state of its own.
//!
//! # Promises this module keeps
//!
//! * **Unavailable is never healthy.** A signal whose input is missing reports the
//!   unavailability (§2.3), and [`PressureSnapshot::worst_state`](crate::model::PressureSnapshot::worst_state)
//!   reports `WarmingUp` rather than `normal` for a system nothing could be measured on.
//! * **A reset is not an event.** A counter reset, a permission failure, or a
//!   sleep/wake gap clears the hysteresis window instead of feeding it (§11.3).
//! * **No sustained claim without the samples to support it.** Below
//!   `sustained_samples` observations every signal is
//!   [`WarmingUp`](crate::MetricState::WarmingUp) and every sustained rule is silent.
//! * **No rule diagnoses an out-of-memory kill, a memory leak, a failing disk,
//!   malware, or thermal throttling.** §11.3 forbids it from ambiguous metrics, and
//!   monitrs has only ambiguous metrics for those things. The rules report what was
//!   measured and leave the conclusion to the person who knows what the machine is
//!   supposed to be doing. A test in this module asserts it of every rule id, title,
//!   and summary.
//!
//! # The §11.1 signature
//!
//! §11.1 sketches `evaluate(&SystemSnapshot, &HistoryView)`. In this codebase a
//! [`HistoryView`](crate::history::HistoryView) is only a cursor — it holds no
//! reference to the ring it indexes, so that it can be copied around application
//! state — and a rule needs both. [`HistoryWindow`] is that pair, and
//! [`HistoryWindow::view`] hands back the cursor unchanged. Evaluating against a
//! cursor rather than always against live data is also what lets the Inspect screen
//! explain a *selected* historical sample with the same rules that produced the live
//! radar (§2.1, §7.5).

mod engine;
mod finding;
mod hysteresis;
mod signals;
mod thresholds;
mod window;

pub mod rules;

#[cfg(test)]
mod fixtures;

pub use engine::PressureEngine;
pub use finding::{DiagnosticRule, Evidence, Finding, TimeWindow};
pub use hysteresis::Hysteresis;
pub use rules::{
    COLLECTOR_BEHIND, CPU_SATURATION, DISK_SUSTAINED_BUSY, LOAD_HIGH, MEMORY_AVAILABILITY_LOW,
    PROCESS_CPU_SPIKE, PROCESS_RSS_GROWTH, PSI_IO_ELEVATED, PSI_MEMORY_ELEVATED, RuleSet,
    SELF_OVERHEAD, SNAPSHOT_STALE, SWAP_ACTIVITY, ZOMBIE_PRESENT,
};
pub use signals::{SignalReading, read as read_signal, rule_text as signal_rule_text};
pub use thresholds::{
    DEFAULT_CPU_CRITICAL_PERCENT, DEFAULT_CPU_WATCH_PERCENT,
    DEFAULT_MEMORY_CRITICAL_AVAILABLE_PERCENT, DEFAULT_MEMORY_WATCH_AVAILABLE_PERCENT,
    DEFAULT_SUSTAINED_SAMPLES, DEFAULT_SUSTAINED_WINDOW, MAX_INTERVAL_MULTIPLE,
    MAX_SUSTAINED_WINDOW, MIN_INTERVAL_MULTIPLE, Thresholds,
};
pub use window::{Counted, HistoryWindow, contributor_value};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{
        Timeline, add_process, set_cpu, set_disk_busy, set_health, set_load, set_memory,
        set_network, set_psi, set_self_overhead, set_swap,
    };
    use crate::model::{
        Confidence, PressureId, PressureState, ProcessState, Severity, SystemSnapshot,
        UnavailableReason,
    };
    use crate::units::ByteUnits;
    use core::time::Duration;

    const TOTAL: u64 = 32 * 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    /// A machine in trouble in every way the §11.2 rules can observe.
    ///
    /// Returns the timeline (so the window borrows something that outlives the call)
    /// and the current snapshot with its pressure already derived, which is the
    /// order the runtime uses.
    fn distressed() -> (Timeline, SystemSnapshot) {
        let mut engine = PressureEngine::default();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut rss = 512 * MIB;
        let mut swap_used = 64 * MIB;
        let mut current = None;

        for index in 0..20u32 {
            rss = rss.saturating_add(96 * MIB);
            swap_used = swap_used.saturating_add(32 * MIB);
            let cpu_spike = index == 19;
            let mut snapshot = timeline.build(|snapshot| {
                set_cpu(snapshot, 99.0);
                set_load(snapshot, 24.0);
                set_memory(snapshot, TOTAL, TOTAL / 100);
                set_swap(
                    snapshot,
                    8 * 1024 * MIB,
                    swap_used,
                    12.0 * MIB as f64,
                    12.0 * MIB as f64,
                );
                set_disk_busy(snapshot, "nvme0n1", 99.0);
                set_network(snapshot, "en0", 95_000_000.0, 1_000.0, Some(1_000));
                set_psi(snapshot, 80.0, 60.0, 55.0);
                set_health(snapshot, Duration::from_secs(9), Duration::from_millis(600));
                set_self_overhead(snapshot, 9.0, 120 * MIB);
                add_process(
                    snapshot,
                    4_242,
                    "server",
                    Some(if cpu_spike { 287.0 } else { 12.0 }),
                    Some(rss),
                    ProcessState::Running,
                );
                for pid in 100..112 {
                    add_process(snapshot, pid, "worker", None, None, ProcessState::Zombie);
                }
            });
            // Record before deriving pressure, exactly as the runtime does.
            assert!(timeline.record(&snapshot));
            snapshot.pressure = engine.observe(&snapshot);
            current = Some(snapshot);
        }
        // The last snapshot also carries a retained value, so the staleness rule has
        // something to report.
        let mut current = current.expect("twenty samples were built");
        current.memory.swap.used = current.memory.swap.used.into_stale(Duration::from_secs(6));
        (timeline, current)
    }

    #[test]
    fn every_rule_in_section_eleven_two_can_fire() {
        let (timeline, current) = distressed();
        let findings = RuleSet::default().evaluate(&current, &timeline.window());

        let fired: Vec<&str> = findings.iter().map(|finding| finding.rule_id).collect();
        for expected in RuleSet::default().ids() {
            assert!(
                fired.contains(&expected),
                "{expected} did not fire on a distressed system; fired: {fired:?}"
            );
        }
    }

    #[test]
    fn no_rule_claims_a_diagnosis_section_eleven_three_forbids() {
        // §11.3: never diagnose OOM, a memory leak, disk failure, malware, or
        // thermal throttling from a single ambiguous metric. monitrs has only
        // ambiguous metrics for all five, so no rule may name any of them at all.
        const FORBIDDEN: [&str; 12] = [
            "oom",
            "out of memory",
            "out-of-memory",
            "memory leak",
            "leak",
            "disk failure",
            "disk failing",
            "failing disk",
            "malware",
            "virus",
            "thermal",
            "throttl",
        ];

        let (timeline, current) = distressed();
        let findings = RuleSet::default().evaluate(&current, &timeline.window());
        assert!(!findings.is_empty(), "the fixture must produce findings");

        let mut texts: Vec<String> = Vec::new();
        for id in RuleSet::default().ids() {
            texts.push(id.to_owned());
        }
        for finding in &findings {
            texts.push(finding.rule_id.to_owned());
            texts.push(finding.title.clone());
            texts.push(finding.summary.clone());
        }
        for id in PressureId::DISPLAY_ORDER {
            texts.push(signal_rule_text(id).to_owned());
        }

        for text in texts {
            let lowered = text.to_lowercase();
            for claim in FORBIDDEN {
                assert!(
                    !lowered.contains(claim),
                    "diagnostic text claims {claim:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn every_finding_carries_evidence_a_window_and_a_confidence() {
        let (timeline, current) = distressed();
        let findings = RuleSet::default().evaluate(&current, &timeline.window());

        for finding in &findings {
            assert!(
                !finding.evidence.is_empty(),
                "{} has no evidence (§11.3)",
                finding.rule_id
            );
            assert!(
                !finding.title.is_empty(),
                "{} has no title",
                finding.rule_id
            );
            assert!(
                finding.summary.len() > 20,
                "{} has no explanation: {}",
                finding.rule_id,
                finding.summary
            );
            assert!(
                finding.evidence.iter().all(|item| item.window.samples > 0),
                "{} has evidence covering no samples",
                finding.rule_id
            );
            // Every severity carries a symbol, so color is never the only cue (§5.2).
            assert!(
                ['.', '!', 'X'].contains(&finding.symbol()),
                "{} has no redundant cue",
                finding.rule_id
            );
        }
    }

    #[test]
    fn a_single_sample_rule_is_marked_low_confidence_and_a_sustained_one_is_not() {
        let (timeline, current) = distressed();
        let findings = RuleSet::default().evaluate(&current, &timeline.window());
        let by_id = |id: &str| {
            findings
                .iter()
                .find(|finding| finding.rule_id == id)
                .unwrap_or_else(|| panic!("{id} did not fire"))
        };

        assert_eq!(
            by_id(PROCESS_CPU_SPIKE).confidence,
            Confidence::Low,
            "§11.3: a rule that infers from one sample is low confidence"
        );
        assert_eq!(by_id(CPU_SATURATION).confidence, Confidence::Medium);
        assert_eq!(
            by_id(ZOMBIE_PRESENT).confidence,
            Confidence::High,
            "a count of zombies is measured, not inferred"
        );
    }

    #[test]
    fn a_healthy_machine_produces_a_normal_radar_and_no_findings() {
        let mut engine = PressureEngine::default();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut current = None;
        for _ in 0..20 {
            let mut snapshot = timeline.build(|snapshot| {
                set_cpu(snapshot, 12.0);
                set_load(snapshot, 1.2);
                set_memory(snapshot, TOTAL, TOTAL / 2);
                set_disk_busy(snapshot, "nvme0n1", 3.0);
                set_network(snapshot, "en0", 200_000.0, 90_000.0, Some(1_000));
                set_health(
                    snapshot,
                    Duration::from_millis(60),
                    Duration::from_millis(30),
                );
                set_self_overhead(snapshot, 0.4, 22 * MIB);
                add_process(
                    snapshot,
                    1,
                    "init",
                    Some(0.1),
                    Some(4 * MIB),
                    ProcessState::Sleeping,
                );
            });
            assert!(timeline.record(&snapshot));
            snapshot.pressure = engine.observe(&snapshot);
            current = Some(snapshot);
        }
        let current = current.expect("twenty samples");

        let findings = RuleSet::default().evaluate(&current, &timeline.window());
        assert!(findings.is_empty(), "{findings:#?}");
        assert_eq!(
            current.pressure.worst_state().fresh().copied(),
            Some(PressureState::Normal)
        );
        for id in [
            PressureId::Cpu,
            PressureId::Memory,
            PressureId::Disk,
            PressureId::Network,
        ] {
            let signal = current.pressure.signal(id).expect("signal exists");
            assert!(signal.state.is_available(), "{id:?} is {:?}", signal.state);
            assert_eq!(signal.symbol(), '.');
        }
    }

    #[test]
    fn a_machine_that_cannot_be_measured_does_not_read_as_healthy() {
        let mut engine = PressureEngine::default();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut current = None;
        for _ in 0..20 {
            let mut snapshot = timeline.build(|snapshot| {
                snapshot.cpu.total = crate::MetricState::PermissionDenied;
                snapshot.load = crate::MetricState::Unsupported;
                snapshot.memory.available =
                    crate::MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
            });
            assert!(timeline.record(&snapshot));
            snapshot.pressure = engine.observe(&snapshot);
            current = Some(snapshot);
        }
        let current = current.expect("twenty samples");

        assert!(
            current.pressure.worst_state().fresh().is_none(),
            "an unmeasurable system must not report a state"
        );
        for signal in &current.pressure.signals {
            assert!(
                !signal.state.is_available(),
                "{:?} derived {:?} from nothing",
                signal.id,
                signal.state
            );
            // Every unmeasured signal renders a placeholder instead of a number,
            // which is what stops it from reading as `normal` (§4).
            assert!(
                signal.state.placeholder().is_some(),
                "{:?} has no explanation for its missing state",
                signal.id
            );
            assert!(signal.severity.fresh().is_none());
        }
        assert!(
            RuleSet::default()
                .evaluate(&current, &timeline.window())
                .is_empty()
        );
    }

    #[test]
    fn findings_render_the_three_lines_the_specification_prints() {
        let (timeline, current) = distressed();
        let findings = RuleSet::default().evaluate(&current, &timeline.window());
        let cpu = findings
            .iter()
            .find(|finding| finding.rule_id == CPU_SATURATION)
            .expect("cpu saturation fired");

        assert_eq!(cpu.headline(), "CRITICAL: Sustained CPU saturation");
        let evidence = cpu.render_evidence(ByteUnits::Iec);
        assert!(evidence.contains("cpu busy"), "{evidence}");
        assert!(evidence.contains("samples"), "{evidence}");
        assert_eq!(cpu.render_confidence(), "confidence: medium");
        assert_eq!(cpu.severity, Severity::Critical);
    }

    #[test]
    fn the_engine_and_the_rules_agree_on_an_empty_ring() {
        // The very first tick: nothing recorded, nothing measured, nothing claimed.
        let timeline = Timeline::new(Duration::from_secs(1));
        let window = timeline.window();
        let snapshot = fixtures::snapshot();
        let mut engine = PressureEngine::default();

        assert!(window.is_empty());
        let radar = engine.observe(&snapshot);
        assert!(radar.worst_state().is_warming_up());
        assert!(RuleSet::default().evaluate(&snapshot, &window).is_empty());
    }
}

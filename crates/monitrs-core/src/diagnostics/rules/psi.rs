//! Linux pressure-stall information rules (§11.2, §9.2).
//!
//! PSI is the one place where the kernel does the hard part for us: it reports how
//! much time tasks spent *stalled* waiting for a resource, which is a far better
//! signal than utilization. Two properties follow, and both shape these rules:
//!
//! * A single read of `some avg10` already summarizes ten seconds, so the evidence
//!   window is those ten seconds rather than one sample — no extra counting needed.
//! * PSI exists only on Linux, and only on kernels built with it. Everywhere else
//!   these rules produce nothing at all, which is not the same as reporting that
//!   there is no pressure (§4).

use core::time::Duration;

use crate::model::{MeasuredValue, Measurement, PressureId, PsiResource, Severity, SystemSnapshot};

use super::super::{
    DiagnosticRule, Evidence, Finding, HistoryWindow, Thresholds, TimeWindow, signals,
};
use super::{SUSTAINED_CONFIDENCE, as_percent, escalate, ratio};

/// Rule id for elevated Linux memory PSI.
pub const PSI_MEMORY_ELEVATED: &str = "psi.memory_elevated";
/// Rule id for elevated Linux I/O PSI.
pub const PSI_IO_ELEVATED: &str = "psi.io_elevated";

/// The spans the three `some` averages cover.
const AVG10: Duration = Duration::from_secs(10);
const AVG60: Duration = Duration::from_secs(60);
const AVG300: Duration = Duration::from_secs(300);

/// Builds the finding shared by both PSI rules.
///
/// `noun` is the resource as it appears in prose, `waiting_for` completes the
/// sentence describing what a stall means for that resource. Neither string draws a
/// conclusion beyond what PSI measures (§11.3).
fn evaluate_psi(
    rule_id: &'static str,
    id: PressureId,
    title: &'static str,
    waiting_for: &'static str,
    thresholds: &Thresholds,
    current: &SystemSnapshot,
) -> Option<Finding> {
    let psi = current.pressure.psi.fresh()?;
    let resource: &PsiResource = signals::psi_resource(psi, id);
    let some10 = f64::from(resource.some_avg10.value());

    let severity = escalate(
        some10 >= f64::from(thresholds.psi_watch_percent),
        some10 >= f64::from(thresholds.psi_critical_percent),
    )?;
    let threshold = if severity == Severity::Critical {
        thresholds.psi_critical_percent
    } else {
        thresholds.psi_watch_percent
    };

    let mut evidence = vec![
        Evidence::new(
            Measurement::new("some avg10", MeasuredValue::Percent(resource.some_avg10)),
            TimeWindow::moving_average(AVG10),
        ),
        Evidence::new(
            Measurement::new("some avg60", MeasuredValue::Percent(resource.some_avg60)),
            TimeWindow::moving_average(AVG60),
        ),
        Evidence::new(
            Measurement::new("some avg300", MeasuredValue::Percent(resource.some_avg300)),
            TimeWindow::moving_average(AVG300),
        ),
        Evidence::current(Measurement::new(
            "threshold",
            MeasuredValue::Percent(as_percent(threshold)),
        )),
        Evidence::current(Measurement::new(
            "total stalled",
            MeasuredValue::Duration(resource.total_stalled),
        )),
    ];
    // `full` is absent for some resources on some kernels, so it is extra evidence
    // rather than part of the condition (§4).
    if let Some(full) = resource.full_avg10.fresh() {
        evidence.push(Evidence::new(
            Measurement::new("full avg10", MeasuredValue::Percent(*full)),
            TimeWindow::moving_average(AVG10),
        ));
    }

    let multiple = ratio(some10, f64::from(threshold))
        .map_or_else(String::new, |value| format!(" ({value:.1}x the threshold)"));
    let summary = format!(
        "The kernel reports at least one task stalled {waiting_for} for {} of the last 10 seconds{multiple}, \
         and {} of the last 60. A pressure share is stalled time, not utilization.",
        resource.some_avg10, resource.some_avg60,
    );

    Some(
        Finding::new(rule_id, severity, title, summary, SUSTAINED_CONFIDENCE)
            .with_evidence(evidence),
    )
}

/// Linux memory PSI elevated (§11.2).
///
/// Reports stalled time waiting on memory reclaim. It deliberately stops there:
/// §11.3 forbids concluding anything about an impending kill or an allocation
/// pattern from a stall share.
#[derive(Clone, Copy, Debug)]
pub struct MemoryPsiElevatedRule {
    thresholds: Thresholds,
}

impl MemoryPsiElevatedRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for MemoryPsiElevatedRule {
    fn id(&self) -> &'static str {
        PSI_MEMORY_ELEVATED
    }

    fn evaluate(&self, current: &SystemSnapshot, _history: &HistoryWindow<'_>) -> Option<Finding> {
        evaluate_psi(
            PSI_MEMORY_ELEVATED,
            PressureId::PsiMemory,
            "Linux memory pressure stalls elevated",
            "on memory reclaim",
            &self.thresholds,
            current,
        )
    }
}

/// Linux I/O PSI elevated (§11.2).
#[derive(Clone, Copy, Debug)]
pub struct IoPsiElevatedRule {
    thresholds: Thresholds,
}

impl IoPsiElevatedRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for IoPsiElevatedRule {
    fn id(&self) -> &'static str {
        PSI_IO_ELEVATED
    }

    fn evaluate(&self, current: &SystemSnapshot, _history: &HistoryWindow<'_>) -> Option<Finding> {
        evaluate_psi(
            PSI_IO_ELEVATED,
            PressureId::PsiIo,
            "Linux I/O pressure stalls elevated",
            "on block i/o",
            &self.thresholds,
            current,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{Timeline, set_psi, snapshot};
    use crate::model::{Confidence, MetricState};

    fn memory_rule() -> MemoryPsiElevatedRule {
        MemoryPsiElevatedRule::new(Thresholds::default().sanitized())
    }

    fn io_rule() -> IoPsiElevatedRule {
        IoPsiElevatedRule::new(Thresholds::default().sanitized())
    }

    #[test]
    fn no_psi_data_produces_no_finding_on_either_rule() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let window = timeline.window();
        let snapshot = snapshot();
        assert!(snapshot.pressure.psi.fresh().is_none());
        assert!(memory_rule().evaluate(&snapshot, &window).is_none());
        assert!(io_rule().evaluate(&snapshot, &window).is_none());
    }

    #[test]
    fn quiet_psi_produces_no_finding() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 30.0, 0.4, 0.9);
        assert!(
            memory_rule()
                .evaluate(&snapshot, &timeline.window())
                .is_none()
        );
        assert!(io_rule().evaluate(&snapshot, &timeline.window()).is_none());
    }

    #[test]
    fn each_rule_reads_only_its_own_resource() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 90.0, 12.0, 0.0);

        let memory = memory_rule()
            .evaluate(&snapshot, &timeline.window())
            .expect("memory psi is elevated");
        assert_eq!(memory.rule_id, PSI_MEMORY_ELEVATED);
        assert_eq!(memory.severity, Severity::Watch);
        assert!(
            io_rule().evaluate(&snapshot, &timeline.window()).is_none(),
            "an idle i/o resource must not inherit the memory reading"
        );
    }

    #[test]
    fn elevated_io_psi_escalates_to_critical() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 0.0, 0.0, 62.0);
        let finding = io_rule()
            .evaluate(&snapshot, &timeline.window())
            .expect("62% stalled is critical");
        assert_eq!(finding.severity, Severity::Critical);
        assert_eq!(finding.title, "Linux I/O pressure stalls elevated");
    }

    #[test]
    fn the_evidence_window_is_the_span_the_average_covers() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 0.0, 45.0, 0.0);
        let finding = memory_rule()
            .evaluate(&snapshot, &timeline.window())
            .expect("elevated memory psi");

        let avg10 = finding
            .evidence
            .iter()
            .find(|item| item.measurement.label == "some avg10")
            .expect("avg10 is evidence");
        assert_eq!(avg10.window.span, AVG10);
        assert!(
            !avg10.window.is_current_sample(),
            "one read of avg10 still covers ten seconds"
        );

        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"some avg60"), "{labels:?}");
        assert!(labels.contains(&"full avg10"), "{labels:?}");
        assert!(labels.contains(&"total stalled"), "{labels:?}");
    }

    #[test]
    fn a_kernel_without_the_full_figure_still_produces_a_finding() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 0.0, 45.0, 0.0);
        if let MetricState::Available(psi) = &mut snapshot.pressure.psi {
            psi.memory.full_avg10 = MetricState::Unsupported;
        }
        let finding = memory_rule()
            .evaluate(&snapshot, &timeline.window())
            .expect("some avg10 is enough to fire");
        assert!(
            !finding
                .evidence
                .iter()
                .any(|item| item.measurement.label == "full avg10")
        );
    }

    #[test]
    fn the_summary_says_a_pressure_share_is_not_a_utilization() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 0.0, 45.0, 0.0);
        let finding = memory_rule()
            .evaluate(&snapshot, &timeline.window())
            .expect("elevated memory psi");
        assert!(
            finding.summary.contains("not utilization"),
            "{}",
            finding.summary
        );
        assert_eq!(finding.confidence, Confidence::Medium);
    }

    #[test]
    fn stale_psi_does_not_produce_a_finding() {
        let timeline = Timeline::new(Duration::from_secs(1));
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 0.0, 99.0, 0.0);
        snapshot.pressure.psi = snapshot.pressure.psi.into_stale(Duration::from_secs(5));
        assert!(
            memory_rule()
                .evaluate(&snapshot, &timeline.window())
                .is_none(),
            "a retained value is not a current measurement (§4)"
        );
    }
}

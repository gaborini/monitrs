//! Sustained CPU saturation and high load (§11.2).

use crate::history::{ContributorMetric, HistoryMetric};
use crate::model::{MeasuredValue, Measurement, Severity, SystemSnapshot};
use crate::units::format_duration;

use super::super::{DiagnosticRule, Evidence, Finding, HistoryWindow, Thresholds};
use super::{
    SUSTAINED_CONFIDENCE, as_count, as_percent, coverage_sentence, escalate, percent_contributors,
};

/// Rule id for sustained CPU saturation.
pub const CPU_SATURATION: &str = "cpu.sustained_saturation";
/// Rule id for load high relative to the logical CPU count.
pub const LOAD_HIGH: &str = "load.high_per_cpu";

/// Aggregate CPU utilization sustained above its threshold (§11.2).
///
/// Counts *history*, not the current sample: one busy tick is a compile finishing,
/// and §11.3 requires a minimum number of samples before a sustained claim.
#[derive(Clone, Copy, Debug)]
pub struct SustainedCpuSaturationRule {
    thresholds: Thresholds,
}

impl SustainedCpuSaturationRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for SustainedCpuSaturationRule {
    fn id(&self) -> &'static str {
        CPU_SATURATION
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let thresholds = &self.thresholds;
        let span = thresholds.sustained_window;
        let required = thresholds.sustained_samples;
        let minimum = thresholds.minimum_samples();

        let watch = history.count_at_least(
            HistoryMetric::CpuBusy,
            span,
            f64::from(thresholds.cpu_watch_percent),
        );
        let critical = history.count_at_least(
            HistoryMetric::CpuBusy,
            span,
            f64::from(thresholds.cpu_critical_percent),
        );
        let severity = escalate(
            watch.sustained(required, minimum),
            critical.sustained(required, minimum),
        )?;
        let (counted, threshold) = if severity == Severity::Critical {
            (critical, thresholds.cpu_critical_percent)
        } else {
            (watch, thresholds.cpu_watch_percent)
        };

        let mut evidence = vec![
            Evidence::new(
                Measurement::new(
                    "samples at or above threshold",
                    MeasuredValue::Count(as_count(counted.matched)),
                ),
                counted.window(),
            ),
            Evidence::current(Measurement::new(
                "threshold",
                MeasuredValue::Percent(as_percent(threshold)),
            )),
        ];
        if let Some(usage) = current.cpu.total.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "cpu busy",
                MeasuredValue::Percent(usage.busy),
            )));
        }
        if let Some(load) = current.load.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "load1",
                MeasuredValue::Load(load.one),
            )));
        }
        if let Some(total) = current.total_process_cpu() {
            evidence.push(Evidence::current(Measurement::new(
                "observed process cpu",
                MeasuredValue::Percent(total),
            )));
        }

        let mut summary = format!(
            "CPU busy at or above {} in {} of the last {} samples ({}).",
            as_percent(threshold),
            counted.matched,
            counted.considered,
            format_duration(counted.span),
        );
        if let Some(sample) = history.selected() {
            if let Some(contributors) =
                percent_contributors(&sample.contributors, ContributorMetric::Cpu)
            {
                summary.push_str(&format!(" Top observed contributors: {contributors}."));
            }
            if let Some(coverage) =
                coverage_sentence(&sample.contributors, ContributorMetric::Cpu, "cpu")
            {
                summary.push_str(&coverage);
            }
        }

        Some(
            Finding::new(
                CPU_SATURATION,
                severity,
                "Sustained CPU saturation",
                summary,
                SUSTAINED_CONFIDENCE,
            )
            .with_evidence(evidence),
        )
    }
}

/// One-minute load sustained high relative to the logical CPU count (§11.2).
///
/// Normalized per CPU, because a load of eleven is unremarkable on 64 cores and
/// severe on two. The summary states what load actually counts, since on Linux it
/// includes tasks blocked in uninterruptible I/O and is therefore not a CPU
/// utilization figure.
#[derive(Clone, Copy, Debug)]
pub struct LoadHighRule {
    thresholds: Thresholds,
}

impl LoadHighRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for LoadHighRule {
    fn id(&self) -> &'static str {
        LOAD_HIGH
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let thresholds = &self.thresholds;
        let logical = current.cpu.logical_count;
        if logical == 0 {
            // Without a CPU count there is nothing to normalize against, and an
            // absolute load average is not comparable to any threshold (§8.3).
            return None;
        }
        let cpus = f64::from(logical);
        let span = thresholds.sustained_window;
        let required = thresholds.sustained_samples;
        let minimum = thresholds.minimum_samples();

        let watch = history.count_at_least(
            HistoryMetric::LoadOne,
            span,
            f64::from(thresholds.load_watch_per_cpu) * cpus,
        );
        let critical = history.count_at_least(
            HistoryMetric::LoadOne,
            span,
            f64::from(thresholds.load_critical_per_cpu) * cpus,
        );
        let severity = escalate(
            watch.sustained(required, minimum),
            critical.sustained(required, minimum),
        )?;
        let (counted, per_cpu_threshold) = if severity == Severity::Critical {
            (critical, thresholds.load_critical_per_cpu)
        } else {
            (watch, thresholds.load_watch_per_cpu)
        };

        let mut evidence = vec![
            Evidence::new(
                Measurement::new(
                    "samples at or above threshold",
                    MeasuredValue::Count(as_count(counted.matched)),
                ),
                counted.window(),
            ),
            Evidence::current(Measurement::new(
                "logical cpus",
                MeasuredValue::Count(u64::from(logical)),
            )),
            Evidence::current(Measurement::new(
                "threshold per cpu",
                MeasuredValue::Load(per_cpu_threshold),
            )),
        ];
        let mut current_per_cpu = None;
        if let Some(load) = current.load.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "load1",
                MeasuredValue::Load(load.one),
            )));
            if let Some(per_cpu) = load.per_cpu(logical) {
                current_per_cpu = Some(per_cpu);
                evidence.push(Evidence::current(Measurement::new(
                    "load1 per cpu",
                    MeasuredValue::Load(per_cpu),
                )));
            }
        }

        let observed = current_per_cpu
            .map(|per_cpu| format!("{per_cpu:.2} per logical cpu"))
            // The current reading may be unavailable even though the window was
            // sustained; the counts below still carry the finding.
            .unwrap_or_else(|| "elevated".to_owned());
        let summary = format!(
            "One-minute load is {observed} on {logical} logical cpus, at or above {per_cpu_threshold:.2} \
             per cpu in {} of the last {} samples ({}). Load counts runnable tasks and, on Linux, \
             tasks blocked in uninterruptible i/o, so it is not a cpu utilization figure.",
            counted.matched,
            counted.considered,
            format_duration(counted.span),
        );

        Some(
            Finding::new(
                LOAD_HIGH,
                severity,
                "Load high relative to logical CPU count",
                summary,
                SUSTAINED_CONFIDENCE,
            )
            .with_evidence(evidence),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{
        Timeline, add_process, set_cpu, set_load, set_memory, snapshot,
    };
    use crate::model::{MetricState, ProcessState, UnavailableReason};
    use core::time::Duration;

    fn rule() -> SustainedCpuSaturationRule {
        SustainedCpuSaturationRule::new(Thresholds::default().sanitized())
    }

    fn load_rule() -> LoadHighRule {
        LoadHighRule::new(Thresholds::default().sanitized())
    }

    fn timeline() -> Timeline {
        Timeline::new(Duration::from_secs(1))
    }

    #[test]
    fn an_idle_system_produces_no_cpu_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_cpu(snapshot, 4.0));
        assert!(rule().evaluate(&current, &timeline.window()).is_none());
    }

    #[test]
    fn a_single_busy_sample_is_not_a_sustained_finding() {
        let mut timeline = timeline();
        timeline.push_many(19, |snapshot| set_cpu(snapshot, 3.0));
        let current = timeline.push(|snapshot| set_cpu(snapshot, 99.0));
        assert!(
            rule().evaluate(&current, &timeline.window()).is_none(),
            "§11.3 requires a minimum number of samples"
        );
    }

    #[test]
    fn nothing_fires_before_the_minimum_sample_count_is_reached() {
        let mut timeline = timeline();
        let current = timeline.push_many(9, |snapshot| set_cpu(snapshot, 99.0));
        assert!(
            rule().evaluate(&current, &timeline.window()).is_none(),
            "nine samples cannot support a ten-sample claim"
        );
    }

    #[test]
    fn ten_of_the_last_fifteen_samples_above_the_watch_threshold_is_a_watch() {
        let mut timeline = timeline();
        timeline.push_many(5, |snapshot| set_cpu(snapshot, 10.0));
        let current = timeline.push_many(10, |snapshot| set_cpu(snapshot, 85.0));

        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("ten of fifteen above 80% is sustained");
        assert_eq!(finding.severity, Severity::Watch);
        assert_eq!(finding.rule_id, CPU_SATURATION);
        assert_eq!(finding.confidence, crate::model::Confidence::Medium);
        assert!(
            finding.summary.contains("10 of the last 15 samples"),
            "{}",
            finding.summary
        );
    }

    #[test]
    fn sustained_saturation_escalates_to_critical() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_cpu(snapshot, 97.0));
        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("sustained above 95%");
        assert_eq!(finding.severity, Severity::Critical);
        assert_eq!(finding.symbol(), 'X');
    }

    #[test]
    fn the_finding_carries_raw_evidence_and_a_time_window() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_cpu(snapshot, 97.0);
            set_load(snapshot, 11.4);
        });
        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("sustained saturation");

        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"cpu busy"), "{labels:?}");
        assert!(labels.contains(&"load1"), "{labels:?}");
        assert!(
            labels.contains(&"samples at or above threshold"),
            "{labels:?}"
        );

        let counted = finding
            .evidence
            .iter()
            .find(|item| item.measurement.label == "samples at or above threshold")
            .expect("the count is evidence");
        assert_eq!(counted.window.samples, 15);
        assert_eq!(counted.window.span, Duration::from_secs(14));
    }

    #[test]
    fn the_summary_names_top_contributors_without_claiming_causation() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_cpu(snapshot, 97.0);
            set_memory(snapshot, 32 * 1024 * 1024 * 1024, 8 * 1024 * 1024 * 1024);
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                None,
                ProcessState::Running,
            );
            add_process(
                snapshot,
                1_221,
                "postgres",
                Some(54.0),
                None,
                ProcessState::Sleeping,
            );
        });
        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("sustained saturation");

        assert!(
            finding
                .summary
                .contains("Top observed contributors: rustc 287%"),
            "{}",
            finding.summary
        );
        assert!(
            finding.summary.contains("account for"),
            "the coverage sentence is evidence, not proof: {}",
            finding.summary
        );
        for forbidden in ["caused", "because of", "responsible for"] {
            assert!(
                !finding.summary.contains(forbidden),
                "§2.2 forbids claiming causation: {}",
                finding.summary
            );
        }
    }

    #[test]
    fn unavailable_samples_do_not_count_towards_saturation() {
        let mut timeline = timeline();
        timeline.push_many(10, |snapshot| set_cpu(snapshot, 99.0));
        let current = timeline.push_many(10, |snapshot| {
            snapshot.cpu.total =
                MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset);
        });
        assert!(
            rule().evaluate(&current, &timeline.window()).is_none(),
            "a counter reset is not a saturated sample"
        );
    }

    #[test]
    fn an_empty_history_produces_no_finding() {
        let timeline = timeline();
        assert!(rule().evaluate(&snapshot(), &timeline.window()).is_none());
        assert!(
            load_rule()
                .evaluate(&snapshot(), &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn load_is_judged_per_logical_cpu() {
        let mut timeline = timeline();
        // 7.9 on eight cpus is below one per cpu.
        let quiet = timeline.push_many(20, |snapshot| set_load(snapshot, 7.9));
        assert!(load_rule().evaluate(&quiet, &timeline.window()).is_none());

        let mut busy_timeline = Timeline::new(Duration::from_secs(1));
        let busy = busy_timeline.push_many(20, |snapshot| set_load(snapshot, 11.4));
        let finding = load_rule()
            .evaluate(&busy, &busy_timeline.window())
            .expect("1.4 per cpu is above the watch threshold");
        assert_eq!(finding.severity, Severity::Watch);
        assert!(
            finding.summary.contains("8 logical cpus"),
            "{}",
            finding.summary
        );
        assert!(
            finding.summary.contains("uninterruptible"),
            "the summary must say what load actually counts: {}",
            finding.summary
        );
    }

    #[test]
    fn load_escalates_to_critical_at_two_per_cpu() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_load(snapshot, 24.0));
        let finding = load_rule()
            .evaluate(&current, &timeline.window())
            .expect("3.0 per cpu is critical");
        assert_eq!(finding.severity, Severity::Critical);
        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"load1 per cpu"), "{labels:?}");
        assert!(labels.contains(&"logical cpus"), "{labels:?}");
    }

    #[test]
    fn load_without_a_cpu_count_produces_nothing_rather_than_a_guess() {
        let mut timeline = timeline();
        let mut current = timeline.push_many(20, |snapshot| set_load(snapshot, 99.0));
        current.cpu.logical_count = 0;
        assert!(load_rule().evaluate(&current, &timeline.window()).is_none());
    }

    #[test]
    fn a_missing_load_average_produces_no_load_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            snapshot.load = MetricState::Unsupported;
        });
        assert!(load_rule().evaluate(&current, &timeline.window()).is_none());
    }
}

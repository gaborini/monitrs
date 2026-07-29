//! Memory availability and swap activity (§11.2).

use crate::history::{ContributorMetric, HistoryMetric};
use crate::model::{Confidence, MeasuredValue, Measurement, Severity, SystemSnapshot};
use crate::units::{Percent, format_duration};

use super::super::{DiagnosticRule, Evidence, Finding, HistoryWindow, Thresholds};
use super::{SUSTAINED_CONFIDENCE, as_count, as_percent, escalate, ratio, share_contributors};

/// Rule id for low available memory.
pub const MEMORY_AVAILABILITY_LOW: &str = "memory.availability_low";
/// Rule id for swap in/out activity.
pub const SWAP_ACTIVITY: &str = "memory.swap_activity";

/// Available memory sustained below its threshold (§11.2).
///
/// Judged against the ceiling that actually applies to this process tree — the
/// cgroup limit where there is one, the host total otherwise (§9.2) — and reported
/// alongside the platform's memory semantics, because §8.4 forbids treating the two
/// definitions as interchangeable.
///
/// The finding says *available memory is low*. It draws no conclusion about what
/// will happen next: §11.3 forbids diagnosing a kill or an allocation failure from
/// an availability figure.
#[derive(Clone, Copy, Debug)]
pub struct MemoryAvailabilityLowRule {
    thresholds: Thresholds,
}

impl MemoryAvailabilityLowRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for MemoryAvailabilityLowRule {
    fn id(&self) -> &'static str {
        MEMORY_AVAILABILITY_LOW
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let thresholds = &self.thresholds;
        let span = thresholds.sustained_window;
        let required = thresholds.sustained_samples;
        let minimum = thresholds.minimum_samples();

        // History retains the *used* share, so the available-share thresholds are
        // counted in used terms (§8.5).
        let watch = history.count_at_least(
            HistoryMetric::MemoryUsedShare,
            span,
            f64::from(thresholds.memory_watch_used_percent()),
        );
        let critical = history.count_at_least(
            HistoryMetric::MemoryUsedShare,
            span,
            f64::from(thresholds.memory_critical_used_percent()),
        );
        let severity = escalate(
            watch.sustained(required, minimum),
            critical.sustained(required, minimum),
        )?;
        let (counted, available_threshold) = if severity == Severity::Critical {
            (critical, thresholds.memory_critical_available_percent)
        } else {
            (watch, thresholds.memory_watch_available_percent)
        };

        let limit = current.memory.effective_limit_bytes();
        let mut evidence = vec![
            Evidence::new(
                Measurement::new(
                    "samples at or below the available threshold",
                    MeasuredValue::Count(as_count(counted.matched)),
                ),
                counted.window(),
            ),
            Evidence::current(Measurement::new(
                "available threshold",
                MeasuredValue::Percent(as_percent(available_threshold)),
            )),
            Evidence::current(Measurement::new(
                "memory limit",
                MeasuredValue::Bytes(limit),
            )),
        ];

        let mut available_share = None;
        if let Some(&available) = current.memory.available.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "available",
                MeasuredValue::Bytes(available),
            )));
            if let Some(share) = Percent::ratio(available, limit) {
                available_share = Some(share);
                evidence.push(Evidence::current(Measurement::new(
                    "available share",
                    MeasuredValue::Percent(share),
                )));
            }
        }
        if let Some(&swap_used) = current.memory.swap.used.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "swap used",
                MeasuredValue::Bytes(swap_used),
            )));
        }

        let observed = available_share.map_or_else(
            || "Available memory".to_owned(),
            |share| format!("Available memory is {share} of the effective limit and"),
        );
        let mut summary = format!(
            "{observed} was at or below {} in {} of the last {} samples ({}). Memory accounting: {}.",
            as_percent(available_threshold),
            counted.matched,
            counted.considered,
            format_duration(counted.span),
            current.memory.semantics.description(),
        );
        if let Some(sample) = history.selected()
            && let Some(contributors) = share_contributors(
                &sample.contributors,
                ContributorMetric::ResidentMemory,
                current.memory.total_bytes,
            )
        {
            summary.push_str(&format!(
                " Largest observed resident sets: {contributors} of total memory."
            ));
        }

        Some(
            Finding::new(
                MEMORY_AVAILABILITY_LOW,
                severity,
                "Available memory low",
                summary,
                SUSTAINED_CONFIDENCE,
            )
            .with_evidence(evidence),
        )
    }
}

/// Swap being read back or written out (§11.2).
///
/// A large but idle swap file is unremarkable; the metric that matters is the
/// *rate*, which is why this rule ignores swap usage as a trigger and reports it
/// only as supporting evidence.
///
/// Confidence is [`Confidence::Low`] when only the instantaneous rate supports the
/// finding, and rises to medium when swap in use also grew across the window —
/// §11.3 requires a one-sample inference to be marked as one.
#[derive(Clone, Copy, Debug)]
pub struct SwapActivityRule {
    thresholds: Thresholds,
}

impl SwapActivityRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for SwapActivityRule {
    fn id(&self) -> &'static str {
        SWAP_ACTIVITY
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let thresholds = &self.thresholds;
        let swap = &current.memory.swap;
        if !swap.is_enabled() {
            return None;
        }
        let (Some(in_rate), Some(out_rate)) = (swap.in_rate.fresh(), swap.out_rate.fresh()) else {
            // §26: a platform that does not report swap rates has not reported
            // zero swap activity.
            return None;
        };
        let total = in_rate.per_second() + out_rate.per_second();
        let severity = escalate(
            total >= thresholds.swap_watch_bytes_per_second,
            total >= thresholds.swap_critical_bytes_per_second,
        )?;
        let threshold = if severity == Severity::Critical {
            thresholds.swap_critical_bytes_per_second
        } else {
            thresholds.swap_watch_bytes_per_second
        };

        let mut evidence = vec![
            Evidence::current(Measurement::new(
                "swap in",
                MeasuredValue::ByteRate(*in_rate),
            )),
            Evidence::current(Measurement::new(
                "swap out",
                MeasuredValue::ByteRate(*out_rate),
            )),
        ];
        if let Some(&used) = swap.used.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "swap used",
                MeasuredValue::Bytes(used),
            )));
        }
        if let Some(&available) = current.memory.available.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "available",
                MeasuredValue::Bytes(available),
            )));
        }

        // A rising amount of swap in use across the window is independent
        // corroboration; without it this is one sample of evidence.
        let growth = history
            .trend(HistoryMetric::SwapUsed, thresholds.sustained_window)
            .filter(|(_, _, span)| !span.is_zero());
        let mut confidence = Confidence::Low;
        let mut corroboration = String::new();
        if let Some((first, last, span)) = growth
            && last > first
        {
            confidence = SUSTAINED_CONFIDENCE;
            corroboration = format!(
                " Swap in use also grew over the last {}.",
                format_duration(span)
            );
            // The difference of two byte counts is a byte count; it is only
            // floating point because history stores comparable scalars, and the
            // guard above plus `max(0.0)` make the narrowing lossless in practice.
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let grown = (last - first).max(0.0) as u64;
            evidence.push(Evidence::new(
                Measurement::new("swap used growth", MeasuredValue::Bytes(grown)),
                crate::diagnostics::TimeWindow::new(span, 2),
            ));
        }

        let multiple = ratio(total, threshold)
            .map_or_else(String::new, |value| format!(" ({value:.1}x the threshold)"));
        let summary = format!(
            "Pages are being moved between memory and swap{multiple}. Swap activity is the metric \
             that indicates memory distress; a large but idle swap area is not.{corroboration}"
        );

        Some(
            Finding::new(
                SWAP_ACTIVITY,
                severity,
                "Swap in/out activity",
                summary,
                confidence,
            )
            .with_evidence(evidence),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{Timeline, add_process, set_memory, set_swap, snapshot};
    use crate::model::{MetricState, ProcessState};
    use core::time::Duration;

    const TOTAL: u64 = 32 * 1024 * 1024 * 1024;
    const SWAP_TOTAL: u64 = 8 * 1024 * 1024 * 1024;

    fn memory_rule() -> MemoryAvailabilityLowRule {
        MemoryAvailabilityLowRule::new(Thresholds::default().sanitized())
    }

    fn swap_rule() -> SwapActivityRule {
        SwapActivityRule::new(Thresholds::default().sanitized())
    }

    fn timeline() -> Timeline {
        Timeline::new(Duration::from_secs(1))
    }

    /// `percent` of total memory available.
    fn available(percent: u64) -> u64 {
        TOTAL / 100 * percent
    }

    #[test]
    fn plenty_of_available_memory_produces_no_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_memory(snapshot, TOTAL, available(60)));
        assert!(
            memory_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn sustained_low_availability_is_a_watch() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_memory(snapshot, TOTAL, available(12)));
        let finding = memory_rule()
            .evaluate(&current, &timeline.window())
            .expect("12% available is below the 15% watch threshold");

        assert_eq!(finding.severity, Severity::Watch);
        assert_eq!(finding.title, "Available memory low");
        assert!(
            finding.summary.contains("Memory accounting:"),
            "{}",
            finding.summary
        );
    }

    #[test]
    fn sustained_very_low_availability_is_critical() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_memory(snapshot, TOTAL, available(3)));
        let finding = memory_rule()
            .evaluate(&current, &timeline.window())
            .expect("3% available is below the 5% critical threshold");
        assert_eq!(finding.severity, Severity::Critical);
    }

    #[test]
    fn a_brief_dip_in_availability_is_not_a_finding() {
        let mut timeline = timeline();
        timeline.push_many(19, |snapshot| set_memory(snapshot, TOTAL, available(50)));
        let current = timeline.push(|snapshot| set_memory(snapshot, TOTAL, available(1)));
        assert!(
            memory_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn the_finding_reports_bytes_as_evidence_and_shares_in_the_summary() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| set_memory(snapshot, TOTAL, available(4)));
        let finding = memory_rule()
            .evaluate(&current, &timeline.window())
            .expect("sustained low availability");

        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"available"), "{labels:?}");
        assert!(labels.contains(&"memory limit"), "{labels:?}");
        assert!(labels.contains(&"available share"), "{labels:?}");

        // Byte counts are a display decision, so they must not be baked into text.
        assert!(!finding.summary.contains("GiB"), "{}", finding.summary);
        assert!(!finding.summary.contains(" GB"), "{}", finding.summary);
    }

    #[test]
    fn a_cgroup_limit_is_the_ceiling_the_finding_is_measured_against() {
        let limit = 2 * 1024 * 1024 * 1024;
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            // Comfortable against the host total, critical against the container.
            set_memory(snapshot, TOTAL, 100 * 1024 * 1024);
            snapshot.memory.cgroup_limit_bytes = MetricState::Available(limit);
            snapshot.memory.usage = Percent::ratio(limit - 100 * 1024 * 1024, limit)
                .map_or(MetricState::Unsupported, MetricState::Available);
        });
        let finding = memory_rule()
            .evaluate(&current, &timeline.window())
            .expect("100 MiB of a 2 GiB limit is critical");
        assert_eq!(finding.severity, Severity::Critical);
        let limit_evidence = finding
            .evidence
            .iter()
            .find(|item| item.measurement.label == "memory limit")
            .expect("the ceiling is evidence");
        assert_eq!(
            limit_evidence.measurement.value,
            MeasuredValue::Bytes(limit)
        );
    }

    #[test]
    fn unavailable_memory_samples_do_not_count_as_low_availability() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            snapshot.memory.total_bytes = TOTAL;
            snapshot.memory.usage = MetricState::PermissionDenied;
            snapshot.memory.available = MetricState::PermissionDenied;
        });
        assert!(
            memory_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn the_summary_names_the_largest_resident_sets_as_shares() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_memory(snapshot, TOTAL, available(4));
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                Some(2_600_000_000),
                ProcessState::Running,
            );
        });
        let finding = memory_rule()
            .evaluate(&current, &timeline.window())
            .expect("sustained low availability");
        assert!(
            finding
                .summary
                .contains("Largest observed resident sets: rustc"),
            "{}",
            finding.summary
        );
    }

    #[test]
    fn no_swap_configured_produces_no_swap_finding() {
        let timeline = timeline();
        assert!(
            swap_rule()
                .evaluate(&snapshot(), &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn an_idle_swap_area_produces_no_finding_however_full_it_is() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_swap(snapshot, SWAP_TOTAL, SWAP_TOTAL - 1024, 0.0, 0.0);
        });
        assert!(
            swap_rule().evaluate(&current, &timeline.window()).is_none(),
            "capacity in use is not activity"
        );
    }

    #[test]
    fn swap_activity_from_one_sample_is_marked_low_confidence() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_swap(snapshot, SWAP_TOTAL, 1024, 2_000_000.0, 0.0);
        });
        let finding = swap_rule()
            .evaluate(&current, &timeline.window())
            .expect("2 MiB/s is above the watch threshold");

        assert_eq!(finding.severity, Severity::Watch);
        assert_eq!(
            finding.confidence,
            Confidence::Low,
            "§11.3: an inference from one sample is low confidence"
        );
        assert!(
            finding.summary.contains("1.9x the threshold"),
            "2 MB/s against a 1 MiB/s threshold: {}",
            finding.summary
        );
    }

    #[test]
    fn growing_swap_usage_raises_the_confidence() {
        let mut timeline = timeline();
        let mut used = 1024u64;
        let current = timeline.push_many(20, move |snapshot| {
            used = used.saturating_add(16 * 1024 * 1024);
            set_swap(snapshot, SWAP_TOTAL, used, 2_000_000.0, 0.0);
        });
        let finding = swap_rule()
            .evaluate(&current, &timeline.window())
            .expect("swap is active");
        assert_eq!(finding.confidence, Confidence::Medium);
        assert!(finding.summary.contains("also grew"), "{}", finding.summary);
        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"swap used growth"), "{labels:?}");
    }

    #[test]
    fn heavy_paging_escalates_to_critical() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_swap(snapshot, SWAP_TOTAL, 1024, 12_000_000.0, 12_000_000.0);
        });
        let finding = swap_rule()
            .evaluate(&current, &timeline.window())
            .expect("24 MiB/s combined is critical");
        assert_eq!(finding.severity, Severity::Critical);
    }

    #[test]
    fn a_platform_without_swap_rates_produces_no_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_swap(snapshot, SWAP_TOTAL, 1024, 0.0, 0.0);
            snapshot.memory.swap.in_rate = MetricState::Unsupported;
            snapshot.memory.swap.out_rate = MetricState::Unsupported;
        });
        assert!(swap_rule().evaluate(&current, &timeline.window()).is_none());
    }
}

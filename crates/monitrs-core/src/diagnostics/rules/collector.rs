//! Rules about the monitor itself: collector lag, stale data, and self-overhead
//! (§11.2, §16.1).
//!
//! §26 is blunt about why these exist: *a system monitor must measure and expose its
//! own overhead.* These three rules are the part of that promise that speaks up
//! without being asked, and all three are directly measured rather than inferred,
//! so they carry [`Confidence::High`].

use core::time::Duration;

use crate::model::{Confidence, MeasuredValue, Measurement, MetricState, Severity, SystemSnapshot};
use crate::units::{Percent, format_age, format_duration};

use super::super::{DiagnosticRule, Evidence, Finding, HistoryWindow, Thresholds};
use super::{as_count, as_percent, ratio};

/// Rule id for a collector that cannot keep up.
pub const COLLECTOR_BEHIND: &str = "collector.falling_behind";
/// Rule id for a snapshot showing data older than it should be.
pub const SNAPSHOT_STALE: &str = "collector.snapshot_stale";
/// Rule id for monitrs exceeding one of its own §16.1 budgets.
pub const SELF_OVERHEAD: &str = "self.overhead_above_budget";

/// The multiple of a budget at which exceeding it becomes critical.
const CRITICAL_BUDGET_MULTIPLE: f64 = 2.0;

/// The collector is delivering snapshots later than the configured interval
/// (§11.2, §16.2).
#[derive(Clone, Copy, Debug)]
pub struct CollectorBehindRule {
    thresholds: Thresholds,
}

impl CollectorBehindRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for CollectorBehindRule {
    fn id(&self) -> &'static str {
        COLLECTOR_BEHIND
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let interval = history.expected_interval();
        if interval.is_zero() {
            return None;
        }
        let lag = current.health.lag.as_secs_f64();
        let watch = Thresholds::intervals_as_seconds(
            interval,
            self.thresholds.collector_lag_watch_intervals,
        );
        let critical = Thresholds::intervals_as_seconds(
            interval,
            self.thresholds.collector_lag_critical_intervals,
        );
        let severity = super::escalate(lag >= watch, lag >= critical)?;

        let evidence = vec![
            Evidence::current(Measurement::new(
                "lag",
                MeasuredValue::Duration(current.health.lag),
            )),
            Evidence::current(Measurement::new(
                "sample interval",
                MeasuredValue::Duration(interval),
            )),
            Evidence::current(Measurement::new(
                "fast collection p95",
                MeasuredValue::Duration(current.health.fast.p95_duration),
            )),
            Evidence::current(Measurement::new(
                "dropped samples",
                MeasuredValue::Count(current.health.dropped_samples),
            )),
            Evidence::current(Measurement::new(
                "coalesced samples",
                MeasuredValue::Count(current.health.coalesced_samples),
            )),
        ];

        let intervals = ratio(lag, interval.as_secs_f64()).map_or_else(String::new, |value| {
            format!(" ({value:.1} sample intervals)")
        });
        let summary = format!(
            "The newest snapshot is {} behind live{intervals}. Displayed values are real \
             measurements taken later than intended, not predictions; expensive enrichment is \
             reduced before samples are dropped.",
            format_age(current.health.lag),
        );

        Some(
            Finding::new(
                COLLECTOR_BEHIND,
                severity,
                "Collector falling behind",
                summary,
                Confidence::High,
            )
            .with_evidence(evidence),
        )
    }
}

/// The snapshot is showing retained values, or follows an unexpectedly long gap
/// (§11.2, §7.5).
///
/// Two different problems with the same consequence: what is on screen is older than
/// one sample interval. §4 requires a retained value to be shown with its age, and
/// this rule is the summary of that state for the diagnostics panel.
#[derive(Clone, Copy, Debug)]
pub struct SnapshotStaleRule {
    thresholds: Thresholds,
}

impl SnapshotStaleRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

/// How many of a snapshot's headline metrics are showing retained values, and the
/// oldest such age.
///
/// Deliberately a fixed list of the metrics the header and overview render (§5.5):
/// a stale reading in a panel nobody is looking at is not what the warning is for.
fn stale_headline_metrics(snapshot: &SystemSnapshot) -> (usize, Duration) {
    let mut count = 0usize;
    let mut oldest = Duration::ZERO;
    let mut note = |age: Option<Duration>| {
        if let Some(age) = age {
            count = count.saturating_add(1);
            oldest = oldest.max(age);
        }
    };

    note(stale_age(&snapshot.cpu.total));
    note(stale_age(&snapshot.memory.available));
    note(stale_age(&snapshot.memory.used));
    note(stale_age(&snapshot.memory.swap.used));
    note(stale_age(&snapshot.load));
    for disk in &snapshot.disks {
        note(stale_age(&disk.read));
        note(stale_age(&disk.write));
    }
    for interface in &snapshot.networks {
        note(stale_age(&interface.rx));
        note(stale_age(&interface.tx));
    }
    (count, oldest)
}

/// The age of a retained value, or `None` when the metric is not stale.
fn stale_age<T>(state: &MetricState<T>) -> Option<Duration> {
    match state {
        MetricState::Stale { age, .. } => Some(*age),
        _ => None,
    }
}

impl DiagnosticRule for SnapshotStaleRule {
    fn id(&self) -> &'static str {
        SNAPSHOT_STALE
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let interval = history.expected_interval();
        if interval.is_zero() {
            return None;
        }
        let watch =
            Thresholds::intervals_as_seconds(interval, self.thresholds.stale_watch_intervals);
        let critical =
            Thresholds::intervals_as_seconds(interval, self.thresholds.stale_critical_intervals);

        let (stale_count, oldest) = stale_headline_metrics(current);
        // A first snapshot has no interval at all, which is warming up rather than
        // a gap (§8.2).
        let gap = if current.has_valid_interval() {
            current.elapsed
        } else {
            Duration::ZERO
        };
        let worst = oldest.max(gap).as_secs_f64();
        let triggered = stale_count > 0 || gap.as_secs_f64() >= watch;
        if !triggered || worst < watch {
            return None;
        }
        let severity = if worst >= critical {
            Severity::Critical
        } else {
            Severity::Watch
        };

        let mut evidence = vec![
            Evidence::current(Measurement::new(
                "sample interval",
                MeasuredValue::Duration(interval),
            )),
            Evidence::current(Measurement::new(
                "interval since previous sample",
                MeasuredValue::Duration(gap),
            )),
            Evidence::current(Measurement::new(
                "stale headline metrics",
                MeasuredValue::Count(as_count(stale_count)),
            )),
        ];
        if !oldest.is_zero() {
            evidence.push(Evidence::current(Measurement::new(
                "oldest retained value",
                MeasuredValue::Duration(oldest),
            )));
        }

        let mut summary = String::new();
        if stale_count > 0 {
            summary.push_str(&format!(
                "{stale_count} headline metric(s) are showing retained values, the oldest {} old. ",
                format_age(oldest)
            ));
        }
        if gap.as_secs_f64() >= watch {
            summary.push_str(&format!(
                "The interval since the previous sample was {}, against a configured {}. ",
                format_duration(gap),
                format_duration(interval)
            ));
        }
        summary.push_str(
            "Readings either side of a gap are not comparable, and rates across it were not \
             computed from an assumed interval.",
        );

        Some(
            Finding::new(
                SNAPSHOT_STALE,
                severity,
                "Snapshot data stale",
                summary,
                Confidence::High,
            )
            .with_evidence(evidence),
        )
    }
}

/// monitrs is over one of its own §16.1 budgets.
#[derive(Clone, Copy, Debug)]
pub struct SelfOverheadRule {
    thresholds: Thresholds,
}

impl SelfOverheadRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for SelfOverheadRule {
    fn id(&self) -> &'static str {
        SELF_OVERHEAD
    }

    fn evaluate(&self, current: &SystemSnapshot, _history: &HistoryWindow<'_>) -> Option<Finding> {
        let overhead = current.health.self_overhead.as_ref()?;
        let thresholds = &self.thresholds;

        let cpu_budget = f64::from(thresholds.self_cpu_budget_percent);
        let rss_budget = thresholds.self_rss_budget_bytes as f64;
        let sample_budget = thresholds.self_sample_budget();

        let cpu_over =
            ratio(f64::from(overhead.cpu.value()), cpu_budget).filter(|over| *over >= 1.0);
        let rss_over = ratio(overhead.rss_bytes as f64, rss_budget).filter(|over| *over >= 1.0);
        let sample_over = ratio(
            current.health.fast.p95_duration.as_secs_f64(),
            sample_budget.as_secs_f64(),
        )
        .filter(|over| *over >= 1.0);

        let exceeded: Vec<(&str, f64)> = [
            ("cpu", cpu_over),
            ("resident memory", rss_over),
            ("sample duration", sample_over),
        ]
        .into_iter()
        .filter_map(|(label, over)| over.map(|over| (label, over)))
        .collect();
        let worst = exceeded
            .iter()
            .map(|(_, over)| *over)
            .fold(0.0f64, f64::max);
        if exceeded.is_empty() {
            return None;
        }
        let severity = if worst >= CRITICAL_BUDGET_MULTIPLE {
            Severity::Critical
        } else {
            Severity::Watch
        };

        let mut evidence = vec![
            Evidence::current(Measurement::new(
                "self cpu",
                MeasuredValue::Percent(overhead.cpu),
            )),
            Evidence::current(Measurement::new(
                "self cpu budget",
                MeasuredValue::Percent(as_percent(thresholds.self_cpu_budget_percent)),
            )),
            Evidence::current(Measurement::new(
                "self resident memory",
                MeasuredValue::Bytes(overhead.rss_bytes),
            )),
            Evidence::current(Measurement::new(
                "self resident memory budget",
                MeasuredValue::Bytes(thresholds.self_rss_budget_bytes),
            )),
            Evidence::current(Measurement::new(
                "history bytes",
                MeasuredValue::Bytes(overhead.history_bytes),
            )),
            Evidence::current(Measurement::new(
                "fast collection p95",
                MeasuredValue::Duration(current.health.fast.p95_duration),
            )),
            Evidence::current(Measurement::new(
                "fast collection budget",
                MeasuredValue::Duration(sample_budget),
            )),
        ];
        if let Some(&open_files) = overhead.open_files.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "open files",
                MeasuredValue::Count(u64::from(open_files)),
            )));
        }

        let named: Vec<String> = exceeded
            .iter()
            .map(|(label, over)| format!("{label} {over:.1}x budget"))
            .collect();
        let summary = format!(
            "monitrs is over its own budget: {}. A monitor that costs more than what it measures \
             is a bug in the monitor, not a property of the system.",
            named.join(", ")
        );

        Some(
            Finding::new(
                SELF_OVERHEAD,
                severity,
                "monitrs self-overhead above budget",
                summary,
                Confidence::High,
            )
            .with_evidence(evidence),
        )
    }
}

/// The share of a budget a measurement occupies, for the Inspect screen (§7.5).
///
/// Returns `None` when the budget is zero, because a share of nothing is undefined
/// rather than infinite.
#[must_use]
pub fn budget_share(measured: f64, budget: f64) -> Option<Percent> {
    let share = ratio(measured, budget)?;
    // Narrowing a calculated percentage to f32 is what `Percent` stores; a value the
    // narrowing could not represent is rejected by `Percent::new` rather than shown.
    #[allow(clippy::cast_possible_truncation)]
    let percent = (share * 100.0) as f32;
    Percent::new(percent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{
        Timeline, set_cpu, set_health, set_self_overhead, snapshot,
    };

    const MIB: u64 = 1024 * 1024;

    fn behind_rule() -> CollectorBehindRule {
        CollectorBehindRule::new(Thresholds::default().sanitized())
    }

    fn stale_rule() -> SnapshotStaleRule {
        SnapshotStaleRule::new(Thresholds::default().sanitized())
    }

    fn overhead_rule() -> SelfOverheadRule {
        SelfOverheadRule::new(Thresholds::default().sanitized())
    }

    fn timeline() -> Timeline {
        Timeline::new(Duration::from_secs(1))
    }

    #[test]
    fn a_collector_keeping_up_produces_no_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(5, |snapshot| {
            set_cpu(snapshot, 10.0);
            set_health(
                snapshot,
                Duration::from_millis(120),
                Duration::from_millis(40),
            );
        });
        assert!(
            behind_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn lag_beyond_two_intervals_is_a_watch() {
        let mut timeline = timeline();
        let current = timeline.push_many(5, |snapshot| {
            set_health(
                snapshot,
                Duration::from_millis(2_500),
                Duration::from_millis(400),
            );
        });
        let finding = behind_rule()
            .evaluate(&current, &timeline.window())
            .expect("2.5s of lag on a 1s interval");
        assert_eq!(finding.severity, Severity::Watch);
        assert_eq!(finding.confidence, Confidence::High);
        assert!(
            finding.summary.contains("2.5 sample intervals"),
            "{}",
            finding.summary
        );
    }

    #[test]
    fn lag_beyond_five_intervals_is_critical() {
        let mut timeline = timeline();
        let current = timeline.push_many(5, |snapshot| {
            set_health(snapshot, Duration::from_secs(9), Duration::from_millis(900));
        });
        let finding = behind_rule()
            .evaluate(&current, &timeline.window())
            .expect("9s of lag on a 1s interval");
        assert_eq!(finding.severity, Severity::Critical);
        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"lag"), "{labels:?}");
        assert!(labels.contains(&"dropped samples"), "{labels:?}");
        assert!(labels.contains(&"coalesced samples"), "{labels:?}");
    }

    #[test]
    fn lag_is_judged_against_the_configured_interval_not_one_second() {
        // A 2.5s lag is fine when samples are five seconds apart.
        let mut slow = Timeline::new(Duration::from_secs(5));
        let current = slow.push_many(3, |snapshot| {
            set_health(
                snapshot,
                Duration::from_millis(2_500),
                Duration::from_millis(400),
            );
        });
        assert!(
            behind_rule().evaluate(&current, &slow.window()).is_none(),
            "§8.1 forbids assuming a one-second interval"
        );
    }

    #[test]
    fn a_fresh_snapshot_is_not_stale() {
        let mut timeline = timeline();
        let current = timeline.push_many(5, |snapshot| set_cpu(snapshot, 10.0));
        assert!(
            stale_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn retained_headline_values_are_reported_with_their_age() {
        let mut timeline = timeline();
        let mut current = timeline.push_many(5, |snapshot| set_cpu(snapshot, 10.0));
        current.cpu.total = current.cpu.total.into_stale(Duration::from_secs(4));

        let finding = stale_rule()
            .evaluate(&current, &timeline.window())
            .expect("a retained value four seconds old");
        assert_eq!(finding.severity, Severity::Watch);
        assert!(
            finding.summary.contains("retained values"),
            "{}",
            finding.summary
        );
        let count = finding
            .evidence
            .iter()
            .find(|item| item.measurement.label == "stale headline metrics")
            .expect("the count is evidence");
        assert_eq!(count.measurement.value, MeasuredValue::Count(1));
    }

    #[test]
    fn a_briefly_retained_value_is_not_worth_a_warning() {
        let mut timeline = timeline();
        let mut current = timeline.push_many(5, |snapshot| set_cpu(snapshot, 10.0));
        current.cpu.total = current.cpu.total.into_stale(Duration::from_millis(1_500));
        assert!(
            stale_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "one and a half intervals is within tolerance"
        );
    }

    #[test]
    fn a_long_gap_between_samples_is_reported_as_stale_data() {
        let mut timeline = timeline();
        timeline.push_many(3, |snapshot| set_cpu(snapshot, 10.0));
        let mut current = timeline.build(|snapshot| set_cpu(snapshot, 10.0));
        current.elapsed = Duration::from_secs(30);

        let finding = stale_rule()
            .evaluate(&current, &timeline.window())
            .expect("a thirty second gap on a one second interval");
        assert_eq!(finding.severity, Severity::Critical);
        assert!(finding.summary.contains("30s"), "{}", finding.summary);
        assert!(
            finding.summary.contains("not comparable"),
            "{}",
            finding.summary
        );
    }

    #[test]
    fn the_first_snapshot_is_warming_up_rather_than_stale() {
        let timeline = timeline();
        let current = snapshot();
        assert!(!current.has_valid_interval());
        assert!(
            stale_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn no_measured_overhead_produces_no_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(3, |snapshot| set_cpu(snapshot, 10.0));
        assert!(current.health.self_overhead.is_none());
        assert!(
            overhead_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn overhead_inside_budget_produces_no_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(3, |snapshot| {
            set_health(snapshot, Duration::ZERO, Duration::from_millis(80));
            set_self_overhead(snapshot, 0.8, 30 * MIB);
        });
        assert!(
            overhead_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn our_own_cpu_above_budget_is_reported_against_the_budget() {
        let mut timeline = timeline();
        let current = timeline.push_many(3, |snapshot| {
            set_health(snapshot, Duration::ZERO, Duration::from_millis(80));
            set_self_overhead(snapshot, 3.0, 30 * MIB);
        });
        let finding = overhead_rule()
            .evaluate(&current, &timeline.window())
            .expect("3% against a 2% budget");
        assert_eq!(finding.severity, Severity::Watch);
        assert!(
            finding.summary.contains("cpu 1.5x budget"),
            "{}",
            finding.summary
        );

        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"self cpu budget"), "{labels:?}");
        assert!(labels.contains(&"history bytes"), "{labels:?}");
        assert!(labels.contains(&"open files"), "{labels:?}");
    }

    #[test]
    fn double_the_budget_is_critical_and_names_every_breach() {
        let mut timeline = timeline();
        let current = timeline.push_many(3, |snapshot| {
            set_health(snapshot, Duration::ZERO, Duration::from_millis(600));
            set_self_overhead(snapshot, 9.0, 120 * MIB);
        });
        let finding = overhead_rule()
            .evaluate(&current, &timeline.window())
            .expect("every budget is exceeded");
        assert_eq!(finding.severity, Severity::Critical);
        for expected in ["cpu", "resident memory", "sample duration"] {
            assert!(finding.summary.contains(expected), "{}", finding.summary);
        }
    }

    #[test]
    fn a_slow_collection_alone_is_enough_to_report_overhead() {
        let mut timeline = timeline();
        let current = timeline.push_many(3, |snapshot| {
            set_health(snapshot, Duration::ZERO, Duration::from_millis(250));
            set_self_overhead(snapshot, 0.5, 20 * MIB);
        });
        let finding = overhead_rule()
            .evaluate(&current, &timeline.window())
            .expect("250ms against a 200ms budget");
        assert!(
            finding.summary.contains("sample duration"),
            "{}",
            finding.summary
        );
    }

    #[test]
    fn a_budget_share_of_a_zero_budget_is_undefined_rather_than_infinite() {
        assert!(budget_share(1.0, 0.0).is_none());
        let share = budget_share(1.0, 4.0).expect("a quarter of the budget");
        assert!((share.value() - 25.0).abs() < f32::EPSILON);
    }
}

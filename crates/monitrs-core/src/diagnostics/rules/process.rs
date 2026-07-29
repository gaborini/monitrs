//! Per-process rules: resident-set growth, zombies, and CPU spikes (§11.2).

use core::time::Duration;

use crate::history::{ContributorMetric, HistoricalSample};
use crate::model::{
    Confidence, MeasuredValue, Measurement, ProcessIdentity, ProcessSnapshot, ProcessState,
    Severity, SystemSnapshot,
};
use crate::units::{Ellipsis, Percent, Rate, format_duration, truncate_tail};

use super::super::{
    DiagnosticRule, Evidence, Finding, HistoryWindow, Thresholds, TimeWindow, contributor_value,
};
use super::{SUSTAINED_CONFIDENCE, as_count, as_percent};

/// Rule id for a process whose resident set is growing steadily.
pub const PROCESS_RSS_GROWTH: &str = "process.rss_increasing";
/// Rule id for the presence of unreaped processes.
pub const ZOMBIE_PRESENT: &str = "process.zombie_present";
/// Rule id for a sudden rise in one process's CPU usage.
pub const PROCESS_CPU_SPIKE: &str = "process.cpu_spike";

/// Display width process names are truncated to in summaries (§5.4).
const NAME_WIDTH: usize = 24;

/// How many zombie names a summary lists.
const SUMMARY_ZOMBIES: usize = 3;

/// Seconds per minute, for the growth rate.
const SECONDS_PER_MINUTE: f64 = 60.0;

/// A process whose resident set has been rising across the window (§11.2).
///
/// # What this rule does not say
///
/// It reports *growth*, with the samples and the span it was measured over, and
/// nothing else. §11.3 forbids concluding why memory is growing from a size series:
/// a cache filling to its configured bound, a JIT warming up, and a genuine bug all
/// look identical here, and only the user knows which is expected.
///
/// Evidence comes from the retained contributor lists (§2.2), which are bounded to
/// the top `K` per sample. A process that dropped out of the top `K` leaves a gap
/// rather than a zero, and gaps do not count towards the minimum sample requirement.
#[derive(Clone, Copy, Debug)]
pub struct ProcessRssGrowthRule {
    thresholds: Thresholds,
}

impl ProcessRssGrowthRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

/// One process's retained resident-set series.
struct RssSeries {
    first: f64,
    last: f64,
    span: Duration,
    points: usize,
    rises: usize,
    comparisons: usize,
}

impl RssSeries {
    /// Collects a process's retained RSS values from the window, oldest first.
    fn collect(samples: &[&HistoricalSample], identity: ProcessIdentity) -> Option<Self> {
        let mut values: Vec<(f64, Duration)> = Vec::new();
        for sample in samples {
            if let Some(value) =
                contributor_value(sample, ContributorMetric::ResidentMemory, identity)
            {
                values.push((value, sample.monotonic_offset));
            }
        }
        let (first, first_at) = *values.first()?;
        let (last, last_at) = *values.last()?;
        let rises = values
            .iter()
            .zip(values.iter().skip(1))
            .filter(|(earlier, later)| later.0 > earlier.0)
            .count();
        Some(Self {
            first,
            last,
            span: last_at.saturating_sub(first_at),
            points: values.len(),
            rises,
            comparisons: values.len().saturating_sub(1),
        })
    }

    /// Growth in bytes per minute across the series, or `None` if it did not grow.
    fn per_minute(&self) -> Option<f64> {
        let seconds = self.span.as_secs_f64();
        if seconds <= 0.0 {
            return None;
        }
        let growth = self.last - self.first;
        (growth > 0.0).then(|| growth / seconds * SECONDS_PER_MINUTE)
    }

    /// Growth as a share of where the series started.
    fn growth_percent(&self) -> Option<Percent> {
        if self.first <= 0.0 {
            return None;
        }
        // A calculated percentage is what `Percent` stores; anything the narrowing
        // could not represent is rejected by `Percent::new` rather than displayed.
        #[allow(clippy::cast_possible_truncation)]
        let percent = ((self.last - self.first) / self.first * 100.0) as f32;
        Percent::new(percent)
    }
}

impl DiagnosticRule for ProcessRssGrowthRule {
    fn id(&self) -> &'static str {
        PROCESS_RSS_GROWTH
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let thresholds = &self.thresholds;
        let minimum = thresholds.minimum_samples();
        let samples: Vec<&HistoricalSample> = history
            .recent(thresholds.sustained_window)
            .filter(|sample| sample.sequence <= current.sequence)
            .collect();
        if samples.len() < minimum {
            return None;
        }

        let mut worst: Option<(&ProcessSnapshot, RssSeries, f64)> = None;
        for process in &current.processes {
            let Some(&rss) = process.memory.rss_bytes.fresh() else {
                continue;
            };
            if rss < thresholds.process_rss_minimum_bytes {
                continue;
            }
            let Some(series) = RssSeries::collect(&samples, process.identity) else {
                continue;
            };
            if series.points < minimum {
                continue;
            }
            // A sawtooth rises about half the time and ends where it started, so
            // the end-to-end rate check below is what rejects it. This only filters
            // series that mostly fall.
            if series.rises * 2 < series.comparisons {
                continue;
            }
            let Some(per_minute) = series.per_minute() else {
                continue;
            };
            let threshold = thresholds.process_rss_growth_bytes_per_minute as f64;
            if per_minute < threshold {
                continue;
            }
            if worst
                .as_ref()
                .is_none_or(|(_, _, current_rate)| per_minute > *current_rate)
            {
                worst = Some((process, series, per_minute));
            }
        }

        let (process, series, per_minute) = worst?;
        let window = TimeWindow::new(series.span, series.points);
        // The difference of two resident set sizes is a byte count (§10.4); it is
        // only floating point because history stores comparable scalars, and the
        // rule above only reaches here when the difference is positive.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let growth_bytes = (series.last - series.first).max(0.0) as u64;

        let mut evidence = vec![
            Evidence::new(
                Measurement::new("resident growth", MeasuredValue::Bytes(growth_bytes)),
                window,
            ),
            Evidence::new(
                Measurement::new(
                    "retained samples",
                    MeasuredValue::Count(as_count(series.points)),
                ),
                window,
            ),
            Evidence::current(Measurement::new(
                "pid",
                MeasuredValue::Count(u64::from(process.identity.pid)),
            )),
        ];
        if let Some(&rss) = process.memory.rss_bytes.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "resident set",
                MeasuredValue::Bytes(rss),
            )));
        }
        if let Some(rate) = Rate::new(per_minute / SECONDS_PER_MINUTE) {
            evidence.push(Evidence::new(
                Measurement::new("growth rate", MeasuredValue::ByteRate(rate)),
                window,
            ));
        }
        if let Some(share) = process.memory.share_of_total.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "share of total memory",
                MeasuredValue::Percent(*share),
            )));
        }

        let name = truncate_tail(&process.name, NAME_WIDTH, Ellipsis::Ascii);
        let grown = series
            .growth_percent()
            .map_or_else(String::new, |percent| format!(" by {percent}"));
        let summary = format!(
            "{name} (pid {}) resident memory grew{grown} over {}, rising in {} of {} retained \
             samples. This is an observed trend, not a diagnosis of its cause; a cache filling to \
             its configured bound looks the same.",
            process.identity.pid,
            format_duration(series.span),
            series.rises,
            series.comparisons,
        );

        Some(
            Finding::new(
                PROCESS_RSS_GROWTH,
                Severity::Watch,
                "Process resident memory rising",
                summary,
                SUSTAINED_CONFIDENCE,
            )
            .with_evidence(evidence),
        )
    }
}

/// Processes that have exited and not been reaped (§11.2).
///
/// Directly measured, so the confidence is high — but what it *means* is left to
/// the reader: a zombie is a normal instant in a process's teardown, and only a
/// growing count suggests a parent that is not reaping. That is why the escalation
/// to `watch` needs a count rather than a single occurrence.
#[derive(Clone, Copy, Debug)]
pub struct ZombieProcessRule {
    thresholds: Thresholds,
}

impl ZombieProcessRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for ZombieProcessRule {
    fn id(&self) -> &'static str {
        ZOMBIE_PRESENT
    }

    fn evaluate(&self, current: &SystemSnapshot, _history: &HistoryWindow<'_>) -> Option<Finding> {
        let zombies: Vec<&ProcessSnapshot> = current
            .processes
            .iter()
            .filter(|process| process.state == ProcessState::Zombie)
            .collect();
        if zombies.is_empty() {
            return None;
        }
        let severity = if zombies.len() >= self.thresholds.zombie_watch_count {
            Severity::Watch
        } else {
            Severity::Info
        };

        let evidence = vec![
            Evidence::current(Measurement::new(
                "zombie processes",
                MeasuredValue::Count(as_count(zombies.len())),
            )),
            Evidence::current(Measurement::new(
                "processes",
                MeasuredValue::Count(as_count(current.process_count())),
            )),
            Evidence::current(Measurement::new(
                "watch threshold",
                MeasuredValue::Count(as_count(self.thresholds.zombie_watch_count)),
            )),
        ];

        let named: Vec<String> = zombies
            .iter()
            .take(SUMMARY_ZOMBIES)
            .map(|process| {
                format!(
                    "{} (pid {})",
                    truncate_tail(&process.name, NAME_WIDTH, Ellipsis::Ascii),
                    process.identity.pid
                )
            })
            .collect();
        let summary = format!(
            "{} process(es) have exited without being reaped by their parent: {}. A zombie holds \
             only a process table entry, and signalling one has no effect.",
            zombies.len(),
            named.join(", "),
        );

        Some(
            Finding::new(
                ZOMBIE_PRESENT,
                severity,
                "Unreaped (zombie) processes present",
                summary,
                Confidence::High,
            )
            .with_evidence(evidence),
        )
    }
}

/// One process's CPU usage rising sharply between two samples (§11.2).
///
/// Explicitly a one-sample inference, and therefore [`Confidence::Low`] as §11.3
/// requires. The previous value comes from the retained contributor evidence, keyed
/// on the full [`ProcessIdentity`], so a reused PID cannot be reported as a spike in
/// the process that used to hold it (§26).
#[derive(Clone, Copy, Debug)]
pub struct ProcessCpuSpikeRule {
    thresholds: Thresholds,
}

impl ProcessCpuSpikeRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for ProcessCpuSpikeRule {
    fn id(&self) -> &'static str {
        PROCESS_CPU_SPIKE
    }

    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding> {
        let thresholds = &self.thresholds;
        let previous = history.previous_sample(current.sequence)?;

        let mut worst: Option<(&ProcessSnapshot, Percent, f32)> = None;
        for process in &current.processes {
            let Some(&cpu) = process.cpu.fresh() else {
                continue;
            };
            if cpu.value() < thresholds.process_cpu_spike_percent {
                continue;
            }
            let Some(earlier) =
                contributor_value(previous, ContributorMetric::Cpu, process.identity)
            else {
                // Not in the previous retained set: §8.2 makes a first delta
                // warming up, not a spike.
                continue;
            };
            // The retained value was an f32 percentage before history widened it to
            // a comparable scalar, so narrowing it back is lossless.
            #[allow(clippy::cast_possible_truncation)]
            let rise = cpu.value() - earlier as f32;
            if rise < thresholds.process_cpu_spike_points {
                continue;
            }
            if worst
                .as_ref()
                .is_none_or(|(_, _, current_rise)| rise > *current_rise)
            {
                #[allow(clippy::cast_possible_truncation)]
                let earlier_percent = as_percent(earlier as f32);
                worst = Some((process, earlier_percent, rise));
            }
        }

        let (process, earlier, rise) = worst?;
        let span = if current.elapsed.is_zero() {
            history.expected_interval()
        } else {
            current.elapsed
        };
        let window = TimeWindow::new(span, 2);

        let mut evidence = vec![
            Evidence::current(Measurement::new(
                "pid",
                MeasuredValue::Count(u64::from(process.identity.pid)),
            )),
            Evidence::new(
                Measurement::new("previous cpu", MeasuredValue::Percent(earlier)),
                window,
            ),
            Evidence::new(
                Measurement::new("rise", MeasuredValue::Percent(as_percent(rise))),
                window,
            ),
        ];
        if let Some(&cpu) = process.cpu.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "process cpu",
                MeasuredValue::Percent(cpu),
            )));
        }
        if let Some(usage) = current.cpu.total.fresh() {
            evidence.push(Evidence::current(Measurement::new(
                "cpu busy",
                MeasuredValue::Percent(usage.busy),
            )));
        }

        let name = truncate_tail(&process.name, NAME_WIDTH, Ellipsis::Ascii);
        let now = process
            .cpu
            .fresh()
            .map_or_else(String::new, |cpu| format!(" to {cpu}"));
        let summary = format!(
            "{name} (pid {}) rose {} points{now} between two samples ({}). One sample of \
             correlation: it is not an explanation of what the system is doing.",
            process.identity.pid,
            as_percent(rise),
            format_duration(span),
        );

        Some(
            Finding::new(
                PROCESS_CPU_SPIKE,
                Severity::Info,
                "Process CPU spike",
                summary,
                Confidence::Low,
            )
            .with_evidence(evidence),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{Timeline, add_process, set_cpu, set_memory, snapshot};
    use crate::model::MetricState;

    const TOTAL: u64 = 32 * 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    fn growth_rule() -> ProcessRssGrowthRule {
        ProcessRssGrowthRule::new(Thresholds::default().sanitized())
    }

    fn zombie_rule() -> ZombieProcessRule {
        ZombieProcessRule::new(Thresholds::default().sanitized())
    }

    fn spike_rule() -> ProcessCpuSpikeRule {
        ProcessCpuSpikeRule::new(Thresholds::default().sanitized())
    }

    fn timeline() -> Timeline {
        Timeline::new(Duration::from_secs(1))
    }

    #[test]
    fn a_process_with_a_steady_resident_set_produces_no_finding() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                Some(512 * MIB),
                ProcessState::Running,
            );
        });
        assert!(
            growth_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn a_steadily_growing_resident_set_is_reported_as_growth_not_as_a_cause() {
        let mut timeline = timeline();
        let mut rss = 512 * MIB;
        let current = timeline.push_many(20, move |snapshot| {
            rss = rss.saturating_add(64 * MIB);
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                Some(rss),
                ProcessState::Running,
            );
        });

        let finding = growth_rule()
            .evaluate(&current, &timeline.window())
            .expect("64 MiB per second is far above 64 MiB per minute");
        assert_eq!(finding.rule_id, PROCESS_RSS_GROWTH);
        assert_eq!(finding.severity, Severity::Watch);
        assert_eq!(finding.confidence, Confidence::Medium);
        assert!(
            finding.summary.contains("server (pid 4242)"),
            "{}",
            finding.summary
        );
        assert!(
            finding.summary.contains("not a diagnosis of its cause"),
            "{}",
            finding.summary
        );

        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"resident growth"), "{labels:?}");
        assert!(labels.contains(&"growth rate"), "{labels:?}");
        assert!(labels.contains(&"retained samples"), "{labels:?}");
    }

    #[test]
    fn a_sawtooth_resident_set_is_not_growth() {
        let mut timeline = timeline();
        let mut index = 0u64;
        let current = timeline.push_many(20, move |snapshot| {
            index += 1;
            // The series ends on the same value the window begins on, so there is
            // no end-to-end growth however often it rose.
            let rss = if index.is_multiple_of(2) {
                900 * MIB
            } else {
                500 * MIB
            };
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                Some(rss),
                ProcessState::Running,
            );
        });
        assert!(
            growth_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "a process that returns to where it started has not grown"
        );
    }

    #[test]
    fn a_small_process_doubling_is_ignored() {
        let mut timeline = timeline();
        let mut rss = MIB;
        let current = timeline.push_many(20, move |snapshot| {
            rss = rss.saturating_mul(2).min(64 * MIB);
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "tiny",
                Some(1.0),
                Some(rss),
                ProcessState::Running,
            );
        });
        assert!(
            growth_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "below the minimum resident set the noise is not worth reporting"
        );
    }

    #[test]
    fn growth_needs_the_minimum_number_of_retained_samples() {
        let mut timeline = timeline();
        let mut rss = 512 * MIB;
        let current = timeline.push_many(5, move |snapshot| {
            rss = rss.saturating_add(128 * MIB);
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                Some(rss),
                ProcessState::Running,
            );
        });
        assert!(
            growth_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn a_reused_pid_does_not_inherit_the_previous_processes_growth() {
        // The first process grows, exits, and its pid is reused by a new process
        // whose resident set is large but new. Keyed on identity, the new process
        // has no history at all (§26).
        let mut timeline = timeline();
        let mut rss = 512 * MIB;
        timeline.push_many(15, move |snapshot| {
            rss = rss.saturating_add(64 * MIB);
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                Some(rss),
                ProcessState::Running,
            );
        });
        let mut current = timeline.build(|snapshot| {
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                Some(4 * 1024 * MIB),
                ProcessState::Running,
            );
        });
        // Same pid, different start key: a different process.
        if let Some(process) = current.processes.first_mut() {
            process.identity = ProcessIdentity::new(4_242, 999_999);
        }
        assert!(
            growth_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "a reused pid must not inherit another process's series"
        );
    }

    #[test]
    fn an_unmeasured_resident_set_is_not_growth() {
        let mut timeline = timeline();
        let current = timeline.push_many(20, |snapshot| {
            set_memory(snapshot, TOTAL, TOTAL / 2);
            add_process(
                snapshot,
                4_242,
                "server",
                Some(5.0),
                None,
                ProcessState::Running,
            );
        });
        assert!(
            growth_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn no_zombies_means_no_finding() {
        let timeline = timeline();
        let mut current = snapshot();
        add_process(
            &mut current,
            1,
            "init",
            Some(0.0),
            None,
            ProcessState::Sleeping,
        );
        assert!(
            zombie_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn one_zombie_is_informational_rather_than_a_warning() {
        let timeline = timeline();
        let mut current = snapshot();
        add_process(
            &mut current,
            1,
            "init",
            Some(0.0),
            None,
            ProcessState::Sleeping,
        );
        add_process(
            &mut current,
            9_182,
            "node",
            None,
            None,
            ProcessState::Zombie,
        );

        let finding = zombie_rule()
            .evaluate(&current, &timeline.window())
            .expect("presence is reported");
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(finding.confidence, Confidence::High);
        assert!(
            finding.summary.contains("node (pid 9182)"),
            "{}",
            finding.summary
        );
        assert!(
            finding.summary.contains("no effect"),
            "§15.1: signalling a zombie does nothing: {}",
            finding.summary
        );
    }

    #[test]
    fn a_pile_of_zombies_escalates_to_watch() {
        let timeline = timeline();
        let mut current = snapshot();
        for pid in 100..112 {
            add_process(
                &mut current,
                pid,
                "worker",
                None,
                None,
                ProcessState::Zombie,
            );
        }
        let finding = zombie_rule()
            .evaluate(&current, &timeline.window())
            .expect("twelve zombies is above the watch threshold");
        assert_eq!(finding.severity, Severity::Watch);
        let count = finding
            .evidence
            .iter()
            .find(|item| item.measurement.label == "zombie processes")
            .expect("the count is evidence");
        assert_eq!(count.measurement.value, MeasuredValue::Count(12));
    }

    #[test]
    fn a_spike_needs_a_previous_sample_to_compare_against() {
        let mut timeline = timeline();
        let current = timeline.push(|snapshot| {
            set_cpu(snapshot, 90.0);
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                None,
                ProcessState::Running,
            );
        });
        assert!(
            spike_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "§8.2: the first delta sample is warming up, not a spike"
        );
    }

    #[test]
    fn a_sharp_rise_in_one_process_is_reported_with_low_confidence() {
        let mut timeline = timeline();
        timeline.push_many(3, |snapshot| {
            set_cpu(snapshot, 20.0);
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(12.0),
                None,
                ProcessState::Running,
            );
        });
        let current = timeline.build(|snapshot| {
            set_cpu(snapshot, 95.0);
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                None,
                ProcessState::Running,
            );
        });

        let finding = spike_rule()
            .evaluate(&current, &timeline.window())
            .expect("a 275 point rise is a spike");
        assert_eq!(finding.severity, Severity::Info);
        assert_eq!(
            finding.confidence,
            Confidence::Low,
            "§11.3: one sample of evidence is low confidence"
        );
        assert!(
            finding.summary.contains("rustc (pid 31842)"),
            "{}",
            finding.summary
        );
        assert!(
            finding.summary.contains("not an explanation"),
            "§2.2 forbids claiming causation: {}",
            finding.summary
        );

        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"previous cpu"), "{labels:?}");
        assert!(labels.contains(&"rise"), "{labels:?}");
    }

    #[test]
    fn a_process_below_the_spike_floor_is_ignored_however_fast_it_rose() {
        let mut timeline = timeline();
        timeline.push_many(3, |snapshot| {
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(1.0),
                None,
                ProcessState::Running,
            );
        });
        let current = timeline.build(|snapshot| {
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(80.0),
                None,
                ProcessState::Running,
            );
        });
        assert!(
            spike_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "80% of one core is not a spike worth reporting"
        );
    }

    #[test]
    fn a_new_process_at_high_cpu_is_not_a_spike() {
        let mut timeline = timeline();
        timeline.push_many(3, |snapshot| {
            add_process(snapshot, 1, "init", Some(0.0), None, ProcessState::Sleeping);
        });
        let current = timeline.build(|snapshot| {
            add_process(snapshot, 1, "init", Some(0.0), None, ProcessState::Sleeping);
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                None,
                ProcessState::Running,
            );
        });
        assert!(
            spike_rule()
                .evaluate(&current, &timeline.window())
                .is_none(),
            "a process with no previous retained value has no delta (§8.2)"
        );
    }

    #[test]
    fn an_unmeasured_process_cpu_is_not_a_spike() {
        let mut timeline = timeline();
        timeline.push_many(3, |snapshot| {
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(12.0),
                None,
                ProcessState::Running,
            );
        });
        let mut current = timeline.build(|snapshot| {
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                None,
                ProcessState::Running,
            );
        });
        if let Some(process) = current.processes.first_mut() {
            process.cpu = MetricState::PermissionDenied;
        }
        assert!(
            spike_rule()
                .evaluate(&current, &timeline.window())
                .is_none()
        );
    }

    #[test]
    fn the_largest_rise_is_the_one_reported() {
        let mut timeline = timeline();
        timeline.push_many(3, |snapshot| {
            add_process(
                snapshot,
                1_221,
                "postgres",
                Some(10.0),
                None,
                ProcessState::Running,
            );
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(10.0),
                None,
                ProcessState::Running,
            );
        });
        let current = timeline.build(|snapshot| {
            add_process(
                snapshot,
                1_221,
                "postgres",
                Some(120.0),
                None,
                ProcessState::Running,
            );
            add_process(
                snapshot,
                31_842,
                "rustc",
                Some(287.0),
                None,
                ProcessState::Running,
            );
        });
        let finding = spike_rule()
            .evaluate(&current, &timeline.window())
            .expect("both rose; the larger rise wins");
        assert!(finding.summary.contains("rustc"), "{}", finding.summary);
    }
}

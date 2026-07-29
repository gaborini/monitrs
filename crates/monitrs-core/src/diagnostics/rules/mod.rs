//! The §11.2 rules, and the set that evaluates all of them.
//!
//! Each rule is a small, stateless struct holding a copy of the thresholds it
//! compares against. That shape is what makes the rules deterministic: given the
//! same snapshot and the same history they always produce the same finding, which
//! is what §21 M5 means by "diagnostic rules have deterministic tests".
//!
//! # What a rule may and may not say
//!
//! §11.3 draws a hard line: **never** diagnose out-of-memory kills, a memory leak,
//! a failing disk, malware, or thermal throttling from a single ambiguous metric.
//! No rule here names any of those things, and
//! [`crate::diagnostics`]'s own tests assert it. What the rules do instead is
//! report the measurement, the count, and the window, and mark how much of a
//! conclusion the evidence really supports:
//!
//! * [`Confidence::High`] — the finding *is* the measurement (a zombie count, our
//!   own CPU usage, the collector's lag).
//! * [`Confidence::Medium`] — a threshold judgement sustained across several samples.
//! * [`Confidence::Low`] — an inference from one sample, which §11.3 requires to be
//!   marked as such.
//!
//! # Bytes in summaries
//!
//! Summaries carry percentages, counts, durations, and ratios only. Byte-valued
//! figures go into [`Evidence`](super::Evidence), because the IEC/SI choice is a
//! display setting (§12) and the diagnostic engine does not get to decide how a byte
//! count looks.

mod collector;
mod cpu;
mod memory;
mod process;
mod psi;
mod storage;

use core::fmt;

use crate::history::{ContributorMetric, ContributorSet};
use crate::model::{Confidence, Severity, SystemSnapshot};
use crate::units::Percent;

use super::{DiagnosticRule, Finding, HistoryWindow, Thresholds};

pub use collector::{
    COLLECTOR_BEHIND, CollectorBehindRule, SELF_OVERHEAD, SNAPSHOT_STALE, SelfOverheadRule,
    SnapshotStaleRule, budget_share,
};
pub use cpu::{CPU_SATURATION, LOAD_HIGH, LoadHighRule, SustainedCpuSaturationRule};
pub use memory::{
    MEMORY_AVAILABILITY_LOW, MemoryAvailabilityLowRule, SWAP_ACTIVITY, SwapActivityRule,
};
pub use process::{
    PROCESS_CPU_SPIKE, PROCESS_RSS_GROWTH, ProcessCpuSpikeRule, ProcessRssGrowthRule,
    ZOMBIE_PRESENT, ZombieProcessRule,
};
pub use psi::{IoPsiElevatedRule, MemoryPsiElevatedRule, PSI_IO_ELEVATED, PSI_MEMORY_ELEVATED};
pub use storage::{DISK_SUSTAINED_BUSY, DiskBusyRule, disk_signal_ready};

/// How many contributors a summary names.
///
/// Three is what the §11.3 example prints ("rustc 287%, postgres 54%") plus one:
/// enough to recognise a pattern, few enough to fit a status line (§5.4).
const SUMMARY_CONTRIBUTORS: usize = 3;

/// Every §11.2 rule, in one evaluable set.
pub struct RuleSet {
    rules: Vec<Box<dyn DiagnosticRule>>,
    enabled: bool,
}

impl RuleSet {
    /// Builds the full §11.2 rule set from configuration.
    #[must_use]
    pub fn new(thresholds: Thresholds) -> Self {
        let thresholds = thresholds.sanitized();
        Self {
            enabled: thresholds.enabled,
            rules: vec![
                Box::new(SustainedCpuSaturationRule::new(thresholds)),
                Box::new(LoadHighRule::new(thresholds)),
                Box::new(MemoryAvailabilityLowRule::new(thresholds)),
                Box::new(SwapActivityRule::new(thresholds)),
                Box::new(MemoryPsiElevatedRule::new(thresholds)),
                Box::new(IoPsiElevatedRule::new(thresholds)),
                Box::new(DiskBusyRule::new(thresholds)),
                Box::new(ProcessRssGrowthRule::new(thresholds)),
                Box::new(ZombieProcessRule::new(thresholds)),
                Box::new(ProcessCpuSpikeRule::new(thresholds)),
                Box::new(CollectorBehindRule::new(thresholds)),
                Box::new(SnapshotStaleRule::new(thresholds)),
                Box::new(SelfOverheadRule::new(thresholds)),
            ],
        }
    }

    /// Evaluates every rule, most severe first.
    ///
    /// The order is total and stable — severity descending, then rule id — so the
    /// Inspect screen does not reshuffle between frames (§7.5). Returns nothing at
    /// all when `diagnostics.enabled` is false (§12).
    #[must_use]
    pub fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Vec<Finding> {
        if !self.enabled {
            return Vec::new();
        }
        let mut findings: Vec<Finding> = self
            .rules
            .iter()
            .filter_map(|rule| rule.evaluate(current, history))
            .collect();
        findings.sort_by(|left, right| {
            right
                .severity
                .cmp(&left.severity)
                .then_with(|| left.rule_id.cmp(right.rule_id))
        });
        findings
    }

    /// The rules in the set.
    #[must_use]
    pub fn rules(&self) -> &[Box<dyn DiagnosticRule>] {
        &self.rules
    }

    /// Every rule id, in registration order.
    #[must_use]
    pub fn ids(&self) -> Vec<&'static str> {
        self.rules.iter().map(|rule| rule.id()).collect()
    }

    /// How many rules are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rules.len()
    }

    /// Whether the set holds no rules.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl Default for RuleSet {
    /// The §11.2 rules with the §12 default thresholds.
    fn default() -> Self {
        Self::new(Thresholds::default())
    }
}

impl fmt::Debug for RuleSet {
    /// Prints the rule ids, since a `dyn DiagnosticRule` has no other shape.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuleSet")
            .field("enabled", &self.enabled)
            .field("rules", &self.ids())
            .finish()
    }
}

/// A threshold expressed as a [`Percent`], falling back to zero.
///
/// Thresholds are sanitized at construction, so the fallback is unreachable in
/// practice; it exists because §14.3 forbids a panicking conversion.
pub(crate) fn as_percent(value: f32) -> Percent {
    Percent::new(value).unwrap_or(Percent::ZERO)
}

/// A count as a `u64` for [`crate::model::MeasuredValue::Count`].
pub(crate) fn as_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// How many times larger `value` is than `reference`, for unit-free summaries.
///
/// Returns `None` when the ratio is undefined, so a summary never prints `inf`.
pub(crate) fn ratio(value: f64, reference: f64) -> Option<f64> {
    if reference <= 0.0 {
        return None;
    }
    let ratio = value / reference;
    ratio.is_finite().then_some(ratio)
}

/// The top contributors for a percentage-valued metric, as `name value` pairs.
///
/// Reads the retained contributor evidence (§2.2) rather than re-sorting the
/// process table: the list is already deduplicated by identity and bounded to the
/// top `K`, so this costs nothing that scales with the process count (§8.5).
pub(crate) fn percent_contributors(
    contributors: &ContributorSet,
    metric: ContributorMetric,
) -> Option<String> {
    let rendered: Vec<String> = contributors
        .metric(metric)
        .entries()
        .iter()
        .take(SUMMARY_CONTRIBUTORS)
        .filter_map(|entry| match entry.value {
            crate::model::MeasuredValue::Percent(percent) => {
                Some(format!("{} {percent}", entry.name))
            }
            _ => None,
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join(", "))
}

/// The top contributors for a byte-valued metric, as shares of `whole`.
///
/// Shares rather than byte counts so the sentence does not have to pick an IEC or
/// SI rendering (§12).
pub(crate) fn share_contributors(
    contributors: &ContributorSet,
    metric: ContributorMetric,
    whole: u64,
) -> Option<String> {
    let rendered: Vec<String> = contributors
        .metric(metric)
        .entries()
        .iter()
        .take(SUMMARY_CONTRIBUTORS)
        .filter_map(|entry| match entry.value {
            crate::model::MeasuredValue::Bytes(bytes) => {
                Percent::ratio(bytes, whole).map(|share| format!("{} {share}", entry.name))
            }
            _ => None,
        })
        .collect();
    (!rendered.is_empty()).then(|| rendered.join(", "))
}

/// The evidence-coverage sentence from §2.2, when coverage was measurable.
///
/// Worded as "account for" rather than "caused": §2.2 forbids claiming causation,
/// and the wording is part of that promise.
pub(crate) fn coverage_sentence(
    contributors: &ContributorSet,
    metric: ContributorMetric,
    noun: &str,
) -> Option<String> {
    contributors
        .metric(metric)
        .coverage()
        .fresh()
        .map(|coverage| {
            format!(" Retained top processes account for {coverage} of observed {noun}.")
        })
}

/// The severity a sustained pair of counts resolves to.
///
/// Returns `None` when even the watch condition was not sustained, which is the
/// normal answer for a healthy system.
pub(crate) fn escalate(watch: bool, critical: bool) -> Option<Severity> {
    match (critical, watch) {
        (true, _) => Some(Severity::Critical),
        (false, true) => Some(Severity::Watch),
        (false, false) => None,
    }
}

/// The confidence a multi-sample threshold judgement deserves (§11.3).
pub(crate) const SUSTAINED_CONFIDENCE: Confidence = Confidence::Medium;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{Timeline, set_cpu};
    use core::time::Duration;

    #[test]
    fn the_set_registers_every_rule_named_in_section_eleven_two() {
        let set = RuleSet::default();
        assert_eq!(set.len(), 13, "§11.2 lists thirteen rules");
        assert!(!set.is_empty());
        assert_eq!(set.rules().len(), set.len());

        let mut ids = set.ids();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "rule ids must be unique");
    }

    #[test]
    fn rule_ids_are_stable_lower_case_dotted_names() {
        for id in RuleSet::default().ids() {
            assert!(id.is_ascii(), "{id} is not ASCII");
            assert!(id.contains('.'), "{id} is not namespaced");
            assert_eq!(id.to_lowercase(), id, "{id} is not lower case");
        }
    }

    #[test]
    fn a_healthy_system_produces_no_findings() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let current = timeline.push_many(20, |snapshot| set_cpu(snapshot, 12.0));
        let findings = RuleSet::default().evaluate(&current, &timeline.window());
        assert!(findings.is_empty(), "{findings:#?}");
    }

    #[test]
    fn disabling_diagnostics_produces_no_findings_at_all() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let current = timeline.push_many(20, |snapshot| set_cpu(snapshot, 99.0));
        let set = RuleSet::new(Thresholds {
            enabled: false,
            ..Thresholds::default()
        });
        assert!(set.evaluate(&current, &timeline.window()).is_empty());
    }

    #[test]
    fn findings_are_ordered_most_severe_first_and_deterministically() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let current = timeline.push_many(20, |snapshot| {
            set_cpu(snapshot, 99.0);
            crate::diagnostics::fixtures::set_load(snapshot, 24.0);
        });
        let set = RuleSet::default();
        let first = set.evaluate(&current, &timeline.window());
        let second = set.evaluate(&current, &timeline.window());

        assert_eq!(first, second, "evaluation must be deterministic");
        assert!(first.len() >= 2, "{first:#?}");
        for pair in first.windows(2) {
            let [left, right] = pair else { continue };
            assert!(
                left.severity >= right.severity,
                "{} before {}",
                left.rule_id,
                right.rule_id
            );
        }
    }

    #[test]
    fn a_ratio_is_none_when_it_would_be_undefined() {
        assert!(ratio(1.0, 0.0).is_none());
        assert!(ratio(f64::NAN, 1.0).is_none());
        assert!(ratio(4.0, 2.0).is_some_and(|value| (value - 2.0).abs() < f64::EPSILON));
    }

    #[test]
    fn escalation_prefers_the_more_severe_outcome() {
        assert_eq!(escalate(false, false), None);
        assert_eq!(escalate(true, false), Some(Severity::Watch));
        assert_eq!(escalate(true, true), Some(Severity::Critical));
        assert_eq!(
            escalate(false, true),
            Some(Severity::Critical),
            "a critical condition is critical even if watch was not counted"
        );
    }

    #[test]
    fn the_debug_form_names_the_registered_rules() {
        let printed = format!("{:?}", RuleSet::default());
        assert!(printed.contains(CPU_SATURATION), "{printed}");
        assert!(printed.contains("enabled: true"), "{printed}");
    }
}

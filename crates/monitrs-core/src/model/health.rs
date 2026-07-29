//! Collector health and self-overhead.
//!
//! §26: *a system monitor must measure and expose its own overhead.* §16.1 sets
//! budgets for it, §11.2 has diagnostic rules that fire when they are exceeded,
//! and §7.5 renders the result. All three read this type.

use core::time::Duration;

use crate::model::MetricState;
use crate::units::Percent;

/// Which sampling tier a measurement belongs to (§8.6).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Tier {
    /// CPU, memory, processes, network and disk counters. Default 1 s.
    Fast,
    /// Filesystem capacity, static device state, sensors. Default 5 s.
    Medium,
    /// Users, device lists, cgroup metadata. Default 30 s.
    Slow,
    /// Selected-process details, ancestry, open files.
    OnDemand,
}

impl Tier {
    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Fast => "fast",
            Self::Medium => "medium",
            Self::Slow => "slow",
            Self::OnDemand => "on demand",
        }
    }

    /// All tiers, in the order the Inspect screen lists them.
    pub const ALL: [Self; 4] = [Self::Fast, Self::Medium, Self::Slow, Self::OnDemand];
}

/// Timing and failure counts for one tier.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TierHealth {
    /// How long the most recent collection took.
    pub last_duration: Duration,
    /// The slowest collection observed in this run.
    pub max_duration: Duration,
    /// A running estimate of the 95th percentile, against the §16.1 budget.
    pub p95_duration: Duration,
    /// Completed collections.
    pub completed: u64,
    /// Collections that returned an error.
    pub failed: u64,
    /// How long ago this tier last completed, for staleness display.
    pub since_last: Option<Duration>,
}

impl TierHealth {
    /// Whether this tier has produced at least one successful collection.
    #[must_use]
    pub const fn has_sampled(&self) -> bool {
        self.completed > 0
    }
}

/// A recurring collector problem, aggregated rather than logged per occurrence.
///
/// §9.2 forbids logging one error per vanished process, and the same reasoning
/// applies on screen: a repeated failure is one row with a count, not a flood.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CollectorIssue {
    /// Where it came from, e.g. `"/proc/diskstats"`.
    pub source: Box<str>,
    /// What went wrong.
    pub message: Box<str>,
    /// How many times it has happened.
    pub occurrences: u32,
    /// How long ago it last happened.
    pub last_seen: Option<Duration>,
}

/// monitrs's own resource use.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct SelfOverhead {
    /// Our own CPU usage, core-normalized. Budget: median < 1% (§16.1).
    pub cpu: Percent,
    /// Our own resident memory. Budget: < 50 MiB by default (§16.1).
    pub rss_bytes: u64,
    /// Bytes the history ring currently occupies, against the configured budget.
    pub history_bytes: u64,
    /// Open file descriptors, watched for the unbounded-growth check (§16.1).
    pub open_files: MetricState<u32>,
}

/// The maximum number of distinct issues retained.
///
/// Bounded because §10.3 forbids unbounded accumulation anywhere in the
/// pipeline, and this list is written from the sampler thread.
pub const MAX_RETAINED_ISSUES: usize = 16;

/// Overall collector health.
#[derive(Clone, Debug, Default, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct CollectorHealth {
    /// Fast tier timing.
    pub fast: TierHealth,
    /// Medium tier timing.
    pub medium: TierHealth,
    /// Slow tier timing.
    pub slow: TierHealth,
    /// On-demand detail worker timing.
    pub on_demand: TierHealth,
    /// Snapshots dropped because the channel was full.
    pub dropped_samples: u64,
    /// Snapshots superseded before the UI rendered them (§10.3).
    pub coalesced_samples: u64,
    /// How far behind live the most recent snapshot is.
    ///
    /// Rendered in the header when it exceeds the sample interval, which is what
    /// §16.2 means by "display collector lag".
    pub lag: Duration,
    /// Distinct problems, at most [`MAX_RETAINED_ISSUES`].
    pub issues: Vec<CollectorIssue>,
    /// Our own overhead.
    pub self_overhead: Option<SelfOverhead>,
}

impl CollectorHealth {
    /// Timing for one tier.
    #[must_use]
    pub const fn tier(&self, tier: Tier) -> &TierHealth {
        match tier {
            Tier::Fast => &self.fast,
            Tier::Medium => &self.medium,
            Tier::Slow => &self.slow,
            Tier::OnDemand => &self.on_demand,
        }
    }

    /// Records an issue, merging it into an existing entry when the source and
    /// message match, and dropping it once the list is full.
    ///
    /// Dropping rather than evicting keeps the *first* distinct failures, which
    /// are usually the root cause; a later flood cannot push them out.
    pub fn record_issue(&mut self, source: &str, message: &str, since_start: Duration) {
        if let Some(existing) = self
            .issues
            .iter_mut()
            .find(|issue| &*issue.source == source && &*issue.message == message)
        {
            existing.occurrences = existing.occurrences.saturating_add(1);
            existing.last_seen = Some(since_start);
            return;
        }
        if self.issues.len() >= MAX_RETAINED_ISSUES {
            return;
        }
        self.issues.push(CollectorIssue {
            source: source.into(),
            message: message.into(),
            occurrences: 1,
            last_seen: Some(since_start),
        });
    }

    /// Whether the collector is behind by more than one sample interval.
    #[must_use]
    pub fn is_behind(&self, interval: Duration) -> bool {
        self.lag > interval
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_issues_are_aggregated_rather_than_duplicated() {
        let mut health = CollectorHealth::default();
        for _ in 0..1_000 {
            health.record_issue("/proc/diskstats", "read failed", Duration::from_secs(1));
        }
        assert_eq!(health.issues.len(), 1);
        assert_eq!(health.issues.first().map(|i| i.occurrences), Some(1_000));
    }

    #[test]
    fn distinct_issues_are_kept_separate() {
        let mut health = CollectorHealth::default();
        health.record_issue("/proc/diskstats", "read failed", Duration::ZERO);
        health.record_issue("/proc/net/dev", "read failed", Duration::ZERO);
        health.record_issue("/proc/diskstats", "parse failed", Duration::ZERO);
        assert_eq!(health.issues.len(), 3);
    }

    #[test]
    fn the_issue_list_is_bounded_and_keeps_the_earliest_distinct_failures() {
        let mut health = CollectorHealth::default();
        for index in 0..(MAX_RETAINED_ISSUES * 4) {
            health.record_issue("source", &format!("failure {index}"), Duration::ZERO);
        }
        assert_eq!(health.issues.len(), MAX_RETAINED_ISSUES);
        assert_eq!(
            health.issues.first().map(|i| &*i.message),
            Some("failure 0"),
            "a later flood must not evict the root cause"
        );
    }

    #[test]
    fn lag_is_reported_only_beyond_one_interval() {
        let mut health = CollectorHealth {
            lag: Duration::from_millis(900),
            ..CollectorHealth::default()
        };
        assert!(!health.is_behind(Duration::from_secs(1)));
        health.lag = Duration::from_millis(1_100);
        assert!(health.is_behind(Duration::from_secs(1)));
    }

    #[test]
    fn a_fresh_health_record_has_not_sampled_any_tier() {
        let health = CollectorHealth::default();
        for tier in Tier::ALL {
            assert!(!health.tier(tier).has_sampled(), "{tier:?}");
        }
        assert_eq!(health.dropped_samples, 0);
        assert_eq!(health.coalesced_samples, 0);
    }
}

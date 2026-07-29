//! Sustained block-device busy state, where the platform supports it (§11.2, §7.3).

use crate::model::{
    MeasuredValue, Measurement, MetricState, PressureId, PressureState, Severity, SystemSnapshot,
};
use crate::units::{Ellipsis, format_age, truncate_tail};

use super::super::{DiagnosticRule, Evidence, Finding, HistoryWindow, Thresholds};
use super::{SUSTAINED_CONFIDENCE, as_percent};

/// Rule id for a device that has been busy for a sustained period.
pub const DISK_SUSTAINED_BUSY: &str = "disk.sustained_busy";

/// Display width a device name is truncated to in the summary.
const DEVICE_NAME_WIDTH: usize = 24;

/// How many devices the summary names.
const SUMMARY_DEVICES: usize = 3;

/// A block device busy for a sustained period (§11.2).
///
/// # Why this rule reads the radar instead of counting history
///
/// History retains device *throughput*, not the busy share (§8.5) — a busy
/// percentage per device per sample would grow every retained sample with the
/// machine's device count. The sustained-ness of device busy is therefore
/// established by the hysteresis the [`PressureEngine`](super::super::PressureEngine)
/// already applies to the disk signal, and this rule reports it with the evidence
/// the engine kept. The consequence is an ordering requirement, documented on the
/// engine: pressure must be derived before the rules run. Until it is, the signal
/// is warming up and this rule stays silent — which is the correct answer, not a
/// missed detection.
///
/// The finding says the device is busy. §11.3 forbids concluding anything about the
/// health of the hardware from it: a device at 100% busy is usually a device doing
/// its job.
#[derive(Clone, Copy, Debug)]
pub struct DiskBusyRule {
    thresholds: Thresholds,
}

impl DiskBusyRule {
    /// Builds the rule from sanitized thresholds.
    #[must_use]
    pub const fn new(thresholds: Thresholds) -> Self {
        Self { thresholds }
    }
}

impl DiagnosticRule for DiskBusyRule {
    fn id(&self) -> &'static str {
        DISK_SUSTAINED_BUSY
    }

    fn evaluate(&self, current: &SystemSnapshot, _history: &HistoryWindow<'_>) -> Option<Finding> {
        let signal = current.pressure.signal(PressureId::Disk)?;
        let state = *signal.state.fresh()?;
        let severity = match state {
            PressureState::Normal => return None,
            PressureState::Watch => Severity::Watch,
            PressureState::Critical => Severity::Critical,
        };
        let threshold = if severity == Severity::Critical {
            self.thresholds.disk_busy_critical_percent
        } else {
            self.thresholds.disk_busy_watch_percent
        };

        let mut evidence = vec![Evidence::current(Measurement::new(
            "threshold",
            MeasuredValue::Percent(as_percent(threshold)),
        ))];
        if let Some(raw) = signal.raw {
            evidence.push(Evidence::current(raw));
        }
        if let Some(held) = signal.held_for {
            evidence.push(Evidence::current(Measurement::new(
                "held for",
                MeasuredValue::Duration(held),
            )));
        }

        // Name the devices that actually reported a busy share, so the finding
        // points at a device rather than at "the disk".
        let mut devices: Vec<(&str, crate::units::Percent)> = current
            .disks
            .iter()
            .filter_map(|disk| disk.busy.fresh().map(|busy| (&*disk.device, *busy)))
            .collect();
        devices.sort_by(|left, right| {
            right
                .1
                .value()
                .total_cmp(&left.1.value())
                .then_with(|| left.0.cmp(right.0))
        });
        let named: Vec<String> = devices
            .iter()
            .take(SUMMARY_DEVICES)
            .map(|(device, busy)| {
                format!(
                    "{} {busy}",
                    truncate_tail(device, DEVICE_NAME_WIDTH, Ellipsis::Ascii)
                )
            })
            .collect();
        let held = signal
            .held_for
            .map_or_else(|| "the sustained window".to_owned(), format_age);
        let observed = if named.is_empty() {
            String::new()
        } else {
            format!(" Busiest observed devices: {}.", named.join(", "))
        };
        let summary = format!(
            "A block device has been at or above {} busy for {held}. Device busy is the share of \
             wall time with at least one request in flight; it is not filesystem capacity, and a \
             device working hard is not a device in trouble.{observed}",
            as_percent(threshold),
        );

        Some(
            Finding::new(
                DISK_SUSTAINED_BUSY,
                severity,
                "Disk device sustained busy",
                summary,
                SUSTAINED_CONFIDENCE,
            )
            .with_evidence(evidence),
        )
    }
}

/// Whether a snapshot's disk signal has been derived yet.
///
/// Exposed for the runtime's benefit: a caller that wants disk findings must run
/// the pressure engine first, and this is how it can assert that it did.
#[must_use]
pub fn disk_signal_ready(current: &SystemSnapshot) -> bool {
    current
        .pressure
        .signal(PressureId::Disk)
        .is_some_and(|signal| matches!(signal.state, MetricState::Available(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::PressureEngine;
    use crate::diagnostics::fixtures::{Timeline, set_cpu, set_disk_busy, snapshot};
    use core::time::Duration;

    fn rule() -> DiskBusyRule {
        DiskBusyRule::new(Thresholds::default().sanitized())
    }

    /// Runs the engine over `count` snapshots whose busiest device reports `busy`,
    /// returning the last snapshot with its pressure filled in — the order the
    /// runtime uses.
    fn derived(busy: f32, count: usize) -> (Timeline, SystemSnapshot) {
        let mut engine = PressureEngine::default();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut current = timeline.push(|snapshot| {
            set_cpu(snapshot, 5.0);
            set_disk_busy(snapshot, "nvme0n1", busy);
        });
        current.pressure = engine.observe(&current);
        for _ in 1..count {
            let mut next = timeline.push(|snapshot| {
                set_cpu(snapshot, 5.0);
                set_disk_busy(snapshot, "nvme0n1", busy);
            });
            next.pressure = engine.observe(&next);
            current = next;
        }
        (timeline, current)
    }

    #[test]
    fn a_quiet_device_produces_no_finding() {
        let (timeline, current) = derived(4.0, 20);
        assert!(rule().evaluate(&current, &timeline.window()).is_none());
    }

    #[test]
    fn a_device_busy_for_the_sustained_window_is_a_finding() {
        let (timeline, current) = derived(85.0, 20);
        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("the disk signal has escalated");

        assert_eq!(finding.severity, Severity::Watch);
        assert_eq!(finding.rule_id, DISK_SUSTAINED_BUSY);
        assert!(
            finding.summary.contains("nvme0n1 85%"),
            "the finding must name the device: {}",
            finding.summary
        );
        assert!(
            finding.summary.contains("not filesystem capacity"),
            "§7.3 keeps the two metrics apart: {}",
            finding.summary
        );
    }

    #[test]
    fn a_saturated_device_escalates_to_critical() {
        let (timeline, current) = derived(99.0, 20);
        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("the disk signal has escalated");
        assert_eq!(finding.severity, Severity::Critical);
        let labels: Vec<&str> = finding
            .evidence
            .iter()
            .map(|item| item.measurement.label)
            .collect();
        assert!(labels.contains(&"device busy"), "{labels:?}");
        assert!(labels.contains(&"held for"), "{labels:?}");
    }

    #[test]
    fn a_brief_burst_does_not_fire_because_the_signal_has_not_escalated() {
        let (timeline, current) = derived(99.0, 5);
        assert!(
            rule().evaluate(&current, &timeline.window()).is_none(),
            "five samples cannot sustain a ten-sample condition"
        );
    }

    #[test]
    fn nothing_fires_before_pressure_has_been_derived() {
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let current = timeline.push_many(20, |snapshot| set_disk_busy(snapshot, "nvme0n1", 99.0));
        assert!(!disk_signal_ready(&current));
        assert!(
            rule().evaluate(&current, &timeline.window()).is_none(),
            "the collector's warming-up radar carries no state to report"
        );
    }

    #[test]
    fn a_platform_without_a_busy_figure_never_fires() {
        let mut engine = PressureEngine::default();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut current = snapshot();
        for _ in 0..20 {
            let mut next = timeline.push(|snapshot| {
                set_disk_busy(snapshot, "disk0", 99.0);
                if let Some(device) = snapshot.disks.first_mut() {
                    device.busy = MetricState::Unsupported;
                }
            });
            next.pressure = engine.observe(&next);
            current = next;
        }
        assert!(!disk_signal_ready(&current));
        assert!(rule().evaluate(&current, &timeline.window()).is_none());
    }

    #[test]
    fn the_busiest_device_is_named_first() {
        let mut engine = PressureEngine::default();
        let mut timeline = Timeline::new(Duration::from_secs(1));
        let mut current = snapshot();
        for _ in 0..20 {
            let mut next = timeline.push(|snapshot| {
                set_disk_busy(snapshot, "nvme0n1", 30.0);
                set_disk_busy(snapshot, "nvme1n1", 96.0);
            });
            next.pressure = engine.observe(&next);
            current = next;
        }
        let finding = rule()
            .evaluate(&current, &timeline.window())
            .expect("the busiest device escalated the signal");
        assert!(
            finding
                .summary
                .contains("Busiest observed devices: nvme1n1 96%, nvme0n1 30%"),
            "{}",
            finding.summary
        );
    }
}

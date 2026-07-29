//! Turning one snapshot into one candidate state per radar signal (§2.3).
//!
//! Everything here is *instantaneous*: it looks at the current sample only and
//! answers "what does this reading say right now". Sustaining that answer over
//! time is [`super::Hysteresis`]'s job, and combining the two is
//! [`super::PressureEngine`]'s. Keeping them apart is what makes both testable:
//! a reading has no memory, and a tracker has no idea what a percentage means.
//!
//! Two rules run through every function in this file:
//!
//! * **An unavailable input produces an unavailable reading**, never `normal`.
//!   §2.3 requires an explicit unavailable state, and a system whose pressure
//!   cannot be measured must not look healthy.
//! * **The raw metric is always reported when it was measured**, even when no
//!   state can be derived from it. That is the `? NET unknown 18M/s` row in §5.5:
//!   the throughput is real, only the utilization is unknowable without a link
//!   speed (§7.4).

use crate::model::{
    InterfaceKind, MeasuredValue, Measurement, MetricState, PressureId, PressureState, PsiResource,
    PsiSnapshot, SystemSnapshot, UnavailableReason,
};
use crate::units::{Percent, Rate};

use super::Thresholds;

/// One signal's instantaneous evaluation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SignalReading {
    /// The candidate state, or why none could be derived.
    pub state: MetricState<PressureState>,
    /// Normalized `0..=100` closeness to critical, for bar length and sorting.
    pub severity: MetricState<Percent>,
    /// The raw metric the state was derived from (§2.3).
    ///
    /// May be present even when `state` is unavailable.
    pub raw: Option<Measurement>,
    /// The human-readable rule that produced `state` (§2.3).
    pub rule: &'static str,
}

impl SignalReading {
    /// A reading derived from a measured value and its two thresholds.
    ///
    /// `watch` and `critical` are expressed as a *pressure magnitude*: higher is
    /// always worse. Inverted metrics such as available memory are converted by
    /// their caller, which keeps one comparison direction in one place.
    #[must_use]
    fn measured(
        rule: &'static str,
        raw: Measurement,
        value: f64,
        watch: f64,
        critical: f64,
    ) -> Self {
        let state = if value >= critical {
            PressureState::Critical
        } else if value >= watch {
            PressureState::Watch
        } else {
            PressureState::Normal
        };
        Self {
            state: MetricState::Available(state),
            severity: normalized_severity(value, watch, critical),
            raw: Some(raw),
            rule,
        }
    }

    /// A reading whose input was not available, optionally still carrying the raw
    /// metric that *was* measured.
    #[must_use]
    fn unavailable(
        rule: &'static str,
        reason: MetricState<PressureState>,
        raw: Option<Measurement>,
    ) -> Self {
        Self {
            state: reason,
            severity: propagate(&reason),
            raw,
            rule,
        }
    }
}

/// How close a value is to critical, as `0..=100` (§2.3).
///
/// Half the scale is spent below the watch threshold and half between watch and
/// critical, so two signals in the same state can still be ordered by how bad they
/// are. Returns an unavailable state only if the arithmetic could not produce a
/// valid percentage, which `Percent::new` decides rather than this function.
fn normalized_severity(value: f64, watch: f64, critical: f64) -> MetricState<Percent> {
    let value = value.max(0.0);
    let scaled = if value >= critical {
        100.0
    } else if value >= watch {
        let span = critical - watch;
        if span > 0.0 {
            50.0 + 50.0 * (value - watch) / span
        } else {
            100.0
        }
    } else if watch > 0.0 {
        50.0 * value / watch
    } else {
        // A watch threshold of zero makes every non-negative value "at watch";
        // reporting the midpoint keeps the bar honest rather than empty.
        50.0
    };
    // The arithmetic runs in f64 so wide byte rates keep their precision;
    // narrowing the bounded `0..=100` result is intentional, and `Percent::new`
    // rejects anything the narrowing could not represent.
    #[allow(clippy::cast_possible_truncation)]
    let scaled = scaled as f32;
    Percent::new(scaled).map_or(
        MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
        MetricState::Available,
    )
}

/// Re-expresses one metric's unavailability as the unavailability of a value
/// derived from it.
///
/// A `Stale` or `Available` input becomes
/// [`UnavailableReason::NeedsSecondSample`]: a derived state must not be presented
/// as current when the reading behind it is not (§4, §26).
fn propagate<T, U>(state: &MetricState<T>) -> MetricState<U> {
    match state {
        MetricState::Available(_) | MetricState::Stale { .. } => {
            MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample)
        }
        MetricState::WarmingUp => MetricState::WarmingUp,
        MetricState::PermissionDenied => MetricState::PermissionDenied,
        MetricState::Unsupported => MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(reason) => MetricState::TemporarilyUnavailable(*reason),
    }
}

/// How informative an unavailable state is about a *group* of readings.
///
/// Mirrors the ranking [`crate::history`] uses for aggregate metrics: a permission
/// problem is actionable, a typed transient reason names what happened, and
/// "unsupported" says the least.
const fn rank<T>(state: &MetricState<T>) -> u8 {
    match state {
        MetricState::PermissionDenied => 4,
        MetricState::TemporarilyUnavailable(_) => 3,
        MetricState::Stale { .. } => 2,
        MetricState::WarmingUp => 1,
        MetricState::Unsupported | MetricState::Available(_) => 0,
    }
}

/// Keeps whichever of two unavailable states better explains the group.
fn most_informative<T>(
    current: Option<MetricState<T>>,
    candidate: MetricState<T>,
) -> MetricState<T> {
    match current {
        Some(current) if rank(&current) >= rank(&candidate) => current,
        _ => candidate,
    }
}

/// The rule text shown for each signal (§2.3).
///
/// Names the configuration keys rather than their current values, because the text
/// is `&'static str` in [`crate::model::PressureSignal`] and because §12 asks that
/// the user be pointed at the exact key.
#[must_use]
pub const fn rule_text(id: PressureId) -> &'static str {
    match id {
        PressureId::Cpu => {
            "cpu busy at or above diagnostics.cpu_watch_percent (watch) or \
             cpu_critical_percent (critical), sustained"
        }
        PressureId::Memory => {
            "available memory at or below diagnostics.memory_watch_available_percent (watch) or \
             memory_critical_available_percent (critical), sustained"
        }
        PressureId::Disk => {
            "busiest device busy at or above diagnostics.disk_busy_watch_percent (watch) or \
             disk_busy_critical_percent (critical), sustained; requires a device busy figure"
        }
        PressureId::Network => {
            "link utilization at or above diagnostics.network_watch_percent (watch) or \
             network_critical_percent (critical), sustained; requires a known link speed"
        }
        PressureId::Swap => {
            "swap in plus out at or above diagnostics.swap_watch_bytes_per_second (watch) or \
             swap_critical_bytes_per_second (critical), sustained"
        }
        PressureId::Load => {
            "load1 per logical cpu at or above diagnostics.load_watch_per_cpu (watch) or \
             load_critical_per_cpu (critical), sustained"
        }
        PressureId::PsiCpu => {
            "psi cpu some avg10 at or above diagnostics.psi_watch_percent (watch) or \
             psi_critical_percent (critical), sustained"
        }
        PressureId::PsiMemory => {
            "psi memory some avg10 at or above diagnostics.psi_watch_percent (watch) or \
             psi_critical_percent (critical), sustained"
        }
        PressureId::PsiIo => {
            "psi io some avg10 at or above diagnostics.psi_watch_percent (watch) or \
             psi_critical_percent (critical), sustained"
        }
    }
}

/// Evaluates one signal against the current sample.
#[must_use]
pub fn read(id: PressureId, snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    match id {
        PressureId::Cpu => cpu(snapshot, thresholds),
        PressureId::Memory => memory(snapshot, thresholds),
        PressureId::Disk => disk(snapshot, thresholds),
        PressureId::Network => network(snapshot, thresholds),
        PressureId::Swap => swap(snapshot, thresholds),
        PressureId::Load => load(snapshot, thresholds),
        PressureId::PsiCpu => psi(snapshot, thresholds, PressureId::PsiCpu),
        PressureId::PsiMemory => psi(snapshot, thresholds, PressureId::PsiMemory),
        PressureId::PsiIo => psi(snapshot, thresholds, PressureId::PsiIo),
    }
}

/// Aggregate CPU utilization (§8.3).
fn cpu(snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    let rule = rule_text(PressureId::Cpu);
    let Some(usage) = snapshot.cpu.total.fresh() else {
        return SignalReading::unavailable(rule, propagate(&snapshot.cpu.total), None);
    };
    SignalReading::measured(
        rule,
        Measurement::new("cpu busy", MeasuredValue::Percent(usage.busy)),
        f64::from(usage.busy.value()),
        f64::from(thresholds.cpu_watch_percent),
        f64::from(thresholds.cpu_critical_percent),
    )
}

/// Memory availability against the ceiling that actually applies (§9.2).
fn memory(snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    let rule = rule_text(PressureId::Memory);
    let Some(&available) = snapshot.memory.available.fresh() else {
        return SignalReading::unavailable(rule, propagate(&snapshot.memory.available), None);
    };
    let limit = snapshot.memory.effective_limit_bytes();
    let Some(share) = Percent::ratio(available, limit) else {
        // No known ceiling means no defined share; §4 forbids inventing one.
        return SignalReading::unavailable(
            rule,
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            Some(Measurement::new(
                "available",
                MeasuredValue::Bytes(available),
            )),
        );
    };
    // Inverted metric: less available is worse, so the magnitude is scarcity.
    let scarcity = f64::from((100.0 - share.value()).max(0.0));
    SignalReading::measured(
        rule,
        Measurement::new("available", MeasuredValue::Percent(share)),
        scarcity,
        f64::from(thresholds.memory_watch_used_percent()),
        f64::from(thresholds.memory_critical_used_percent()),
    )
}

/// The busiest block device, where a busy figure is semantically correct (§7.3).
fn disk(snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    let rule = rule_text(PressureId::Disk);
    let mut busiest: Option<Percent> = None;
    let mut fallback: Option<MetricState<PressureState>> = None;

    for device in &snapshot.disks {
        match device.busy.fresh() {
            Some(busy) => {
                if busiest.is_none_or(|current| busy.value() > current.value()) {
                    busiest = Some(*busy);
                }
            }
            None => fallback = Some(most_informative(fallback, propagate(&device.busy))),
        }
    }

    let Some(busy) = busiest else {
        // An empty device list is unsupported: there was nothing to measure.
        return SignalReading::unavailable(
            rule,
            fallback.unwrap_or(MetricState::Unsupported),
            None,
        );
    };
    SignalReading::measured(
        rule,
        Measurement::new("device busy", MeasuredValue::Percent(busy)),
        f64::from(busy.value()),
        f64::from(thresholds.disk_busy_watch_percent),
        f64::from(thresholds.disk_busy_critical_percent),
    )
}

/// Link saturation, which only exists when the link speed is known (§7.4).
fn network(snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    let rule = rule_text(PressureId::Network);
    let mut busiest: Option<Percent> = None;
    let mut throughput: Option<f64> = None;
    let mut fallback: Option<MetricState<PressureState>> = None;

    for interface in snapshot
        .networks
        .iter()
        .filter(|interface| interface.kind != InterfaceKind::Loopback)
    {
        // The raw throughput is reported even when utilization is unknowable, so
        // the radar can show `? NET unknown 18M/s` rather than nothing (§5.5).
        for direction in [&interface.rx, &interface.tx] {
            if let Some(rate) = direction.fresh()
                && throughput.is_none_or(|current| rate.per_second() > current)
            {
                throughput = Some(rate.per_second());
            }
        }
        let utilization = interface.utilization();
        match utilization.fresh() {
            Some(percent) => {
                if busiest.is_none_or(|current| percent.value() > current.value()) {
                    busiest = Some(*percent);
                }
            }
            None => fallback = Some(most_informative(fallback, propagate(&utilization))),
        }
    }

    let raw = throughput
        .and_then(Rate::new)
        .map(|rate| Measurement::new("throughput", MeasuredValue::ByteRate(rate)));

    let Some(utilization) = busiest else {
        return SignalReading::unavailable(rule, fallback.unwrap_or(MetricState::Unsupported), raw);
    };
    SignalReading::measured(
        rule,
        raw.unwrap_or_else(|| Measurement::new("utilization", MeasuredValue::Percent(utilization))),
        f64::from(utilization.value()),
        f64::from(thresholds.network_watch_percent),
        f64::from(thresholds.network_critical_percent),
    )
}

/// Swap activity, which is the metric that indicates distress (§11.2).
fn swap(snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    let rule = rule_text(PressureId::Swap);
    let swap = &snapshot.memory.swap;
    if !swap.is_enabled() {
        // With no swap configured there is no swap activity to measure. Reporting
        // `normal` would claim a measurement that was never made (§2.3).
        return SignalReading::unavailable(rule, MetricState::Unsupported, None);
    }
    let (Some(in_rate), Some(out_rate)) = (swap.in_rate.fresh(), swap.out_rate.fresh()) else {
        let reason = if swap.in_rate.fresh().is_none() {
            propagate(&swap.in_rate)
        } else {
            propagate(&swap.out_rate)
        };
        return SignalReading::unavailable(rule, reason, None);
    };
    let total = in_rate.per_second() + out_rate.per_second();
    let Some(rate) = Rate::new(total) else {
        return SignalReading::unavailable(
            rule,
            MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed),
            None,
        );
    };
    SignalReading::measured(
        rule,
        Measurement::new("swap in+out", MeasuredValue::ByteRate(rate)),
        total,
        thresholds.swap_watch_bytes_per_second,
        thresholds.swap_critical_bytes_per_second,
    )
}

/// Run-queue pressure, expressed per logical CPU so it is comparable (§11.2).
fn load(snapshot: &SystemSnapshot, thresholds: &Thresholds) -> SignalReading {
    let rule = rule_text(PressureId::Load);
    let Some(load) = snapshot.load.fresh() else {
        return SignalReading::unavailable(rule, propagate(&snapshot.load), None);
    };
    let raw = Measurement::new("load1", MeasuredValue::Load(load.one));
    let Some(per_cpu) = load.per_cpu(snapshot.cpu.logical_count) else {
        // Without a CPU count the figure cannot be normalized, and an
        // un-normalized load average is not comparable to any threshold.
        return SignalReading::unavailable(rule, MetricState::Unsupported, Some(raw));
    };
    SignalReading::measured(
        rule,
        raw,
        f64::from(per_cpu),
        f64::from(thresholds.load_watch_per_cpu),
        f64::from(thresholds.load_critical_per_cpu),
    )
}

/// One Linux PSI resource (§9.2).
///
/// Uses the `some avg10` figure: it is available for every resource on every
/// kernel that has PSI at all, and it is already a ten-second moving average, so
/// one read describes a window rather than an instant.
fn psi(snapshot: &SystemSnapshot, thresholds: &Thresholds, id: PressureId) -> SignalReading {
    let rule = rule_text(id);
    let Some(psi) = snapshot.pressure.psi.fresh() else {
        return SignalReading::unavailable(rule, propagate(&snapshot.pressure.psi), None);
    };
    let resource = psi_resource(psi, id);
    let label = match id {
        PressureId::PsiMemory => "psi memory some avg10",
        PressureId::PsiIo => "psi io some avg10",
        _ => "psi cpu some avg10",
    };
    SignalReading::measured(
        rule,
        Measurement::new(label, MeasuredValue::Percent(resource.some_avg10)),
        f64::from(resource.some_avg10.value()),
        f64::from(thresholds.psi_watch_percent),
        f64::from(thresholds.psi_critical_percent),
    )
}

/// Selects the PSI resource a signal id refers to.
///
/// Non-PSI ids resolve to the CPU resource; [`read`] never routes them here, and a
/// panicking branch is forbidden in production code (§14.3).
pub(super) const fn psi_resource(psi: &PsiSnapshot, id: PressureId) -> &PsiResource {
    match id {
        PressureId::PsiMemory => &psi.memory,
        PressureId::PsiIo => &psi.io,
        _ => &psi.cpu,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::fixtures::{
        percent, psi_snapshot, rate, set_cpu, set_disk_busy, set_load, set_memory, set_network,
        set_psi, set_swap, snapshot,
    };

    fn thresholds() -> Thresholds {
        Thresholds::default().sanitized()
    }

    fn state(reading: &SignalReading) -> Option<PressureState> {
        reading.state.fresh().copied()
    }

    #[test]
    fn every_signal_carries_the_rule_that_derived_it() {
        for id in PressureId::DISPLAY_ORDER {
            let reading = read(id, &snapshot(), &thresholds());
            assert!(!reading.rule.is_empty(), "{id:?} has no rule text");
            assert!(reading.rule.is_ascii(), "{id:?} rule text is not ASCII");
            assert!(
                reading.rule.contains("diagnostics."),
                "{id:?} rule text must name the configuration key"
            );
        }
    }

    #[test]
    fn a_warming_up_snapshot_derives_no_state_for_any_signal() {
        for id in PressureId::DISPLAY_ORDER {
            let reading = read(id, &snapshot(), &thresholds());
            assert!(
                reading.state.fresh().is_none(),
                "{id:?} claimed a state from an unmeasured system"
            );
            assert!(reading.severity.fresh().is_none());
        }
    }

    #[test]
    fn cpu_escalates_through_watch_to_critical() {
        let cases = [
            (10.0, PressureState::Normal),
            (79.9, PressureState::Normal),
            (80.0, PressureState::Watch),
            (94.9, PressureState::Watch),
            (95.0, PressureState::Critical),
            (100.0, PressureState::Critical),
        ];
        for (busy, expected) in cases {
            let mut snapshot = snapshot();
            set_cpu(&mut snapshot, busy);
            let reading = read(PressureId::Cpu, &snapshot, &thresholds());
            assert_eq!(state(&reading), Some(expected), "{busy}% busy");
        }
    }

    #[test]
    fn normalized_severity_orders_two_signals_in_the_same_state() {
        let mut mild = snapshot();
        set_cpu(&mut mild, 82.0);
        let mut severe = snapshot();
        set_cpu(&mut severe, 94.0);

        let mild = read(PressureId::Cpu, &mild, &thresholds());
        let severe = read(PressureId::Cpu, &severe, &thresholds());
        assert_eq!(state(&mild), Some(PressureState::Watch));
        assert_eq!(state(&severe), Some(PressureState::Watch));
        assert!(
            severe.severity.fresh().map(|p| p.value()) > mild.severity.fresh().map(|p| p.value()),
            "§2.3 wants a normalized severity, not just a state"
        );
    }

    #[test]
    fn severity_saturates_at_one_hundred_and_never_exceeds_it() {
        let mut snapshot = snapshot();
        set_cpu(&mut snapshot, 100.0);
        let reading = read(PressureId::Cpu, &snapshot, &thresholds());
        let severity = reading.severity.fresh().expect("measured").value();
        assert!((severity - 100.0).abs() < f32::EPSILON, "got {severity}");
    }

    #[test]
    fn an_unavailable_cpu_reading_is_unavailable_not_normal() {
        let mut snapshot = snapshot();
        snapshot.cpu.total = MetricState::PermissionDenied;
        let reading = read(PressureId::Cpu, &snapshot, &thresholds());
        assert_eq!(reading.state, MetricState::PermissionDenied);
        assert_eq!(reading.severity, MetricState::PermissionDenied);
        assert!(reading.raw.is_none());
    }

    #[test]
    fn a_stale_reading_does_not_become_a_current_state() {
        let mut snapshot = snapshot();
        set_cpu(&mut snapshot, 99.0);
        snapshot.cpu.total = snapshot
            .cpu
            .total
            .into_stale(core::time::Duration::from_secs(4));
        let reading = read(PressureId::Cpu, &snapshot, &thresholds());
        assert_eq!(
            reading.state,
            MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample)
        );
    }

    #[test]
    fn memory_pressure_grows_as_available_memory_shrinks() {
        let total = 32 * 1024 * 1024 * 1024;
        let cases = [
            (50, PressureState::Normal),
            (16, PressureState::Normal),
            (15, PressureState::Watch),
            (6, PressureState::Watch),
            (5, PressureState::Critical),
            (1, PressureState::Critical),
        ];
        for (available_percent, expected) in cases {
            let mut snapshot = snapshot();
            let available = total / 100 * available_percent;
            set_memory(&mut snapshot, total, available);
            let reading = read(PressureId::Memory, &snapshot, &thresholds());
            assert_eq!(state(&reading), Some(expected), "{available_percent}% free");
        }
    }

    #[test]
    fn memory_pressure_is_measured_against_a_cgroup_limit_when_there_is_one() {
        let host_total = 32 * 1024 * 1024 * 1024;
        let mut snapshot = snapshot();
        // 1 GiB available out of a 2 GiB container limit is critical, even though
        // it is a rounding error of the host total (§9.2).
        set_memory(&mut snapshot, host_total, 100 * 1024 * 1024);
        snapshot.memory.cgroup_limit_bytes = MetricState::Available(2 * 1024 * 1024 * 1024);
        let reading = read(PressureId::Memory, &snapshot, &thresholds());
        assert_eq!(state(&reading), Some(PressureState::Critical));
    }

    #[test]
    fn disk_pressure_follows_the_busiest_device_and_is_unsupported_without_one() {
        let mut snapshot = snapshot();
        assert_eq!(
            read(PressureId::Disk, &snapshot, &thresholds()).state,
            MetricState::Unsupported,
            "no devices means nothing was measured"
        );

        set_disk_busy(&mut snapshot, "nvme0n1", 12.0);
        set_disk_busy(&mut snapshot, "nvme1n1", 97.0);
        let reading = read(PressureId::Disk, &snapshot, &thresholds());
        assert_eq!(state(&reading), Some(PressureState::Critical));
        assert_eq!(
            reading.raw.map(|raw| raw.label),
            Some("device busy"),
            "§2.3 requires the raw metric"
        );
    }

    #[test]
    fn a_device_that_cannot_report_busy_keeps_the_signal_unsupported() {
        // macOS: a queue-depth approximation would be misleading, so §7.3 leaves
        // the metric unsupported rather than guessing.
        let mut snapshot = snapshot();
        set_disk_busy(&mut snapshot, "disk0", 50.0);
        if let Some(device) = snapshot.disks.first_mut() {
            device.busy = MetricState::Unsupported;
        }
        assert_eq!(
            read(PressureId::Disk, &snapshot, &thresholds()).state,
            MetricState::Unsupported
        );
    }

    #[test]
    fn network_reports_throughput_but_no_state_without_a_link_speed() {
        let mut snapshot = snapshot();
        set_network(&mut snapshot, "en0", 18_200_000.0, 2_300_000.0, None);
        let reading = read(PressureId::Network, &snapshot, &thresholds());

        assert_eq!(
            reading.state,
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
            "§7.4 forbids a utilization percentage without known capacity"
        );
        let raw = reading.raw.expect("the throughput itself is measured");
        assert_eq!(raw.label, "throughput");
        assert_eq!(
            raw.value,
            MeasuredValue::ByteRate(rate(18_200_000.0)),
            "the busiest direction is the raw metric"
        );
    }

    #[test]
    fn network_derives_a_state_once_the_link_speed_is_known() {
        let mut snapshot = snapshot();
        // 95 MB/s on a gigabit link is roughly 76% of capacity.
        set_network(&mut snapshot, "en0", 95_000_000.0, 1_000.0, Some(1_000));
        let reading = read(PressureId::Network, &snapshot, &thresholds());
        assert_eq!(state(&reading), Some(PressureState::Watch));
    }

    #[test]
    fn loopback_traffic_does_not_create_network_pressure() {
        let mut snapshot = snapshot();
        set_network(
            &mut snapshot,
            "lo0",
            9_000_000_000.0,
            9_000_000_000.0,
            Some(10),
        );
        if let Some(interface) = snapshot.networks.first_mut() {
            interface.kind = InterfaceKind::Loopback;
        }
        assert_eq!(
            read(PressureId::Network, &snapshot, &thresholds()).state,
            MetricState::Unsupported,
            "local traffic is not link saturation (§7.4)"
        );
    }

    #[test]
    fn swap_is_unsupported_when_no_swap_is_configured() {
        let snapshot = snapshot();
        assert!(!snapshot.memory.swap.is_enabled());
        let reading = read(PressureId::Swap, &snapshot, &thresholds());
        assert_eq!(
            reading.state,
            MetricState::Unsupported,
            "no swap device means no swap measurement, not a healthy one"
        );
    }

    #[test]
    fn swap_activity_escalates_on_combined_throughput() {
        let mut snapshot = snapshot();
        set_swap(
            &mut snapshot,
            8 * 1024 * 1024 * 1024,
            1024,
            600_000.0,
            600_000.0,
        );
        let reading = read(PressureId::Swap, &snapshot, &thresholds());
        assert_eq!(
            state(&reading),
            Some(PressureState::Watch),
            "in and out are summed: neither alone reaches 1 MiB/s"
        );

        set_swap(
            &mut snapshot,
            8 * 1024 * 1024 * 1024,
            1024,
            20_000_000.0,
            0.0,
        );
        assert_eq!(
            state(&read(PressureId::Swap, &snapshot, &thresholds())),
            Some(PressureState::Critical)
        );
    }

    #[test]
    fn swap_activity_is_unavailable_when_the_platform_withholds_the_rates() {
        let mut snapshot = snapshot();
        set_swap(&mut snapshot, 8 * 1024 * 1024 * 1024, 1024, 0.0, 0.0);
        snapshot.memory.swap.in_rate = MetricState::Unsupported;
        assert_eq!(
            read(PressureId::Swap, &snapshot, &thresholds()).state,
            MetricState::Unsupported
        );
    }

    #[test]
    fn load_is_normalized_per_logical_cpu() {
        let mut snapshot = snapshot();
        assert_eq!(snapshot.cpu.logical_count, 8);
        set_load(&mut snapshot, 7.9);
        assert_eq!(
            state(&read(PressureId::Load, &snapshot, &thresholds())),
            Some(PressureState::Normal),
            "7.9 on eight CPUs is below one per CPU"
        );

        set_load(&mut snapshot, 11.4);
        assert_eq!(
            state(&read(PressureId::Load, &snapshot, &thresholds())),
            Some(PressureState::Watch)
        );

        set_load(&mut snapshot, 24.0);
        assert_eq!(
            state(&read(PressureId::Load, &snapshot, &thresholds())),
            Some(PressureState::Critical)
        );
    }

    #[test]
    fn load_without_a_cpu_count_reports_the_raw_figure_and_no_state() {
        let mut snapshot = snapshot();
        snapshot.cpu.logical_count = 0;
        set_load(&mut snapshot, 4.0);
        let reading = read(PressureId::Load, &snapshot, &thresholds());
        assert_eq!(reading.state, MetricState::Unsupported);
        assert_eq!(
            reading.raw.map(|raw| raw.value),
            Some(MeasuredValue::Load(4.0))
        );
    }

    #[test]
    fn psi_signals_are_unsupported_off_linux() {
        let snapshot = snapshot();
        for id in [PressureId::PsiCpu, PressureId::PsiMemory, PressureId::PsiIo] {
            let reading = read(id, &snapshot, &thresholds());
            assert!(
                reading.state.fresh().is_none(),
                "{id:?} must not be derived without PSI data"
            );
        }
    }

    #[test]
    fn each_psi_signal_reads_its_own_resource() {
        let mut snapshot = snapshot();
        set_psi(&mut snapshot, 1.0, 45.0, 12.0);
        assert_eq!(
            state(&read(PressureId::PsiCpu, &snapshot, &thresholds())),
            Some(PressureState::Normal)
        );
        assert_eq!(
            state(&read(PressureId::PsiMemory, &snapshot, &thresholds())),
            Some(PressureState::Critical)
        );
        assert_eq!(
            state(&read(PressureId::PsiIo, &snapshot, &thresholds())),
            Some(PressureState::Watch)
        );
    }

    #[test]
    fn psi_resource_selection_covers_all_three_resources() {
        let psi = psi_snapshot(1.0, 2.0, 3.0);
        assert_eq!(
            psi_resource(&psi, PressureId::PsiCpu).some_avg10,
            percent(1.0)
        );
        assert_eq!(
            psi_resource(&psi, PressureId::PsiMemory).some_avg10,
            percent(2.0)
        );
        assert_eq!(
            psi_resource(&psi, PressureId::PsiIo).some_avg10,
            percent(3.0)
        );
    }

    #[test]
    fn a_denied_reading_outranks_an_unsupported_one_when_devices_disagree() {
        let mut snapshot = snapshot();
        set_disk_busy(&mut snapshot, "a", 10.0);
        set_disk_busy(&mut snapshot, "b", 10.0);
        if let Some(device) = snapshot.disks.first_mut() {
            device.busy = MetricState::Unsupported;
        }
        if let Some(device) = snapshot.disks.get_mut(1) {
            device.busy = MetricState::PermissionDenied;
        }
        assert_eq!(
            read(PressureId::Disk, &snapshot, &thresholds()).state,
            MetricState::PermissionDenied,
            "the actionable explanation wins"
        );
    }
}

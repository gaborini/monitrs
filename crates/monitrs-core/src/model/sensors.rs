//! Temperature and battery readings.
//!
//! Both are optional everywhere: many servers expose no `hwmon` sensors, and
//! §9.3 forbids reaching for private macOS APIs to get them. Missing sensors are
//! [`MetricState::Unsupported`], never zero degrees.
//!
//! A battery is the sharpest case of that rule in the whole model. Every desktop,
//! every server, every CI runner and every container has none, so the *absence* of
//! a battery is the normal reading rather than the exception, and it is
//! [`MetricState::Unsupported`] — a fact about the hardware — rather than a
//! failure, a zero charge, or an empty panel.

use core::time::Duration;

use crate::model::{MetricState, UnavailableReason};
use crate::units::Percent;

/// One temperature sensor reading.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TemperatureReading {
    /// Sensor label, e.g. `coretemp Package id 0`.
    pub label: Box<str>,
    /// Current temperature in degrees Celsius.
    pub celsius: f32,
    /// The highest value this sensor has been seen at, where the platform offers one.
    ///
    /// **Not a threshold, and deliberately not named like one.** The underlying
    /// interface reports either the sensor's declared high limit or the maximum
    /// value observed since the process started, depending on the platform and the
    /// driver, and the two are indistinguishable from here. It is therefore useful
    /// as context — "it has been this hot" — and never usable as a full scale for a
    /// bar or a percentage. Only [`TemperatureReading::critical_celsius`] is a
    /// declared ceiling.
    pub peak_celsius: Option<f32>,
    /// The critical threshold the sensor reports, where available.
    ///
    /// The one figure here that is a genuine ceiling, which is why it is the only
    /// denominator anything is allowed to draw a scale against.
    pub critical_celsius: Option<f32>,
}

impl TemperatureReading {
    /// Whether the reading is at or above the sensor's own critical threshold.
    ///
    /// Returns `None` when the sensor reports no threshold. §11.3 forbids
    /// diagnosing thermal throttling from an ambiguous metric, so this only ever
    /// reports what the *sensor itself* declares critical, and the diagnostic
    /// engine draws no throttling conclusion from it.
    #[must_use]
    pub fn is_critical(&self) -> Option<bool> {
        self.critical_celsius
            .map(|threshold| self.celsius >= threshold)
    }

    /// The reading as a share of the sensor's own declared ceiling.
    ///
    /// `None` when the sensor declares none, which is what stops a caller drawing a
    /// bar: a temperature has no natural full scale, and 62 °C is most of the way to
    /// a laptop's limit while being barely warm for a GPU. Deliberately refuses
    /// [`TemperatureReading::peak_celsius`] as a substitute — a bar scaled against
    /// the highest value seen so far would sit at 100% forever.
    ///
    /// Lives here rather than in the UI so the refusal is the *model's*, and every
    /// screen that wants a thermal bar gets the same answer (§7.4's rule about
    /// utilization without a known capacity, applied to temperature).
    #[must_use]
    pub fn share_of_critical(&self) -> Option<Percent> {
        let ceiling = self.critical_celsius?;
        if ceiling <= 0.0 {
            return None;
        }
        Percent::new(self.celsius / ceiling * 100.0)
    }
}

/// Whether the battery is charging.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ChargeState {
    /// Charging from external power.
    Charging,
    /// Running on battery.
    Discharging,
    /// At full charge on external power.
    Full,
    /// On external power but deliberately not charging.
    NotCharging,
    /// The platform did not report a state.
    #[default]
    Unknown,
}

impl ChargeState {
    /// A redundant non-color cue (§5.2).
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Charging => '+',
            Self::Discharging => '-',
            Self::Full => '=',
            Self::NotCharging => '.',
            Self::Unknown => '?',
        }
    }

    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Charging => "charging",
            Self::Discharging => "discharging",
            Self::Full => "full",
            Self::NotCharging => "not charging",
            Self::Unknown => "unknown",
        }
    }
}

/// A battery's design capacity beside the capacity it can hold today.
///
/// The pair is one metric rather than two, because the only interesting thing
/// either number does is stand next to the other: 48 Wh means nothing until you
/// know the cell shipped holding 52 Wh. Keeping them together also makes
/// [`BatteryCapacity::health`] the *only* way to obtain a wear percentage, so a
/// health figure can never disagree with the capacities it was derived from.
///
/// Micro-watt-hours because that is the unit Linux's `energy_full_design` uses;
/// a collector holding amp-hours converts once, at the point it knows the cell
/// voltage, rather than leaving two possible units in the model.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BatteryCapacity {
    /// What the cell held when it left the factory, in µWh.
    pub design_microwatt_hours: u64,
    /// What a full charge holds today, in µWh. This is the worn figure.
    pub full_microwatt_hours: u64,
}

impl BatteryCapacity {
    /// Today's full charge as a share of the design capacity: battery health.
    ///
    /// `None` when the design capacity is zero, which is not 0% health but an
    /// unusable pair of numbers (§4). Deliberately *not* clamped to 100: a cell
    /// whose first full charge measures above its design capacity is a real and
    /// common reading, and clamping it would hide a working battery behind a
    /// suspiciously exact figure.
    #[must_use]
    pub fn health(self) -> Option<Percent> {
        Percent::ratio(self.full_microwatt_hours, self.design_microwatt_hours)
    }
}

/// Battery state.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BatterySnapshot {
    /// Charge level.
    pub charge: Percent,
    /// Charging state.
    pub state: ChargeState,
    /// Time to empty while discharging, or to full while charging.
    ///
    /// Only ever what the platform itself reports. §4 forbids deriving one from a
    /// single sample: a figure computed from one instantaneous current reading
    /// swings by hours between consecutive samples, and a monitor that showed it
    /// would be inventing the one number users trust most.
    pub time_remaining: MetricState<Duration>,
    /// Charge cycles, where reported.
    pub cycle_count: MetricState<u32>,
    /// Design capacity beside present full-charge capacity, i.e. wear.
    pub capacity: MetricState<BatteryCapacity>,
    /// Cell temperature in degrees Celsius, where the pack reports one.
    ///
    /// Separate from [`SensorSnapshot::temperatures`] because it is not a machine
    /// sensor: it describes the pack, and a battery pack at 45 °C means something
    /// quite different from a CPU package at 45 °C.
    pub temperature_celsius: MetricState<f32>,
    /// Instantaneous power flowing through the pack, in watts.
    ///
    /// A magnitude, never signed. Direction is [`BatterySnapshot::state`]'s job:
    /// the sign of Linux's `current_now` is driver-dependent, so a signed watt
    /// figure here would mean "out" on one laptop and "in" on the next.
    pub power_watts: MetricState<f32>,
}

impl BatterySnapshot {
    /// Battery health, derived from the capacity pair and from nothing else.
    ///
    /// A method rather than a field so there is no way to store a health figure
    /// that contradicts the capacities beside it. An unavailable capacity keeps
    /// its own reason, so "no capacity reported" and "capacity refused" stay
    /// distinguishable on screen (§4).
    #[must_use]
    pub fn health(&self) -> MetricState<Percent> {
        match self.capacity.map(BatteryCapacity::health) {
            MetricState::Available(Some(health)) => MetricState::Available(health),
            MetricState::Stale {
                value: Some(health),
                age,
            } => MetricState::Stale { value: health, age },
            // A design capacity of zero is an unusable pair, not 0% health.
            MetricState::Available(None) | MetricState::Stale { value: None, .. } => {
                MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed)
            }
            MetricState::WarmingUp => MetricState::WarmingUp,
            MetricState::PermissionDenied => MetricState::PermissionDenied,
            MetricState::Unsupported => MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(reason) => {
                MetricState::TemporarilyUnavailable(reason)
            }
        }
    }
}

/// All sensor readings.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct SensorSnapshot {
    /// Temperature sensors.
    pub temperatures: MetricState<Vec<TemperatureReading>>,
    /// Battery, on systems that have one.
    pub battery: MetricState<BatterySnapshot>,
}

impl SensorSnapshot {
    /// A snapshot with nothing measured yet.
    #[must_use]
    pub const fn warming_up() -> Self {
        Self {
            temperatures: MetricState::WarmingUp,
            battery: MetricState::WarmingUp,
        }
    }

    /// The hottest reading, for the compact overview summary (§7.1).
    ///
    /// Deliberately returns only a freshly measured reading: it filters to
    /// [`MetricState::fresh`], so a retained (`Stale`) list answers `None` here
    /// rather than handing back an aged value with no way to say how old it is.
    /// A caller that wants to keep showing a retained reading — as monitrs-tui's
    /// header does — must read [`SensorSnapshot::temperatures`] through the
    /// metric's own state (e.g. [`MetricState::displayable`]) instead, so the
    /// age travels with the value rather than being silently discarded.
    #[must_use]
    pub fn hottest(&self) -> Option<&TemperatureReading> {
        self.temperatures
            .fresh()?
            .iter()
            .max_by(|a, b| a.celsius.total_cmp(&b.celsius))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(label: &str, celsius: f32, critical: Option<f32>) -> TemperatureReading {
        TemperatureReading {
            label: label.into(),
            celsius,
            peak_celsius: None,
            critical_celsius: critical,
        }
    }

    #[test]
    fn criticality_is_unknown_without_a_sensor_reported_threshold() {
        assert_eq!(reading("pkg", 95.0, None).is_critical(), None);
        assert_eq!(reading("pkg", 95.0, Some(100.0)).is_critical(), Some(false));
        assert_eq!(reading("pkg", 101.0, Some(100.0)).is_critical(), Some(true));
    }

    #[test]
    fn a_temperature_has_no_scale_without_a_declared_critical_threshold() {
        // The rule that stops a thermal bar being drawn against a made-up ceiling.
        // Real Apple Silicon sensors report no critical threshold at all, so on that
        // machine every one of these is `None` — and the screen shows the figure
        // without a bar rather than a bar without a meaning.
        assert_eq!(reading("ambient", 62.5, None).share_of_critical(), None);
        let scaled = reading("pkg", 52.5, Some(105.0))
            .share_of_critical()
            .expect("a declared ceiling");
        assert!((scaled.value() - 50.0).abs() < 0.01, "{scaled}");
        // A zero or negative ceiling is not a scale either; it is a broken sensor.
        assert_eq!(reading("pkg", 52.5, Some(0.0)).share_of_critical(), None);
    }

    #[test]
    fn the_peak_is_not_offered_as_a_substitute_scale() {
        // `peak_celsius` is the highest value *seen* on macOS and a declared limit on
        // some Linux drivers, and the two are indistinguishable here. A bar scaled
        // against the highest value seen would sit at 100% for the whole run.
        let mut hot = reading("pkg", 71.2, None);
        hot.peak_celsius = Some(72.1);
        assert_eq!(hot.share_of_critical(), None);
        assert_eq!(hot.is_critical(), None);
    }

    #[test]
    fn missing_sensors_are_unsupported_not_zero_degrees() {
        let sensors = SensorSnapshot::warming_up();
        assert!(sensors.hottest().is_none());
        assert!(sensors.temperatures.fresh().is_none());
    }

    #[test]
    fn hottest_finds_the_maximum_reading() {
        let sensors = SensorSnapshot {
            temperatures: MetricState::Available(vec![
                reading("efficiency", 44.0, None),
                reading("performance", 78.5, None),
                reading("ambient", 31.0, None),
            ]),
            battery: MetricState::Unsupported,
        };
        let hottest = sensors.hottest().expect("three readings");
        assert_eq!(&*hottest.label, "performance");
    }

    #[test]
    fn an_empty_sensor_list_has_no_hottest_reading() {
        let sensors = SensorSnapshot {
            temperatures: MetricState::Available(Vec::new()),
            battery: MetricState::Unsupported,
        };
        assert!(sensors.hottest().is_none());
    }

    fn battery(capacity: MetricState<BatteryCapacity>) -> BatterySnapshot {
        BatterySnapshot {
            charge: Percent::new(82.0).unwrap_or(Percent::ZERO),
            state: ChargeState::Discharging,
            time_remaining: MetricState::Unsupported,
            cycle_count: MetricState::Unsupported,
            capacity,
            temperature_celsius: MetricState::Unsupported,
            power_watts: MetricState::Unsupported,
        }
    }

    #[test]
    fn health_is_the_worn_capacity_against_the_design_capacity() {
        // The number that tells a user the pack is worn. 48.2 of 52.6 Wh is a
        // four-year-old laptop; the figure has to come out of those two and not
        // out of a separate field that could drift away from them.
        let capacity = BatteryCapacity {
            design_microwatt_hours: 52_600_000,
            full_microwatt_hours: 48_200_000,
        };
        let health = capacity.health().expect("a non-zero design capacity");
        assert!((health.value() - 91.6).abs() < 0.1, "{health}");
        assert_eq!(
            battery(MetricState::Available(capacity)).health(),
            MetricState::Available(health)
        );
    }

    #[test]
    fn a_battery_reporting_no_capacity_reports_no_health_rather_than_zero_percent() {
        // §4: the one thing a worn-battery figure must never do is claim a pack is
        // 0% healthy because the platform declined to say how big it is.
        for capacity in [
            MetricState::Unsupported,
            MetricState::PermissionDenied,
            MetricState::WarmingUp,
        ] {
            let health = battery(capacity).health();
            assert!(health.fresh().is_none(), "{health:?}");
            assert!(health.displayable().is_none(), "{health:?}");
            // The reason survives the derivation, so "no such thing here" and
            // "the OS refused" stay distinguishable on screen.
            assert_eq!(health.placeholder(), capacity.placeholder());
        }
    }

    #[test]
    fn a_zero_design_capacity_is_unusable_rather_than_zero_health() {
        // Some ACPI firmware reports a design capacity of zero. Dividing by it
        // would either panic or produce infinity; either way it is not 0% health.
        let health = battery(MetricState::Available(BatteryCapacity {
            design_microwatt_hours: 0,
            full_microwatt_hours: 48_200_000,
        }))
        .health();
        assert!(health.fresh().is_none());
        assert_eq!(health.placeholder(), Some("unparsable data"));
    }

    #[test]
    fn health_above_one_hundred_percent_is_reported_as_measured() {
        // A new cell often measures above its design capacity. Clamping would
        // replace a real reading with a suspiciously exact one.
        let health = battery(MetricState::Available(BatteryCapacity {
            design_microwatt_hours: 50_000_000,
            full_microwatt_hours: 51_500_000,
        }))
        .health();
        let value = health.fresh().expect("measured").value();
        assert!(value > 100.0, "{value}");
    }

    #[test]
    fn a_stale_capacity_yields_a_stale_health_carrying_the_same_age() {
        // §4: a retained value may only be displayed with its age, and a figure
        // derived from a retained value is no fresher than its input.
        let age = Duration::from_secs(7);
        let stale = MetricState::Available(BatteryCapacity {
            design_microwatt_hours: 52_600_000,
            full_microwatt_hours: 48_200_000,
        })
        .into_stale(age);
        let health = battery(stale).health();
        assert!(health.is_stale());
        assert!(health.fresh().is_none());
        assert_eq!(health.displayable().map(|(_, age)| age), Some(age));
    }

    #[test]
    fn charge_state_symbols_are_distinguishable_without_color() {
        let mut symbols: Vec<char> = [
            ChargeState::Charging,
            ChargeState::Discharging,
            ChargeState::Full,
            ChargeState::NotCharging,
            ChargeState::Unknown,
        ]
        .iter()
        .map(|s| s.symbol())
        .collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(symbols.len(), 5);
    }
}

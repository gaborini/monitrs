//! Temperature and battery readings.
//!
//! Both are optional everywhere: many servers expose no `hwmon` sensors, and
//! §9.3 forbids reaching for private macOS APIs to get them. Missing sensors are
//! [`MetricState::Unsupported`], never zero degrees.

use core::time::Duration;

use crate::model::MetricState;
use crate::units::Percent;

/// One temperature sensor reading.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TemperatureReading {
    /// Sensor label, e.g. `coretemp Package id 0`.
    pub label: Box<str>,
    /// Current temperature in degrees Celsius.
    pub celsius: f32,
    /// The high threshold the sensor reports, where available.
    pub high_celsius: Option<f32>,
    /// The critical threshold the sensor reports, where available.
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

/// Battery state.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct BatterySnapshot {
    /// Charge level.
    pub charge: Percent,
    /// Charging state.
    pub state: ChargeState,
    /// Estimated time to empty or to full.
    pub time_remaining: MetricState<Duration>,
    /// Charge cycles, where reported.
    pub cycle_count: MetricState<u32>,
    /// Full-charge capacity as a share of design capacity, i.e. battery health.
    pub health: MetricState<Percent>,
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
            high_celsius: None,
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

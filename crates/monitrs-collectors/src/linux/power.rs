//! `/sys/class/power_supply/*`: the battery, and what it is not.
//!
//! # Which power supplies count
//!
//! The directory is not a list of batteries. A laptop shows `BAT0` beside `AC` or
//! `ADP1`, a bluetooth mouse adds `hid-e4:5f:01-battery`, and a UPS on USB adds one
//! more. Two documented attributes separate the system battery from all of that,
//! and both are checked:
//!
//! * `type` must be `Battery`. `Mains` is the charger, and a charger has no charge
//!   level.
//! * `scope`, where present, must be `System`. `Device` is the kernel's own word for
//!   a peripheral's battery, so the mouse is excluded by the ABI rather than by
//!   guessing from its name — a name-prefix whitelist would have to be extended for
//!   every new driver and would silently drop the real battery on the one machine
//!   that spells it differently.
//!
//! # Two unit systems, one model
//!
//! The power-supply ABI lets a driver report energy in µWh (`energy_full_design`)
//! *or* charge in µAh (`charge_full_design`), never both, and which one you get
//! depends on the firmware. [`BatteryCapacity`] is µWh only, so an amp-hour battery
//! is converted here using `voltage_min_design` — the nominal pack voltage the same
//! ABI provides for exactly this purpose. A driver reporting amp-hours and no
//! nominal voltage leaves the capacity [`MetricState::Unsupported`]: there is no
//! second source for the missing factor, and multiplying by a plausible 11.4 V would
//! be a fabricated watt-hour figure.
//!
//! # The zeroes that are not measurements
//!
//! Two attributes lie by reporting zero, and both are the exact §4 trap:
//!
//! * `cycle_count` is `0` on a great many ACPI implementations that simply do not
//!   count cycles, and `4294967295` on those that pass through the `_BIX` "unknown"
//!   sentinel. A four-year-old laptop reading "0 cycles" is worse than one reading
//!   "n/a", so both become [`MetricState::Unsupported`].
//! * `power_now` and `current_now` are `0` while a pack sits full on mains, which is
//!   a true zero, and also `0` on drivers that export the file without filling it.
//!   Those are indistinguishable, so a zero is reported as measured — the field is
//!   an instantaneous rate, and unlike a cycle count a real zero is the common case.
//!
//! The asymmetry is deliberate and is what the tests below pin.
//!
//! Every function here takes `&[u8]`, so all of it runs on the macOS host this was
//! written on (§17.2).

use core::time::Duration;

use monitrs_core::model::{
    BatteryCapacity, BatterySnapshot, ChargeState, MetricState, UnavailableReason,
};
use monitrs_core::units::Percent;

use crate::linux::parse::{ParseFailure, ParseResult, parse_i64, parse_u64, trim_ascii};

/// What one `/sys/class/power_supply` entry is.
///
/// Only the distinction this module acts on: everything that is not a system
/// battery is [`PowerSupplyKind::Other`], because a charger and a UPS are equally
/// not the thing the Battery screen reports.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerSupplyKind {
    /// `type` is `Battery` and `scope` is absent or `System`.
    SystemBattery,
    /// A charger, a UPS, or a peripheral's own cell.
    Other,
}

/// Classifies one power supply from its `type` and `scope` attributes.
///
/// `scope` is absent on most drivers, and absent means system: the attribute was
/// added for peripherals, so requiring it would exclude every laptop battery that
/// predates it.
#[must_use]
pub fn classify(kind: Option<&[u8]>, scope: Option<&[u8]>) -> PowerSupplyKind {
    let kind = kind.map(trim_ascii).unwrap_or_default();
    if !kind.eq_ignore_ascii_case(b"Battery") {
        return PowerSupplyKind::Other;
    }
    match scope.map(trim_ascii) {
        None => PowerSupplyKind::SystemBattery,
        Some(scope) if scope.is_empty() || scope.eq_ignore_ascii_case(b"System") => {
            PowerSupplyKind::SystemBattery
        }
        // `Device`, and anything a future kernel adds: not the system battery, and
        // not something to guess about.
        Some(_) => PowerSupplyKind::Other,
    }
}

/// Parses `status`.
///
/// The five strings the ABI documents, spelled as the kernel writes them —
/// `Not charging` has a space in it. An unrecognised word is
/// [`ChargeState::Unknown`] rather than a nearest match, because the states differ
/// in the direction of the arrow the UI draws.
pub fn parse_status(bytes: &[u8]) -> ParseResult<ChargeState> {
    let trimmed = trim_ascii(bytes);
    if trimmed.is_empty() {
        return Err(ParseFailure::Empty);
    }
    Ok(if trimmed.eq_ignore_ascii_case(b"Charging") {
        ChargeState::Charging
    } else if trimmed.eq_ignore_ascii_case(b"Discharging") {
        ChargeState::Discharging
    } else if trimmed.eq_ignore_ascii_case(b"Full") {
        ChargeState::Full
    } else if trimmed.eq_ignore_ascii_case(b"Not charging") {
        ChargeState::NotCharging
    } else {
        ChargeState::Unknown
    })
}

/// Parses `capacity`, the kernel's own charge percentage.
///
/// Rejects anything above 100: the attribute is documented as `0..=100`, and a
/// driver reporting 255 is reporting a sentinel rather than a charge level.
pub fn parse_capacity_percent(bytes: &[u8]) -> ParseResult<Percent> {
    let value = parse_u64(trim_ascii(bytes), "power_supply.capacity")?;
    if value > 100 {
        return Err(ParseFailure::Malformed("power_supply.capacity"));
    }
    #[allow(clippy::cast_precision_loss)]
    Percent::new(value as f32).ok_or(ParseFailure::Malformed("power_supply.capacity"))
}

/// Parses a plain unsigned micro-unit attribute such as `energy_full_design`.
pub fn parse_micro(bytes: &[u8], field: &'static str) -> ParseResult<u64> {
    parse_u64(trim_ascii(bytes), field)
}

/// The `_BIX` "unknown" sentinel, passed straight through by several ACPI drivers.
const CYCLE_COUNT_UNKNOWN: u64 = 0xFFFF_FFFF;

/// Parses `cycle_count`, treating the two "I do not know" spellings as unknown.
///
/// See the module documentation for why a zero is not a measurement here.
pub fn parse_cycle_count(bytes: &[u8]) -> ParseResult<Option<u32>> {
    let value = parse_u64(trim_ascii(bytes), "power_supply.cycle_count")?;
    if value == 0 || value >= CYCLE_COUNT_UNKNOWN {
        return Ok(None);
    }
    u32::try_from(value)
        .map(Some)
        .map_err(|_| ParseFailure::Malformed("power_supply.cycle_count"))
}

/// Parses `temp`, which the ABI specifies in tenths of a degree Celsius.
///
/// Signed, because a pack really can be below freezing and because a driver with a
/// broken sensor reports `-2731` — absolute zero — rather than an error. That value
/// is rejected: it is the sentinel, not a reading.
pub fn parse_temperature_deci_celsius(bytes: &[u8]) -> ParseResult<f32> {
    let value = parse_i64(trim_ascii(bytes), "power_supply.temp")?;
    // -273.1 °C is the "no sensor" sentinel; nothing below it is physical either.
    if value <= -2_731 {
        return Err(ParseFailure::Malformed("power_supply.temp"));
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(value as f32 / 10.0)
}

/// Parses `time_to_empty_now` or `time_to_full_now`, which the ABI gives in seconds.
///
/// Zero becomes `None`: the drivers that export these attributes write zero when
/// they have not computed an estimate, and "0 seconds to empty" on a battery at 82%
/// is the most alarming fabricated number this screen could show.
pub fn parse_time_seconds(bytes: &[u8], field: &'static str) -> ParseResult<Option<Duration>> {
    let value = parse_u64(trim_ascii(bytes), field)?;
    Ok((value > 0).then(|| Duration::from_secs(value)))
}

/// Every attribute of one battery, already read, `None` where the file was absent.
///
/// A struct of borrowed byte slices rather than of parsed values, so
/// [`battery_from`] is a pure function of what the filesystem held and can be
/// driven straight from fixtures.
#[derive(Clone, Copy, Debug, Default)]
pub struct BatteryAttributes<'a> {
    /// `status`.
    pub status: Option<&'a [u8]>,
    /// `capacity`, the charge percentage.
    pub capacity: Option<&'a [u8]>,
    /// `cycle_count`.
    pub cycle_count: Option<&'a [u8]>,
    /// `energy_full_design`, in µWh.
    pub energy_full_design: Option<&'a [u8]>,
    /// `energy_full`, in µWh.
    pub energy_full: Option<&'a [u8]>,
    /// `charge_full_design`, in µAh, on drivers that report charge instead.
    pub charge_full_design: Option<&'a [u8]>,
    /// `charge_full`, in µAh.
    pub charge_full: Option<&'a [u8]>,
    /// `voltage_min_design`, in µV: the factor that turns µAh into µWh.
    pub voltage_min_design: Option<&'a [u8]>,
    /// `power_now`, in µW.
    pub power_now: Option<&'a [u8]>,
    /// `current_now`, in µA, possibly signed.
    pub current_now: Option<&'a [u8]>,
    /// `voltage_now`, in µV.
    pub voltage_now: Option<&'a [u8]>,
    /// `temp`, in tenths of a degree Celsius.
    pub temp: Option<&'a [u8]>,
    /// `time_to_empty_now`, in seconds.
    pub time_to_empty: Option<&'a [u8]>,
    /// `time_to_full_now`, in seconds.
    pub time_to_full: Option<&'a [u8]>,
}

/// Turns one battery's attributes into a snapshot.
///
/// Returns [`MetricState::TemporarilyUnavailable`] rather than a partial reading
/// when `capacity` is missing or unusable: [`BatterySnapshot::charge`] is the one
/// field with no [`MetricState`] of its own, so there is no honest value to put
/// there, and a panel full of secondary figures around an invented charge level
/// would be worse than a panel that says the read failed.
#[must_use]
pub fn battery_from(attributes: &BatteryAttributes<'_>) -> MetricState<BatterySnapshot> {
    let Some(charge) = attributes
        .capacity
        .and_then(|bytes| parse_capacity_percent(bytes).ok())
    else {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
    };
    let state = attributes
        .status
        .and_then(|bytes| parse_status(bytes).ok())
        .unwrap_or(ChargeState::Unknown);

    MetricState::Available(BatterySnapshot {
        charge,
        state,
        time_remaining: time_remaining(attributes, state),
        cycle_count: cycle_count(attributes),
        capacity: capacity(attributes),
        temperature_celsius: temperature(attributes),
        power_watts: power_watts(attributes),
    })
}

/// The estimate the *kernel* published for the direction the pack is moving.
///
/// Which attribute is meaningful depends on the state, and reading the wrong one
/// would put a time-to-full on a discharging laptop. Neither is derived: a pack
/// that publishes no estimate has none here, and §4 says that is the answer rather
/// than an invitation to divide charge by current.
fn time_remaining(attributes: &BatteryAttributes<'_>, state: ChargeState) -> MetricState<Duration> {
    let source = match state {
        ChargeState::Discharging => attributes.time_to_empty,
        ChargeState::Charging => attributes.time_to_full,
        // Full, held below full, or unreported: there is no direction to estimate
        // toward, which is a fact about the state and not a failed read.
        ChargeState::Full | ChargeState::NotCharging | ChargeState::Unknown => {
            return MetricState::Unsupported;
        }
    };
    let field = if matches!(state, ChargeState::Charging) {
        "power_supply.time_to_full_now"
    } else {
        "power_supply.time_to_empty_now"
    };
    match source.map(|bytes| parse_time_seconds(bytes, field)) {
        Some(Ok(Some(duration))) => MetricState::Available(duration),
        // Exported but not yet computed: the estimate is coming, which is exactly
        // what `WarmingUp` says.
        Some(Ok(None)) => MetricState::WarmingUp,
        Some(Err(failure)) => MetricState::TemporarilyUnavailable(failure.reason()),
        // The overwhelmingly common case: ACPI batteries export neither attribute.
        None => MetricState::Unsupported,
    }
}

fn cycle_count(attributes: &BatteryAttributes<'_>) -> MetricState<u32> {
    match attributes.cycle_count.map(parse_cycle_count) {
        Some(Ok(Some(count))) => MetricState::Available(count),
        // Present but zero or sentinel: the pack does not count cycles.
        Some(Ok(None)) | None => MetricState::Unsupported,
        Some(Err(failure)) => MetricState::TemporarilyUnavailable(failure.reason()),
    }
}

/// µWh per µAh·µV, i.e. the divisor that turns amp-hours into watt-hours.
const MICRO: u64 = 1_000_000;

/// Design capacity beside today's full-charge capacity, in µWh.
///
/// Energy-reporting drivers are used directly; charge-reporting drivers are
/// converted through `voltage_min_design`. Both halves must come from the same
/// unit system, because mixing a µWh design figure with a converted µAh full
/// figure would produce a wear percentage out of two unrelated scales.
fn capacity(attributes: &BatteryAttributes<'_>) -> MetricState<BatteryCapacity> {
    let energy = |bytes: Option<&[u8]>, field: &'static str| {
        bytes.and_then(|bytes| parse_micro(bytes, field).ok())
    };
    if let Some(design) = energy(attributes.energy_full_design, "energy_full_design")
        && let Some(full) = energy(attributes.energy_full, "energy_full")
    {
        return MetricState::Available(BatteryCapacity {
            design_microwatt_hours: design,
            full_microwatt_hours: full,
        });
    }

    let Some(voltage) =
        energy(attributes.voltage_min_design, "voltage_min_design").filter(|voltage| *voltage > 0)
    else {
        // Either the driver reports nothing at all, or it reports amp-hours and no
        // nominal voltage. There is no second source for the missing factor.
        return MetricState::Unsupported;
    };
    let (Some(design), Some(full)) = (
        energy(attributes.charge_full_design, "charge_full_design"),
        energy(attributes.charge_full, "charge_full"),
    ) else {
        return MetricState::Unsupported;
    };
    MetricState::Available(BatteryCapacity {
        design_microwatt_hours: design.saturating_mul(voltage) / MICRO,
        full_microwatt_hours: full.saturating_mul(voltage) / MICRO,
    })
}

fn temperature(attributes: &BatteryAttributes<'_>) -> MetricState<f32> {
    match attributes.temp.map(parse_temperature_deci_celsius) {
        Some(Ok(celsius)) => MetricState::Available(celsius),
        Some(Err(failure)) => MetricState::TemporarilyUnavailable(failure.reason()),
        None => MetricState::Unsupported,
    }
}

/// Instantaneous power through the pack, in watts, as a magnitude.
///
/// `power_now` where the driver computes it; otherwise `current_now × voltage_now`,
/// which is the same product the kernel would have written. The absolute value is
/// taken because the sign of `current_now` is driver-dependent — some write a
/// negative current while discharging, some while charging — and
/// [`BatterySnapshot::state`] already carries the direction unambiguously.
fn power_watts(attributes: &BatteryAttributes<'_>) -> MetricState<f32> {
    #[allow(clippy::cast_precision_loss)]
    let watts = |microwatts: u64| MetricState::Available(microwatts as f32 / 1e6);

    if let Some(bytes) = attributes.power_now {
        return match parse_i64(trim_ascii(bytes), "power_supply.power_now") {
            Ok(microwatts) => watts(microwatts.unsigned_abs()),
            Err(failure) => MetricState::TemporarilyUnavailable(failure.reason()),
        };
    }
    let (Some(current), Some(voltage)) = (attributes.current_now, attributes.voltage_now) else {
        return MetricState::Unsupported;
    };
    let (Ok(current), Ok(voltage)) = (
        parse_i64(trim_ascii(current), "power_supply.current_now"),
        parse_i64(trim_ascii(voltage), "power_supply.voltage_now"),
    ) else {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ParseFailed);
    };
    // µA × µV is 10^-12 W, so the product needs dividing by 10^6 to reach µW.
    let microwatts = current
        .unsigned_abs()
        .saturating_mul(voltage.unsigned_abs())
        / MICRO;
    watts(microwatts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::linux::fixtures;

    /// The reference laptop: energy-reporting ACPI battery, discharging at 12 W.
    fn discharging() -> BatteryAttributes<'static> {
        BatteryAttributes {
            status: Some(fixtures::POWER_STATUS_DISCHARGING),
            capacity: Some(fixtures::POWER_CAPACITY_82),
            cycle_count: Some(fixtures::POWER_CYCLE_COUNT_214),
            energy_full_design: Some(fixtures::POWER_ENERGY_FULL_DESIGN),
            energy_full: Some(fixtures::POWER_ENERGY_FULL),
            power_now: Some(fixtures::POWER_POWER_NOW),
            temp: Some(fixtures::POWER_TEMP_314),
            ..BatteryAttributes::default()
        }
    }

    #[test]
    fn a_charger_and_a_mouse_are_not_the_system_battery() {
        // Both are in `/sys/class/power_supply` on an ordinary laptop, and reporting
        // either one as "the battery" would put a mouse's charge on the screen.
        assert_eq!(
            classify(Some(fixtures::POWER_TYPE_MAINS), None),
            PowerSupplyKind::Other
        );
        assert_eq!(
            classify(
                Some(fixtures::POWER_TYPE_BATTERY),
                Some(fixtures::POWER_SCOPE_DEVICE)
            ),
            PowerSupplyKind::Other
        );
    }

    #[test]
    fn a_battery_without_a_scope_attribute_is_the_system_battery() {
        // `scope` was added for peripherals; requiring it would exclude every
        // laptop battery older than the attribute.
        assert_eq!(
            classify(Some(fixtures::POWER_TYPE_BATTERY), None),
            PowerSupplyKind::SystemBattery
        );
        assert_eq!(
            classify(
                Some(fixtures::POWER_TYPE_BATTERY),
                Some(fixtures::POWER_SCOPE_SYSTEM)
            ),
            PowerSupplyKind::SystemBattery
        );
        // A directory with no `type` at all is not claimed either.
        assert_eq!(classify(None, None), PowerSupplyKind::Other);
    }

    #[test]
    fn the_five_documented_statuses_parse_and_a_sixth_word_does_not_become_one_of_them() {
        assert_eq!(
            parse_status(fixtures::POWER_STATUS_DISCHARGING),
            Ok(ChargeState::Discharging)
        );
        assert_eq!(
            parse_status(fixtures::POWER_STATUS_CHARGING),
            Ok(ChargeState::Charging)
        );
        assert_eq!(
            parse_status(fixtures::POWER_STATUS_FULL),
            Ok(ChargeState::Full)
        );
        assert_eq!(
            parse_status(fixtures::POWER_STATUS_NOT_CHARGING),
            Ok(ChargeState::NotCharging)
        );
        // A future kernel word must not be rounded to the nearest known state: the
        // states differ in which way the UI draws the arrow.
        assert_eq!(parse_status(b"Trickling\n"), Ok(ChargeState::Unknown));
        assert_eq!(parse_status(b"  \n"), Err(ParseFailure::Empty));
    }

    #[test]
    fn a_cycle_count_of_zero_is_unknown_and_never_a_brand_new_battery() {
        // The §4 trap this module exists to avoid: a great many ACPI batteries
        // export `cycle_count` and never fill it, and "0 cycles" on a four-year-old
        // laptop is a fabricated all-clear about the pack's age.
        assert_eq!(parse_cycle_count(b"0\n"), Ok(None));
        assert_eq!(parse_cycle_count(b"4294967295\n"), Ok(None));
        assert_eq!(
            parse_cycle_count(fixtures::POWER_CYCLE_COUNT_214),
            Ok(Some(214))
        );
        let unknown = cycle_count(&BatteryAttributes {
            cycle_count: Some(b"0\n"),
            ..BatteryAttributes::default()
        });
        assert!(unknown.is_unsupported());
        assert!(unknown.fresh().is_none());
    }

    #[test]
    fn a_capacity_above_one_hundred_is_a_sentinel_rather_than_a_charge_level() {
        assert!(parse_capacity_percent(b"255\n").is_err());
        assert_eq!(
            parse_capacity_percent(fixtures::POWER_CAPACITY_82)
                .map(Percent::value)
                .ok(),
            Some(82.0)
        );
        assert_eq!(parse_capacity_percent(b"0\n").map(Percent::value), Ok(0.0));
    }

    #[test]
    fn a_temperature_at_absolute_zero_is_a_broken_sensor_and_not_a_reading() {
        // Drivers with an unwired thermistor write -2731, which as a number is a
        // perfectly plausible-looking -273.1 °C.
        assert!(parse_temperature_deci_celsius(b"-2731\n").is_err());
        assert_eq!(
            parse_temperature_deci_celsius(fixtures::POWER_TEMP_314),
            Ok(31.4)
        );
        // A cold pack in a car boot is a real reading and must survive.
        assert_eq!(parse_temperature_deci_celsius(b"-115\n"), Ok(-11.5));
    }

    #[test]
    fn health_comes_out_of_the_energy_attributes_as_the_worn_figure() {
        let battery = battery_from(&discharging())
            .fresh()
            .copied()
            .expect("the reference battery reads");
        let capacity = battery.capacity.fresh().copied().expect("energy reported");
        assert_eq!(capacity.design_microwatt_hours, 52_600_000);
        assert_eq!(capacity.full_microwatt_hours, 48_200_000);
        let health = battery.health().fresh().copied().expect("derived");
        assert!((health.value() - 91.6).abs() < 0.1, "{health}");
    }

    #[test]
    fn a_charge_reporting_driver_is_converted_through_its_nominal_voltage() {
        // The µAh half of the ABI. 5 000 mAh at 11.4 V nominal is 57 Wh, and the
        // conversion has to land on the same scale as the µWh path or the wear
        // percentage would be computed across two unit systems.
        let battery = battery_from(&BatteryAttributes {
            status: Some(fixtures::POWER_STATUS_DISCHARGING),
            capacity: Some(fixtures::POWER_CAPACITY_82),
            charge_full_design: Some(b"5000000\n"),
            charge_full: Some(b"4600000\n"),
            voltage_min_design: Some(b"11400000\n"),
            ..BatteryAttributes::default()
        })
        .fresh()
        .copied()
        .expect("reads");
        let capacity = battery.capacity.fresh().copied().expect("converted");
        assert_eq!(capacity.design_microwatt_hours, 57_000_000);
        assert_eq!(capacity.full_microwatt_hours, 52_440_000);
        assert!((battery.health().fresh().expect("derived").value() - 92.0).abs() < 0.01);
    }

    #[test]
    fn amp_hours_without_a_nominal_voltage_leave_the_capacity_unsupported() {
        // The missing factor has no second source, and 11.4 V is a guess even
        // though it is usually right.
        let battery = battery_from(&BatteryAttributes {
            status: Some(fixtures::POWER_STATUS_DISCHARGING),
            capacity: Some(fixtures::POWER_CAPACITY_82),
            charge_full_design: Some(b"5000000\n"),
            charge_full: Some(b"4600000\n"),
            ..BatteryAttributes::default()
        })
        .fresh()
        .copied()
        .expect("reads");
        assert!(battery.capacity.is_unsupported());
        assert!(battery.health().is_unsupported());
        assert!(battery.health().displayable().is_none());
    }

    #[test]
    fn power_is_a_magnitude_whichever_sign_the_driver_chose() {
        // Some drivers write a negative `current_now` while discharging and some
        // while charging. The direction is the status, so the watts are unsigned.
        let from_power_now = battery_from(&discharging())
            .fresh()
            .copied()
            .expect("reads")
            .power_watts;
        assert_eq!(from_power_now.fresh().copied(), Some(12.4));

        let mut negative = discharging();
        negative.power_now = Some(b"-12400000\n");
        assert_eq!(
            battery_from(&negative)
                .fresh()
                .copied()
                .expect("reads")
                .power_watts
                .fresh()
                .copied(),
            Some(12.4)
        );
    }

    #[test]
    fn power_falls_back_to_current_times_voltage_on_drivers_that_do_not_multiply() {
        let mut attributes = discharging();
        attributes.power_now = None;
        attributes.current_now = Some(b"1000000\n");
        attributes.voltage_now = Some(b"11400000\n");
        let watts = battery_from(&attributes)
            .fresh()
            .copied()
            .expect("reads")
            .power_watts
            .fresh()
            .copied()
            .expect("computed");
        assert!((watts - 11.4).abs() < 0.01, "{watts}");
    }

    #[test]
    fn a_pack_that_publishes_no_estimate_gets_none_rather_than_a_derived_one() {
        // §4's whole point on this screen. The reference battery exports no
        // `time_to_empty_now`, and charge divided by current is not an answer.
        let battery = battery_from(&discharging())
            .fresh()
            .copied()
            .expect("reads");
        assert!(battery.time_remaining.is_unsupported());
        assert!(battery.time_remaining.displayable().is_none());
    }

    #[test]
    fn the_estimate_read_is_the_one_the_current_direction_makes_meaningful() {
        // Reading `time_to_full_now` on a discharging laptop would show a
        // time-to-full that never arrives.
        let mut both = discharging();
        both.time_to_empty = Some(b"14400\n");
        both.time_to_full = Some(b"3600\n");
        assert_eq!(
            battery_from(&both)
                .fresh()
                .copied()
                .expect("reads")
                .time_remaining,
            MetricState::Available(Duration::from_secs(14_400))
        );

        both.status = Some(fixtures::POWER_STATUS_CHARGING);
        assert_eq!(
            battery_from(&both)
                .fresh()
                .copied()
                .expect("reads")
                .time_remaining,
            MetricState::Available(Duration::from_secs(3_600))
        );
    }

    #[test]
    fn a_zero_second_estimate_is_not_yet_computed_rather_than_imminent_shutdown() {
        let mut attributes = discharging();
        attributes.time_to_empty = Some(b"0\n");
        let remaining = battery_from(&attributes)
            .fresh()
            .copied()
            .expect("reads")
            .time_remaining;
        assert!(remaining.is_warming_up());
        assert!(remaining.displayable().is_none());
    }

    #[test]
    fn a_full_pack_on_mains_has_no_direction_to_estimate_toward() {
        let mut attributes = discharging();
        attributes.status = Some(fixtures::POWER_STATUS_FULL);
        attributes.time_to_empty = Some(b"14400\n");
        let battery = battery_from(&attributes).fresh().copied().expect("reads");
        assert_eq!(battery.state, ChargeState::Full);
        // The attribute is stale kernel bookkeeping while on mains; showing it
        // would claim four hours of runtime for a pack that is not discharging.
        assert!(battery.time_remaining.is_unsupported());
    }

    #[test]
    fn an_unreadable_charge_level_produces_no_battery_rather_than_a_partial_one() {
        // `charge` has no `MetricState` of its own, so there is no honest number to
        // put beside the cycle count and the temperature.
        let mut attributes = discharging();
        attributes.capacity = None;
        let battery = battery_from(&attributes);
        assert_eq!(
            battery,
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
        );
        assert!(battery.fresh().is_none());

        attributes.capacity = Some(b"not a number\n");
        assert!(battery_from(&attributes).fresh().is_none());
    }

    #[test]
    fn an_absent_status_is_unknown_and_still_yields_the_numbers_that_did_read() {
        // A driver mid-suspend can leave `status` unreadable while every capacity
        // attribute is fine. Discarding the whole battery for that would hide real
        // readings behind one missing word.
        let mut attributes = discharging();
        attributes.status = None;
        let battery = battery_from(&attributes).fresh().copied().expect("reads");
        assert_eq!(battery.state, ChargeState::Unknown);
        assert_eq!(battery.state.symbol(), '?');
        assert!(battery.capacity.fresh().is_some());
    }
}

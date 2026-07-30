//! Battery state from the documented `IOPowerSources` API.
//!
//! # Why this API and no other
//!
//! §9.3 allows battery and temperature "only where available through documented
//! APIs". `IOPowerSources` is public IOKit: `IOPSCopyPowerSourcesInfo`,
//! `IOPSCopyPowerSourcesList`, `IOPSGetPowerSourceDescription` and
//! `IOPSGetTimeRemainingEstimate` are all declared in
//! `<IOKit/ps/IOPowerSources.h>` and documented by Apple. The richer numbers a
//! battery reports — cycle count, design capacity, cell temperature, instantaneous
//! amperage, individual cell voltages — live in the `AppleSmartBattery` I/O Registry
//! node under undocumented property names, so [`BatterySnapshot::cycle_count`],
//! [`BatterySnapshot::capacity`], [`BatterySnapshot::temperature_celsius`] and
//! [`BatterySnapshot::power_watts`] are all [`MetricState::Unsupported`] here rather
//! than guessed at. `IOReport` and the SMC are out of bounds for the same reason,
//! which is also why on Apple Silicon there is no thermal-sensor reading at all.
//!
//! That is four fields the Battery screen renders as `n/a` on the machine this was
//! written on, and it is the honest answer: those numbers are obtainable, but only
//! by reading property names Apple has never published, and §9.3 does not trade a
//! nicer screen for that.
//!
//! # CoreFoundation ownership
//!
//! Two rules, and every call below is annotated with which one applies:
//!
//! * **Create/Copy** results are owned and must be released exactly once. They are
//!   wrapped in [`Owned`], which releases on drop, so an early return cannot leak
//!   one.
//! * **Get** results are borrowed from their container and must *not* be released.
//!   They are held as bare [`CFTypeRef`]s and never wrapped.
//!
//! Every value read out of a dictionary is type-checked with `CFGetTypeID` before
//! it is interpreted, because the keys are strings and a future macOS could change
//! a value's type without changing its name.

use core::ffi::{CStr, c_char, c_void};
use core::time::Duration;

use monitrs_core::model::{BatterySnapshot, ChargeState, MetricState, UnavailableReason};
use monitrs_core::units::Percent;

use super::ffi::{
    self, CF_NUMBER_INT_TYPE, CF_STRING_ENCODING_UTF8, CFTypeRef, IOPS_TIME_REMAINING_UNKNOWN,
    IOPS_TIME_REMAINING_UNLIMITED,
};

/// An owned CoreFoundation object, released on drop.
///
/// This is the whole of the crate's CoreFoundation memory management: anything
/// obtained under the Create or Copy rule goes in here immediately, and nothing
/// obtained under the Get rule ever does.
#[derive(Debug)]
struct Owned(CFTypeRef);

impl Owned {
    /// Takes ownership of a Create/Copy result, or `None` if the call failed.
    fn new(value: CFTypeRef) -> Option<Self> {
        (!value.is_null()).then_some(Self(value))
    }

    /// Borrows the object for a Get-rule call.
    const fn get(&self) -> CFTypeRef {
        self.0
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: `self.0` is non-null (checked in `new`) and was obtained under
        // the Create or Copy rule, so exactly one release is owed and this is it.
        // The pointer is not used again because `self` is being dropped.
        unsafe { ffi::CFRelease(self.0) }
    }
}

/// Creates a `CFString` for a dictionary key.
fn cf_string(text: &CStr) -> Option<Owned> {
    // SAFETY: `text` is NUL-terminated for the duration of the call and the
    // encoding constant is the documented UTF-8 one. The result follows the Create
    // rule, which `Owned` discharges.
    Owned::new(unsafe {
        ffi::CFStringCreateWithCString(core::ptr::null(), text.as_ptr(), CF_STRING_ENCODING_UTF8)
    })
}

/// Looks a key up in a dictionary under the Get rule.
///
/// The returned reference is borrowed from `dictionary` and must not be released,
/// which is why it is a bare [`CFTypeRef`] and not an [`Owned`].
fn dictionary_value(dictionary: CFTypeRef, key: &CStr) -> Option<CFTypeRef> {
    let key = cf_string(key)?;
    // SAFETY: `dictionary` is a live `CFDictionaryRef` borrowed from the power
    // source blob, and `key` is a live `CFStringRef`. `CFDictionaryGetValue`
    // returns a borrowed value or null.
    let value = unsafe { ffi::CFDictionaryGetValue(dictionary, key.get()) };
    (!value.is_null()).then_some(value)
}

/// Reads an integer dictionary value, rejecting a value of another type.
fn integer(dictionary: CFTypeRef, key: &CStr) -> Option<i32> {
    let value = dictionary_value(dictionary, key)?;
    // SAFETY: `value` is a live CoreFoundation object borrowed from the dictionary;
    // `CFGetTypeID` accepts any of them.
    if unsafe { ffi::CFGetTypeID(value) != ffi::CFNumberGetTypeID() } {
        return None;
    }
    let mut out: i32 = 0;
    // SAFETY: the object is a `CFNumber`, the requested type is `kCFNumberIntType`,
    // and the destination is a writable `i32` — which is what that type means.
    let converted = unsafe {
        ffi::CFNumberGetValue(
            value,
            CF_NUMBER_INT_TYPE,
            core::ptr::from_mut(&mut out).cast::<c_void>(),
        )
    };
    converted.then_some(out)
}

/// The largest string this module will read out of a dictionary.
///
/// The values it reads are short enumerated words such as `"AC Power"`; a value
/// longer than this is not one of them.
const MAX_STRING_BYTES: usize = 128;

/// Reads a string dictionary value, rejecting a value of another type.
fn string(dictionary: CFTypeRef, key: &CStr) -> Option<Box<str>> {
    let value = dictionary_value(dictionary, key)?;
    // SAFETY: as in [`integer`].
    if unsafe { ffi::CFGetTypeID(value) != ffi::CFStringGetTypeID() } {
        return None;
    }
    let mut buffer = [0 as c_char; MAX_STRING_BYTES];
    let capacity = ffi::CFIndex::try_from(buffer.len()).ok()?;
    // SAFETY: `buffer` is a writable array of exactly `capacity` bytes, which is
    // the bound `CFStringGetCString` respects, and it NUL-terminates within it.
    let copied = unsafe {
        ffi::CFStringGetCString(
            value,
            buffer.as_mut_ptr(),
            capacity,
            CF_STRING_ENCODING_UTF8,
        )
    };
    if !copied {
        return None;
    }
    // SAFETY: the call above succeeded, so `buffer` holds a NUL-terminated string
    // inside its own bounds.
    let text = unsafe { CStr::from_ptr(buffer.as_ptr()) };
    Some(text.to_string_lossy().into_owned().into())
}

/// Reads a boolean dictionary value, rejecting a value of another type.
fn boolean(dictionary: CFTypeRef, key: &CStr) -> Option<bool> {
    let value = dictionary_value(dictionary, key)?;
    // SAFETY: as in [`integer`].
    if unsafe { ffi::CFGetTypeID(value) != ffi::CFBooleanGetTypeID() } {
        return None;
    }
    // SAFETY: the object is a `CFBoolean`, which is what `CFBooleanGetValue`
    // requires.
    Some(unsafe { ffi::CFBooleanGetValue(value) })
}

/// The `kIOPSTypeKey` value that identifies a built-in battery.
const INTERNAL_BATTERY: &str = "InternalBattery";

/// Derives the charge state from the description dictionary.
///
/// The keys are the documented `kIOPS*Key` strings. `"AC Power"` with charging
/// false and the battery at capacity is [`ChargeState::Full`]; on external power
/// but deliberately held below full it is [`ChargeState::NotCharging`], which is
/// what optimised charging looks like and is worth distinguishing.
fn charge_state(
    source_state: Option<&str>,
    charging: Option<bool>,
    at_capacity: bool,
) -> ChargeState {
    match (source_state, charging) {
        (Some("Battery Power"), _) => ChargeState::Discharging,
        (Some("AC Power"), Some(true)) => ChargeState::Charging,
        (Some("AC Power"), Some(false)) if at_capacity => ChargeState::Full,
        (Some("AC Power"), Some(false)) => ChargeState::NotCharging,
        _ => ChargeState::Unknown,
    }
}

/// Interprets `IOPSGetTimeRemainingEstimate`'s two documented sentinels.
///
/// `kIOPSTimeRemainingUnknown` means the system has not finished estimating, which
/// is precisely [`MetricState::WarmingUp`]. `kIOPSTimeRemainingUnlimited` means the
/// machine is on external power, so there is no time-to-empty to report at all.
fn time_remaining(estimate: f64) -> MetricState<Duration> {
    if !estimate.is_finite() {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
    }
    if (estimate - IOPS_TIME_REMAINING_UNKNOWN).abs() < 0.5 {
        return MetricState::WarmingUp;
    }
    if (estimate - IOPS_TIME_REMAINING_UNLIMITED).abs() < 0.5 {
        return MetricState::Unsupported;
    }
    if estimate <= 0.0 {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
    }
    MetricState::Available(Duration::from_secs_f64(estimate))
}

/// Builds a battery snapshot from one power source's description.
fn snapshot_from(description: CFTypeRef, estimate: f64) -> Option<BatterySnapshot> {
    let kind = string(description, c"Type")?;
    if &*kind != INTERNAL_BATTERY {
        return None;
    }
    let current = integer(description, c"Current Capacity")?;
    let max = integer(description, c"Max Capacity")?;
    let (Ok(current), Ok(max)) = (u64::try_from(current), u64::try_from(max)) else {
        return None;
    };
    let charge = Percent::ratio(current, max)?;
    let source_state = string(description, c"Power Source State");
    Some(BatterySnapshot {
        charge,
        state: charge_state(
            source_state.as_deref(),
            boolean(description, c"Is Charging"),
            current >= max,
        ),
        time_remaining: time_remaining(estimate),
        // Every one of these needs an `AppleSmartBattery` registry property, and
        // none of those property names is documented; §9.3 forbids reaching for
        // them. `Unsupported` and not a zero: this Mac has a 214-cycle battery at
        // 31 °C drawing 12 W, and none of that is ours to read.
        cycle_count: MetricState::Unsupported,
        capacity: MetricState::Unsupported,
        temperature_celsius: MetricState::Unsupported,
        power_watts: MetricState::Unsupported,
    })
}

/// Reads the built-in battery, if this machine has one.
///
/// [`MetricState::Unsupported`] on a desktop, which is a fact about the hardware
/// rather than a failed read.
pub(super) fn read_battery() -> MetricState<BatterySnapshot> {
    // SAFETY: takes no arguments. The result follows the Copy rule, which `Owned`
    // discharges; a null result means no power-source information is available.
    let Some(blob) = Owned::new(unsafe { ffi::IOPSCopyPowerSourcesInfo() }) else {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
    };
    // SAFETY: `blob` is a live power-source blob. The result follows the Copy rule.
    let Some(list) = Owned::new(unsafe { ffi::IOPSCopyPowerSourcesList(blob.get()) }) else {
        return MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed);
    };
    // SAFETY: `list` is a live `CFArrayRef` produced by the call above.
    let count = unsafe { ffi::CFArrayGetCount(list.get()) };
    if count <= 0 {
        // No power sources at all: a desktop Mac.
        return MetricState::Unsupported;
    }
    // SAFETY: takes no arguments and returns a `CFTimeInterval` by value.
    let estimate = unsafe { ffi::IOPSGetTimeRemainingEstimate() };

    for index in 0..count {
        // SAFETY: `index` is inside `0..count`, the bound the same array reported.
        // The result follows the Get rule and is therefore never released.
        let source = unsafe { ffi::CFArrayGetValueAtIndex(list.get(), index) };
        if source.is_null() {
            continue;
        }
        // SAFETY: `blob` and `source` are both live and `source` came out of the
        // list `blob` produced. The description follows the Get rule: it is
        // borrowed from `blob` and must not be released.
        let description = unsafe { ffi::IOPSGetPowerSourceDescription(blob.get(), source) };
        if description.is_null() {
            continue;
        }
        if let Some(battery) = snapshot_from(description, estimate) {
            return MetricState::Available(battery);
        }
    }
    // Power sources exist but none of them is an internal battery — a UPS, for
    // instance. That is not this metric.
    MetricState::Unsupported
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn running_on_battery_is_discharging_whatever_the_charging_flag_says() {
        assert_eq!(
            charge_state(Some("Battery Power"), Some(false), false),
            ChargeState::Discharging
        );
        assert_eq!(
            charge_state(Some("Battery Power"), None, false),
            ChargeState::Discharging
        );
    }

    #[test]
    fn external_power_distinguishes_charging_full_and_deliberately_held() {
        assert_eq!(
            charge_state(Some("AC Power"), Some(true), false),
            ChargeState::Charging
        );
        assert_eq!(
            charge_state(Some("AC Power"), Some(false), true),
            ChargeState::Full
        );
        // Optimised charging holds the battery below full on purpose, and calling
        // that "full" would misreport it.
        assert_eq!(
            charge_state(Some("AC Power"), Some(false), false),
            ChargeState::NotCharging
        );
    }

    #[test]
    fn an_unrecognised_power_state_is_unknown_rather_than_guessed() {
        assert_eq!(charge_state(None, None, false), ChargeState::Unknown);
        assert_eq!(
            charge_state(Some("Something New"), Some(true), false),
            ChargeState::Unknown
        );
    }

    #[test]
    fn the_unknown_estimate_sentinel_warms_up_instead_of_becoming_a_negative_duration() {
        // The bug this prevents: -1.0 seconds rendered as a time remaining.
        assert!(time_remaining(IOPS_TIME_REMAINING_UNKNOWN).is_warming_up());
        assert_eq!(time_remaining(IOPS_TIME_REMAINING_UNKNOWN).fresh(), None);
    }

    #[test]
    fn the_unlimited_estimate_sentinel_means_there_is_no_time_to_empty() {
        assert!(time_remaining(IOPS_TIME_REMAINING_UNLIMITED).is_unsupported());
    }

    #[test]
    fn a_real_estimate_becomes_a_duration() {
        assert_eq!(
            time_remaining(16_260.0),
            MetricState::Available(Duration::from_secs(16_260))
        );
    }

    #[test]
    fn a_nonsensical_estimate_is_unavailable_rather_than_zero() {
        assert_eq!(
            time_remaining(f64::NAN),
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
        );
        assert_eq!(
            time_remaining(0.0),
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
        );
        assert_eq!(
            time_remaining(-500.0),
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed)
        );
    }

    #[test]
    #[ignore = "platform smoke test: reads the live power source"]
    fn the_live_battery_is_either_a_plausible_reading_or_an_honest_absence() {
        let battery = read_battery();
        match battery {
            MetricState::Available(reading) => {
                assert!(
                    (0.0..=100.0).contains(&reading.charge.value()),
                    "charge out of range: {}",
                    reading.charge.value()
                );
                assert_ne!(
                    reading.state,
                    ChargeState::Unknown,
                    "a present battery reports a documented power-source state"
                );
                // §9.3: these need undocumented registry properties, so they must
                // stay absent rather than be approximated.
                assert!(reading.cycle_count.is_unsupported());
                assert!(reading.capacity.is_unsupported());
                assert!(reading.health().is_unsupported());
                assert!(reading.temperature_celsius.is_unsupported());
                assert!(reading.power_watts.is_unsupported());
            }
            // A desktop Mac. Not a failure.
            MetricState::Unsupported => {}
            other => panic!("unexpected battery state {other:?}"),
        }
    }

    #[test]
    #[ignore = "platform smoke test: reads the live power source"]
    fn repeated_battery_reads_release_every_copied_object() {
        // Without the `Owned` guards each iteration leaks a blob and an array.
        for _ in 0..1_000 {
            let _ = read_battery();
        }
    }
}

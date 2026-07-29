//! Per-second rates derived from cumulative counters.

use core::fmt;

/// A non-negative, finite "units per second" value.
///
/// The unit itself (bytes, packets, operations) is implied by the field the rate
/// is stored in. Rates are calculated, so floating point is permitted (§10.4).
///
/// A rate can only be constructed from a *validated* delta: [`Rate::new`]
/// rejects negatives, so a counter reset cannot silently become a huge or
/// negative rate (§8.2).
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Rate(f64);

impl Rate {
    /// Zero units per second.
    pub const ZERO: Self = Self(0.0);

    /// Builds a rate, rejecting NaN, infinities, and negative values.
    #[must_use]
    pub fn new(per_second: f64) -> Option<Self> {
        if per_second.is_finite() && per_second >= 0.0 {
            Some(Self(per_second))
        } else {
            None
        }
    }

    /// Builds a rate from a validated non-negative delta and the *actual*
    /// elapsed time.
    ///
    /// Never assume a one-second interval: suspend/resume, load, and scheduler
    /// delay all make the real interval variable (§8.1). Returns `None` when
    /// `elapsed` is zero or the result is not finite.
    #[must_use]
    pub fn from_delta(delta: u64, elapsed: core::time::Duration) -> Option<Self> {
        let seconds = elapsed.as_secs_f64();
        if seconds <= 0.0 {
            return None;
        }
        Self::new(delta as f64 / seconds)
    }

    /// The underlying units-per-second value.
    #[must_use]
    pub const fn per_second(self) -> f64 {
        self.0
    }

    /// Signed difference against another rate, for comparison columns (§2.5).
    #[must_use]
    pub fn delta_from(self, other: Self) -> f64 {
        self.0 - other.0
    }
}

impl fmt::Display for Rate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:.0}/s", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    #[test]
    fn rejects_non_finite_and_negative() {
        assert!(Rate::new(-1.0).is_none());
        assert!(Rate::new(f64::NAN).is_none());
        assert!(Rate::new(f64::INFINITY).is_none());
    }

    #[test]
    fn zero_elapsed_yields_no_rate_instead_of_infinity() {
        assert!(Rate::from_delta(1024, Duration::ZERO).is_none());
    }

    #[test]
    fn uses_actual_elapsed_time_not_an_assumed_second() {
        let half = Rate::from_delta(1000, Duration::from_millis(500)).expect("valid");
        assert!((half.per_second() - 2000.0).abs() < f64::EPSILON);
        let double = Rate::from_delta(1000, Duration::from_secs(2)).expect("valid");
        assert!((double.per_second() - 500.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_zero_delta_is_a_real_zero_rate() {
        let r = Rate::from_delta(0, Duration::from_secs(1)).expect("valid");
        assert_eq!(r, Rate::ZERO);
    }
}

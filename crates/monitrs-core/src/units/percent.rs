//! Validated percentage values.
//!
//! Percentages are *calculated* values, so §10.4 permits floating point here.
//! Raw counters and timestamps must never use these types.

use core::fmt;

/// A non-negative, finite percentage.
///
/// Deliberately **not** clamped to `0..=100`: process CPU under the default
/// `"core"` normalization legitimately exceeds 100% for a multi-threaded
/// process (§8.3). Meters that need a bounded value call
/// [`Percent::clamped_to_100`].
#[derive(Clone, Copy, Debug, Default, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct Percent(f32);

impl Percent {
    /// Zero percent.
    pub const ZERO: Self = Self(0.0);
    /// One hundred percent.
    pub const FULL: Self = Self(100.0);

    /// Builds a percentage, rejecting NaN, infinities, and negative values.
    ///
    /// Returning `None` rather than clamping is intentional: a NaN percentage
    /// means the *calculation* was wrong, and §4 forbids silently converting an
    /// unavailable value to a number.
    #[must_use]
    pub fn new(value: f32) -> Option<Self> {
        if value.is_finite() && value >= 0.0 {
            Some(Self(value))
        } else {
            None
        }
    }

    /// Builds a percentage from a `part / whole` ratio.
    ///
    /// Returns `None` when `whole` is zero, because "0 of 0" has no defined
    /// utilization and must be reported as unavailable rather than as 0%.
    #[must_use]
    pub fn ratio(part: u64, whole: u64) -> Option<Self> {
        if whole == 0 {
            return None;
        }
        // The division happens in f64 so that exbibyte-scale counters keep their
        // precision; narrowing the *result* to f32 is intentional, and any value
        // the narrowing could not represent is rejected by `new`.
        #[allow(clippy::cast_possible_truncation)]
        let percent = (part as f64 / whole as f64 * 100.0) as f32;
        Self::new(percent)
    }

    /// Clamps into `0.0..=100.0` for bar and meter rendering.
    #[must_use]
    pub fn clamped_to_100(self) -> Self {
        Self(self.0.min(100.0))
    }

    /// The underlying value.
    #[must_use]
    pub const fn value(self) -> f32 {
        self.0
    }

    /// The value as a `0.0..=1.0`-ish fraction (may exceed 1.0).
    #[must_use]
    pub fn fraction(self) -> f32 {
        self.0 / 100.0
    }

    /// Difference in percentage *points*, which may be negative.
    #[must_use]
    pub fn points_from(self, other: Self) -> f32 {
        self.0 - other.0
    }
}

impl fmt::Display for Percent {
    /// Renders with one decimal only when it adds information (§5.4).
    ///
    /// Below 10% a single decimal distinguishes 0.4% from 0.9%; above that the
    /// decimal is noise and causes column jitter.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 < 10.0 && self.0 != 0.0 {
            write!(f, "{:.1}%", self.0)
        } else {
            write!(f, "{:.0}%", self.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_finite_and_negative() {
        assert!(Percent::new(f32::NAN).is_none());
        assert!(Percent::new(f32::INFINITY).is_none());
        assert!(Percent::new(-0.5).is_none());
        assert!(Percent::new(0.0).is_some());
        assert!(Percent::new(287.0).is_some(), "process CPU may exceed 100%");
    }

    #[test]
    fn zero_whole_is_undefined_not_zero() {
        assert!(Percent::ratio(0, 0).is_none());
        assert!(Percent::ratio(5, 0).is_none());
    }

    #[test]
    fn ratio_computes_expected_values() {
        let p = Percent::ratio(1, 4).expect("4 is non-zero");
        assert!((p.value() - 25.0).abs() < f32::EPSILON);
    }

    #[test]
    fn ratio_survives_counters_that_overflow_f32_precision() {
        // 16 EiB-scale counters must not produce NaN or a negative percentage.
        let p = Percent::ratio(u64::MAX / 2, u64::MAX).expect("non-zero whole");
        assert!((p.value() - 50.0).abs() < 0.01, "got {}", p.value());
    }

    #[test]
    fn clamping_only_affects_the_upper_bound() {
        let p = Percent::new(287.0).expect("valid");
        assert!((p.clamped_to_100().value() - 100.0).abs() < f32::EPSILON);
        let q = Percent::new(37.0).expect("valid");
        assert!((q.clamped_to_100().value() - 37.0).abs() < f32::EPSILON);
    }

    #[test]
    fn display_adds_a_decimal_only_below_ten_percent() {
        assert_eq!(Percent::new(0.0).expect("valid").to_string(), "0%");
        assert_eq!(Percent::new(4.2).expect("valid").to_string(), "4.2%");
        assert_eq!(Percent::new(37.4).expect("valid").to_string(), "37%");
        assert_eq!(Percent::new(287.0).expect("valid").to_string(), "287%");
    }
}

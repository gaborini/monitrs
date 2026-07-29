//! Per-metric availability.
//!
//! §4 forbids representing platform support as one global boolean. Every metric
//! that an OS may withhold is wrapped in [`MetricState`], and the single most
//! important invariant in this crate is that **unavailable is never zero**.

use core::fmt;
use core::time::Duration;

/// Why a normally-available metric is missing from this particular sample.
///
/// The specification sketches `TemporarilyUnavailable { reason: String }`, but
/// §4 explicitly permits a typed enum when a per-sample `String` is too costly.
/// It is: a `String` in every field of every sample would allocate thousands of
/// times per second. The human-readable message is produced at the UI layer by
/// [`UnavailableReason::message`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum UnavailableReason {
    /// A cumulative counter moved backwards, so this sample's delta is invalid.
    ///
    /// §8.2 requires returning this rather than a huge or negative rate.
    CounterReset,
    /// The device, mount, or interface vanished between two reads.
    DeviceDisappeared,
    /// The interface was renamed, invalidating the previous counter baseline.
    InterfaceRenamed,
    /// The process exited between enumeration and the detail read (§8.2).
    ///
    /// Expected during normal sampling and never worth a warning log (§14.1).
    ProcessExited,
    /// The underlying read failed for a reason other than permissions.
    ReadFailed,
    /// The data was present but did not match the expected format.
    ParseFailed,
    /// Collection exceeded its time budget and was abandoned for this sample.
    Timeout,
    /// Enrichment was skipped to stay inside a budget under high load (§16.2).
    SkippedUnderLoad,
    /// A utilization percentage was requested but the link speed is unknown.
    ///
    /// §7.4 forbids rendering a network utilization percentage without a known
    /// link capacity.
    LinkSpeedUnknown,
    /// The metric requires at least two samples and only one exists so far.
    NeedsSecondSample,
}

impl UnavailableReason {
    /// A short, lower-case explanation suitable for a status line or tooltip.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::CounterReset => "counter reset",
            Self::DeviceDisappeared => "device disappeared",
            Self::InterfaceRenamed => "interface renamed",
            Self::ProcessExited => "process exited",
            Self::ReadFailed => "read failed",
            Self::ParseFailed => "unparsable data",
            Self::Timeout => "collection timed out",
            Self::SkippedUnderLoad => "skipped under load",
            Self::LinkSpeedUnknown => "link speed unknown",
            Self::NeedsSecondSample => "needs a second sample",
        }
    }
}

impl fmt::Display for UnavailableReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// The availability of a single metric in a single sample.
///
/// The `Stale` variant is an addition to the five states listed in §4. It exists
/// because §4 also states that a temporarily unavailable metric may retain its
/// last good value *only* if that value is visibly marked stale and carries its
/// age. Encoding the value and its age in the type makes it impossible to render
/// a retained value without knowing it is stale.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MetricState<T> {
    /// Measured in this sample.
    Available(T),
    /// The last known-good value, retained across a transient failure.
    ///
    /// Must be rendered with a visible stale marker and `age` (§4).
    Stale {
        /// The retained value.
        value: T,
        /// How long ago `value` was actually measured.
        age: Duration,
    },
    /// The metric needs more samples before it means anything (§8.2, §26).
    ///
    /// The first sample of delta-based data is *not* zero.
    WarmingUp,
    /// The OS refused the read. Not a fatal error (§9.2).
    PermissionDenied,
    /// This platform does not expose the metric at all.
    Unsupported,
    /// Normally available, absent from this sample.
    TemporarilyUnavailable(UnavailableReason),
}

impl<T> MetricState<T> {
    /// The freshly measured value, if this sample actually measured it.
    ///
    /// Returns `None` for stale values. Use this for anything that feeds a
    /// calculation, a diagnostic rule, or a rate baseline.
    #[must_use]
    pub const fn fresh(&self) -> Option<&T> {
        match self {
            Self::Available(value) => Some(value),
            _ => None,
        }
    }

    /// The best available value, fresh or stale, paired with its age.
    ///
    /// Use this only for *display*, and only alongside the returned age so the
    /// staleness can be shown.
    #[must_use]
    pub const fn displayable(&self) -> Option<(&T, Duration)> {
        match self {
            Self::Available(value) => Some((value, Duration::ZERO)),
            Self::Stale { value, age } => Some((value, *age)),
            _ => None,
        }
    }

    /// Whether this sample measured the metric.
    #[must_use]
    pub const fn is_available(&self) -> bool {
        matches!(self, Self::Available(_))
    }

    /// Whether a retained value is being shown instead of a fresh one.
    #[must_use]
    pub const fn is_stale(&self) -> bool {
        matches!(self, Self::Stale { .. })
    }

    /// Whether the metric will never be available on this platform.
    ///
    /// Layout code uses this to drop optional panels when space is scarce (§4).
    #[must_use]
    pub const fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }

    /// Whether the metric is expected to become available shortly.
    #[must_use]
    pub const fn is_warming_up(&self) -> bool {
        matches!(self, Self::WarmingUp)
    }

    /// Transforms the contained value, preserving the availability state.
    #[must_use]
    pub fn map<U, F: FnOnce(T) -> U>(self, f: F) -> MetricState<U> {
        match self {
            Self::Available(value) => MetricState::Available(f(value)),
            Self::Stale { value, age } => MetricState::Stale {
                value: f(value),
                age,
            },
            Self::WarmingUp => MetricState::WarmingUp,
            Self::PermissionDenied => MetricState::PermissionDenied,
            Self::Unsupported => MetricState::Unsupported,
            Self::TemporarilyUnavailable(reason) => MetricState::TemporarilyUnavailable(reason),
        }
    }

    /// Borrows the contained value.
    #[must_use]
    pub const fn as_ref(&self) -> MetricState<&T> {
        match self {
            Self::Available(value) => MetricState::Available(value),
            Self::Stale { value, age } => MetricState::Stale { value, age: *age },
            Self::WarmingUp => MetricState::WarmingUp,
            Self::PermissionDenied => MetricState::PermissionDenied,
            Self::Unsupported => MetricState::Unsupported,
            Self::TemporarilyUnavailable(reason) => MetricState::TemporarilyUnavailable(*reason),
        }
    }

    /// The placeholder to render when there is no value.
    ///
    /// Returns `None` when a value *is* present. The strings are the ones §4
    /// mandates, and all of them are strict 7-bit ASCII so they are legal in
    /// both glyph modes (§5.1).
    #[must_use]
    pub const fn placeholder(&self) -> Option<&'static str> {
        match self {
            Self::Available(_) | Self::Stale { .. } => None,
            Self::WarmingUp => Some("warming up"),
            Self::PermissionDenied => Some("permission denied"),
            Self::Unsupported => Some("n/a"),
            Self::TemporarilyUnavailable(reason) => Some(reason.message()),
        }
    }

    /// A single-character redundant cue, so meaning survives without color (§5.2).
    #[must_use]
    pub const fn symbol(&self) -> char {
        match self {
            Self::Available(_) => ' ',
            Self::Stale { .. } => '~',
            Self::WarmingUp => '.',
            Self::PermissionDenied => '!',
            Self::Unsupported => '-',
            Self::TemporarilyUnavailable(_) => '?',
        }
    }

    /// Converts a fresh value into a stale one aged by `age`.
    ///
    /// Collectors call this when a read fails but a previous value is worth
    /// keeping on screen. Anything that is not currently `Available` is returned
    /// unchanged, so staleness cannot compound into a fake value.
    #[must_use]
    pub fn into_stale(self, age: Duration) -> Self {
        match self {
            Self::Available(value) => Self::Stale { value, age },
            other => other,
        }
    }
}

impl<T> From<Option<T>> for MetricState<T> {
    /// Treats a missing optional value as [`MetricState::Unsupported`].
    ///
    /// Collectors that know a more specific reason must construct the variant
    /// directly rather than going through `Option`.
    fn from(value: Option<T>) -> Self {
        match value {
            Some(value) => Self::Available(value),
            None => Self::Unsupported,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_is_never_zero() {
        // The single most important invariant in the crate: there is no API that
        // turns an unavailable metric into a number.
        let state: MetricState<u64> = MetricState::PermissionDenied;
        assert!(state.fresh().is_none());
        assert!(state.displayable().is_none());
        assert_eq!(state.placeholder(), Some("permission denied"));
    }

    #[test]
    fn warming_up_is_distinct_from_zero() {
        let warming: MetricState<u64> = MetricState::WarmingUp;
        let zero = MetricState::Available(0u64);
        assert_ne!(warming, zero);
        assert_eq!(warming.fresh(), None);
        assert_eq!(zero.fresh(), Some(&0));
    }

    #[test]
    fn stale_values_are_only_readable_together_with_their_age() {
        let state = MetricState::Available(42u64).into_stale(Duration::from_secs(3));
        assert!(state.is_stale());
        // A stale value is deliberately invisible to calculations...
        assert_eq!(state.fresh(), None);
        // ...and readable for display only alongside its age.
        let (value, age) = state.displayable().expect("stale values are displayable");
        assert_eq!(*value, 42);
        assert_eq!(age, Duration::from_secs(3));
    }

    #[test]
    fn staleness_cannot_be_applied_to_a_missing_value() {
        let state: MetricState<u64> = MetricState::Unsupported.into_stale(Duration::from_secs(9));
        assert_eq!(state, MetricState::Unsupported);
        let state: MetricState<u64> = MetricState::WarmingUp.into_stale(Duration::from_secs(9));
        assert_eq!(state, MetricState::WarmingUp);
    }

    #[test]
    fn staleness_does_not_compound() {
        let once = MetricState::Available(7u64).into_stale(Duration::from_secs(1));
        let twice = once.into_stale(Duration::from_secs(30));
        assert_eq!(
            once, twice,
            "re-staling must not overwrite the original age"
        );
    }

    #[test]
    fn map_preserves_availability_state() {
        assert_eq!(
            MetricState::Available(2u64).map(|v| v * 2),
            MetricState::Available(4u64)
        );
        let stale = MetricState::Stale {
            value: 2u64,
            age: Duration::from_secs(5),
        };
        assert_eq!(
            stale.map(|v| v * 2),
            MetricState::Stale {
                value: 4,
                age: Duration::from_secs(5)
            }
        );
        let denied: MetricState<u64> = MetricState::PermissionDenied;
        assert_eq!(denied.map(|v| v * 2), MetricState::PermissionDenied);
    }

    #[test]
    fn every_state_has_a_redundant_non_color_cue() {
        let states: [MetricState<u64>; 6] = [
            MetricState::Available(1),
            MetricState::Stale {
                value: 1,
                age: Duration::ZERO,
            },
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(UnavailableReason::ReadFailed),
        ];
        let mut symbols: Vec<char> = states.iter().map(MetricState::symbol).collect();
        symbols.sort_unstable();
        symbols.dedup();
        assert_eq!(
            symbols.len(),
            states.len(),
            "symbols must be distinguishable"
        );
    }

    #[test]
    fn placeholders_are_strict_ascii_so_they_are_legal_in_both_glyph_modes() {
        let reasons = [
            UnavailableReason::CounterReset,
            UnavailableReason::DeviceDisappeared,
            UnavailableReason::InterfaceRenamed,
            UnavailableReason::ProcessExited,
            UnavailableReason::ReadFailed,
            UnavailableReason::ParseFailed,
            UnavailableReason::Timeout,
            UnavailableReason::SkippedUnderLoad,
            UnavailableReason::LinkSpeedUnknown,
            UnavailableReason::NeedsSecondSample,
        ];
        for reason in reasons {
            assert!(
                reason.message().is_ascii(),
                "{reason:?} message is not strict ASCII"
            );
        }
        for text in ["warming up", "permission denied", "n/a"] {
            assert!(text.is_ascii());
        }
    }
}

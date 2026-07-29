//! Self-describing measurements and the severity vocabulary shared by the
//! pressure radar and the diagnostic engine.
//!
//! §2.3 requires every pressure signal to show *the raw metric* alongside its
//! normalized severity and the rule that produced it. A [`Measurement`] carries
//! the raw number plus enough type information for the UI to format it, without
//! the collector needing to know anything about formatting.

use core::time::Duration;

use crate::units::{ByteUnits, Percent, Rate, format_age, format_byte_rate, format_bytes};

/// A raw measured quantity, tagged with what kind of quantity it is.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum MeasuredValue {
    /// A byte count.
    Bytes(u64),
    /// A per-second rate of bytes.
    ByteRate(Rate),
    /// A per-second rate of discrete events (packets, operations).
    EventRate(Rate),
    /// A percentage.
    Percent(Percent),
    /// A plain count of things.
    Count(u64),
    /// A span of time.
    Duration(Duration),
    /// A load average figure, which is a queue length rather than a percentage.
    Load(f32),
}

impl MeasuredValue {
    /// Renders the value for display, honouring the active byte unit family.
    #[must_use]
    pub fn render(self, units: ByteUnits) -> String {
        match self {
            Self::Bytes(bytes) => format_bytes(bytes, units),
            Self::ByteRate(rate) => format_byte_rate(rate, units),
            Self::EventRate(rate) => format!("{:.0}/s", rate.per_second()),
            Self::Percent(percent) => percent.to_string(),
            Self::Count(count) => count.to_string(),
            Self::Duration(duration) => format_age(duration),
            Self::Load(load) => format!("{load:.2}"),
        }
    }
}

/// A labelled raw measurement, used as pressure evidence and diagnostic evidence.
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Measurement {
    /// Short label, e.g. `"available"` or `"load1"`.
    pub label: &'static str,
    /// The measured quantity.
    pub value: MeasuredValue,
}

impl Measurement {
    /// Builds a labelled measurement.
    #[must_use]
    pub const fn new(label: &'static str, value: MeasuredValue) -> Self {
        Self { label, value }
    }

    /// Renders as `label value`, e.g. `available 4.2 GiB`.
    #[must_use]
    pub fn render(&self, units: ByteUnits) -> String {
        format!("{} {}", self.label, self.value.render(units))
    }
}

/// How serious a finding is.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Severity {
    /// Informational; no action implied.
    Info,
    /// Worth watching. Maps to the `watch` pressure state.
    Watch,
    /// Actively degrading the system. Maps to the `critical` pressure state.
    Critical,
}

impl Severity {
    /// A redundant non-color cue (§2.3, §5.2).
    #[must_use]
    pub const fn symbol(self) -> char {
        match self {
            Self::Info => '.',
            Self::Watch => '!',
            Self::Critical => 'X',
        }
    }

    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Watch => "watch",
            Self::Critical => "critical",
        }
    }
}

/// How much the evidence actually supports a heuristic conclusion.
///
/// §11.3 requires heuristic findings to be marked, and §2.2 forbids claiming
/// causation. Confidence is what the UI renders to keep that promise.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Confidence {
    /// Weak or partial evidence; a plausible correlation only.
    Low,
    /// Consistent evidence across several samples.
    Medium,
    /// Directly measured, not inferred.
    High,
}

impl Confidence {
    /// Lower-case label.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn measurements_render_with_their_unit_family() {
        let m = Measurement::new("available", MeasuredValue::Bytes(4 * 1024 * 1024 * 1024));
        assert_eq!(m.render(ByteUnits::Iec), "available 4.0 GiB");
        assert_eq!(m.render(ByteUnits::Si), "available 4.3 GB");
    }

    #[test]
    fn load_renders_as_a_queue_length_not_a_percentage() {
        let m = Measurement::new("load1", MeasuredValue::Load(11.4));
        assert_eq!(m.render(ByteUnits::Iec), "load1 11.40");
    }

    #[test]
    fn event_rates_are_distinct_from_byte_rates() {
        let rate = Rate::new(1024.0).expect("valid");
        assert_eq!(
            MeasuredValue::EventRate(rate).render(ByteUnits::Iec),
            "1024/s"
        );
        assert_eq!(
            MeasuredValue::ByteRate(rate).render(ByteUnits::Iec),
            "1.0K/s"
        );
    }

    #[test]
    fn severity_symbols_match_the_specified_ascii_cues() {
        assert_eq!(Severity::Info.symbol(), '.');
        assert_eq!(Severity::Watch.symbol(), '!');
        assert_eq!(Severity::Critical.symbol(), 'X');
    }

    #[test]
    fn severity_and_confidence_order_from_least_to_most() {
        assert!(Severity::Info < Severity::Watch);
        assert!(Severity::Watch < Severity::Critical);
        assert!(Confidence::Low < Confidence::Medium);
        assert!(Confidence::Medium < Confidence::High);
    }
}

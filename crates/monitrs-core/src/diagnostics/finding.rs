//! The §11.1 rule interface: what a rule is, and what it may say.
//!
//! A [`Finding`] is the *only* thing a rule may produce. It carries the raw
//! evidence and the time window that evidence covers, because §11.3 requires both
//! and because a conclusion without its inputs cannot be checked by the person
//! reading it. [`Confidence`] is part of the payload for the same reason: §11.3
//! requires heuristics to be marked as such, and §2.2 forbids claiming causation.

use core::time::Duration;

use crate::model::{Confidence, Measurement, Severity, SystemSnapshot};
use crate::units::{ByteUnits, format_duration};

use super::HistoryWindow;

/// The span of time one piece of evidence covers (§11.3).
///
/// A measurement read from the current sample and a count taken over fifteen
/// samples are both evidence, but they support very different claims, so the
/// window travels with the measurement rather than being described in prose.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct TimeWindow {
    /// Monotonic span between the oldest and newest sample the evidence covers.
    pub span: Duration,
    /// How many samples were actually read.
    ///
    /// Samples whose input was unavailable are not counted: §26 forbids treating
    /// a missing reading as a reading (§8.2).
    pub samples: usize,
}

impl TimeWindow {
    /// A single reading from the snapshot being evaluated.
    pub const CURRENT_SAMPLE: Self = Self {
        span: Duration::ZERO,
        samples: 1,
    };

    /// A window covering `samples` readings across `span`.
    #[must_use]
    pub const fn new(span: Duration, samples: usize) -> Self {
        Self { span, samples }
    }

    /// A single reading that is itself a moving average, such as a Linux PSI
    /// `avg10` figure.
    ///
    /// One read of `avg10` already summarizes ten seconds of kernel-measured
    /// stall time, so the honest window is those ten seconds even though only one
    /// sample was read.
    #[must_use]
    pub const fn moving_average(span: Duration) -> Self {
        Self { span, samples: 1 }
    }

    /// Whether this is a single instantaneous reading.
    #[must_use]
    pub const fn is_current_sample(&self) -> bool {
        self.samples <= 1 && self.span.is_zero()
    }

    /// Renders the window for the Inspect screen (§7.5).
    #[must_use]
    pub fn render(&self) -> String {
        if self.is_current_sample() {
            return "current sample".to_owned();
        }
        if self.samples <= 1 {
            return format!("last {}", format_duration(self.span));
        }
        format!(
            "{} samples over {}",
            self.samples,
            format_duration(self.span)
        )
    }
}

/// One raw measurement plus the window it was measured over (§11.3).
#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Evidence {
    /// The raw measurement. Never a derived verdict, always a number.
    pub measurement: Measurement,
    /// The time window `measurement` covers.
    pub window: TimeWindow,
}

impl Evidence {
    /// Builds evidence covering an explicit window.
    #[must_use]
    pub const fn new(measurement: Measurement, window: TimeWindow) -> Self {
        Self {
            measurement,
            window,
        }
    }

    /// Builds evidence read from the snapshot being evaluated.
    #[must_use]
    pub const fn current(measurement: Measurement) -> Self {
        Self::new(measurement, TimeWindow::CURRENT_SAMPLE)
    }

    /// Renders as `label value (window)`, e.g. `cpu busy 91% (15 samples over 14s)`.
    ///
    /// The byte unit family is passed in because §12 makes it a display setting;
    /// nothing in the diagnostic engine decides how a byte count looks.
    #[must_use]
    pub fn render(&self, units: ByteUnits) -> String {
        if self.window.is_current_sample() {
            return self.measurement.render(units);
        }
        format!(
            "{} ({})",
            self.measurement.render(units),
            self.window.render()
        )
    }
}

/// One rule's conclusion about the current state of the system (§11.1).
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Finding {
    /// The stable identifier of the rule that produced this.
    ///
    /// A `&'static str` rather than an enum so that rules can be registered
    /// without every consumer needing to be recompiled against a closed set, and
    /// so that the id can be logged and exported verbatim.
    pub rule_id: &'static str,
    /// How serious this is.
    pub severity: Severity,
    /// A short headline, e.g. `Sustained CPU saturation`.
    pub title: String,
    /// One or two sentences of explanation, including the counts the rule used.
    ///
    /// Deliberately free of byte counts: the unit family is a display setting, so
    /// byte-valued figures belong in [`Self::evidence`] where the UI can format
    /// them (§12).
    pub summary: String,
    /// The raw measurements the conclusion rests on (§11.3).
    pub evidence: Vec<Evidence>,
    /// How much the evidence actually supports the conclusion (§11.3).
    pub confidence: Confidence,
}

impl Finding {
    /// Builds a finding with no evidence attached yet.
    #[must_use]
    pub fn new(
        rule_id: &'static str,
        severity: Severity,
        title: impl Into<String>,
        summary: impl Into<String>,
        confidence: Confidence,
    ) -> Self {
        Self {
            rule_id,
            severity,
            title: title.into(),
            summary: summary.into(),
            evidence: Vec::new(),
            confidence,
        }
    }

    /// Attaches the raw evidence §11.3 requires.
    #[must_use]
    pub fn with_evidence(mut self, evidence: Vec<Evidence>) -> Self {
        self.evidence = evidence;
        self
    }

    /// The redundant non-color cue for this finding's severity (§5.2).
    #[must_use]
    pub const fn symbol(&self) -> char {
        self.severity.symbol()
    }

    /// The `WATCH: Sustained CPU saturation` line from §11.3's example.
    #[must_use]
    pub fn headline(&self) -> String {
        format!("{}: {}", self.severity.label().to_uppercase(), self.title)
    }

    /// The `Evidence: ...` line from §11.3's example.
    #[must_use]
    pub fn render_evidence(&self, units: ByteUnits) -> String {
        self.evidence
            .iter()
            .map(|item| item.render(units))
            .collect::<Vec<_>>()
            .join("; ")
    }

    /// The `Confidence: medium.` line from §11.3's example.
    #[must_use]
    pub fn render_confidence(&self) -> String {
        format!("confidence: {}", self.confidence.label())
    }
}

/// A deterministic rule over collected evidence (§11.1).
///
/// Rules are stateless and side-effect free: everything they may read is in the
/// two arguments, so the same inputs always produce the same finding. That is
/// what makes them testable from fixtures, and it is why hysteresis lives in
/// [`super::PressureEngine`] rather than inside a rule (§11.3).
///
/// `Send + Sync` because §10.3 puts sampling on its own thread; a rule set must
/// be shareable without a lock.
pub trait DiagnosticRule: Send + Sync {
    /// The stable identifier used in logs, exports, and tests.
    fn id(&self) -> &'static str;

    /// Evaluates the rule, returning a finding only when the rule actually fires.
    ///
    /// `None` is the normal answer. §11.3's minimum-sample requirement means a
    /// rule must also return `None` while history is too short to support the
    /// claim it makes, rather than guessing from one sample.
    fn evaluate(&self, current: &SystemSnapshot, history: &HistoryWindow<'_>) -> Option<Finding>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MeasuredValue;
    use crate::units::Percent;

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("valid percent")
    }

    #[test]
    fn a_single_reading_renders_without_a_window_suffix() {
        let evidence = Evidence::current(Measurement::new(
            "cpu busy",
            MeasuredValue::Percent(percent(91.0)),
        ));
        assert_eq!(evidence.render(ByteUnits::Iec), "cpu busy 91%");
        assert!(evidence.window.is_current_sample());
    }

    #[test]
    fn evidence_over_a_window_names_the_window_it_covers() {
        let evidence = Evidence::new(
            Measurement::new("samples above threshold", MeasuredValue::Count(12)),
            TimeWindow::new(Duration::from_secs(14), 15),
        );
        assert_eq!(
            evidence.render(ByteUnits::Iec),
            "samples above threshold 12 (15 samples over 14s)"
        );
    }

    #[test]
    fn a_moving_average_reports_the_span_it_summarizes_not_one_sample() {
        let window = TimeWindow::moving_average(Duration::from_secs(10));
        assert_eq!(window.samples, 1);
        assert_eq!(window.render(), "last 10s");
        assert!(!window.is_current_sample());
    }

    #[test]
    fn byte_evidence_is_rendered_in_the_callers_unit_family() {
        let evidence = Evidence::current(Measurement::new(
            "available",
            MeasuredValue::Bytes(4 * 1024 * 1024 * 1024),
        ));
        assert_eq!(evidence.render(ByteUnits::Iec), "available 4.0 GiB");
        assert_eq!(evidence.render(ByteUnits::Si), "available 4.3 GB");
    }

    #[test]
    fn a_finding_renders_the_three_lines_from_the_specification_example() {
        let finding = Finding::new(
            "cpu.sustained_saturation",
            Severity::Watch,
            "Sustained CPU saturation",
            "CPU busy at or above 90% in 12 of the last 15 samples.",
            Confidence::Medium,
        )
        .with_evidence(vec![
            Evidence::new(
                Measurement::new("cpu busy", MeasuredValue::Percent(percent(91.0))),
                TimeWindow::new(Duration::from_secs(14), 15),
            ),
            Evidence::current(Measurement::new("load1", MeasuredValue::Load(11.4))),
        ]);

        assert_eq!(finding.headline(), "WATCH: Sustained CPU saturation");
        assert_eq!(
            finding.render_evidence(ByteUnits::Iec),
            "cpu busy 91% (15 samples over 14s); load1 11.40"
        );
        assert_eq!(finding.render_confidence(), "confidence: medium");
        assert_eq!(finding.symbol(), '!', "§5.2 requires a non-color cue");
    }

    #[test]
    fn a_finding_without_evidence_renders_an_empty_evidence_line_rather_than_panicking() {
        let finding = Finding::new(
            "test.rule",
            Severity::Info,
            "Title",
            "Summary",
            Confidence::Low,
        );
        assert_eq!(finding.render_evidence(ByteUnits::Iec), "");
        assert_eq!(finding.headline(), "INFO: Title");
    }
}

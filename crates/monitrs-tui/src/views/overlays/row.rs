//! The line builders every overlay in this module is assembled from.
//!
//! Nine overlays render the same two shapes over and over: a heading, and a
//! `LABEL   value` row. Building them here once is what keeps the label column of
//! the confirmation dialog aligned with the label column of the process detail —
//! and, more importantly, it is what makes the §4/§5.2 rules structural rather
//! than repeated: [`metric_field`] is the *only* way an overlay turns a
//! [`MetricDisplay`] into a row, and it always emits the state symbol beside the
//! text. An overlay therefore cannot render `permission denied` without its `!`,
//! and cannot render a retained value without its age.
//!
//! Widths are measured, never assumed: [`line_width`] and [`widest`] are what let
//! an overlay size its own panel from its content instead of from a guess, which is
//! §5.4's "reserve widths based on panel geometry" applied to a dialog.

use monitrs_core::units::{display_width, format_age, pad_right};
use ratatui::text::{Line, Span};

use crate::theme::Token;
use crate::widgets::{MetricDisplay, Presentation};

/// The label column width of a `LABEL   value` row, in cells.
///
/// Wide enough for the longest label the dialogs use (`DESCENDANTS`) so that no
/// dialog has to shorten a word, and fixed rather than computed so two overlays
/// stacked on top of each other line their values up (§5.4).
pub const FIELD_LABEL_WIDTH: usize = 12;

/// Cells between a field's label and its value.
const FIELD_GAP: usize = 2;

/// The display width of `line` in terminal cells.
///
/// Sums the spans with `monitrs-core`'s width function rather than trusting
/// [`Line::width`], so that every width in this crate — truncation, padding,
/// panel sizing — is computed by exactly one implementation.
#[must_use]
pub fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| display_width(span.content.as_ref()))
        .sum()
}

/// The width of the widest line, in cells. Zero for an empty slice.
#[must_use]
pub fn widest(lines: &[Line<'_>]) -> usize {
    lines.iter().map(line_width).max().unwrap_or(0)
}

/// A section heading, drawn in the screen's single accent (§5.2).
#[must_use]
pub fn heading(presentation: Presentation<'_>, text: &str) -> Line<'static> {
    Line::from(vec![Span::styled(
        text.to_owned(),
        presentation.style(Token::Accent),
    )])
}

/// A line of ordinary prose.
#[must_use]
pub fn plain(presentation: Presentation<'_>, text: &str) -> Line<'static> {
    styled(presentation, text, Token::Text)
}

/// A line of secondary prose: notes, provenance, and disclaimers.
#[must_use]
pub fn muted(presentation: Presentation<'_>, text: &str) -> Line<'static> {
    styled(presentation, text, Token::Muted)
}

/// A line in an explicit token, for text whose meaning is its severity.
///
/// The caller is responsible for the redundant character cue §5.2 requires; every
/// call site in this module writes one into `text` itself.
#[must_use]
pub fn styled(presentation: Presentation<'_>, text: &str, token: Token) -> Line<'static> {
    Line::from(vec![Span::styled(
        text.to_owned(),
        presentation.style(token),
    )])
}

/// An empty spacer line.
#[must_use]
pub fn blank() -> Line<'static> {
    Line::from(Vec::new())
}

/// A `LABEL   value` row whose value is known text rather than a metric.
///
/// Used for the things that cannot be unavailable — a PID, a signal name, the
/// process state — so that they read identically to the fields that can.
#[must_use]
pub fn text_field(
    presentation: Presentation<'_>,
    label: &str,
    value: &str,
    token: Token,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            label_cell(presentation, label),
            presentation.style(Token::Muted),
        ),
        Span::styled(value.to_owned(), presentation.style(token)),
    ])
}

/// A `LABEL   value` row for a [`MetricDisplay`].
///
/// The value carries its state symbol and, when it is a retained value, its age —
/// §4 allows a stale value on screen only alongside how old it is, and this is the
/// one place an overlay can produce such a row, so the pair cannot come apart.
#[must_use]
pub fn metric_field(
    presentation: Presentation<'_>,
    label: &str,
    display: &MetricDisplay,
) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            label_cell(presentation, label),
            presentation.style(Token::Muted),
        ),
        Span::styled(metric_text(display), presentation.metric_style(display)),
    ])
}

/// The text of a metric display for a dialog: `symbol value`, plus the measurement
/// age when the value is a retained one.
///
/// The symbol occupies a leading cell whatever the state is, so a field does not
/// shift when a metric becomes unavailable (§5.4). The age is spelled out rather
/// than abbreviated because §2.1 and §26 require historical or retained data to be
/// *unmistakable*, and a dialog has the room to say it in words.
#[must_use]
pub fn metric_text(display: &MetricDisplay) -> String {
    match display.age() {
        Some(age) => format!(
            "{} {} measured {} ago",
            display.symbol(),
            display.text(),
            format_age(age)
        ),
        None => format!("{} {}", display.symbol(), display.text()),
    }
}

/// One `LABEL value` pair of a dense row.
#[derive(Clone, Debug)]
pub struct Pair {
    /// The field name, in [`Token::Muted`].
    pub label: &'static str,
    /// The rendered value.
    pub value: String,
    /// The token the value is drawn in.
    pub token: Token,
}

impl Pair {
    /// A pair whose value is known text.
    #[must_use]
    pub fn text(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
            token: Token::Text,
        }
    }

    /// A pair whose value is a metric, carrying its symbol and age (§4, §5.2).
    #[must_use]
    pub fn metric(label: &'static str, display: &MetricDisplay) -> Self {
        Self {
            label,
            value: metric_text(display),
            token: display.token(),
        }
    }
}

/// Several `LABEL value` pairs on one row (§5.4's dense grid).
///
/// A dialog that gave every field its own row would be twenty rows tall before the
/// scrolling part started, which at §5.7's 80×24 leaves nothing to scroll. Packing the
/// short fields is what keeps the fixed part of a dialog short — and because each pair
/// is measured rather than padded to a reserved width, the row is exactly as wide as
/// its content and the panel can size itself from it.
#[must_use]
pub fn pairs(presentation: Presentation<'_>, pairs: &[Pair]) -> Line<'static> {
    let mut spans = Vec::with_capacity(pairs.len().saturating_mul(3));
    for (index, pair) in pairs.iter().enumerate() {
        if index > 0 {
            spans.push(Span::raw(" ".repeat(PAIR_GAP)));
        }
        spans.push(Span::styled(
            format!("{} ", pair.label),
            presentation.style(Token::Muted),
        ));
        spans.push(Span::styled(
            pair.value.clone(),
            presentation.style(pair.token),
        ));
    }
    Line::from(spans)
}

/// Cells between two pairs of a dense row.
const PAIR_GAP: usize = 3;

/// The label column of a field row, padded to [`FIELD_LABEL_WIDTH`] plus the gap.
fn label_cell(presentation: Presentation<'_>, label: &str) -> String {
    pad_right(
        label,
        FIELD_LABEL_WIDTH + FIELD_GAP,
        presentation.ellipsis(),
    )
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::MetricState;
    use monitrs_core::units::Percent;

    use super::*;
    use crate::widgets::states::describe_percent;

    fn presentation() -> Presentation<'static> {
        Presentation::default()
    }

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    #[test]
    fn a_field_row_aligns_its_value_at_a_fixed_column() {
        let short = text_field(presentation(), "PID", "31842", Token::Text);
        let long = text_field(presentation(), "DESCENDANTS", "12", Token::Text);
        let column = |line: &Line<'_>| {
            line.spans
                .first()
                .map_or(0, |span| display_width(span.content.as_ref()))
        };
        assert_eq!(column(&short), column(&long));
        assert_eq!(column(&short), FIELD_LABEL_WIDTH + FIELD_GAP);
    }

    #[test]
    fn a_label_longer_than_the_column_is_truncated_rather_than_shifting_the_value() {
        let line = text_field(
            presentation(),
            "AN ABSURDLY LONG LABEL",
            "value",
            Token::Text,
        );
        assert_eq!(
            line.spans
                .first()
                .map_or(0, |span| display_width(span.content.as_ref())),
            FIELD_LABEL_WIDTH + FIELD_GAP
        );
    }

    #[test]
    fn an_unavailable_metric_field_never_reads_as_a_measured_zero() {
        for state in [
            MetricState::<Percent>::PermissionDenied,
            MetricState::WarmingUp,
            MetricState::Unsupported,
        ] {
            let display = describe_percent(&state);
            let line = metric_field(presentation(), "CPU", &display);
            let text: String = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
            assert!(!text.contains('0'), "{state:?} rendered {text:?}");
            assert!(
                text.contains(display.symbol()),
                "{state:?} lost its §5.2 symbol"
            );
        }
    }

    #[test]
    fn a_retained_value_is_rendered_together_with_its_age() {
        let stale = MetricState::Available(percent(71.0)).into_stale(Duration::from_secs(4));
        let display = describe_percent(&stale);
        let text = metric_text(&display);
        assert!(text.contains("71%"), "{text}");
        assert!(text.contains("00:04"), "{text}");
        assert!(text.starts_with('~'), "{text}");
    }

    #[test]
    fn a_fresh_value_keeps_a_leading_cell_so_the_column_does_not_shift() {
        let fresh = describe_percent(&MetricState::Available(percent(37.0)));
        let denied = describe_percent(&MetricState::<Percent>::PermissionDenied);
        assert_eq!(metric_text(&fresh), "  37%");
        assert_eq!(metric_text(&denied), "! permission denied");
    }

    #[test]
    fn widths_are_measured_across_every_span_of_a_line() {
        let line = text_field(presentation(), "NAME", "rustc", Token::Text);
        assert_eq!(line_width(&line), FIELD_LABEL_WIDTH + FIELD_GAP + 5);
        assert_eq!(widest(&[]), 0);
        assert_eq!(
            widest(&[blank(), line.clone()]),
            FIELD_LABEL_WIDTH + FIELD_GAP + 5
        );
    }

    #[test]
    fn a_double_width_value_is_measured_in_cells_not_characters() {
        let line = text_field(presentation(), "NAME", "\u{65e5}\u{672c}", Token::Text);
        assert_eq!(line_width(&line), FIELD_LABEL_WIDTH + FIELD_GAP + 4);
    }
}

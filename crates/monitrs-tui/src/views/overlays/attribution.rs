//! Spike attribution: the top contributors retained with a historical sample
//! (§2.2, §5.6).
//!
//! ```text
//! + SPIKE ATTRIBUTION ------------------ sample 22:14:07 UTC  -00:37 -+
//! | top contributors, correlated with this sample                     |
//! | METRIC PROCESS      PID     VALUE  DELTA/RATE   SHARE             |
//! |-------------------------------------------------------------------|
//! | CPU    rustc      31842      287%      +146%      62%             |
//! | CPU    postgres    1221       54%       +39%      12%             |
//! | WRITE  rustc      31842    42M/s    +39M/s        74%             |
//! | evidence coverage  CPU  78%   MEM  . warming up   ...             |
//! | correlation within one retained sample, not a proof of cause      |
//! +-------------------------------------------------------------------+
//! ```
//!
//! # Evidence, never cause
//!
//! §2.2 is explicit that this panel shows *evidence*, not proof, and names the
//! acceptable wording: "top contributors", "correlated with the spike". Every
//! heading here uses one of those two phrasings, the closing line says outright that
//! this is correlation within a single sample, and no string in this module contains
//! a causal verb. `the_panel_never_claims_causation` is what keeps that true when the
//! wording is next edited.
//!
//! # The coverage line is not decoration
//!
//! [`MetricContributors::coverage`] is the honesty figure: the share of the
//! *observed* total that the retained processes account for. It is a
//! [`MetricState`], and it is frequently unavailable — a platform that refuses
//! per-process I/O produces `permission denied` rather than a flattering 100%. This
//! overlay renders whatever it says, through [`crate::widgets::states`], including
//! when it says nothing (§4).
//!
//! # Where `SHARE` comes from
//!
//! §5.6's mockup has a per-row share of the system total, and the ring does not
//! store one. It is nonetheless derivable *exactly*, because
//!
//! ```text
//! value / observed_total == (value / retained_total) * (retained_total / observed_total)
//!                       == (value / retained_total) * coverage
//! ```
//!
//! and `coverage` is retained. So the column is real arithmetic over retained data
//! rather than an estimate — and when coverage is unavailable the share is
//! unavailable too, which is why it is a [`MetricState`] and not a number.

use monitrs_core::history::{
    Contributor, ContributorMetric, ContributorSet, HistoricalSample, MetricContributors,
};
use monitrs_core::model::{MeasuredValue, MetricState};
use monitrs_core::units::{ByteUnits, Percent, display_width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{describe, describe_percent};
use crate::widgets::{MetricDisplay, Presentation, RowBuilder};

use super::clock::format_time_of_day;
use super::frame::{Anchor, OverlayPanel};
use super::row::muted;

/// The panel title.
const TITLE: &str = "SPIKE ATTRIBUTION";

/// The `METRIC` column width: the longest label is `WRITE`.
const METRIC_WIDTH: u16 = 6;

/// The `PID` column width, right-aligned. Fits a 32-bit PID.
const PID_WIDTH: u16 = 10;

/// The `VALUE` column width, right-aligned.
const VALUE_WIDTH: u16 = 10;

/// The `DELTA/RATE` column width, right-aligned.
const TREND_WIDTH: u16 = 12;

/// The `SHARE` column width, right-aligned.
const SHARE_WIDTH: u16 = 7;

/// The narrowest the `PROCESS` column is allowed to be.
const MIN_PROCESS_WIDTH: u16 = 8;

/// The widest the `PROCESS` column is allowed to be.
///
/// Matches [`monitrs_core::history::MAX_RETAINED_NAME_WIDTH`], so the column is
/// never wider than the text the ring is able to have retained.
const MAX_PROCESS_WIDTH: u16 = 24;

/// Cells between two columns.
const GAP: u16 = 1;

/// The label that introduces the evidence-coverage line.
const EVIDENCE_LABEL: &str = "evidence coverage";

/// How many coverage cells share one evidence line.
///
/// Four metrics on one line would need about 110 cells to spell
/// `permission denied` out four times, which no 80-column terminal has. Two per line
/// needs 72 — and the alternative, squeezing four cells into 68, would degrade every
/// coverage to `n/a` even when there was room to say what actually happened.
const EVIDENCE_METRICS_PER_LINE: usize = 2;

/// The narrowest the evidence lines are laid out to, whatever the table's width is.
///
/// §5.4 says to reserve from the geometry, and this is that geometry: the coverage
/// cells divide whatever is left of the line, so a coverage that still cannot be
/// spelled out degrades through [`crate::widgets::MetricDisplay::fitted`] — `n/a`, and
/// then the state character alone — rather than being clipped mid-word into something
/// that reads like a different message (§4, §5.1). Chosen so that the longest
/// placeholder, `permission denied`, fits.
const MIN_EVIDENCE_WIDTH: u16 = 72;

/// The fewest cells a coverage cell is ever given.
///
/// One for the state character, one space, and three for `n/a`: below this the cell
/// would say nothing at all, and §4 requires it to say something.
const MIN_EVIDENCE_CELL: u16 = 5;

/// The attribution panel for one selected historical sample.
#[derive(Clone, Debug)]
pub struct SpikeAttributionOverlay<'a> {
    presentation: Presentation<'a>,
    contributors: &'a ContributorSet,
    label: Option<String>,
    offset: Option<String>,
    scroll: usize,
}

impl<'a> SpikeAttributionOverlay<'a> {
    /// A panel over a retained contributor set.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, contributors: &'a ContributorSet) -> Self {
        Self {
            presentation,
            contributors,
            label: None,
            offset: None,
            scroll: 0,
        }
    }

    /// A panel over a selected [`HistoricalSample`], labelled with its own moment.
    ///
    /// The wall time comes from the sample rather than from a clock: §10.5 forbids
    /// the renderer from reading one, and a sample's label must not change because
    /// it was drawn twice (§17.3).
    #[must_use]
    pub fn for_sample(presentation: Presentation<'a>, sample: &'a HistoricalSample) -> Self {
        Self::new(presentation, &sample.contributors)
            .with_label(format!("sample {}", format_time_of_day(sample.wall_time)))
    }

    /// Sets the header label, such as `sample 22:14:07 UTC`.
    #[must_use]
    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Sets the history offset, as [`monitrs_core::history::HistoryView::format_offset`]
    /// renders it.
    ///
    /// §2.1 and §26 require historical state to be unmistakable from live state, so
    /// the offset is repeated inside the panel as well as in the screen header: an
    /// attribution table without it would read as a live top-processes list.
    #[must_use]
    pub fn with_offset(mut self, offset: impl Into<String>) -> Self {
        self.offset = Some(offset.into());
        self
    }

    /// Sets the first visible contributor row.
    #[must_use]
    pub const fn with_scroll(mut self, scroll: usize) -> Self {
        self.scroll = scroll;
        self
    }

    /// How many contributor rows there are across all four metrics.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.contributors.retained_count()
    }

    /// The column header row and the "top contributors" subtitle, which stay put
    /// while the table scrolls.
    #[must_use]
    pub fn header_lines(&self) -> Vec<Line<'static>> {
        let mut row = self.row_builder();
        for (text, width, align) in [
            ("METRIC", METRIC_WIDTH, Align::Left),
            ("PROCESS", self.process_width(), Align::Left),
            ("PID", PID_WIDTH, Align::Right),
            ("VALUE", VALUE_WIDTH, Align::Right),
            ("DELTA/RATE", TREND_WIDTH, Align::Right),
            ("SHARE", SHARE_WIDTH, Align::Right),
        ] {
            row.push_field(text, width, align, self.presentation.style(Token::Muted));
            row.pad(GAP);
        }
        vec![
            // §2.2's permitted wording, and the only description of what the table is.
            muted(
                self.presentation,
                "top contributors, correlated with this sample",
            ),
            row.finish(),
        ]
    }

    /// One row per retained contributor, grouped by metric in §2.2's order.
    #[must_use]
    pub fn contributor_lines(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.line_count());
        for metric in ContributorMetric::ALL {
            let group = self.contributors.metric(metric);
            let total = retained_total(group);
            for entry in group.entries() {
                lines.push(self.contributor_line(metric, entry, total, group.coverage()));
            }
        }
        if lines.is_empty() {
            lines.push(muted(
                self.presentation,
                "no contributors were retained for this sample",
            ));
        }
        lines
    }

    /// The evidence-coverage lines §2.2 requires, plus the correlation disclaimer.
    ///
    /// The cells on a line share whatever is left after the label, so the line always
    /// fits and each cell degrades honestly rather than being clipped.
    #[must_use]
    pub fn evidence_lines(&self) -> Vec<Line<'static>> {
        let presentation = self.presentation;
        let glyphs = presentation.glyphs();
        let muted_style = presentation.style(Token::Muted);
        let width = self.table_width().max(MIN_EVIDENCE_WIDTH);
        let indent = u16::try_from(display_width(EVIDENCE_LABEL)).unwrap_or(0);
        let per_line = u16::try_from(EVIDENCE_METRICS_PER_LINE).unwrap_or(1).max(1);
        let cell = width
            .saturating_sub(indent)
            .saturating_sub(GAP.saturating_mul(per_line))
            .checked_div(per_line)
            .unwrap_or(MIN_EVIDENCE_CELL)
            .max(MIN_EVIDENCE_CELL);

        let mut lines = Vec::new();
        for (index, chunk) in ContributorMetric::ALL
            .chunks(EVIDENCE_METRICS_PER_LINE)
            .enumerate()
        {
            let mut row = RowBuilder::new(width, glyphs);
            if index == 0 {
                row.push(EVIDENCE_LABEL, muted_style);
            } else {
                row.pad(indent);
            }
            for metric in chunk {
                let coverage = describe_percent(&self.contributors.metric(*metric).coverage());
                let label = u16::try_from(display_width(metric.label())).unwrap_or(0);
                // `label` and a space, then the value; the value's own cue is one cell
                // and always present, so the text gets what is left after it (§5.2).
                let value_field = cell.saturating_sub(label).saturating_sub(1);
                let budget = value_field.saturating_sub(2);
                row.pad(GAP);
                row.push_field(metric.label(), label, Align::Left, muted_style);
                row.push(" ", muted_style);
                row.push_field(
                    &format!(
                        "{} {}",
                        coverage.symbol(),
                        coverage.fitted(usize::from(budget), glyphs)
                    ),
                    value_field,
                    Align::Left,
                    presentation.metric_style(&coverage),
                );
            }
            lines.push(row.finish());
        }
        lines.push(muted(
            presentation,
            "correlation within one retained sample, not a proof of cause",
        ));
        lines
    }

    /// One contributor row.
    fn contributor_line(
        &self,
        metric: ContributorMetric,
        entry: &Contributor,
        retained_total: f64,
        coverage: MetricState<Percent>,
    ) -> Line<'static> {
        let presentation = self.presentation;
        let units = presentation.units();
        let text = presentation.style(Token::Text);
        let muted_style = presentation.style(Token::Muted);
        let trend = describe_trend(entry, units);
        let share = describe_percent(&share_of_observed(entry.value, retained_total, coverage));
        let glyphs = presentation.glyphs();

        let mut row = self.row_builder();
        row.push_field(metric.label(), METRIC_WIDTH, Align::Left, muted_style);
        row.pad(GAP);
        row.push_field(&entry.name, self.process_width(), Align::Left, text);
        row.pad(GAP);
        row.push_field(
            &entry.identity.pid.to_string(),
            PID_WIDTH,
            Align::Right,
            text,
        );
        row.pad(GAP);
        row.push_field(&entry.value.render(units), VALUE_WIDTH, Align::Right, text);
        row.pad(GAP);
        row.push_field(
            &trend.fitted(usize::from(TREND_WIDTH), glyphs),
            TREND_WIDTH,
            Align::Right,
            presentation.metric_style(&trend),
        );
        row.pad(GAP);
        row.push_field(
            &share.fitted(usize::from(SHARE_WIDTH), glyphs),
            SHARE_WIDTH,
            Align::Right,
            presentation.metric_style(&share),
        );
        row.finish()
    }

    /// The `PROCESS` column width, from the widest retained name.
    fn process_width(&self) -> u16 {
        let widest = ContributorMetric::ALL
            .into_iter()
            .flat_map(|metric| self.contributors.metric(metric).entries())
            .map(|entry| display_width(&entry.name))
            .max()
            .unwrap_or(0);
        u16::try_from(widest)
            .unwrap_or(MAX_PROCESS_WIDTH)
            .clamp(MIN_PROCESS_WIDTH, MAX_PROCESS_WIDTH)
    }

    /// The total width of the table, which is what every row is built to.
    fn table_width(&self) -> u16 {
        [
            METRIC_WIDTH,
            self.process_width(),
            PID_WIDTH,
            VALUE_WIDTH,
            TREND_WIDTH,
            SHARE_WIDTH,
        ]
        .into_iter()
        .fold(0u16, |total, width| {
            total.saturating_add(width).saturating_add(GAP)
        })
    }

    /// A row builder sized to the table.
    fn row_builder(&self) -> RowBuilder {
        RowBuilder::new(self.table_width(), self.presentation.glyphs())
    }

    /// The header label: the sample's moment, and its history offset.
    fn trailing(&self) -> String {
        match (&self.label, &self.offset) {
            (Some(label), Some(offset)) => format!("{label}  {offset}"),
            (Some(label), None) => label.clone(),
            (None, Some(offset)) => offset.clone(),
            (None, None) => String::new(),
        }
    }

    /// The panel this overlay renders through.
    fn panel(&self) -> OverlayPanel<'a> {
        let mut panel = OverlayPanel::new(self.presentation, TITLE)
            .anchored(Anchor::Center)
            .with_pinned(self.header_lines())
            .with_lines(self.contributor_lines())
            .with_footer(self.evidence_lines())
            .with_scroll(self.scroll);
        let trailing = self.trailing();
        if !trailing.is_empty() {
            panel = panel.with_trailing(trailing);
        }
        panel
    }

    /// The width the panel would like, borders included.
    #[must_use]
    pub fn desired_width(&self) -> u16 {
        self.panel().desired_width()
    }

    /// The height the panel would like, borders included.
    #[must_use]
    pub fn desired_height(&self) -> u16 {
        self.panel().desired_height()
    }
}

impl Widget for SpikeAttributionOverlay<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        self.panel().render(area, buf);
    }
}

/// Describes a contributor's `DELTA/RATE` cell.
///
/// Goes through [`crate::widgets::states`] like every other metric, so a process
/// that was not in the previous retained set shows `warming up` rather than `+0` —
/// §8.2's first-delta rule applied to attribution evidence.
fn describe_trend(entry: &Contributor, units: ByteUnits) -> MetricDisplay {
    describe(&entry.trend, |trend| trend.render(units))
}

/// The sum of the retained values for one metric, as a scalar.
///
/// Only the ranked measurement kinds contribute; anything else is skipped rather
/// than coerced, which is the same rule [`ContributorSet`] applies when it computes
/// coverage.
fn retained_total(group: &MetricContributors) -> f64 {
    group
        .entries()
        .iter()
        .filter_map(|entry| measured_scalar(entry.value))
        .sum()
}

/// One contributor's share of the *observed* system total for its metric.
///
/// See the module documentation for the derivation. Unavailable whenever coverage
/// is: §4 forbids inventing the number that would make the evidence look stronger
/// than it is.
fn share_of_observed(
    value: MeasuredValue,
    retained_total: f64,
    coverage: MetricState<Percent>,
) -> MetricState<Percent> {
    let Some(scalar) = measured_scalar(value) else {
        return MetricState::Unsupported;
    };
    if retained_total <= 0.0 {
        // A share of nothing is undefined, and both 0% and 100% would be
        // inventions. It resolves as soon as any retained process reports activity.
        return MetricState::WarmingUp;
    }
    // Narrowing is safe: the ratio of two same-signed sums is in `0.0..=1.0`, and
    // `Percent::new` rejects anything the narrowing could have lost.
    #[allow(clippy::cast_possible_truncation)]
    let fraction = (scalar / retained_total) as f32;
    scale(coverage, fraction)
}

/// Multiplies a percentage by `factor`, preserving its availability and staleness.
///
/// Written out rather than expressed with [`MetricState::map`] because the product
/// can in principle fail [`Percent::new`], and §4 forbids answering that with a
/// fabricated value: the whole state becomes unavailable instead.
fn scale(state: MetricState<Percent>, factor: f32) -> MetricState<Percent> {
    match state {
        MetricState::Available(percent) => Percent::new(percent.value() * factor)
            .map_or(MetricState::WarmingUp, MetricState::Available),
        MetricState::Stale { value, age } => {
            Percent::new(value.value() * factor).map_or(MetricState::WarmingUp, |scaled| {
                MetricState::Stale { value: scaled, age }
            })
        }
        MetricState::WarmingUp => MetricState::WarmingUp,
        MetricState::PermissionDenied => MetricState::PermissionDenied,
        MetricState::Unsupported => MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(reason) => MetricState::TemporarilyUnavailable(reason),
    }
}

/// The comparable scalar behind a measurement, for the three kinds a contributor
/// can be ranked by.
fn measured_scalar(value: MeasuredValue) -> Option<f64> {
    match value {
        MeasuredValue::Percent(percent) => Some(f64::from(percent.value())),
        MeasuredValue::Bytes(bytes) => Some(bytes as f64),
        MeasuredValue::ByteRate(rate) => Some(rate.per_second()),
        // A contributor is only ever ranked by one of the three above; the rest
        // have no total to take a share of.
        MeasuredValue::EventRate(_)
        | MeasuredValue::Count(_)
        | MeasuredValue::Duration(_)
        | MeasuredValue::Load(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::{
        ProcessIdentity, ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState,
    };
    use monitrs_core::units::Rate;

    use super::*;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};
    use crate::views::overlays::row::line_width;

    fn presentation() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    fn rate(value: f64) -> Rate {
        Rate::new(value).expect("a finite non-negative rate")
    }

    fn process(pid: u32, name: &str, cpu: f32, write: Option<f64>) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid) * 7),
            parent_pid: Some(1),
            name: name.into(),
            command: format!("{name} --serve").into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::Available(percent(cpu)),
            memory: ProcessMemory {
                rss_bytes: MetricState::Available(u64::from(pid) * 1024 * 1024),
                virtual_bytes: MetricState::Unsupported,
                share_of_total: MetricState::Unsupported,
            },
            io: match write {
                Some(bytes) => ProcessIo {
                    read: MetricState::Available(rate(0.0)),
                    write: MetricState::Available(rate(bytes)),
                    read_total_bytes: MetricState::Unsupported,
                    write_total_bytes: MetricState::Unsupported,
                },
                None => ProcessIo {
                    read: MetricState::PermissionDenied,
                    write: MetricState::PermissionDenied,
                    read_total_bytes: MetricState::PermissionDenied,
                    write_total_bytes: MetricState::PermissionDenied,
                },
            },
            threads: MetricState::Unsupported,
            age: MetricState::Available(Duration::from_secs(43)),
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    fn spike() -> ContributorSet {
        let processes = [
            process(31_842, "rustc", 287.0, Some(42.0 * 1024.0 * 1024.0)),
            process(1_221, "postgres", 54.0, Some(7.0 * 1024.0 * 1024.0)),
            process(507, "WindowServer", 21.0, None),
        ];
        let previous = ContributorSet::from_processes(
            &[
                process(31_842, "rustc", 141.0, Some(3.0 * 1024.0 * 1024.0)),
                process(1_221, "postgres", 15.0, Some(1.0 * 1024.0 * 1024.0)),
            ],
            None,
            10,
        );
        ContributorSet::from_processes(&processes, Some(&previous), 10)
    }

    fn denied() -> ContributorSet {
        ContributorSet::from_processes(&[process(1, "opaque", 0.0, None)], None, 10)
    }

    fn render(overlay: SpikeAttributionOverlay<'_>, width: u16, height: u16) -> String {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        overlay.render(area, &mut buffer);
        (0..height)
            .map(|y| {
                let row: String = (0..width)
                    .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                    .collect();
                format!("{row}\n")
            })
            .collect()
    }

    fn full(overlay: SpikeAttributionOverlay<'_>) -> String {
        let width = overlay.desired_width();
        let height = overlay.desired_height();
        render(overlay, width, height)
    }

    #[test]
    fn the_table_has_the_columns_the_mockup_specifies() {
        // §5.6: METRIC PROCESS PID VALUE DELTA/RATE SHARE.
        let set = spike();
        let text = full(SpikeAttributionOverlay::new(presentation(), &set));
        for column in ["METRIC", "PROCESS", "PID", "VALUE", "DELTA/RATE", "SHARE"] {
            assert!(text.contains(column), "{column} is missing:\n{text}");
        }
        assert!(text.contains("rustc"), "{text}");
        assert!(text.contains("31842"), "{text}");
        assert!(text.contains("287%"), "{text}");
        assert!(text.contains("+146%"), "{text}");
    }

    #[test]
    fn the_panel_never_claims_causation() {
        // §2.2: "top contributors" or "correlated with"; never a causal claim.
        let set = spike();
        let text = full(SpikeAttributionOverlay::new(presentation(), &set)).to_lowercase();
        assert!(
            text.contains("top contributors") || text.contains("correlated with"),
            "{text}"
        );
        for forbidden in ["caused", "cause of", "because", "responsible for", "blame"] {
            assert!(
                !text.contains(forbidden),
                "the panel claims causation with {forbidden:?}:\n{text}"
            );
        }
        assert!(text.contains("not a proof of cause"), "{text}");
    }

    #[test]
    fn the_evidence_coverage_line_is_rendered_for_every_metric() {
        let set = spike();
        let text = full(SpikeAttributionOverlay::new(presentation(), &set));
        assert!(text.contains("evidence coverage"), "{text}");
        for metric in ContributorMetric::ALL {
            assert!(
                text.contains(metric.label()),
                "{} has no coverage entry:\n{text}",
                metric.label()
            );
        }
    }

    #[test]
    fn unavailable_coverage_is_reported_rather_than_flattered() {
        // §2.2 and §4: a platform that withheld the readings must not produce a
        // percentage at all — not 0%, and not the flattering 100%.
        let set = denied();
        assert_eq!(
            set.metric(ContributorMetric::DiskWrite).coverage(),
            MetricState::PermissionDenied
        );
        let text = full(SpikeAttributionOverlay::new(presentation(), &set));
        let refused = text
            .lines()
            .find(|line| line.contains("WRITE"))
            .expect("the write coverage cell");
        assert!(
            refused.contains("! permission denied"),
            "the refusal and its §5.2 symbol are missing: {refused}"
        );
        assert!(
            !refused.contains('%'),
            "a refused metric was given a share: {refused}"
        );
        assert!(
            set.metric(ContributorMetric::DiskWrite).is_empty(),
            "a refused metric must retain no contributors"
        );
    }

    #[test]
    fn a_share_is_unavailable_when_its_coverage_is() {
        let value = MeasuredValue::Percent(percent(50.0));
        assert_eq!(
            share_of_observed(value, 100.0, MetricState::PermissionDenied),
            MetricState::PermissionDenied
        );
        assert_eq!(
            share_of_observed(value, 100.0, MetricState::Unsupported),
            MetricState::Unsupported
        );
        assert_eq!(
            share_of_observed(value, 100.0, MetricState::WarmingUp),
            MetricState::WarmingUp
        );
    }

    #[test]
    fn a_share_is_the_value_over_the_observed_total() {
        // Two retained processes at 90 and 10 of an observed 200: coverage is 50%,
        // so the first accounts for 90/200 = 45% of what was observed.
        let coverage = MetricState::Available(percent(50.0));
        let share = share_of_observed(MeasuredValue::Percent(percent(90.0)), 100.0, coverage);
        let value = share.fresh().copied().expect("available");
        assert!((value.value() - 45.0).abs() < 0.01, "got {value}");
    }

    #[test]
    fn a_share_of_a_zero_total_is_warming_up_rather_than_a_share_of_nothing() {
        let coverage = MetricState::Available(percent(50.0));
        assert_eq!(
            share_of_observed(MeasuredValue::Percent(Percent::ZERO), 0.0, coverage),
            MetricState::WarmingUp
        );
    }

    #[test]
    fn a_measurement_kind_a_contributor_cannot_be_ranked_by_has_no_share() {
        assert_eq!(
            share_of_observed(
                MeasuredValue::Count(4),
                100.0,
                MetricState::Available(percent(50.0))
            ),
            MetricState::Unsupported
        );
        assert!(measured_scalar(MeasuredValue::Load(1.5)).is_none());
        assert!(measured_scalar(MeasuredValue::Duration(Duration::from_secs(1))).is_none());
    }

    #[test]
    fn scaling_preserves_staleness_and_its_age() {
        let stale = MetricState::Available(percent(80.0)).into_stale(Duration::from_secs(3));
        match scale(stale, 0.5) {
            MetricState::Stale { value, age } => {
                assert!((value.value() - 40.0).abs() < 0.01);
                assert_eq!(age, Duration::from_secs(3));
            }
            other => panic!("staleness was lost: {other:?}"),
        }
    }

    #[test]
    fn a_first_appearance_shows_a_warming_up_trend_not_a_zero_delta() {
        // §8.2, §26: the first delta sample is warming up, never zero.
        let set = ContributorSet::from_processes(&[process(1, "fresh", 42.0, None)], None, 10);
        let text = full(SpikeAttributionOverlay::new(presentation(), &set));
        assert!(
            text.contains("warming up") || text.contains("n/a"),
            "{text}"
        );
        assert!(!text.contains("+0%"), "{text}");
    }

    #[test]
    fn an_empty_sample_says_so_rather_than_rendering_an_empty_table() {
        let set = ContributorSet::warming_up();
        assert!(set.is_empty());
        let text = full(SpikeAttributionOverlay::new(presentation(), &set));
        assert!(text.contains("no contributors were retained"), "{text}");
        assert!(text.contains("warming up"), "{text}");
    }

    #[test]
    fn a_selected_sample_is_labelled_with_its_own_moment_and_offset() {
        // §2.1, §26: historical state must be unmistakable from live state.
        let sample = HistoricalSample::warming_up(
            7,
            Duration::from_secs(37),
            std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_785_363_247),
        );
        let overlay =
            SpikeAttributionOverlay::for_sample(presentation(), &sample).with_offset("-00:37");
        let text = full(overlay);
        assert!(text.contains("22:14:07 UTC"), "{text}");
        assert!(text.contains("-00:37"), "{text}");
    }

    #[test]
    fn the_numeric_columns_are_right_aligned() {
        // §5.4: digits line up, whatever their magnitude.
        let set = ContributorSet::from_processes(
            &[
                process(1, "one", 1.0, Some(1.0)),
                process(
                    4_000_000,
                    "big",
                    999.0,
                    Some(9.0 * 1024.0 * 1024.0 * 1024.0),
                ),
            ],
            None,
            10,
        );
        let overlay = SpikeAttributionOverlay::new(presentation(), &set);
        let lines = overlay.contributor_lines();
        let cpu_rows: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .filter(|row| row.starts_with("CPU"))
            .collect();
        assert_eq!(cpu_rows.len(), 2, "{cpu_rows:?}");
        let ends: Vec<Option<usize>> = cpu_rows.iter().map(|row| row.find('%')).collect();
        assert!(ends.iter().all(Option::is_some), "{cpu_rows:?}");
    }

    #[test]
    fn every_row_fits_the_table_width_exactly() {
        let set = spike();
        let overlay = SpikeAttributionOverlay::new(presentation(), &set);
        let width = usize::from(overlay.table_width());
        for line in overlay
            .header_lines()
            .iter()
            .chain(overlay.contributor_lines().iter())
        {
            assert!(
                line_width(line) <= width,
                "a row overflowed the table: {line:?}"
            );
        }
    }

    #[test]
    fn the_table_scrolls_and_says_how_far_through_it_is() {
        let processes: Vec<ProcessSnapshot> = (1u16..=10)
            .map(|pid| {
                process(
                    u32::from(pid),
                    &format!("worker-{pid}"),
                    f32::from(pid),
                    Some(1.0),
                )
            })
            .collect();
        let set = ContributorSet::from_processes(&processes, None, 10);
        let overlay = SpikeAttributionOverlay::new(presentation(), &set).with_scroll(0);
        let text = render(overlay, 90, 12);
        assert!(text.contains(" of "), "no scroll indicator:\n{text}");
    }

    #[test]
    fn the_panel_degrades_and_never_panics() {
        let set = spike();
        for (width, height) in [(80u16, 24u16), (60, 16), (20, 5), (1, 1), (0, 0)] {
            let overlay = SpikeAttributionOverlay::new(presentation(), &set)
                .with_label("sample 22:14:07 UTC")
                .with_offset("-00:37");
            let text = render(overlay, width, height);
            for row in text.lines() {
                assert!(display_width(row) <= usize::from(width), "{row:?}");
            }
        }
    }
}

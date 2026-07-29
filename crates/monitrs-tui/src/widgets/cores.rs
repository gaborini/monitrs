//! The per-core strip of §7.1: a bounded heat row for any number of cores.
//!
//! §7.1 allows a per-core view but forbids the obvious implementation of it: "if
//! there are too many cores, aggregate into groups or use a heatmap instead of
//! rendering hundreds of rows". A 256-thread machine has more cores than a 140-cell
//! terminal has columns, so this widget renders one cell per core while it can and
//! one cell per *group* of cores when it cannot.
//!
//! Groups aggregate by **maximum**, not by mean. One saturated core inside a group
//! of four is the thing worth seeing — averaging it with three idle cores would
//! hide exactly the condition the strip exists to reveal. A group whose cores were
//! all unmeasured stays blank, so an unavailable per-core read is a gap and not a
//! cold core (§4).

use monitrs_core::model::MetricState;
use monitrs_core::units::{Percent, display_width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::{Painter, Presentation, RowBuilder};

/// Cells between the label and the strip.
const LABEL_GAP: u16 = 1;

/// A single-row heat strip over the per-core utilizations of one machine.
#[derive(Clone, Debug)]
pub struct CoreStrip<'a> {
    presentation: Presentation<'a>,
    cores: &'a [MetricState<Percent>],
    label: Option<&'a str>,
    label_width: Option<u16>,
    show_count: bool,
    token: Token,
}

impl<'a> CoreStrip<'a> {
    /// A strip over `cores`, in logical CPU order.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, cores: &'a [MetricState<Percent>]) -> Self {
        Self {
            presentation,
            cores,
            label: None,
            label_width: None,
            show_count: false,
            token: Token::Graph2,
        }
    }

    /// Sets the leading label, such as `CORES`.
    #[must_use]
    pub const fn with_label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Reserves a fixed label width so the strip aligns with the meters above it.
    #[must_use]
    pub const fn with_label_width(mut self, cells: u16) -> Self {
        self.label_width = Some(cells);
        self
    }

    /// Appends the core count and the grouping factor to the label.
    ///
    /// A grouped strip is *not* one cell per core, and saying so is the difference
    /// between a heatmap and a lie: `CORES 256/4` reads as "256 cores, four to a
    /// cell" (§7.1).
    #[must_use]
    pub const fn with_count(mut self, show_count: bool) -> Self {
        self.show_count = show_count;
        self
    }

    /// Chooses the strip colour, one of §5.3's six graph tokens.
    #[must_use]
    pub const fn with_token(mut self, token: Token) -> Self {
        self.token = token;
        self
    }

    /// How many cores share one cell at `strip_width` cells of room.
    ///
    /// Always at least one, and one exactly when every core has its own cell.
    #[must_use]
    pub fn cores_per_cell(&self, strip_width: u16) -> usize {
        let width = usize::from(strip_width);
        if width == 0 || self.cores.len() <= width {
            return 1;
        }
        self.cores.len().div_ceil(width.max(1))
    }

    /// Whether cores are being aggregated rather than shown individually.
    #[must_use]
    pub fn is_grouped(&self, strip_width: u16) -> bool {
        self.cores_per_cell(strip_width) > 1
    }

    /// The busiest measured core in each group, or `None` for an unmeasured group.
    #[must_use]
    pub fn groups(&self, strip_width: u16) -> Vec<Option<f32>> {
        let step = self.cores_per_cell(strip_width);
        self.cores
            .chunks(step.max(1))
            .map(|chunk| {
                chunk
                    .iter()
                    .filter_map(|core| core.displayable().map(|(percent, _)| percent.value()))
                    .filter(|value| value.is_finite())
                    .fold(None::<f32>, |best, value| {
                        Some(best.map_or(value, |best| best.max(value)))
                    })
            })
            .collect()
    }

    /// The label text, including the count suffix when it was asked for.
    #[must_use]
    pub fn label_text(&self, strip_width: u16) -> String {
        let base = self.label.unwrap_or("");
        if !self.show_count {
            return base.to_owned();
        }
        let step = self.cores_per_cell(strip_width);
        if step > 1 {
            format!("{base} {}/{step}", self.cores.len())
        } else {
            format!("{base} {}", self.cores.len())
        }
    }

    /// Cells the label occupies in a row of `width` cells, including the gap.
    ///
    /// There is a small circularity here: the label may name the grouping factor,
    /// the factor depends on how much room the strip has, and that depends on the
    /// label's width. The label's width is non-increasing in the strip's width — a
    /// wider strip needs a smaller factor, and `256` is never longer than `256/4` —
    /// so iterating from the widest possible strip converges. Three passes is more
    /// than the four-character suffix can ever need, and the result is a pure
    /// function of the core count and the width either way.
    fn label_cells(&self, width: u16) -> u16 {
        if self.label.is_none() && !self.show_count {
            return 0;
        }
        if let Some(cells) = self.label_width {
            return cells.saturating_add(LABEL_GAP);
        }
        let mut cells = 0u16;
        let mut strip = width;
        for _ in 0..3 {
            cells = u16::try_from(display_width(&self.label_text(strip)))
                .unwrap_or(u16::MAX)
                .saturating_add(LABEL_GAP);
            strip = width.saturating_sub(cells);
        }
        cells
    }

    /// The strip alone, occupying exactly `strip_width` cells.
    ///
    /// A group with no measured core renders blank rather than at the ramp floor
    /// (§4), and an unfilled tail — fewer groups than cells — is blank too.
    #[must_use]
    pub fn strip(&self, strip_width: u16) -> String {
        let glyphs = self.presentation.glyphs();
        // `sparkline` renders a blank cell for any non-finite value and keeps the
        // rightmost cell as the last element, which for a core strip means core 0
        // is on the left only while every core fits. Padding to the full width up
        // front keeps the mapping "cell i is group i" at every size.
        let mut values: Vec<f32> = self
            .groups(strip_width)
            .into_iter()
            .map(|group| group.unwrap_or(f32::NAN))
            .collect();
        values.truncate(usize::from(strip_width));
        while values.len() < usize::from(strip_width) {
            values.push(f32::NAN);
        }
        glyphs.sparkline(&values, usize::from(strip_width), 100.0)
    }

    /// The row as an assembled builder.
    #[must_use]
    pub fn row(&self, width: u16) -> RowBuilder {
        let mut row = RowBuilder::new(width, self.presentation.glyphs());
        if row.is_full() {
            return row;
        }
        let label_cells = self.label_cells(width);
        if label_cells > 0 {
            let text = self.label_text(width.saturating_sub(label_cells));
            row.push_field(
                &text,
                label_cells.saturating_sub(LABEL_GAP),
                Align::Left,
                self.presentation.style(Token::Muted),
            );
            row.pad(LABEL_GAP);
        }
        let strip_width = row.remaining();
        let strip = self.strip(strip_width);
        row.push_field(
            &strip,
            strip_width,
            Align::Left,
            self.presentation.style(self.token),
        );
        row
    }

    /// The row as a plain string of exactly `width` cells.
    #[must_use]
    pub fn line(&self, width: u16) -> String {
        self.row(width).padded_text()
    }

    /// The row as a styled line.
    #[must_use]
    pub fn styled_line(&self, width: u16) -> Line<'static> {
        self.row(width).finish()
    }
}

impl Widget for CoreStrip<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut painter = Painter::new(buf, area);
        if painter.is_empty() {
            return;
        }
        let width = painter.width();
        let line = self.styled_line(width);
        painter.write_line(0, 0, width, &line);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphSet;
    use crate::theme::{ColorDepth, ThemeId};

    fn ascii() -> Presentation<'static> {
        Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        )
    }

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    fn cores(values: &[f32]) -> Vec<MetricState<Percent>> {
        values
            .iter()
            .map(|value| MetricState::Available(percent(*value)))
            .collect()
    }

    #[test]
    fn one_cell_per_core_while_they_all_fit() {
        let list = cores(&[0.0, 50.0, 100.0, 0.0]);
        let strip = CoreStrip::new(ascii(), &list);
        assert_eq!(strip.cores_per_cell(8), 1);
        assert!(!strip.is_grouped(8));
        assert_eq!(strip.strip(4), ".+@.");
    }

    #[test]
    fn a_high_core_count_is_aggregated_rather_than_truncated() {
        // §7.1: hundreds of cores must not become hundreds of rows, and must not
        // silently lose the cores that did not fit.
        let list: Vec<MetricState<Percent>> = (0..256)
            .map(|index| MetricState::Available(percent((index % 101) as f32)))
            .collect();
        let strip = CoreStrip::new(ascii(), &list);
        assert_eq!(strip.cores_per_cell(64), 4);
        assert!(strip.is_grouped(64));
        assert_eq!(strip.groups(64).len(), 64);
        assert_eq!(display_width(&strip.strip(64)), 64);
    }

    #[test]
    fn a_group_reports_its_busiest_core_rather_than_an_average() {
        let list = cores(&[0.0, 0.0, 0.0, 100.0]);
        let strip = CoreStrip::new(ascii(), &list);
        // Two cells: cores 0-1 and cores 2-3.
        assert_eq!(strip.groups(2), vec![Some(0.0), Some(100.0)]);
        assert_eq!(strip.strip(2), ".@");
    }

    #[test]
    fn a_group_with_nothing_measured_stays_blank() {
        let list = vec![
            MetricState::PermissionDenied,
            MetricState::WarmingUp,
            MetricState::Available(percent(100.0)),
            MetricState::Unsupported,
        ];
        let strip = CoreStrip::new(ascii(), &list);
        assert_eq!(strip.groups(2), vec![None, Some(100.0)]);
        assert_eq!(strip.strip(2), " @");
    }

    #[test]
    fn an_unmeasured_core_is_a_gap_and_a_zero_core_is_not() {
        let missing = vec![MetricState::<Percent>::WarmingUp; 3];
        assert_eq!(CoreStrip::new(ascii(), &missing).strip(3), "   ");
        let idle = cores(&[0.0, 0.0, 0.0]);
        assert_eq!(CoreStrip::new(ascii(), &idle).strip(3), "...");
    }

    #[test]
    fn a_strip_narrower_than_the_group_count_is_still_exactly_its_width() {
        let list: Vec<MetricState<Percent>> = (0..7)
            .map(|index| MetricState::Available(percent((index * 15) as f32)))
            .collect();
        for width in 0..=40u16 {
            assert_eq!(
                display_width(&CoreStrip::new(ascii(), &list).strip(width)),
                usize::from(width),
                "width {width}"
            );
        }
    }

    #[test]
    fn the_row_occupies_exactly_its_width_at_every_size_and_core_count() {
        for count in [0usize, 1, 8, 65, 256, 1_024] {
            let list: Vec<MetricState<Percent>> = (0..count)
                .map(|index| MetricState::Available(percent((index % 101) as f32)))
                .collect();
            for width in 0..=80u16 {
                let line = CoreStrip::new(ascii(), &list)
                    .with_label("CORES")
                    .with_count(true)
                    .line(width);
                assert_eq!(
                    display_width(&line),
                    usize::from(width),
                    "{count} cores at width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn the_label_says_when_cores_are_being_grouped() {
        let list: Vec<MetricState<Percent>> = vec![MetricState::Available(Percent::ZERO); 256];
        let strip = CoreStrip::new(ascii(), &list)
            .with_label("CORES")
            .with_count(true);
        assert_eq!(strip.label_text(64), "CORES 256/4");
        assert_eq!(strip.label_text(256), "CORES 256");
        let plain = CoreStrip::new(ascii(), &list).with_label("CORES");
        assert_eq!(plain.label_text(64), "CORES");
    }

    #[test]
    fn no_cores_at_all_draws_a_blank_strip_rather_than_a_cold_machine() {
        let none: Vec<MetricState<Percent>> = Vec::new();
        let strip = CoreStrip::new(ascii(), &none);
        assert_eq!(strip.cores_per_cell(8), 1);
        assert_eq!(strip.strip(8), "        ");
        assert_eq!(strip.line(8), "        ");
    }

    #[test]
    fn a_zero_area_strip_draws_nothing_without_panicking() {
        let list = cores(&[1.0, 2.0]);
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 20, 0),
            Rect::new(0, 0, 0, 3),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 3));
            CoreStrip::new(ascii(), &list)
                .with_label("CORES")
                .render(area, &mut buffer);
            assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
        }
    }

    #[test]
    fn strict_ascii_output_stays_printable_seven_bit() {
        let list: Vec<MetricState<Percent>> = (0..300)
            .map(|index| {
                if index % 5 == 0 {
                    MetricState::PermissionDenied
                } else {
                    MetricState::Available(percent((index % 101) as f32))
                }
            })
            .collect();
        let line = CoreStrip::new(ascii(), &list)
            .with_label("CORES")
            .with_count(true)
            .line(120);
        for byte in line.bytes() {
            assert!(
                (0x20..=0x7e).contains(&byte),
                "{line:?} has byte {byte:#04x}"
            );
        }
    }

    #[test]
    fn a_series_token_can_be_chosen_from_the_six_graph_colours() {
        let presentation = ascii();
        let list = cores(&[50.0; 4]);
        let line = CoreStrip::new(presentation, &list)
            .with_token(Token::Graph5)
            .styled_line(8);
        assert!(
            line.spans
                .iter()
                .any(|span| span.style == presentation.style(Token::Graph5))
        );
    }
}

//! The history graph of §5.5 and §5.6: `CPU  .....::-=+*##@%#*+=--:...`.
//!
//! Strict ASCII uses the nine-level `.:-=+*#%@` ramp §5.1 names; enhanced mode
//! uses the eight-level block ramp, or Braille at double horizontal resolution
//! when [`Sparkline::dense`] is asked for. Neither the ramp nor the packing lives
//! here — [`crate::glyphs::GlyphSet`] owns both, and this widget owns the two
//! decisions the glyph set cannot make.
//!
//! # A missing sample is a gap, not a floor
//!
//! The lowest ramp character means "measured, and it is at the bottom". A sample
//! that was never measured — the warming-up first frame, a permission-denied read,
//! a counter reset — must therefore be *blank*, or the graph tells the user the
//! system was idle when in fact it was unobserved (§4). [`plot_value`] is where
//! that conversion happens, and it is deliberately the only place in the crate
//! that turns a [`MetricState`] into a bare `f32`.
//!
//! # Scaling
//!
//! The default ceiling is 100%, which keeps two frames of the same metric
//! comparable: a spike is only a spike relative to a fixed scale.
//! [`Sparkline::self_scaling`] switches to the observed maximum, which is what
//! rates need — `18M/s` has no natural ceiling — and the caller is then
//! responsible for labelling the scale, because a self-scaling graph with no axis
//! is unreadable.

use monitrs_core::model::MetricState;
use monitrs_core::units::{Percent, display_width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::{Painter, Presentation, RowBuilder};

/// The fixed ceiling a percentage graph is drawn against, so frames compare.
pub const PERCENT_CEILING: f32 = 100.0;

/// Cells between a label and the plot.
const LABEL_GAP: u16 = 1;

/// The value a sample contributes to a plot, or `f32::NAN` when it has none.
///
/// `NAN` is the agreed "no sample" marker of [`crate::glyphs::GlyphSet::sparkline`],
/// which renders a blank cell for any non-finite or negative value. Returning
/// `0.0` here would be the §4 violation this whole module exists to avoid, so the
/// conversion is a named function with its own test rather than an inline `map`.
///
/// A *stale* sample does count: it was really measured, just not in this tick, and
/// §4 allows a retained value on screen as long as it is marked — which the
/// series' own [`MetricState`] still is, wherever the value is also shown
/// numerically.
#[must_use]
pub fn plot_value(sample: &MetricState<Percent>) -> f32 {
    match sample.displayable() {
        Some((percent, _)) => percent.value(),
        None => f32::NAN,
    }
}

/// Converts a whole series for plotting.
#[must_use]
pub fn plot_values(series: &[MetricState<Percent>]) -> Vec<f32> {
    series.iter().map(plot_value).collect()
}

/// One labelled history row.
///
/// The plot is always exactly the cells left after the label, so two stacked
/// sparklines with the same label width line up sample for sample — which is what
/// makes §5.5's CPU/MEM/IO block readable as one timeline.
#[derive(Clone, Debug)]
pub struct Sparkline<'a> {
    presentation: Presentation<'a>,
    series: &'a [MetricState<Percent>],
    label: Option<&'a str>,
    label_width: Option<u16>,
    dense: bool,
    self_scaling: bool,
    token: Token,
}

impl<'a> Sparkline<'a> {
    /// A sparkline over `series`, oldest sample first.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, series: &'a [MetricState<Percent>]) -> Self {
        Self {
            presentation,
            series,
            label: None,
            label_width: None,
            dense: false,
            self_scaling: false,
            token: Token::Graph1,
        }
    }

    /// Sets the leading label, such as `CPU` or `I/O`.
    #[must_use]
    pub const fn with_label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Reserves a fixed label width so stacked sparklines align.
    #[must_use]
    pub const fn with_label_width(mut self, cells: u16) -> Self {
        self.label_width = Some(cells);
        self
    }

    /// Packs two samples per cell where the glyph mode allows it.
    ///
    /// Strict ASCII has no denser form than the nine-level ramp, so this is a
    /// no-op there rather than an invented character (§5.1).
    #[must_use]
    pub const fn dense(mut self, dense: bool) -> Self {
        self.dense = dense;
        self
    }

    /// Scales to the largest sample instead of to 100%.
    #[must_use]
    pub const fn self_scaling(mut self, self_scaling: bool) -> Self {
        self.self_scaling = self_scaling;
        self
    }

    /// Chooses the series colour, one of §5.3's six graph tokens.
    #[must_use]
    pub const fn with_token(mut self, token: Token) -> Self {
        self.token = token;
        self
    }

    /// The ceiling the plot is drawn against.
    ///
    /// A self-scaling plot never uses a ceiling below `1.0`: a series of zeros
    /// would otherwise divide by nothing and the flat line would vanish.
    #[must_use]
    pub fn ceiling(&self) -> f32 {
        if !self.self_scaling {
            return PERCENT_CEILING;
        }
        self.series
            .iter()
            .filter_map(|sample| sample.displayable().map(|(percent, _)| percent.value()))
            .filter(|value| value.is_finite())
            .fold(1.0f32, f32::max)
    }

    /// Cells the label occupies, including the gap after it.
    fn label_cells(&self) -> u16 {
        let width = match (self.label_width, self.label) {
            (Some(cells), _) => cells,
            (None, Some(label)) => u16::try_from(display_width(label)).unwrap_or(u16::MAX),
            (None, None) => return 0,
        };
        width.saturating_add(LABEL_GAP)
    }

    /// The plot alone, occupying exactly `width` cells.
    ///
    /// The newest sample is always the rightmost cell, and a history shorter than
    /// `width` is blank-padded on the left rather than filled with the ramp floor —
    /// "no sample yet" must not read as "zero" (§4).
    #[must_use]
    pub fn plot(&self, width: u16) -> String {
        let glyphs = self.presentation.glyphs();
        let values = plot_values(self.series);
        let ceiling = self.ceiling();
        if self.dense {
            glyphs.dense_sparkline(&values, usize::from(width), ceiling)
        } else {
            glyphs.sparkline(&values, usize::from(width), ceiling)
        }
    }

    /// How many cells of plot a row of `width` cells has room for.
    #[must_use]
    pub fn plot_width(&self, width: u16) -> u16 {
        width.saturating_sub(self.label_cells())
    }

    /// The row as an assembled builder.
    #[must_use]
    pub fn row(&self, width: u16) -> RowBuilder {
        let mut row = RowBuilder::new(width, self.presentation.glyphs());
        if row.is_full() {
            return row;
        }
        if let Some(label) = self.label {
            let cells = self.label_cells();
            row.push_field(
                label,
                cells.saturating_sub(LABEL_GAP),
                Align::Left,
                self.presentation.style(Token::Muted),
            );
            row.pad(LABEL_GAP);
        }
        let plot_width = row.remaining();
        let plot = self.plot(plot_width);
        row.push_field(
            &plot,
            plot_width,
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

impl Widget for Sparkline<'_> {
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

/// The caret row that marks the selected historical sample.
///
/// §5.5 and §5.6 both show it: `^ -00:37 selected` under the sample the Time Lens
/// is parked on. It is a separate widget because it is a separate *row*, and
/// because the alignment rule — the caret sits under its sample, counted from the
/// right so "now" is the anchor — is worth pinning on its own.
#[derive(Clone, Debug)]
pub struct SparklineCaret<'a> {
    presentation: Presentation<'a>,
    series: &'a [MetricState<Percent>],
    /// How many samples back from the newest the caret marks. `0` is "now".
    offset_from_newest: usize,
    label: Option<&'a str>,
    label_width: Option<u16>,
    note: Option<&'a str>,
}

impl<'a> SparklineCaret<'a> {
    /// A caret `offset_from_newest` samples back from the right-hand edge.
    #[must_use]
    pub const fn new(
        presentation: Presentation<'a>,
        series: &'a [MetricState<Percent>],
        offset_from_newest: usize,
    ) -> Self {
        Self {
            presentation,
            series,
            offset_from_newest,
            label: None,
            label_width: None,
            note: None,
        }
    }

    /// Matches the label reservation of the [`Sparkline`] above it, so the caret
    /// lands under the right cell.
    #[must_use]
    pub const fn with_label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Matches an explicit label width.
    #[must_use]
    pub const fn with_label_width(mut self, cells: u16) -> Self {
        self.label_width = Some(cells);
        self
    }

    /// Sets the text beside the caret, such as `-00:37 selected`.
    #[must_use]
    pub const fn with_note(mut self, note: &'a str) -> Self {
        self.note = Some(note);
        self
    }

    /// The sparkline this caret is aligned to.
    fn sibling(&self) -> Sparkline<'a> {
        let mut sparkline = Sparkline::new(self.presentation, self.series);
        if let Some(label) = self.label {
            sparkline = sparkline.with_label(label);
        }
        if let Some(cells) = self.label_width {
            sparkline = sparkline.with_label_width(cells);
        }
        sparkline
    }

    /// The caret's cell offset from the left edge of the row, if it is visible.
    ///
    /// `None` when the selected sample has scrolled off the left of the plot, so
    /// the caller can say so rather than parking the caret on the wrong sample.
    #[must_use]
    pub fn caret_x(&self, width: u16) -> Option<u16> {
        let sibling = self.sibling();
        let plot_width = sibling.plot_width(width);
        if plot_width == 0 {
            return None;
        }
        let offset = u16::try_from(self.offset_from_newest).ok()?;
        if offset >= plot_width {
            return None;
        }
        let label_cells = width.saturating_sub(plot_width);
        Some(label_cells.saturating_add(plot_width.saturating_sub(1).saturating_sub(offset)))
    }

    /// The row as an assembled builder.
    #[must_use]
    pub fn row(&self, width: u16) -> RowBuilder {
        let mut row = RowBuilder::new(width, self.presentation.glyphs());
        if row.is_full() {
            return row;
        }
        let Some(caret_x) = self.caret_x(width) else {
            // Nowhere to point: show the note alone rather than a misplaced caret.
            if let Some(note) = self.note {
                row.push(note, self.presentation.style(Token::Muted));
            }
            return row;
        };
        let caret_style = self.presentation.style(Token::Accent);
        let note_style = self.presentation.style(Token::Muted);
        let note = self.note.filter(|note| !note.is_empty());
        let note_width = note.map_or(0, |note| {
            u16::try_from(display_width(note)).unwrap_or(u16::MAX)
        });
        // The note goes to the right of the caret where there is room, and to its
        // left otherwise. A caret parked near "now" is at the right-hand edge, so
        // insisting on one side would lose the note exactly when the Time Lens is
        // closest to live (§2.1).
        // One cell for the caret, one for the gap, the rest for the note.
        let room_right = width.saturating_sub(caret_x).saturating_sub(2);

        if let Some(note) = note
            && note_width > room_right
            && caret_x >= note_width.saturating_add(1)
        {
            row.pad_to(caret_x.saturating_sub(note_width).saturating_sub(1));
            row.push(note, note_style);
            row.pad_to(caret_x);
            row.push("^", caret_style);
            return row;
        }

        row.pad_to(caret_x);
        row.push("^", caret_style);
        if let Some(note) = note
            && note_width <= room_right
        {
            row.pad(1);
            row.push(note, note_style);
        }
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

impl Widget for SparklineCaret<'_> {
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
    use monitrs_core::model::UnavailableReason;

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

    fn unicode() -> Presentation<'static> {
        ascii().with_glyphs(GlyphSet::unicode())
    }

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    fn series(values: &[f32]) -> Vec<MetricState<Percent>> {
        values
            .iter()
            .map(|value| MetricState::Available(percent(*value)))
            .collect()
    }

    #[test]
    fn a_missing_sample_is_blank_and_a_measured_zero_is_not() {
        // §4: the ramp floor means "measured at the bottom"; a gap means
        // "unobserved". Conflating them would make a permission-denied CPU read
        // look like an idle machine.
        let samples = vec![
            MetricState::Available(Percent::ZERO),
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
            MetricState::Available(percent(100.0)),
        ];
        let plot = Sparkline::new(ascii(), &samples).plot(6);
        assert_eq!(plot, ".    @");
    }

    #[test]
    fn a_stale_sample_still_plots_because_it_was_really_measured() {
        let samples = vec![
            MetricState::Available(percent(100.0)).into_stale(core::time::Duration::from_secs(2)),
        ];
        assert_eq!(Sparkline::new(ascii(), &samples).plot(1), "@");
    }

    #[test]
    fn plot_value_reports_nan_for_every_state_with_no_value() {
        assert!(plot_value(&MetricState::WarmingUp).is_nan());
        assert!(plot_value(&MetricState::PermissionDenied).is_nan());
        assert!(plot_value(&MetricState::Unsupported).is_nan());
        assert!(
            plot_value(&MetricState::TemporarilyUnavailable(
                UnavailableReason::Timeout
            ))
            .is_nan()
        );
        // A measured zero is a number, not a gap.
        let zero = plot_value(&MetricState::Available(Percent::ZERO));
        assert!(zero.is_finite());
        assert!(zero.abs() < f32::EPSILON);
    }

    #[test]
    fn the_ascii_ramp_is_the_one_the_specification_names() {
        let rising: Vec<MetricState<Percent>> =
            series(&[0.0, 12.5, 25.0, 37.5, 50.0, 62.5, 75.0, 87.5, 100.0]);
        let plot = Sparkline::new(ascii(), &rising).plot(9);
        assert_eq!(plot, ".:-=+*#%@");
    }

    #[test]
    fn the_newest_sample_is_always_the_rightmost_cell() {
        let samples = series(&[0.0, 0.0, 0.0, 100.0]);
        assert_eq!(Sparkline::new(ascii(), &samples).plot(2), ".@");
    }

    #[test]
    fn a_short_history_is_blank_padded_on_the_left() {
        let samples = series(&[100.0, 100.0]);
        assert_eq!(Sparkline::new(ascii(), &samples).plot(6), "    @@");
    }

    #[test]
    fn the_plot_occupies_exactly_its_width_in_both_glyph_modes() {
        let samples: Vec<MetricState<Percent>> = (0..200)
            .map(|index| {
                if index % 7 == 0 {
                    MetricState::WarmingUp
                } else {
                    MetricState::Available(percent((index % 101) as f32))
                }
            })
            .collect();
        for presentation in [ascii(), unicode()] {
            for dense in [false, true] {
                for scaling in [false, true] {
                    for width in 0..=80u16 {
                        let sparkline = Sparkline::new(presentation, &samples)
                            .dense(dense)
                            .self_scaling(scaling);
                        assert_eq!(
                            display_width(&sparkline.plot(width)),
                            usize::from(width),
                            "dense {dense} scaling {scaling} width {width}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_row_occupies_exactly_its_width_whatever_the_label() {
        let samples = series(&[1.0, 50.0, 99.0]);
        for presentation in [ascii(), unicode()] {
            for width in 0..=60u16 {
                let line = Sparkline::new(presentation, &samples)
                    .with_label("CPU")
                    .line(width);
                assert_eq!(display_width(&line), usize::from(width), "{line:?}");
            }
        }
    }

    #[test]
    fn a_labelled_row_puts_the_plot_after_a_single_gap() {
        let samples = series(&[100.0, 100.0, 100.0]);
        let line = Sparkline::new(ascii(), &samples).with_label("CPU").line(8);
        assert_eq!(line, "CPU  @@@");
    }

    #[test]
    fn the_reported_plot_width_is_what_the_row_actually_gives_the_plot() {
        let samples = series(&[100.0; 4]);
        let labelled = Sparkline::new(ascii(), &samples).with_label("CPU");
        // Three cells of label plus one of gap.
        assert_eq!(labelled.plot_width(20), 16);
        assert_eq!(display_width(&labelled.plot(labelled.plot_width(20))), 16);
        // A row narrower than the label leaves no plot at all, rather than a
        // negative width.
        assert_eq!(labelled.plot_width(2), 0);
        assert_eq!(labelled.plot_width(0), 0);
        // Without a label the whole row is plot.
        assert_eq!(Sparkline::new(ascii(), &samples).plot_width(20), 20);
    }

    #[test]
    fn stacked_rows_align_when_they_share_a_label_width() {
        let samples = series(&[100.0; 4]);
        let cpu = Sparkline::new(ascii(), &samples)
            .with_label("CPU")
            .with_label_width(4)
            .line(12);
        let io = Sparkline::new(ascii(), &samples)
            .with_label("I/O")
            .with_label_width(4)
            .line(12);
        assert_eq!(cpu.find('@'), io.find('@'));
    }

    #[test]
    fn an_empty_series_draws_nothing_rather_than_a_flat_floor() {
        let empty: Vec<MetricState<Percent>> = Vec::new();
        let plot = Sparkline::new(ascii(), &empty).plot(6);
        assert_eq!(plot, "      ");
        let line = Sparkline::new(ascii(), &empty).with_label("CPU").line(10);
        assert_eq!(line, "CPU       ");
    }

    #[test]
    fn a_fixed_ceiling_keeps_two_frames_comparable() {
        let quiet = series(&[5.0, 5.0, 5.0]);
        let busy = series(&[95.0, 95.0, 95.0]);
        let quiet_plot = Sparkline::new(ascii(), &quiet).plot(3);
        let busy_plot = Sparkline::new(ascii(), &busy).plot(3);
        assert_ne!(
            quiet_plot, busy_plot,
            "a fixed scale must show the difference"
        );
        assert_eq!(quiet_plot, "...");
        assert_eq!(busy_plot, "@@@");
    }

    #[test]
    fn self_scaling_uses_the_observed_maximum() {
        let quiet = series(&[1.0, 2.0, 3.0]);
        let fixed = Sparkline::new(ascii(), &quiet).plot(3);
        let scaled = Sparkline::new(ascii(), &quiet).self_scaling(true).plot(3);
        assert_eq!(fixed, "...", "against 100% these are all near zero");
        assert_eq!(scaled, "=*@", "against 3.0 they span the ramp");
    }

    #[test]
    fn a_self_scaling_plot_of_zeros_does_not_divide_by_nothing() {
        let flat = series(&[0.0, 0.0, 0.0]);
        let sparkline = Sparkline::new(ascii(), &flat).self_scaling(true);
        assert!((sparkline.ceiling() - 1.0).abs() < f32::EPSILON);
        assert_eq!(sparkline.plot(3), "...");
    }

    #[test]
    fn a_self_scaling_plot_ignores_samples_that_have_no_value() {
        let mixed = vec![
            MetricState::PermissionDenied,
            MetricState::Available(percent(50.0)),
        ];
        let sparkline = Sparkline::new(ascii(), &mixed).self_scaling(true);
        assert!((sparkline.ceiling() - 50.0).abs() < f32::EPSILON);
    }

    #[test]
    fn enhanced_mode_packs_two_samples_per_cell_when_asked() {
        let samples = series(&[10.0, 90.0, 10.0, 90.0]);
        let dense = Sparkline::new(unicode(), &samples).dense(true).plot(2);
        assert_eq!(dense.chars().count(), 2);
        assert!(
            dense
                .chars()
                .all(|c| ('\u{2800}'..='\u{28ff}').contains(&c)),
            "{dense:?}"
        );
        // Strict ASCII has no denser form, so it degrades rather than inventing one.
        let plain = Sparkline::new(ascii(), &samples).dense(true).plot(4);
        assert_eq!(plain, Sparkline::new(ascii(), &samples).plot(4));
        assert!(plain.is_ascii());
    }

    #[test]
    fn strict_ascii_output_is_printable_seven_bit_for_every_state() {
        let samples: Vec<MetricState<Percent>> = vec![
            MetricState::WarmingUp,
            MetricState::Available(Percent::ZERO),
            MetricState::Available(percent(100.0)),
            MetricState::PermissionDenied,
        ];
        for width in 0..=32u16 {
            let line = Sparkline::new(ascii(), &samples)
                .with_label("I/O")
                .line(width);
            for byte in line.bytes() {
                assert!(
                    (0x20..=0x7e).contains(&byte),
                    "{line:?} has byte {byte:#04x}"
                );
            }
        }
    }

    #[test]
    fn a_zero_area_sparkline_draws_nothing_without_panicking() {
        let samples = series(&[1.0, 2.0]);
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 20, 0),
            Rect::new(0, 0, 0, 3),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 3));
            Sparkline::new(ascii(), &samples)
                .with_label("CPU")
                .render(area, &mut buffer);
            assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
        }
    }

    #[test]
    fn the_caret_points_at_the_selected_sample_counted_from_now() {
        let samples = series(&[1.0; 10]);
        let caret = SparklineCaret::new(ascii(), &samples, 0).with_label("CPU");
        // Offset 0 is "now": the rightmost cell.
        assert_eq!(caret.caret_x(10), Some(9));
        let back = SparklineCaret::new(ascii(), &samples, 3).with_label("CPU");
        assert_eq!(back.caret_x(10), Some(6));
    }

    #[test]
    fn the_caret_row_matches_the_form_in_the_mockup() {
        // §5.5: `        ^ -00:37 selected`.
        let samples = series(&[1.0; 30]);
        let line = SparklineCaret::new(ascii(), &samples, 20)
            .with_label("CPU")
            .with_note("-00:37 selected")
            .line(30);
        assert_eq!(display_width(&line), 30);
        assert!(line.contains("^ -00:37 selected"), "{line:?}");
        assert_eq!(line.find('^'), Some(9));
    }

    #[test]
    fn a_caret_near_now_keeps_its_note_by_moving_it_to_the_left() {
        // Parked one sample back from live, the caret is at the right-hand edge and
        // there is no room to its right; the note must survive anyway (§2.1).
        let samples = series(&[1.0; 30]);
        let line = SparklineCaret::new(ascii(), &samples, 1)
            .with_label("CPU")
            .with_note("-00:01 selected")
            .line(30);
        assert_eq!(display_width(&line), 30);
        assert!(line.contains("-00:01 selected ^"), "{line:?}");
        assert_eq!(line.find('^'), Some(28));
    }

    #[test]
    fn a_row_too_narrow_for_the_note_keeps_the_caret() {
        let samples = series(&[1.0; 8]);
        let line = SparklineCaret::new(ascii(), &samples, 0)
            .with_note("-00:00 selected")
            .line(6);
        assert_eq!(display_width(&line), 6);
        assert!(line.contains('^'), "{line:?}");
    }

    #[test]
    fn a_caret_beyond_the_plot_shows_the_note_alone_rather_than_a_wrong_cell() {
        let samples = series(&[1.0; 4]);
        let caret = SparklineCaret::new(ascii(), &samples, 500)
            .with_label("CPU")
            .with_note("out of range");
        assert_eq!(caret.caret_x(20), None);
        let line = caret.line(20);
        assert!(!line.contains('^'), "{line:?}");
        assert!(line.contains("out of range"), "{line:?}");
    }

    #[test]
    fn the_caret_row_occupies_exactly_its_width_at_every_size() {
        let samples = series(&[1.0; 40]);
        for offset in [0usize, 1, 7, 39, 40, 1_000] {
            for width in 0..=60u16 {
                let line = SparklineCaret::new(ascii(), &samples, offset)
                    .with_label("CPU")
                    .with_note("-00:37 selected")
                    .line(width);
                assert_eq!(
                    display_width(&line),
                    usize::from(width),
                    "offset {offset} width {width}: {line:?}"
                );
            }
        }
    }

    #[test]
    fn a_caret_aligns_with_the_sparkline_it_was_given_the_same_label_as() {
        let samples = series(&[100.0; 12]);
        let width = 20u16;
        let plot = Sparkline::new(ascii(), &samples)
            .with_label("CPU")
            .line(width);
        let caret = SparklineCaret::new(ascii(), &samples, 0)
            .with_label("CPU")
            .line(width);
        let last_plot_cell = plot.rfind('@');
        assert_eq!(caret.find('^'), last_plot_cell);
    }

    #[test]
    fn a_zero_area_caret_draws_nothing_without_panicking() {
        let samples = series(&[1.0]);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 10, 2));
        SparklineCaret::new(ascii(), &samples, 0).render(Rect::new(0, 0, 0, 0), &mut buffer);
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
    }

    #[test]
    fn a_series_token_can_be_chosen_from_the_six_graph_colours() {
        let presentation = ascii();
        let samples = series(&[50.0; 4]);
        let line = Sparkline::new(presentation, &samples)
            .with_token(Token::Graph3)
            .styled_line(8);
        assert!(
            line.spans
                .iter()
                .any(|span| span.style == presentation.style(Token::Graph3))
        );
    }
}

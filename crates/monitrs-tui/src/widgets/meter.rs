//! The horizontal meter of §5.5: `CPU  37% [#############--------]`.
//!
//! A meter takes a [`MetricState<Percent>`], not a `Percent`, and that is the
//! whole point of the type. §4's rule is that unavailable is never zero, and an
//! empty bar is the most convincing way there is to render a metric as zero. So a
//! meter that has no value draws neither an empty bar nor a full one: it draws the
//! §4 placeholder text, the state's symbol, and — where there is room — the
//! `BarTrack` glyph, which is visibly a *track* rather than a measurement
//! (`.......` rather than `-------`).
//!
//! # Why the value field is fixed and the placeholder field is not
//!
//! §5.4 forbids a value crossing a unit boundary from reflowing what is beside
//! it, so the value field is a constant number of cells and the bar takes the
//! rest. A *state* change is different: it is itself the information, and
//! `permission denied` cannot be squeezed into four cells. The field therefore
//! widens for a placeholder or a stale marker — never for a value — and shrinks
//! back the moment the metric is measurable again.

use monitrs_core::model::MetricState;
use monitrs_core::units::{Percent, display_width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Painter, Presentation, RowBuilder};

/// Cells reserved for the value, which holds `100%` and `9.9%` alike.
///
/// A process CPU percentage can reach `12800%` on a 128-core machine (§8.3), so
/// callers rendering process CPU widen this with [`Meter::with_value_width`]
/// rather than letting the field size itself.
pub const DEFAULT_VALUE_WIDTH: u16 = 4;

/// The narrowest bar worth drawing: two brackets and one cell of bar.
///
/// Below this the meter drops the bar entirely rather than showing brackets around
/// nothing, which would read as a measured zero.
pub const MIN_BAR_WIDTH: u16 = 3;

/// Cells between the bar and the trailing note, as §5.5's header rows show.
const NOTE_GAP: u16 = 2;

/// One labelled horizontal meter, occupying a single row.
///
/// A multi-row area draws the meter on its first row and leaves the rest alone: a
/// meter is one line of information and stretching it would invent some.
#[derive(Clone, Debug)]
pub struct Meter<'a> {
    presentation: Presentation<'a>,
    state: MetricState<Percent>,
    label: Option<&'a str>,
    label_width: Option<u16>,
    value_width: u16,
    note: Option<&'a str>,
    note_width: Option<u16>,
    token: Token,
}

impl<'a> Meter<'a> {
    /// A meter for `state`, with no label and no note.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, state: MetricState<Percent>) -> Self {
        Self {
            presentation,
            state,
            label: None,
            label_width: None,
            value_width: DEFAULT_VALUE_WIDTH,
            note: None,
            note_width: None,
            token: Token::Text,
        }
    }

    /// Sets the leading label, such as `CPU` or `MEM`.
    #[must_use]
    pub const fn with_label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    /// Reserves a fixed label width so stacked meters align.
    ///
    /// §5.5 stacks `CPU` above `MEM`; without a shared reservation a three-letter
    /// and a four-letter label would offset the two bars by one cell.
    #[must_use]
    pub const fn with_label_width(mut self, cells: u16) -> Self {
        self.label_width = Some(cells);
        self
    }

    /// Reserves a different number of cells for the value.
    #[must_use]
    pub const fn with_value_width(mut self, cells: u16) -> Self {
        self.value_width = cells;
        self
    }

    /// Sets the trailing note, such as `load 4.12 3.84 3.21` (§5.5).
    #[must_use]
    pub const fn with_note(mut self, note: &'a str) -> Self {
        self.note = Some(note);
        self
    }

    /// Reserves a fixed note width, so a changing note cannot resize the bar.
    #[must_use]
    pub const fn with_note_width(mut self, cells: u16) -> Self {
        self.note_width = Some(cells);
        self
    }

    /// Overrides the bar's colour token, for a meter that is part of a series.
    ///
    /// The default is [`Token::Text`]. A caller that colours the bar by severity
    /// passes [`Token::Watch`] or [`Token::Critical`]; the value's own token
    /// still comes from its availability, so a stale value stays marked stale.
    #[must_use]
    pub const fn with_token(mut self, token: Token) -> Self {
        self.token = token;
        self
    }

    /// How the metric reads as text, a token, and a symbol (§4, §5.2).
    #[must_use]
    pub fn display(&self) -> MetricDisplay {
        states::describe_percent(&self.state)
    }

    /// The reserved label width in cells.
    fn resolved_label_width(&self) -> u16 {
        match (self.label_width, self.label) {
            (Some(cells), _) => cells,
            (None, Some(label)) => u16::try_from(display_width(label)).unwrap_or(u16::MAX),
            (None, None) => 0,
        }
    }

    /// The reserved note width in cells.
    fn resolved_note_width(&self) -> u16 {
        match (self.note_width, self.note) {
            (Some(cells), _) => cells,
            (None, Some(note)) => u16::try_from(display_width(note)).unwrap_or(u16::MAX),
            (None, None) => 0,
        }
    }

    /// The text that goes in the value field, and the cells it needs.
    ///
    /// `budget` is everything left after the label and the one-cell symbol.
    fn value_field(&self, display: &MetricDisplay, budget: u16) -> (String, u16) {
        let glyphs = self.presentation.glyphs();
        // Room that still leaves a minimal bar behind: preferred, not required.
        let with_bar = budget.saturating_sub(MIN_BAR_WIDTH.saturating_add(1));

        if display.is_placeholder() {
            let preferred = with_bar.max(self.value_width).min(budget);
            let text = display.fitted(usize::from(preferred), glyphs);
            let needed = u16::try_from(display_width(&text)).unwrap_or(preferred);
            return (text, needed.max(self.value_width.min(budget)));
        }

        if display.age().is_some() {
            // §4: a retained value may be shown only with its age. The annotated
            // form is preferred; if it does not fit, the `~` symbol beside the
            // value still marks it and the age moves out of the meter.
            let annotated = display.annotated();
            let needed = u16::try_from(display_width(&annotated)).unwrap_or(u16::MAX);
            let room = with_bar.max(self.value_width).min(budget);
            if needed <= room {
                return (annotated, needed.max(self.value_width.min(budget)));
            }
        }

        let field = self.value_width.min(budget);
        (display.fitted(usize::from(field), glyphs), field)
    }

    /// The meter as one assembled row of `width` cells.
    ///
    /// Exposed because the interesting properties — the bar is never empty for an
    /// unavailable metric, the value field never moves — are far easier to pin as
    /// a string than as a grid of styled cells.
    #[must_use]
    pub fn row(&self, width: u16) -> RowBuilder {
        let glyphs = self.presentation.glyphs();
        let mut row = RowBuilder::new(width, glyphs);
        if row.is_full() {
            return row;
        }

        if let Some(label) = self.label {
            row.push_field(
                label,
                self.resolved_label_width(),
                Align::Left,
                self.presentation.style(Token::Muted),
            );
        }

        let display = self.display();
        let metric_style = self.presentation.metric_style(&display);

        // The note is the lowest-priority element: it appears only if the symbol,
        // the value field, and a minimal bar all still fit beside it.
        let wanted = match self.note.filter(|note| !note.is_empty()) {
            Some(_) => NOTE_GAP.saturating_add(self.resolved_note_width()),
            None => 0,
        }
        .min(row.remaining());
        let minimum_core = self
            .value_width
            .saturating_add(2)
            .saturating_add(MIN_BAR_WIDTH);
        let note_reserve = if row.remaining().saturating_sub(wanted) >= minimum_core {
            wanted
        } else {
            0
        };
        let core = row.remaining().saturating_sub(note_reserve);
        if core == 0 {
            return row;
        }

        // The symbol is always exactly one cell, so nothing after it shifts as the
        // metric's availability changes (§5.2).
        row.push_field(&display.symbol().to_string(), 1, Align::Left, metric_style);
        let budget = core.saturating_sub(1);
        let (value_text, value_cells) = self.value_field(&display, budget);
        row.push_field(&value_text, value_cells, Align::Right, metric_style);

        let bar_room = budget.saturating_sub(value_cells);
        if bar_room >= MIN_BAR_WIDTH.saturating_add(1) {
            row.pad(1);
            let bar_width = bar_room.saturating_sub(1);
            let bar = match self.state.displayable() {
                Some((percent, _)) => glyphs.meter(percent.fraction(), usize::from(bar_width)),
                // §4: a track, not an empty bar. An empty bar means "measured, and
                // it is zero".
                None => glyphs.unknown_meter(usize::from(bar_width)),
            };
            let bar_style = if display.is_placeholder() || display.age().is_some() {
                metric_style
            } else {
                self.presentation.style(self.token)
            };
            row.push_field(&bar, bar_width, Align::Left, bar_style);
        }

        if note_reserve > 0
            && let Some(note) = self.note
        {
            // `note_reserve` is the gap plus the label, so the label itself starts
            // exactly its own width from the right edge (§5.4: reserve from
            // geometry, so a changing note cannot resize the bar).
            let note_start = width.saturating_sub(note_reserve.saturating_sub(NOTE_GAP));
            row.pad_to(note_start);
            let field = row.remaining();
            row.push_field(
                note,
                field,
                Align::Left,
                self.presentation.style(Token::Muted),
            );
        }
        row
    }

    /// The meter as a plain string of exactly `width` cells.
    #[must_use]
    pub fn line(&self, width: u16) -> String {
        self.row(width).padded_text()
    }

    /// The meter as a styled line.
    #[must_use]
    pub fn styled_line(&self, width: u16) -> Line<'static> {
        self.row(width).finish()
    }
}

impl Widget for Meter<'_> {
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

    const MISSING: [MetricState<Percent>; 4] = [
        MetricState::WarmingUp,
        MetricState::PermissionDenied,
        MetricState::Unsupported,
        MetricState::TemporarilyUnavailable(UnavailableReason::NeedsSecondSample),
    ];

    #[test]
    fn the_meter_matches_the_shape_in_the_specification_mockup() {
        // §5.5: `CPU  37% [#############----------------------]`.
        let meter = Meter::new(ascii(), MetricState::Available(percent(37.0))).with_label("CPU");
        let line = meter.line(30);
        assert!(line.starts_with("CPU  37% ["), "{line:?}");
        assert!(line.ends_with(']'), "{line:?}");
        assert_eq!(display_width(&line), 30);
        assert!(line.contains('#'), "{line:?}");
        assert!(line.contains('-'), "{line:?}");
    }

    #[test]
    fn a_meter_occupies_exactly_its_width_at_every_size_and_state() {
        for presentation in [ascii(), unicode()] {
            for state in MISSING.iter().copied().chain([
                MetricState::Available(Percent::ZERO),
                MetricState::Available(percent(100.0)),
                MetricState::Available(percent(287.0)),
                MetricState::Available(percent(4.2)).into_stale(core::time::Duration::from_secs(9)),
            ]) {
                for width in 0..=80u16 {
                    let meter = Meter::new(presentation, state)
                        .with_label("MEM")
                        .with_note("22.8/32.0 GiB");
                    let line = meter.line(width);
                    assert_eq!(
                        display_width(&line),
                        usize::from(width),
                        "{state:?} at width {width}: {line:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_unavailable_metric_draws_a_track_never_an_empty_bar() {
        for state in MISSING {
            let meter = Meter::new(ascii(), state).with_label("CPU");
            let line = meter.line(40);
            let zero = Meter::new(ascii(), MetricState::Available(Percent::ZERO))
                .with_label("CPU")
                .line(40);
            assert_ne!(line, zero, "{state:?} renders like a measured zero");
            assert!(!line.contains('#'), "{state:?} drew a filled bar: {line:?}");
            // `-` is the "measured, below this level" cell; `.` is the track.
            assert!(
                !line.contains("--"),
                "{state:?} drew an empty bar: {line:?}"
            );
            assert!(line.contains(".."), "{state:?} drew no track: {line:?}");
        }
    }

    #[test]
    fn an_unavailable_metric_shows_its_placeholder_text_and_its_symbol() {
        let cases = [
            (MetricState::WarmingUp, "warming up", '.'),
            (MetricState::PermissionDenied, "permission denied", '!'),
            (MetricState::Unsupported, "n/a", '-'),
            (
                MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
                "link speed unknown",
                '?',
            ),
        ];
        for (state, text, symbol) in cases {
            let line = Meter::new(ascii(), state).with_label("NET").line(48);
            assert!(
                line.contains(text),
                "{state:?} lost its placeholder: {line:?}"
            );
            assert!(
                line.contains(symbol),
                "{state:?} lost its symbol {symbol:?}: {line:?}"
            );
        }
    }

    #[test]
    fn a_narrow_meter_degrades_the_placeholder_rather_than_clipping_it_into_a_word() {
        let meter = Meter::new(ascii(), MetricState::<Percent>::PermissionDenied).with_label("MEM");
        // Wide: the full reason.
        assert!(meter.line(40).contains("permission denied"));
        // Narrow: `n/a` plus the `!` symbol, which still distinguishes it from
        // `warming up` (§5.1, §5.2).
        let narrow = meter.line(14);
        assert!(narrow.contains("n/a"), "{narrow:?}");
        assert!(narrow.contains('!'), "{narrow:?}");
        assert!(!narrow.contains("permis"), "{narrow:?}");
    }

    #[test]
    fn a_measured_value_never_moves_the_bar_as_it_changes() {
        // §5.4: the value field is reserved from geometry, so `9.9%` and `100%`
        // start the bar in the same column.
        let bracket = |value: f32| {
            Meter::new(ascii(), MetricState::Available(percent(value)))
                .with_label("CPU")
                .line(30)
                .find('[')
        };
        let positions: Vec<Option<usize>> = [0.0, 4.2, 37.0, 99.9, 100.0].map(bracket).into();
        assert!(
            positions.windows(2).all(|pair| pair.first() == pair.get(1)),
            "the bar moved: {positions:?}"
        );
    }

    #[test]
    fn a_value_above_one_hundred_percent_fills_the_bar_rather_than_overflowing_it() {
        // §8.3: process CPU is core-normalized and legitimately exceeds 100%.
        let meter = Meter::new(ascii(), MetricState::Available(percent(287.0)))
            .with_label("CPU")
            .with_value_width(6);
        let line = meter.line(30);
        assert!(line.contains("287%"), "{line:?}");
        assert_eq!(display_width(&line), 30);
        assert!(
            !line.contains('-'),
            "a saturated bar has no empty cells: {line:?}"
        );
    }

    #[test]
    fn a_stale_value_carries_its_age_where_there_is_room() {
        // §4: a retained value may be shown only alongside its age.
        let state =
            MetricState::Available(percent(71.0)).into_stale(core::time::Duration::from_secs(4));
        let wide = Meter::new(ascii(), state).with_label("MEM").line(40);
        assert!(wide.contains("71%"), "{wide:?}");
        assert!(wide.contains("~00:04"), "{wide:?}");
        // Narrow: the `~` symbol still marks it.
        let narrow = Meter::new(ascii(), state).with_label("MEM").line(16);
        assert!(narrow.contains('~'), "{narrow:?}");
    }

    #[test]
    fn a_stale_meter_is_styled_stale_rather_than_as_a_fresh_reading() {
        let state =
            MetricState::Available(percent(71.0)).into_stale(core::time::Duration::from_secs(4));
        let presentation = ascii();
        let meter = Meter::new(presentation, state);
        let line = meter.styled_line(40);
        let stale = presentation.style(Token::Stale);
        assert!(
            line.spans.iter().any(|span| span.style == stale),
            "no span carries the stale style"
        );
    }

    #[test]
    fn the_note_is_dropped_before_the_bar_is() {
        let meter = Meter::new(ascii(), MetricState::Available(percent(37.0)))
            .with_label("CPU")
            .with_note("load 4.12 3.84 3.21");
        let wide = meter.line(50);
        assert!(wide.contains("load 4.12 3.84 3.21"), "{wide:?}");
        assert!(wide.contains('['), "{wide:?}");
        let narrow = meter.line(20);
        assert!(!narrow.contains("load"), "{narrow:?}");
        assert!(
            narrow.contains('['),
            "the bar outranks the note: {narrow:?}"
        );
    }

    #[test]
    fn a_reserved_note_width_keeps_the_bar_the_same_length() {
        let with_short = Meter::new(ascii(), MetricState::Available(percent(37.0)))
            .with_label("CPU")
            .with_note("swap 0.2G")
            .with_note_width(20)
            .line(60);
        let with_long = Meter::new(ascii(), MetricState::Available(percent(37.0)))
            .with_label("CPU")
            .with_note("swap 12.7G of 16.0G")
            .with_note_width(20)
            .line(60);
        let bar_end = |line: &str| line.find(']');
        assert_eq!(bar_end(&with_short), bar_end(&with_long));
    }

    #[test]
    fn a_shared_label_width_aligns_stacked_meters() {
        let cpu = Meter::new(ascii(), MetricState::Available(percent(37.0)))
            .with_label("CPU")
            .with_label_width(5)
            .line(30);
        let memory = Meter::new(ascii(), MetricState::Available(percent(71.0)))
            .with_label("SWAP")
            .with_label_width(5)
            .line(30);
        assert_eq!(cpu.find('['), memory.find('['));
    }

    #[test]
    fn a_meter_too_narrow_for_a_bar_still_shows_the_value() {
        for width in 1..=9u16 {
            let line = Meter::new(ascii(), MetricState::Available(percent(37.0)))
                .with_label("CPU")
                .line(width);
            assert_eq!(display_width(&line), usize::from(width), "{line:?}");
        }
        // Nine cells: label, symbol, value — no room for brackets plus a bar.
        let tight = Meter::new(ascii(), MetricState::Available(percent(37.0)))
            .with_label("CPU")
            .line(9);
        assert!(tight.contains("37%"), "{tight:?}");
        assert!(!tight.contains('['), "{tight:?}");
    }

    #[test]
    fn a_meter_with_no_room_for_the_value_shows_nothing_rather_than_a_marker() {
        // A lone `.` where a percentage belongs reads as a measurement (§4).
        for width in 1..=8u16 {
            let line = Meter::new(ascii(), MetricState::Available(percent(91.0)))
                .with_label("CPU")
                .with_label_width(4)
                .line(width);
            assert_eq!(display_width(&line), usize::from(width));
            if !line.contains("91%") {
                assert_eq!(
                    line.trim_end(),
                    line.trim_end().trim_end_matches('.'),
                    "{line:?}"
                );
                assert!(!line.contains('.'), "{line:?} shows a marker fragment");
            }
        }
        // A placeholder still speaks at the same widths, because its symbol does.
        let denied = Meter::new(ascii(), MetricState::<Percent>::PermissionDenied)
            .with_label("CPU")
            .with_label_width(4)
            .line(6);
        assert!(denied.contains('!'), "{denied:?}");
    }

    #[test]
    fn zero_width_and_zero_height_render_nothing_without_panicking() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 30, 0),
            Rect::new(0, 0, 0, 4),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 30, 4));
            Meter::new(ascii(), MetricState::Available(percent(37.0)))
                .with_label("CPU")
                .render(area, &mut buffer);
            assert!(
                buffer.content().iter().all(|cell| cell.symbol() == " "),
                "an empty area must not draw"
            );
        }
        for width in 0..=3u16 {
            assert_eq!(
                display_width(&Meter::new(ascii(), MetricState::<Percent>::WarmingUp).line(width)),
                usize::from(width)
            );
        }
    }

    #[test]
    fn a_multi_row_area_draws_one_meter_and_leaves_the_rest_blank() {
        let area = Rect::new(0, 0, 20, 3);
        let mut buffer = Buffer::empty(area);
        Meter::new(ascii(), MetricState::Available(percent(50.0)))
            .with_label("CPU")
            .render(area, &mut buffer);
        let row = |y: u16| -> String {
            (0..20)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                .collect()
        };
        assert!(row(0).contains("50%"));
        assert_eq!(row(1).trim(), "");
        assert_eq!(row(2).trim(), "");
    }

    #[test]
    fn enhanced_mode_uses_block_characters_and_strict_mode_stays_ascii() {
        let plain = Meter::new(ascii(), MetricState::Available(percent(50.0)))
            .with_label("CPU")
            .line(30);
        assert!(plain.is_ascii(), "{plain:?}");
        let rich = Meter::new(unicode(), MetricState::Available(percent(50.0)))
            .with_label("CPU")
            .line(30);
        assert!(!rich.is_ascii(), "{rich:?}");
        assert_eq!(display_width(&rich), 30);
    }

    #[test]
    fn the_bar_token_is_configurable_without_changing_the_value_token() {
        let presentation = ascii();
        let meter = Meter::new(presentation, MetricState::Available(percent(95.0)))
            .with_label("MEM")
            .with_token(Token::Critical);
        let line = meter.styled_line(30);
        let critical = presentation.style(Token::Critical);
        let text = presentation.style(Token::Text);
        assert!(
            line.spans.iter().any(|span| span.style == critical),
            "no bar accent"
        );
        assert!(
            line.spans.iter().any(|span| span.style == text),
            "the value must stay neutral"
        );
    }

    #[test]
    fn the_display_is_the_shared_metric_description() {
        let meter = Meter::new(ascii(), MetricState::<Percent>::PermissionDenied);
        let display = meter.display();
        assert!(display.is_placeholder());
        assert_eq!(display.symbol(), '!');
        assert_eq!(display.token(), Token::Watch);
    }
}

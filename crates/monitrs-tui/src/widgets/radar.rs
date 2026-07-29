//! The Pressure Radar of §2.3 and §5.5: `. CPU normal     37%`.
//!
//! §2.3 lists four things every signal must show, and this widget is responsible
//! for all four being *reachable*:
//!
//! * **the raw metric** — [`PressureSignal::raw`], rendered through
//!   [`Measurement::render`] so the collector never has to know about formatting;
//! * **its normalized severity** — [`PressureSignal::severity`], available as a
//!   percentage and as the bar [`RadarRow::severity_bar`];
//! * **the rule used to derive the state** — [`PressureSignal::rule`], on the row's
//!   second line when [`Radar::with_rules`] is on and from
//!   [`Radar::rule_of`] at any width;
//! * **an explicit unavailable state** — never `normal`.
//!
//! # Why the symbol is not `PressureSignal::symbol`
//!
//! `MetricState::WarmingUp` answers `'.'` and so does `PressureState::Normal`, so a
//! signal still awaiting samples would draw the same leading character as a healthy
//! one. §2.3 requires an explicit unavailable state and §5.2 requires the symbol to
//! carry the meaning on its own, so a signal with no derived state gets `?` here
//! and the state column says which kind of unknown it is (`warming up`,
//! `permission denied`, `n/a`). [`crate::widgets::states::describe_pressure`] is
//! where that decision lives, so the radar and any other consumer agree.
//!
//! [`Measurement::render`]: monitrs_core::model::Measurement::render

use core::cmp::Reverse;

use monitrs_core::model::{PressureId, PressureSignal};
use monitrs_core::units::{display_width, format_age};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;

use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Painter, Presentation, RowBuilder};

/// Cells reserved for the resource label. Holds `PSI-MEM`, the longest §2.3 names.
pub const LABEL_WIDTH: u16 = 7;

/// Cells reserved for the state word: `critical` is the longest (§2.3).
pub const STATE_WIDTH: u16 = 8;

/// Cells reserved for the raw metric, right-aligned as §5.4 requires.
pub const RAW_WIDTH: u16 = 8;

/// Cells the rule line is indented by, so it reads as a continuation.
const RULE_INDENT: u16 = 2;

/// One Pressure Radar signal, occupying a single row.
#[derive(Clone, Debug)]
pub struct RadarRow<'a> {
    presentation: Presentation<'a>,
    signal: &'a PressureSignal,
    label_width: u16,
    state_width: u16,
    raw_width: u16,
    show_bar: bool,
}

impl<'a> RadarRow<'a> {
    /// A row for `signal`.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, signal: &'a PressureSignal) -> Self {
        Self {
            presentation,
            signal,
            label_width: LABEL_WIDTH,
            state_width: STATE_WIDTH,
            raw_width: RAW_WIDTH,
            show_bar: false,
        }
    }

    /// Overrides the reserved label width.
    #[must_use]
    pub const fn with_label_width(mut self, cells: u16) -> Self {
        self.label_width = cells;
        self
    }

    /// Overrides the reserved state width.
    #[must_use]
    pub const fn with_state_width(mut self, cells: u16) -> Self {
        self.state_width = cells;
        self
    }

    /// Overrides the reserved raw-metric width.
    #[must_use]
    pub const fn with_raw_width(mut self, cells: u16) -> Self {
        self.raw_width = cells;
        self
    }

    /// Draws the normalized severity as a bar after the raw metric.
    ///
    /// §2.3 keeps severity separate from state precisely because two signals can
    /// both be `watch` while one is far closer to critical, and a bar is the only
    /// way to see that at a glance.
    #[must_use]
    pub const fn with_bar(mut self, show_bar: bool) -> Self {
        self.show_bar = show_bar;
        self
    }

    /// The signal this row draws.
    #[must_use]
    pub const fn signal(&self) -> &'a PressureSignal {
        self.signal
    }

    /// The leading character: the derived state's cue, or `?` (§2.3, §5.2).
    #[must_use]
    pub fn symbol(&self) -> char {
        self.state_display().symbol()
    }

    /// The state as text, a token, and a symbol.
    #[must_use]
    pub fn state_display(&self) -> MetricDisplay {
        states::describe_pressure(&self.signal.state)
    }

    /// The severity as text, a token, and a symbol.
    #[must_use]
    pub fn severity_display(&self) -> MetricDisplay {
        states::describe_percent(&self.signal.severity)
    }

    /// The raw metric as §2.3 requires it to be shown.
    ///
    /// A signal with no raw measurement falls back to the *severity*, which is the
    /// only number it has; a signal with neither shows the state's placeholder. It
    /// is never blank, because a blank cell reads as a measured nothing (§4).
    #[must_use]
    pub fn raw_text(&self) -> String {
        match self.signal.raw {
            Some(measurement) => measurement.value.render(self.presentation.units()),
            None => {
                let severity = self.severity_display();
                if severity.is_value() {
                    severity.text().to_owned()
                } else {
                    self.state_display()
                        .fitted(usize::from(self.raw_width), self.presentation.glyphs())
                }
            }
        }
    }

    /// The label of the raw measurement, when there is one (§2.3).
    #[must_use]
    pub fn raw_label(&self) -> Option<&'static str> {
        self.signal.raw.map(|measurement| measurement.label)
    }

    /// The normalized severity bar, `width` cells wide.
    ///
    /// An undetermined severity draws the track glyph, never an empty bar (§4).
    #[must_use]
    pub fn severity_bar(&self, width: u16) -> String {
        let glyphs = self.presentation.glyphs();
        match self.signal.severity.displayable() {
            Some((percent, _)) => glyphs.meter(percent.fraction(), usize::from(width)),
            None => glyphs.unknown_meter(usize::from(width)),
        }
    }

    /// The rule that derived this state, as §2.3 requires it to be available.
    #[must_use]
    pub const fn rule(&self) -> &'static str {
        self.signal.rule
    }

    /// How long the signal has held its state, for the hysteresis display (§2.3).
    #[must_use]
    pub fn held_for_text(&self) -> Option<String> {
        self.signal.held_for.map(format_age)
    }

    /// The row as an assembled builder.
    #[must_use]
    pub fn row(&self, width: u16) -> RowBuilder {
        let mut row = RowBuilder::new(width, self.presentation.glyphs());
        if row.is_full() {
            return row;
        }
        let state = self.state_display();
        let state_style = self.presentation.metric_style(&state);

        // Symbol first, exactly one cell, so every row's label starts in the same
        // column whatever the state (§5.5's radar block).
        row.push_field(&state.symbol().to_string(), 1, Align::Left, state_style);
        row.pad(1);
        row.push_field(
            self.signal.id.label(),
            self.label_width,
            Align::Left,
            self.presentation.style(Token::Text),
        );
        row.pad(1);
        row.push_field(
            &state.fitted(usize::from(self.state_width), self.presentation.glyphs()),
            self.state_width,
            Align::Left,
            state_style,
        );
        row.pad(1);
        let raw = self.raw_text();
        row.push_field(
            &raw,
            self.raw_width,
            Align::Right,
            self.presentation.style(Token::Muted),
        );

        if self.show_bar {
            let room = row.remaining();
            if room > 1 {
                row.pad(1);
                let bar_width = row.remaining();
                let bar = self.severity_bar(bar_width);
                row.push_field(&bar, bar_width, Align::Left, state_style);
            }
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

    /// The indented rule line that follows the row, when rules are shown.
    #[must_use]
    pub fn rule_row(&self, width: u16) -> RowBuilder {
        let mut row = RowBuilder::new(width, self.presentation.glyphs());
        if row.is_full() {
            return row;
        }
        row.pad(RULE_INDENT);
        let mut text = self.rule().to_owned();
        if let Some(held) = self.held_for_text() {
            text.push_str(" (held ");
            text.push_str(&held);
            text.push(')');
        }
        row.push(&text, self.presentation.style(Token::Muted));
        row
    }

    /// The rule line as a plain string of exactly `width` cells.
    #[must_use]
    pub fn rule_line(&self, width: u16) -> String {
        self.rule_row(width).padded_text()
    }
}

impl Widget for RadarRow<'_> {
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

/// The whole Pressure Radar: one row per signal, in the order given.
///
/// Signals that do not fit the area are dropped from the bottom, and the caller can
/// ask [`Radar::visible_signals`] how many were shown. Silently truncating a health
/// panel would be the worst possible failure mode, so §2.3's own display order —
/// CPU, memory, disk, network, swap, load, then PSI — puts the signals that matter
/// most at the top, and [`Radar::with_severe_first`] reorders so that a critical
/// signal is never the one that got cut.
#[derive(Clone, Debug)]
pub struct Radar<'a> {
    presentation: Presentation<'a>,
    signals: &'a [PressureSignal],
    show_rules: bool,
    show_bars: bool,
    severe_first: bool,
    hide_unsupported: bool,
}

impl<'a> Radar<'a> {
    /// A radar over `signals`.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, signals: &'a [PressureSignal]) -> Self {
        Self {
            presentation,
            signals,
            show_rules: false,
            show_bars: false,
            severe_first: false,
            hide_unsupported: false,
        }
    }

    /// Renders each signal's derivation rule on a second line (§2.3).
    #[must_use]
    pub const fn with_rules(mut self, show_rules: bool) -> Self {
        self.show_rules = show_rules;
        self
    }

    /// Renders each signal's normalized severity as a bar (§2.3).
    #[must_use]
    pub const fn with_bars(mut self, show_bars: bool) -> Self {
        self.show_bars = show_bars;
        self
    }

    /// Sorts the most severe signal to the top, so truncation loses the calmest.
    ///
    /// The order within one severity is §2.3's display order, so the panel does not
    /// reshuffle from frame to frame while nothing has changed.
    #[must_use]
    pub const fn with_severe_first(mut self, severe_first: bool) -> Self {
        self.severe_first = severe_first;
        self
    }

    /// Drops signals this platform cannot produce at all.
    ///
    /// §4 allows layout code to hide an [`MetricState::Unsupported`] panel when
    /// space is scarce — Linux PSI on macOS is three permanently blank rows —
    /// but never an unavailable one, which is information.
    ///
    /// [`MetricState::Unsupported`]: monitrs_core::model::MetricState::Unsupported
    #[must_use]
    pub const fn hide_unsupported(mut self, hide: bool) -> Self {
        self.hide_unsupported = hide;
        self
    }

    /// The rule text for one resource, whether or not it is on screen (§2.3).
    #[must_use]
    pub fn rule_of(&self, id: PressureId) -> Option<&'static str> {
        self.signals
            .iter()
            .find(|signal| signal.id == id)
            .map(|signal| signal.rule)
    }

    /// The signals this radar will draw, in the order it will draw them.
    #[must_use]
    pub fn ordered_signals(&self) -> Vec<&'a PressureSignal> {
        let mut ordered: Vec<&'a PressureSignal> = self
            .signals
            .iter()
            .filter(|signal| !(self.hide_unsupported && signal.state.is_unsupported()))
            .collect();
        if self.severe_first {
            // A stable sort keeps §2.3's display order inside each severity band.
            // `displayable` rather than `fresh`, because a retained critical signal
            // is still critical and must not sink below a fresh normal one (§4).
            ordered
                .sort_by_key(|signal| Reverse(signal.state.displayable().map(|(state, _)| *state)));
        }
        ordered
    }

    /// Rows one signal occupies: two when its rule is shown, otherwise one.
    #[must_use]
    pub const fn rows_per_signal(&self) -> u16 {
        if self.show_rules { 2 } else { 1 }
    }

    /// How many signals fit in `height` rows.
    #[must_use]
    pub fn visible_signals(&self, height: u16) -> usize {
        let per = self.rows_per_signal().max(1);
        usize::from(height / per).min(self.ordered_signals().len())
    }

    /// Whether any signal had to be dropped for lack of room.
    #[must_use]
    pub fn is_truncated(&self, height: u16) -> bool {
        self.visible_signals(height) < self.ordered_signals().len()
    }

    /// Every rendered line, for assertions and snapshots.
    #[must_use]
    pub fn lines(&self, width: u16, height: u16) -> Vec<String> {
        let mut lines = Vec::new();
        for signal in self
            .ordered_signals()
            .into_iter()
            .take(self.visible_signals(height))
        {
            let row = RadarRow::new(self.presentation, signal).with_bar(self.show_bars);
            lines.push(row.line(width));
            if self.show_rules {
                lines.push(row.rule_line(width));
            }
        }
        lines
    }
}

impl Widget for Radar<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut painter = Painter::new(buf, area);
        if painter.is_empty() {
            return;
        }
        let width = painter.width();
        let height = painter.height();
        let mut y = 0u16;
        for signal in self
            .ordered_signals()
            .into_iter()
            .take(self.visible_signals(height))
        {
            if y >= height {
                break;
            }
            let row = RadarRow::new(self.presentation, signal).with_bar(self.show_bars);
            painter.write_line(0, y, width, &row.styled_line(width));
            y = y.saturating_add(1);
            if self.show_rules && y < height {
                painter.write_line(0, y, width, &row.rule_row(width).finish());
                y = y.saturating_add(1);
            }
        }
    }
}

/// The widest label any §2.3 resource needs, checked against [`LABEL_WIDTH`].
#[must_use]
pub fn widest_label() -> usize {
    PressureId::DISPLAY_ORDER
        .iter()
        .map(|id| display_width(id.label()))
        .max()
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::{
        MeasuredValue, Measurement, MetricState, PressureState, UnavailableReason,
    };
    use monitrs_core::units::{ByteUnits, Percent, Rate};

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

    fn signal(
        id: PressureId,
        state: MetricState<PressureState>,
        severity: MetricState<Percent>,
        raw: Option<Measurement>,
    ) -> PressureSignal {
        PressureSignal {
            id,
            state,
            severity,
            raw,
            rule: "busy > 85% for 10 of 15 samples",
            held_for: None,
        }
    }

    fn reference() -> Vec<PressureSignal> {
        vec![
            signal(
                PressureId::Cpu,
                MetricState::Available(PressureState::Normal),
                MetricState::Available(percent(37.0)),
                Some(Measurement::new(
                    "busy",
                    MeasuredValue::Percent(percent(37.0)),
                )),
            ),
            signal(
                PressureId::Memory,
                MetricState::Available(PressureState::Watch),
                MetricState::Available(percent(71.0)),
                Some(Measurement::new(
                    "used",
                    MeasuredValue::Percent(percent(71.0)),
                )),
            ),
            signal(
                PressureId::Disk,
                MetricState::Available(PressureState::Normal),
                MetricState::Available(percent(12.0)),
                Some(Measurement::new(
                    "busy",
                    MeasuredValue::Percent(percent(12.0)),
                )),
            ),
            signal(
                PressureId::Network,
                MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
                MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
                Some(Measurement::new(
                    "throughput",
                    MeasuredValue::ByteRate(Rate::new(18.0 * 1024.0 * 1024.0).expect("finite")),
                )),
            ),
        ]
    }

    #[test]
    fn the_rows_match_the_shape_in_the_specification_mockup() {
        // §5.5: `. CPU normal     37%` and `? NET unknown   18M/s` — symbol, label,
        // state, then the raw metric right-aligned.
        let signals = reference();
        let lines = Radar::new(ascii(), &signals).lines(30, 10);
        assert_eq!(lines.len(), 4);
        for line in &lines {
            assert_eq!(display_width(line), 30, "{line:?}");
        }
        let cpu = lines.first().expect("the CPU row");
        assert!(cpu.starts_with(". CPU"), "{cpu:?}");
        assert!(cpu.contains("normal"), "{cpu:?}");
        assert!(cpu.trim_end().ends_with("37%"), "{cpu:?}");

        let memory = lines.get(1).expect("the MEM row");
        assert!(memory.starts_with("! MEM"), "{memory:?}");
        assert!(memory.contains("watch"), "{memory:?}");

        let network = lines.get(3).expect("the NET row");
        assert!(network.starts_with("? NET"), "{network:?}");
        assert!(network.trim_end().ends_with("18M/s"), "{network:?}");
        // The state column is eight cells, so the long reason degrades to `n/a`
        // rather than being clipped into a word (§5.1).
        assert!(network.contains("n/a"), "{network:?}");
        assert!(!network.contains("normal"), "{network:?}");
    }

    #[test]
    fn an_unavailable_signal_shows_a_question_mark_and_never_normal() {
        // §2.3's explicit unavailable state, and the specific §5.5 row `? NET`.
        for state in [
            MetricState::WarmingUp,
            MetricState::PermissionDenied,
            MetricState::Unsupported,
            MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
        ] {
            let one = signal(PressureId::Network, state, MetricState::WarmingUp, None);
            let row = RadarRow::new(ascii(), &one);
            assert_eq!(row.symbol(), '?', "{state:?}");
            let line = row.line(40);
            assert!(
                !line.contains("normal"),
                "{state:?} reads as normal: {line:?}"
            );
            assert!(line.starts_with('?'), "{line:?}");
        }
    }

    #[test]
    fn a_warming_up_signal_is_distinguishable_from_a_normal_one() {
        // Both would answer `.` if the radar used `MetricState::symbol`.
        let warming = PressureSignal::warming_up(PressureId::Cpu, "awaiting samples");
        let normal = signal(
            PressureId::Cpu,
            MetricState::Available(PressureState::Normal),
            MetricState::Available(percent(1.0)),
            None,
        );
        let warming_line = RadarRow::new(ascii(), &warming).line(40);
        let normal_line = RadarRow::new(ascii(), &normal).line(40);
        assert_ne!(warming_line, normal_line);
        assert!(warming_line.starts_with('?'), "{warming_line:?}");
        assert!(normal_line.starts_with('.'), "{normal_line:?}");
        // The eight-cell state column degrades `warming up` to `n/a`; a wider
        // column shows the reason in full (§5.1's placeholder ladder).
        assert!(warming_line.contains("n/a"), "{warming_line:?}");
        let wide = RadarRow::new(ascii(), &warming)
            .with_state_width(12)
            .line(48);
        assert!(wide.contains("warming up"), "{wide:?}");
    }

    #[test]
    fn every_state_has_the_symbol_section_two_three_names() {
        for (state, symbol) in [
            (PressureState::Normal, '.'),
            (PressureState::Watch, '!'),
            (PressureState::Critical, 'X'),
        ] {
            let one = signal(
                PressureId::Cpu,
                MetricState::Available(state),
                MetricState::Available(percent(50.0)),
                None,
            );
            assert_eq!(RadarRow::new(ascii(), &one).symbol(), symbol);
        }
    }

    #[test]
    fn all_four_of_the_things_section_two_three_requires_are_reachable() {
        let one = PressureSignal {
            id: PressureId::Memory,
            state: MetricState::Available(PressureState::Watch),
            severity: MetricState::Available(percent(71.0)),
            raw: Some(Measurement::new(
                "available",
                MeasuredValue::Bytes(4 * 1024 * 1024 * 1024),
            )),
            rule: "available < 15% of total for 10 of 15 samples",
            held_for: Some(Duration::from_secs(42)),
        };
        let row = RadarRow::new(ascii(), &one);
        // The raw metric.
        assert_eq!(row.raw_text(), "4.0 GiB");
        assert_eq!(row.raw_label(), Some("available"));
        // The normalized severity.
        assert_eq!(row.severity_display().text(), "71%");
        assert!(row.severity_bar(10).contains('#'));
        // The state.
        assert_eq!(row.state_display().text(), "watch");
        assert_eq!(row.symbol(), '!');
        // The rule.
        assert_eq!(row.rule(), "available < 15% of total for 10 of 15 samples");
        assert!(row.rule_line(60).contains("available < 15%"));
        assert!(row.rule_line(60).contains("held 00:42"));
    }

    #[test]
    fn the_raw_metric_honours_the_byte_unit_family() {
        let one = signal(
            PressureId::Disk,
            MetricState::Available(PressureState::Normal),
            MetricState::Available(percent(10.0)),
            Some(Measurement::new(
                "throughput",
                MeasuredValue::ByteRate(Rate::new(1024.0 * 1024.0).expect("finite")),
            )),
        );
        assert_eq!(RadarRow::new(ascii(), &one).raw_text(), "1.0M/s");
        let si = ascii().with_units(ByteUnits::Si);
        assert_eq!(RadarRow::new(si, &one).raw_text(), "1.0M/s");
        let bytes = signal(
            PressureId::Memory,
            MetricState::Available(PressureState::Normal),
            MetricState::WarmingUp,
            Some(Measurement::new("available", MeasuredValue::Bytes(1_000))),
        );
        assert_eq!(RadarRow::new(ascii(), &bytes).raw_text(), "1000 B");
        assert_eq!(RadarRow::new(si, &bytes).raw_text(), "1.0 kB");
    }

    #[test]
    fn a_signal_with_no_raw_metric_falls_back_rather_than_leaving_a_blank() {
        // A blank cell reads as a measured nothing (§4).
        let with_severity = signal(
            PressureId::Load,
            MetricState::Available(PressureState::Watch),
            MetricState::Available(percent(88.0)),
            None,
        );
        assert_eq!(RadarRow::new(ascii(), &with_severity).raw_text(), "88%");
        let with_nothing = PressureSignal::unsupported(PressureId::PsiIo, "Linux only");
        assert_eq!(RadarRow::new(ascii(), &with_nothing).raw_text(), "n/a");
        assert!(!RadarRow::new(ascii(), &with_nothing).raw_text().is_empty());
    }

    #[test]
    fn an_undetermined_severity_draws_a_track_never_an_empty_bar() {
        let one = PressureSignal::warming_up(PressureId::Cpu, "awaiting samples");
        let row = RadarRow::new(ascii(), &one);
        let bar = row.severity_bar(8);
        assert!(bar.contains('.'), "{bar:?}");
        assert!(!bar.contains('#'), "{bar:?}");
        assert!(
            !bar.contains("--"),
            "an empty bar means measured zero: {bar:?}"
        );
        let zero = signal(
            PressureId::Cpu,
            MetricState::Available(PressureState::Normal),
            MetricState::Available(Percent::ZERO),
            None,
        );
        assert_ne!(bar, RadarRow::new(ascii(), &zero).severity_bar(8));
    }

    #[test]
    fn a_row_occupies_exactly_its_width_at_every_size_and_state() {
        let signals = reference();
        for one in &signals {
            for show_bar in [false, true] {
                for width in 0..=80u16 {
                    let line = RadarRow::new(ascii(), one).with_bar(show_bar).line(width);
                    assert_eq!(
                        display_width(&line),
                        usize::from(width),
                        "{:?} at width {width}: {line:?}",
                        one.id
                    );
                    let rule = RadarRow::new(ascii(), one).rule_line(width);
                    assert_eq!(display_width(&rule), usize::from(width), "{rule:?}");
                }
            }
        }
    }

    #[test]
    fn every_row_starts_its_label_in_the_same_column_whatever_the_state() {
        let signals = reference();
        let lines = Radar::new(ascii(), &signals).lines(34, 10);
        let label_starts: Vec<Option<usize>> = lines
            .iter()
            .map(|line| {
                line.char_indices()
                    .position(|(_, c)| c.is_ascii_uppercase())
            })
            .collect();
        assert!(
            label_starts
                .windows(2)
                .all(|pair| pair.first() == pair.get(1)),
            "labels are misaligned: {label_starts:?}"
        );
    }

    #[test]
    fn the_reserved_label_width_holds_every_resource_name() {
        assert!(
            widest_label() <= usize::from(LABEL_WIDTH),
            "{} resources need more than {LABEL_WIDTH} cells",
            widest_label()
        );
        for id in PressureId::DISPLAY_ORDER {
            let one = signal(
                id,
                MetricState::Available(PressureState::Normal),
                MetricState::Available(percent(1.0)),
                None,
            );
            let line = RadarRow::new(ascii(), &one).line(40);
            assert!(
                line.contains(id.label()),
                "{:?} lost its label: {line:?}",
                id
            );
        }
    }

    #[test]
    fn the_reserved_state_width_holds_every_state_word() {
        for state in [
            PressureState::Normal,
            PressureState::Watch,
            PressureState::Critical,
        ] {
            assert!(display_width(state.label()) <= usize::from(STATE_WIDTH));
        }
    }

    #[test]
    fn the_radar_draws_one_row_per_signal_in_the_order_given() {
        let signals = reference();
        let radar = Radar::new(ascii(), &signals);
        assert_eq!(radar.rows_per_signal(), 1);
        let lines = radar.lines(34, 4);
        assert_eq!(lines.len(), 4);
        assert!(!radar.is_truncated(4));
        assert!(radar.is_truncated(3));
        assert_eq!(radar.visible_signals(3), 3);
        assert_eq!(radar.visible_signals(0), 0);
    }

    #[test]
    fn showing_rules_doubles_the_rows_each_signal_takes() {
        let signals = reference();
        let radar = Radar::new(ascii(), &signals).with_rules(true);
        assert_eq!(radar.rows_per_signal(), 2);
        assert_eq!(radar.visible_signals(4), 2);
        let lines = radar.lines(50, 4);
        assert_eq!(lines.len(), 4);
        assert!(lines.get(1).is_some_and(|line| line.contains("busy > 85%")));
    }

    #[test]
    fn the_rule_is_reachable_even_when_it_is_not_on_screen() {
        // §2.3 requires the rule to be available; a 34-cell radar has no room for
        // it inline, so it must still be retrievable.
        let signals = reference();
        let radar = Radar::new(ascii(), &signals);
        assert_eq!(
            radar.rule_of(PressureId::Cpu),
            Some("busy > 85% for 10 of 15 samples")
        );
        assert_eq!(radar.rule_of(PressureId::PsiCpu), None);
    }

    #[test]
    fn severe_first_keeps_a_critical_signal_from_being_the_row_that_is_cut() {
        let mut signals = reference();
        if let Some(last) = signals.last_mut() {
            last.state = MetricState::Available(PressureState::Critical);
        }
        let radar = Radar::new(ascii(), &signals).with_severe_first(true);
        let first = radar
            .ordered_signals()
            .first()
            .map(|signal| signal.id)
            .expect("a signal");
        assert_eq!(first, PressureId::Network);
        // Within one severity the §2.3 display order is preserved.
        let ordered: Vec<PressureId> = radar
            .ordered_signals()
            .into_iter()
            .map(|signal| signal.id)
            .collect();
        assert_eq!(
            ordered,
            vec![
                PressureId::Network,
                PressureId::Memory,
                PressureId::Cpu,
                PressureId::Disk
            ]
        );
    }

    #[test]
    fn unsupported_signals_can_be_hidden_but_unavailable_ones_cannot() {
        let signals = vec![
            signal(
                PressureId::Cpu,
                MetricState::Available(PressureState::Normal),
                MetricState::Available(percent(1.0)),
                None,
            ),
            PressureSignal::unsupported(PressureId::PsiCpu, "Linux only"),
            signal(
                PressureId::Network,
                MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
                MetricState::TemporarilyUnavailable(UnavailableReason::LinkSpeedUnknown),
                None,
            ),
        ];
        let radar = Radar::new(ascii(), &signals).hide_unsupported(true);
        let shown: Vec<PressureId> = radar
            .ordered_signals()
            .into_iter()
            .map(|signal| signal.id)
            .collect();
        assert_eq!(shown, vec![PressureId::Cpu, PressureId::Network]);
        // Without the option, nothing is hidden.
        let all = Radar::new(ascii(), &signals);
        assert_eq!(all.ordered_signals().len(), 3);
    }

    #[test]
    fn an_empty_radar_draws_nothing_rather_than_a_healthy_system() {
        let radar = Radar::new(ascii(), &[]);
        assert!(radar.lines(34, 10).is_empty());
        assert!(!radar.is_truncated(10));
        let area = Rect::new(0, 0, 34, 4);
        let mut buffer = Buffer::empty(area);
        radar.render(area, &mut buffer);
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
    }

    #[test]
    fn a_zero_area_radar_draws_nothing_without_panicking() {
        let signals = reference();
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 34, 0),
            Rect::new(0, 0, 0, 6),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 34, 6));
            Radar::new(ascii(), &signals)
                .with_rules(true)
                .with_bars(true)
                .render(area, &mut buffer);
            assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
        }
    }

    #[test]
    fn the_radar_never_draws_more_rows_than_its_area_has() {
        let signals = reference();
        let area = Rect::new(0, 0, 40, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 6));
        Radar::new(ascii(), &signals)
            .with_rules(true)
            .render(area, &mut buffer);
        for y in 3..6u16 {
            let row: String = (0..40)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                .collect();
            assert_eq!(row.trim(), "", "row {y} was drawn outside the area");
        }
    }

    #[test]
    fn a_stale_signal_keeps_its_state_but_is_drawn_stale() {
        let mut one = signal(
            PressureId::Memory,
            MetricState::Available(PressureState::Critical),
            MetricState::Available(percent(97.0)),
            None,
        );
        one.state = one.state.into_stale(Duration::from_secs(6));
        let presentation = ascii();
        let row = RadarRow::new(presentation, &one);
        assert_eq!(row.symbol(), 'X');
        assert_eq!(row.state_display().token(), Token::Stale);
        let line = row.styled_line(40);
        assert!(
            line.spans
                .iter()
                .any(|span| span.style == presentation.style(Token::Stale))
        );
    }

    #[test]
    fn strict_ascii_output_stays_printable_seven_bit() {
        let signals = reference();
        for line in Radar::new(ascii(), &signals)
            .with_rules(true)
            .with_bars(true)
            .lines(60, 20)
        {
            for byte in line.bytes() {
                assert!(
                    (0x20..=0x7e).contains(&byte),
                    "{line:?} has byte {byte:#04x}"
                );
            }
        }
    }
}

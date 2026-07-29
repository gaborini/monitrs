//! The reusable widgets the screens are composed from (§5.4, §5.5, §5.6).
//!
//! These are widgets, not screens. Each one draws a single piece of the §5.5
//! mockup — a bordered panel, a horizontal meter, a history sparkline, a process
//! row, a radar signal, a pin — and none of them decides what belongs on a
//! screen or how the screen is divided up. That is [`crate::layout`]'s job and
//! the screen layer's.
//!
//! # The three rules every widget here obeys
//!
//! * **A widget renders from state and area only.** No file is read, no collector
//!   is called, no snapshot is mutated (§10.4, §5's separation of concerns). The
//!   types make this hard to get wrong: a widget borrows a `&SystemSnapshot`
//!   fragment and a [`Presentation`], and neither can perform I/O.
//! * **Nothing escapes its rectangle, and zero area never panics.** Every widget
//!   draws through a [`Painter`], which clips writes to the rectangle it was
//!   built with. §5.7 makes the zero-area case a hard requirement, and
//!   `widgets_never_write_outside_their_rectangle` pins the containment property
//!   over every area up to 200×200 (§17.7).
//! * **Colour is supplementary.** A widget asks the [`Presentation`] for a
//!   [`Token`], never for a colour, and every state it draws also carries the
//!   character cue that [`states`] derives (§5.2).
//!
//! # Composition
//!
//! ```text
//! + PROCESSES ------------------------------- 218 total ---+   panel::Panel
//! |   PID USER       CPU%  MEM%     RSS   COMMAND          |   table::ProcessTable
//! |>31842 gabor      287%   8.1%   2.6G   rustc            |
//! + PRESSURE ---------------+-- HISTORY 5m ----------------+
//! | . CPU normal   37%      | CPU  ...::-=+*##@%#*+=--:... |   radar::Radar
//! | ? NET unknown  18M/s    | MEM  ====+++++****########## |   sparkline::Sparkline
//! + PINS -------------------+--------------------------- --+
//! | rustc  PID 31842  CPU 287%  +42%                       |   pins::Pins
//! +--------------------------------------------------------+
//! ```
//!
//! [`Painter`]: painter::Painter
//! [`Token`]: crate::theme::Token

pub mod cores;
pub mod meter;
pub mod painter;
pub mod panel;
pub mod pins;
pub mod radar;
pub mod sparkline;
pub mod states;
pub mod table;
pub mod tree;

use monitrs_core::units::{ByteUnits, Ellipsis};
use ratatui::style::Style;

pub use cores::CoreStrip;
pub use meter::Meter;
pub use painter::{Painter, RowBuilder};
pub use panel::Panel;
pub use pins::{PinRow, Pins};
pub use radar::{Radar, RadarRow};
pub use sparkline::{Sparkline, SparklineCaret};
pub use states::MetricDisplay;
pub use table::{ProcessRow, ProcessTable};
pub use tree::{tree_prefix, tree_prefix_width, tree_prefixes};

use crate::glyphs::{Glyph, GlyphSet};
use crate::theme::{ColorDepth, Cue, SelectionStyle, Theme, Token};

/// Everything a widget needs to know about *how* to draw, and nothing about what.
///
/// Bundling the glyph set, the theme, the colour depth, and the byte-unit family
/// into one `Copy` value is what keeps widget constructors from growing four
/// presentation parameters each — and, more importantly, what makes it impossible
/// to hand a widget a theme without the colour depth that decides how to read it.
///
/// It deliberately holds no mutable state and no clock. §5.2 forbids anything
/// that alternates or flashes, and a presentation that cannot observe time cannot
/// animate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Presentation<'a> {
    glyphs: GlyphSet,
    theme: &'a Theme,
    depth: ColorDepth,
    units: ByteUnits,
}

impl Default for Presentation<'static> {
    /// Strict ASCII, no colour, the default dark palette, IEC byte units.
    ///
    /// The most conservative rendering there is, and therefore the right starting
    /// point for a test: anything legible here is legible everywhere (§5.1, §5.2).
    fn default() -> Self {
        Self::new(
            GlyphSet::ascii(),
            crate::theme::ThemeId::DefaultDark.theme(),
            ColorDepth::Off,
        )
    }
}

impl<'a> Presentation<'a> {
    /// Builds a presentation with IEC byte units, which §5.4 makes the default.
    #[must_use]
    pub const fn new(glyphs: GlyphSet, theme: &'a Theme, depth: ColorDepth) -> Self {
        Self {
            glyphs,
            theme,
            depth,
            units: ByteUnits::Iec,
        }
    }

    /// Overrides the byte-unit family (§5.4 allows configuring SI).
    #[must_use]
    pub const fn with_units(mut self, units: ByteUnits) -> Self {
        self.units = units;
        self
    }

    /// Overrides the glyph set.
    #[must_use]
    pub const fn with_glyphs(mut self, glyphs: GlyphSet) -> Self {
        self.glyphs = glyphs;
        self
    }

    /// Overrides the theme.
    #[must_use]
    pub const fn with_theme(mut self, theme: &'a Theme) -> Self {
        self.theme = theme;
        self
    }

    /// Overrides the colour depth.
    #[must_use]
    pub const fn with_depth(mut self, depth: ColorDepth) -> Self {
        self.depth = depth;
        self
    }

    /// The resolved glyph set.
    #[must_use]
    pub const fn glyphs(&self) -> GlyphSet {
        self.glyphs
    }

    /// The active theme.
    #[must_use]
    pub const fn theme(&self) -> &'a Theme {
        self.theme
    }

    /// The resolved colour depth.
    #[must_use]
    pub const fn depth(&self) -> ColorDepth {
        self.depth
    }

    /// The active byte-unit family.
    #[must_use]
    pub const fn units(&self) -> ByteUnits {
        self.units
    }

    /// The truncation marker for this glyph mode.
    #[must_use]
    pub const fn ellipsis(&self) -> Ellipsis {
        self.glyphs.ellipsis()
    }

    /// The style for text drawn in `token`.
    #[must_use]
    pub fn style(&self, token: Token) -> Style {
        self.theme.style(token, self.depth)
    }

    /// The style for a panel background drawn in `token`.
    #[must_use]
    pub fn background(&self, token: Token) -> Style {
        self.theme.background_style(token, self.depth)
    }

    /// The style for a [`Cue`], which pairs the token with its symbol (§5.2).
    #[must_use]
    pub fn cue(&self, cue: Cue) -> Style {
        self.style(cue.token)
    }

    /// The style of a [`MetricDisplay`], whose symbol carries the same meaning.
    #[must_use]
    pub fn metric_style(&self, display: &MetricDisplay) -> Style {
        self.style(display.token())
    }

    /// The selected row's resolved foreground, background, and modifier.
    #[must_use]
    pub fn selection(&self) -> SelectionStyle {
        self.theme.selection_style(self.depth)
    }

    /// The border token for a panel, which is the only place focus is expressed
    /// as a colour (§5.3).
    #[must_use]
    pub const fn border_token(focused: bool) -> Token {
        if focused {
            Token::FocusBorder
        } else {
            Token::Border
        }
    }

    /// One glyph's text.
    #[must_use]
    pub const fn glyph(&self, glyph: Glyph) -> &'static str {
        self.glyphs.get(glyph)
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::{
        MetricState, PressureId, PressureSignal, PressureState, ProcessIdentity, ProcessIo,
        ProcessMemory, ProcessSnapshot, ProcessState, UnavailableReason, UserIdentity,
    };
    use monitrs_core::process::TreeRow;
    use monitrs_core::units::{Percent, Rate};
    use proptest::prelude::*;
    use ratatui::buffer::{Buffer, Cell};
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};
    use ratatui::widgets::Widget;

    use super::*;
    use crate::layout::TableLayout;
    use crate::theme::ThemeId;

    /// How far the scratch buffer extends past the rendered rectangle on every
    /// side. Anything a widget writes outside its area lands here.
    const MARGIN: u16 = 2;

    fn percent(value: f32) -> Percent {
        Percent::new(value).expect("a finite non-negative percentage")
    }

    /// A cell no widget produces: no theme uses this colour and §5.2 forbids blink.
    fn sentinel_cell() -> Cell {
        let mut cell = Cell::EMPTY;
        cell.set_symbol("\u{2603}");
        cell.set_style(
            Style::new()
                .fg(Color::Rgb(1, 2, 3))
                .bg(Color::Rgb(4, 5, 6))
                .add_modifier(Modifier::SLOW_BLINK),
        );
        cell
    }

    fn presentations() -> [Presentation<'static>; 4] {
        [
            Presentation::new(
                GlyphSet::ascii(),
                ThemeId::DefaultDark.theme(),
                ColorDepth::TrueColor,
            ),
            Presentation::new(
                GlyphSet::unicode(),
                ThemeId::DefaultDark.theme(),
                ColorDepth::TrueColor,
            ),
            Presentation::new(
                GlyphSet::ascii(),
                ThemeId::HighContrast.theme(),
                ColorDepth::Off,
            ),
            Presentation::new(
                GlyphSet::unicode(),
                ThemeId::DefaultLight.theme(),
                ColorDepth::Ansi16,
            )
            .with_units(ByteUnits::Si),
        ]
    }

    fn process(pid: u32, name: &str, state: ProcessState) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid) * 3),
            parent_pid: Some(1),
            name: name.into(),
            command: format!("/usr/bin/{name} --serve --port 8080").into(),
            exe: Some(format!("/usr/bin/{name}").into()),
            user: MetricState::Available(UserIdentity {
                uid: 501,
                name: Some("gabor".into()),
            }),
            state,
            cpu: MetricState::Available(percent(287.0)),
            memory: ProcessMemory {
                rss_bytes: MetricState::Available(2_814_509_056),
                virtual_bytes: MetricState::PermissionDenied,
                share_of_total: MetricState::Available(percent(8.1)),
            },
            io: ProcessIo {
                read: MetricState::Available(Rate::new(18.0 * 1024.0 * 1024.0).expect("finite")),
                write: MetricState::WarmingUp,
                read_total_bytes: MetricState::Unsupported,
                write_total_bytes: MetricState::Unsupported,
            },
            threads: MetricState::Available(9),
            age: MetricState::Available(Duration::from_secs(43)),
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    fn signals() -> Vec<PressureSignal> {
        vec![
            PressureSignal {
                id: PressureId::Cpu,
                state: MetricState::Available(PressureState::Normal),
                severity: MetricState::Available(percent(37.0)),
                raw: None,
                rule: "busy > 85% for 10 of 15 samples",
                held_for: Some(Duration::from_secs(12)),
            },
            PressureSignal::warming_up(PressureId::Memory, "awaiting samples"),
            PressureSignal::unsupported(PressureId::PsiIo, "Linux only"),
        ]
    }

    fn history() -> Vec<MetricState<Percent>> {
        (0..64)
            .map(|index| match index % 9 {
                0 => MetricState::WarmingUp,
                1 => MetricState::PermissionDenied,
                _ => MetricState::Available(percent((index % 101) as f32)),
            })
            .collect()
    }

    /// A deliberately hostile process name: eight double-width characters, so a
    /// widget that budgets in `char`s instead of cells overflows its column.
    const WIDE_NAME: &str = "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d7}\u{30ed}\u{30bb}\u{30b9}";

    /// Renders every widget in this module into `area` inside a buffer that is
    /// `MARGIN` cells larger on every side.
    ///
    /// `third_name` is the name of the third fixture process. It is a parameter
    /// because §5.1's strict-ASCII guarantee covers the characters the design
    /// system *emits*, not the data it is given: a process really can be called
    /// `\u{65e5}\u{672c}\u{8a9e}`, and transliterating it would be a worse lie than
    /// a wide glyph. The ASCII-purity test therefore renders ASCII data, and the
    /// containment property renders the hostile name.
    fn render_every_widget(
        presentation: Presentation<'_>,
        width: u16,
        height: u16,
        third_name: &str,
    ) -> (Buffer, Rect) {
        let buffer_area = Rect::new(
            0,
            0,
            width.saturating_add(MARGIN * 2),
            height.saturating_add(MARGIN * 2),
        );
        let mut buffer = Buffer::filled(buffer_area, sentinel_cell());
        let area = Rect::new(MARGIN, MARGIN, width, height);

        let processes = [
            process(31_842, "rustc", ProcessState::Running),
            process(1_221, "postgres", ProcessState::Zombie),
            process(507, third_name, ProcessState::UninterruptibleSleep),
        ];
        let rows: Vec<ProcessRow<'_>> = processes
            .iter()
            .enumerate()
            .map(|(index, snapshot)| {
                ProcessRow::new(snapshot)
                    .selected(index == 1)
                    .pinned(index == 0)
            })
            .collect();
        let table_layout = TableLayout::fit(width);
        let series = history();
        let signal_list = signals();
        let pins = [
            PinRow::new(
                "rustc",
                ProcessIdentity::new(31_842, 1),
                MetricState::Available(percent(287.0)),
            )
            .with_baseline(MetricState::Available(percent(245.0))),
            PinRow::new(
                "postgres",
                ProcessIdentity::new(1_221, 2),
                MetricState::PermissionDenied,
            ),
        ];
        let cores: Vec<MetricState<Percent>> = (0..256)
            .map(|index| MetricState::Available(percent((index % 101) as f32)))
            .collect();
        let tree_row = TreeRow {
            identity: ProcessIdentity::new(3, 3),
            process_index: 0,
            depth: 4,
            parent_row: Some(0),
            descendants: 2,
            is_last_child: true,
            parent_link_cut: false,
        };

        Panel::new(presentation, "PROCESSES")
            .with_trailing("218 total")
            .focused(true)
            .render(area, &mut buffer);
        Meter::new(presentation, MetricState::Available(percent(37.0)))
            .with_label("CPU")
            .with_note("load 4.12 3.84 3.21")
            .render(area, &mut buffer);
        Meter::new(presentation, MetricState::PermissionDenied)
            .with_label("MEM")
            .render(area, &mut buffer);
        Sparkline::new(presentation, &series)
            .with_label("CPU")
            .render(area, &mut buffer);
        Sparkline::new(presentation, &series)
            .with_label("I/O")
            .dense(true)
            .self_scaling(true)
            .render(area, &mut buffer);
        SparklineCaret::new(presentation, &series, 5)
            .with_label("CPU")
            .with_note("-00:37 selected")
            .render(area, &mut buffer);
        ProcessTable::new(presentation, &table_layout, &rows)
            .with_header(true)
            .render(area, &mut buffer);
        ProcessTable::new(presentation, &table_layout, &[])
            .with_header(true)
            .render(area, &mut buffer);
        Radar::new(presentation, &signal_list)
            .with_rules(true)
            .render(area, &mut buffer);
        Pins::new(presentation, &pins)
            .with_baseline_label("vs 30s")
            .render(area, &mut buffer);
        CoreStrip::new(presentation, &cores)
            .with_label("CORES")
            .render(area, &mut buffer);

        // `tree_prefix` is a formatter rather than a widget, but its output feeds
        // the table's name column, so its width bound belongs to the same property.
        let prefix = tree_prefix(
            presentation.glyphs(),
            tree_row.depth,
            tree_row.is_last_child,
            &[true, false, true, false],
            usize::from(width),
        );
        assert!(monitrs_core::units::display_width(&prefix) <= usize::from(width));

        (buffer, area)
    }

    fn assert_margin_untouched(buffer: &Buffer, area: Rect) {
        let sentinel = sentinel_cell();
        for y in buffer.area.top()..buffer.area.bottom() {
            for x in buffer.area.left()..buffer.area.right() {
                if area.contains((x, y).into()) {
                    continue;
                }
                let cell = buffer.cell((x, y)).expect("inside the buffer");
                assert_eq!(
                    cell, &sentinel,
                    "a widget wrote to ({x}, {y}), outside {area:?}"
                );
            }
        }
    }

    #[test]
    fn a_zero_area_render_touches_nothing_and_never_panics() {
        for presentation in presentations() {
            for (width, height) in [(0u16, 0u16), (0, 24), (140, 0), (1, 1), (2, 1), (3, 2)] {
                let (buffer, area) = render_every_widget(presentation, width, height, WIDE_NAME);
                assert_margin_untouched(&buffer, area);
            }
        }
    }

    #[test]
    fn every_widget_renders_at_the_named_breakpoint_sizes() {
        for presentation in presentations() {
            for (width, height) in [(60u16, 16u16), (80, 24), (100, 28), (140, 38)] {
                let (buffer, area) = render_every_widget(presentation, width, height, WIDE_NAME);
                assert_margin_untouched(&buffer, area);
            }
        }
    }

    #[test]
    fn the_default_presentation_is_the_most_conservative_one() {
        let presentation = Presentation::default();
        assert!(presentation.glyphs().is_ascii());
        assert_eq!(presentation.depth(), ColorDepth::Off);
        assert_eq!(presentation.units(), ByteUnits::Iec);
        // With colour off the fallback is still legible through modifiers (§5.2).
        assert!(
            presentation
                .style(Token::Critical)
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
    }

    #[test]
    fn a_presentation_never_hands_out_a_literal_colour_choice() {
        // Widgets name meanings; the theme decides the colour. Swapping the theme
        // must change every style without the widget being involved.
        let dark = Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        );
        let light = dark.with_theme(ThemeId::DefaultLight.theme());
        assert_ne!(dark.style(Token::Text), light.style(Token::Text));
        assert_eq!(
            dark.style(Token::Text).add_modifier,
            light.style(Token::Text).add_modifier,
            "emphasis belongs to the token, not the theme"
        );
    }

    #[test]
    fn focus_is_expressed_through_the_border_token() {
        assert_eq!(Presentation::border_token(true), Token::FocusBorder);
        assert_eq!(Presentation::border_token(false), Token::Border);
        // Focus survives with colour off, because FocusBorder is bold and Border dim.
        let plain = Presentation::default();
        assert_ne!(
            plain.style(Token::FocusBorder),
            plain.style(Token::Border),
            "focus must be visible without colour"
        );
    }

    #[test]
    fn a_metric_display_style_follows_its_token() {
        let presentation = Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        );
        let denied = states::describe_percent(&MetricState::PermissionDenied);
        assert_eq!(
            presentation.metric_style(&denied),
            presentation.style(Token::Watch)
        );
        let stale = states::describe_percent(
            &MetricState::Available(percent(4.0)).into_stale(Duration::from_secs(3)),
        );
        assert_eq!(
            presentation.metric_style(&stale),
            presentation.style(Token::Stale)
        );
    }

    #[test]
    fn a_cue_resolves_to_its_tokens_style_and_keeps_its_symbol() {
        // §5.2: the pair travels together, so a caller cannot take the colour and
        // leave the character behind.
        let presentation = Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        );
        let denied = Cue::for_metric(&MetricState::<Percent>::PermissionDenied);
        assert_eq!(presentation.cue(denied), presentation.style(Token::Watch));
        assert_eq!(denied.symbol, '!');
        let critical = Cue::for_pressure(PressureState::Critical);
        assert_eq!(
            presentation.cue(critical),
            presentation.style(Token::Critical)
        );
        assert_eq!(critical.symbol, 'X');
    }

    #[test]
    fn a_presentation_hands_out_glyphs_from_its_own_resolved_set() {
        let plain = Presentation::default();
        assert_eq!(plain.glyph(Glyph::BorderHorizontal), "-");
        assert_eq!(plain.glyph(Glyph::SelectionMarker), ">");
        let rich = plain.with_glyphs(GlyphSet::unicode());
        assert_eq!(rich.glyph(Glyph::BorderHorizontal), "\u{2500}");
        // The state characters are identical in both modes by design (§5.1).
        assert_eq!(
            plain.glyph(Glyph::StateCritical),
            rich.glyph(Glyph::StateCritical)
        );
        assert_eq!(plain.ellipsis().as_str(), "...");
        assert_eq!(rich.ellipsis().as_str(), "\u{2026}");
    }

    #[test]
    fn the_selected_row_stays_readable_at_every_depth() {
        for depth in ColorDepth::ALL {
            for id in ThemeId::ALL {
                let presentation = Presentation::new(GlyphSet::ascii(), id.theme(), depth);
                assert!(presentation.selection().is_readable(), "{id} at {depth:?}");
            }
        }
    }

    #[test]
    fn strict_ascii_mode_renders_only_printable_seven_bit_output() {
        // The crate-wide promise of §5.1, asserted over every widget at once: with
        // ASCII data in, strict mode emits nothing but printable 7-bit ASCII. The
        // guarantee is about the characters the design system *chooses* — borders,
        // ramps, markers, ellipses — not about data it was handed, which is why the
        // fixture name is ASCII here and deliberately not in the other tests.
        let presentation = Presentation::new(
            GlyphSet::ascii(),
            ThemeId::DefaultDark.theme(),
            ColorDepth::TrueColor,
        );
        for (width, height) in [(60u16, 16u16), (100, 28), (140, 38)] {
            let (buffer, area) = render_every_widget(presentation, width, height, "WindowServer");
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    let cell = buffer.cell((x, y)).expect("inside the buffer");
                    let symbol = cell.symbol();
                    if symbol == sentinel_cell().symbol() {
                        continue;
                    }
                    for byte in symbol.bytes() {
                        assert!(
                            (0x20..=0x7e).contains(&byte),
                            "({x}, {y}) holds {symbol:?}, which is not printable ASCII"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_unavailable_metric_is_never_rendered_as_a_zero_bar() {
        // The most important rule in the project, checked at the pixel level: a
        // permission-denied meter must not look like a measured 0%.
        let presentation = Presentation::default();
        let mut denied = Buffer::filled(Rect::new(0, 0, 40, 1), sentinel_cell());
        Meter::new(presentation, MetricState::<Percent>::PermissionDenied)
            .with_label("MEM")
            .render(Rect::new(0, 0, 40, 1), &mut denied);
        let mut zero = Buffer::filled(Rect::new(0, 0, 40, 1), sentinel_cell());
        Meter::new(presentation, MetricState::Available(Percent::ZERO))
            .with_label("MEM")
            .render(Rect::new(0, 0, 40, 1), &mut zero);

        let text = |buffer: &Buffer| -> String {
            (0..40)
                .filter_map(|x| buffer.cell((x, 0)).map(|cell| cell.symbol().to_owned()))
                .collect()
        };
        assert_ne!(text(&denied), text(&zero));
        assert!(text(&denied).contains("n/a") || text(&denied).contains("denied"));
        assert!(!text(&denied).contains("0%"));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(24))]

        /// §17.7 and §5.7: no widget writes outside its rectangle, at any area.
        ///
        /// The rendered rectangle sits inside a buffer that is larger on every
        /// side, so a write that escapes by even one cell lands on a sentinel and
        /// is caught. A zero-width or zero-height area must simply draw nothing.
        #[test]
        fn widgets_never_write_outside_their_rectangle(
            width in 0u16..=200,
            height in 0u16..=200,
        ) {
            for presentation in presentations() {
                let (buffer, area) = render_every_widget(presentation, width, height, WIDE_NAME);
                let sentinel = sentinel_cell();
                for y in buffer.area.top()..buffer.area.bottom() {
                    for x in buffer.area.left()..buffer.area.right() {
                        if area.contains((x, y).into()) {
                            continue;
                        }
                        let cell = buffer.cell((x, y)).expect("inside the buffer");
                        prop_assert_eq!(
                            cell,
                            &sentinel,
                            "a widget wrote to ({}, {}), outside {:?}",
                            x,
                            y,
                            area
                        );
                    }
                }
            }
        }

        /// A rectangle that starts outside the buffer, or runs past its edge, is
        /// clipped rather than panicking. Ratatui hands out the frame area, but a
        /// nested render can compute anything.
        #[test]
        fn a_rectangle_outside_the_buffer_is_clipped_rather_than_fatal(
            x in 0u16..=64,
            y in 0u16..=64,
            width in 0u16..=64,
            height in 0u16..=64,
        ) {
            let presentation = Presentation::default();
            let mut buffer = Buffer::filled(Rect::new(0, 0, 32, 8), sentinel_cell());
            let area = Rect::new(x, y, width, height);
            Panel::new(presentation, "PANEL").render(area, &mut buffer);
            Meter::new(presentation, MetricState::Available(percent(50.0)))
                .with_label("CPU")
                .render(area, &mut buffer);
            let series = [MetricState::Available(percent(1.0)); 4];
            Sparkline::new(presentation, &series).render(area, &mut buffer);
            let layout = TableLayout::fit(width);
            ProcessTable::new(presentation, &layout, &[]).render(area, &mut buffer);
            Radar::new(presentation, &[]).render(area, &mut buffer);
            Pins::new(presentation, &[]).render(area, &mut buffer);
            CoreStrip::new(presentation, &series).render(area, &mut buffer);
            prop_assert_eq!(buffer.area, Rect::new(0, 0, 32, 8));
        }

        /// Every state a metric can be in is renderable at every width, and the
        /// fitted text always respects its budget (§5.4).
        #[test]
        fn any_metric_state_fits_any_column_width(
            width in 0usize..=40,
            variant in 0u8..=5,
            value in 0.0f32..20_000.0,
        ) {
            let state: MetricState<Percent> = match variant {
                0 => MetricState::Available(percent(value)),
                1 => MetricState::Available(percent(value)).into_stale(Duration::from_secs(7)),
                2 => MetricState::WarmingUp,
                3 => MetricState::PermissionDenied,
                4 => MetricState::Unsupported,
                _ => MetricState::TemporarilyUnavailable(UnavailableReason::CounterReset),
            };
            let display = states::describe_percent(&state);
            for glyphs in [GlyphSet::ascii(), GlyphSet::unicode()] {
                let fitted = display.fitted(width, glyphs);
                prop_assert!(monitrs_core::units::display_width(&fitted) <= width);
                if width > 0 && display.is_placeholder() {
                    // A placeholder always says something: the reason, `n/a`, or at
                    // one cell its symbol. Only a *value* may render blank, and only
                    // when the column is too narrow to keep a single digit (§4).
                    prop_assert!(
                        !fitted.is_empty(),
                        "a placeholder must never render as an empty cell"
                    );
                }
            }
        }
    }
}

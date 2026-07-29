//! The pinned-process strip of §2.5: `rustc  PID 31842  CPU 287%  +42%`.
//!
//! §2.5 asks for three things and this widget is built around all three.
//!
//! * **Pinned processes stay visible even below the sort cutoff.** The strip is
//!   therefore fed independently of the process table, and a pin whose process has
//!   exited is still a row — with `process exited` where its value was, not a
//!   silent disappearance.
//! * **Identity is `(pid, start_time)`.** A [`PinRow`] carries a
//!   [`ProcessIdentity`], so a reused PID cannot inherit a pin. The widget shows
//!   the PID because that is what a user types into `kill`, but it never *keys* on
//!   it.
//! * **Comparison against a baseline.** The baseline — one sample ago, thirty
//!   seconds ago, or the selected historical sample — is chosen by the caller and
//!   named by [`Pins::with_baseline_label`]. A delta with no stated baseline is
//!   meaningless, so the label belongs beside the numbers.
//!
//! # The delta is not coloured
//!
//! A rising CPU delta is not *bad* — a compiler is supposed to use the machine —
//! so colouring the sign green or red would assert something the data does not
//! support. The `+` and `-` characters are the cue, which is also what §5.2's rule
//! against relying on colour asks for, and it leaves the row's single accent
//! available for a genuine state (§5.2's one-accent-per-numeric-row cap).

use monitrs_core::model::{MetricState, ProcessIdentity, ProcessSnapshot};
use monitrs_core::units::{Percent, display_width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::glyphs::GlyphSet;
use crate::layout::Align;
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Painter, Presentation, RowBuilder};

/// Cells reserved for the process name before it is tail-truncated.
pub const NAME_WIDTH: u16 = 12;

/// Cells reserved for `PID <n>`: the keyword plus Linux's seven-digit `pid_max`.
pub const PID_WIDTH: u16 = 11;

/// Cells reserved for `CPU <value>`: the keyword plus a 128-core `12800%`.
pub const VALUE_WIDTH: u16 = 10;

/// Cells reserved for the signed delta, such as `+146%` or `-3%`.
pub const DELTA_WIDTH: u16 = 7;

/// Cells between fields.
const FIELD_GAP: u16 = 2;

/// One pinned process.
///
/// The widget takes a name, an identity, and two values rather than a
/// `&ProcessSnapshot`, because a pin outlives the process it points at: §2.5
/// requires the row to remain when the process falls below the sort cutoff *or*
/// exits, and at that point there is no snapshot to borrow. [`PinRow::from_process`]
/// exists for the ordinary case where there is one.
#[derive(Clone, Debug)]
pub struct PinRow<'a> {
    name: &'a str,
    identity: ProcessIdentity,
    value: MetricState<Percent>,
    baseline: Option<MetricState<Percent>>,
    metric_label: &'a str,
}

impl<'a> PinRow<'a> {
    /// A pin with no baseline to compare against yet.
    #[must_use]
    pub const fn new(
        name: &'a str,
        identity: ProcessIdentity,
        value: MetricState<Percent>,
    ) -> Self {
        Self {
            name,
            identity,
            value,
            baseline: None,
            metric_label: "CPU",
        }
    }

    /// A pin for a live process, comparing its CPU against `baseline`.
    #[must_use]
    pub fn from_process(
        process: &'a ProcessSnapshot,
        baseline: Option<MetricState<Percent>>,
    ) -> Self {
        Self {
            name: &process.name,
            identity: process.identity,
            value: process.cpu,
            baseline,
            metric_label: "CPU",
        }
    }

    /// A pin whose process has gone.
    ///
    /// §2.5 keeps the row: a pin that vanishes silently is indistinguishable from a
    /// pin that was never made, and §14.1 treats an exited process as expected
    /// rather than as an error.
    #[must_use]
    pub const fn exited(name: &'a str, identity: ProcessIdentity) -> Self {
        Self {
            name,
            identity,
            value: MetricState::TemporarilyUnavailable(
                monitrs_core::model::UnavailableReason::ProcessExited,
            ),
            baseline: None,
            metric_label: "CPU",
        }
    }

    /// Sets the value to compare against (§2.5).
    #[must_use]
    pub const fn with_baseline(mut self, baseline: MetricState<Percent>) -> Self {
        self.baseline = Some(baseline);
        self
    }

    /// Renames the compared metric, for a strip that pins memory rather than CPU.
    #[must_use]
    pub const fn with_metric_label(mut self, label: &'a str) -> Self {
        self.metric_label = label;
        self
    }

    /// The pinned process's stable identity (§2.5, §26).
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        self.identity
    }

    /// The pinned process's name.
    #[must_use]
    pub const fn name(&self) -> &'a str {
        self.name
    }

    /// The current value as text, a token, and a symbol.
    #[must_use]
    pub fn value_display(&self) -> MetricDisplay {
        states::describe_percent(&self.value)
    }

    /// The change in percentage *points* from the baseline, when both are known.
    ///
    /// `None` when either side is missing — including when the metric is warming up,
    /// which §8.2 makes the normal state of the first sample. A delta computed
    /// against an absent baseline would be the current value dressed up as a
    /// change.
    #[must_use]
    pub fn delta_points(&self) -> Option<f32> {
        let current = self.value.displayable().map(|(percent, _)| *percent)?;
        let baseline = self
            .baseline
            .as_ref()?
            .displayable()
            .map(|(percent, _)| *percent)?;
        Some(current.points_from(baseline))
    }

    /// The signed delta as text, or the reason there is none.
    ///
    /// A sign is always present, `+0%` included: "measured, and unchanged" is
    /// information, and it must not look like "not measured" (§4).
    ///
    /// When there is no delta the cell shows the placeholder of whichever *side* is
    /// missing — never the side that is present. Rendering a known baseline in the
    /// delta column would put a number where a change belongs, which is the §4
    /// failure mode in its most confusing form: `+100%` and `100%` differ by one
    /// character and by everything.
    #[must_use]
    pub fn delta_text(&self, width: u16, glyphs: GlyphSet) -> String {
        if let Some(points) = self.delta_points() {
            let sign = if points < 0.0 { '-' } else { '+' };
            let magnitude = Percent::new(points.abs()).unwrap_or(Percent::ZERO);
            return format!("{sign}{magnitude}");
        }
        // No baseline at all: nothing is claimed, so nothing is shown.
        let Some(baseline) = self.baseline.as_ref() else {
            return String::new();
        };
        let missing = if self.value.displayable().is_none() {
            &self.value
        } else {
            baseline
        };
        states::describe_percent(missing).fitted(usize::from(width), glyphs)
    }

    /// The row as an assembled builder.
    #[must_use]
    pub fn row(&self, presentation: Presentation<'_>, width: u16) -> RowBuilder {
        let glyphs = presentation.glyphs();
        let mut row = RowBuilder::new(width, glyphs);
        if row.is_full() {
            return row;
        }
        let value = self.value_display();

        row.push_field(
            self.name,
            NAME_WIDTH,
            Align::Left,
            presentation.style(Token::Text),
        );
        row.pad(FIELD_GAP);
        row.push_field(
            &format!("PID {}", self.identity.pid),
            PID_WIDTH,
            Align::Left,
            presentation.style(Token::Muted),
        );
        row.pad(FIELD_GAP);
        // The symbol is one cell and always present, so the value column does not
        // shift as the metric's availability changes (§5.2).
        let value_text = format!(
            "{} {}{}",
            self.metric_label,
            value.symbol(),
            value.fitted(
                usize::from(VALUE_WIDTH).saturating_sub(display_width(self.metric_label) + 2),
                glyphs,
            )
        );
        row.push_field(
            &value_text,
            VALUE_WIDTH,
            Align::Left,
            presentation.metric_style(&value),
        );
        row.pad(FIELD_GAP);
        row.push_field(
            &self.delta_text(DELTA_WIDTH, glyphs),
            DELTA_WIDTH,
            Align::Right,
            presentation.style(Token::Muted),
        );
        row
    }

    /// The row as a plain string of exactly `width` cells.
    #[must_use]
    pub fn line(&self, presentation: Presentation<'_>, width: u16) -> String {
        self.row(presentation, width).padded_text()
    }
}

/// The compact pinned-process strip (§2.5).
#[derive(Clone, Debug)]
pub struct Pins<'a> {
    presentation: Presentation<'a>,
    pins: &'a [PinRow<'a>],
    baseline_label: Option<&'a str>,
}

impl<'a> Pins<'a> {
    /// A strip over `pins`, in the order the caller pinned them.
    #[must_use]
    pub const fn new(presentation: Presentation<'a>, pins: &'a [PinRow<'a>]) -> Self {
        Self {
            presentation,
            pins,
            baseline_label: None,
        }
    }

    /// Names the baseline the deltas are against, such as `vs 30s` (§2.5).
    ///
    /// Rendered as the first row's suffix when there is room. A delta without a
    /// stated baseline is not a measurement of anything, so a caller that shows
    /// deltas should always set this — the panel's trailing label is the other
    /// reasonable place for it.
    #[must_use]
    pub const fn with_baseline_label(mut self, label: &'a str) -> Self {
        self.baseline_label = Some(label);
        self
    }

    /// The stated baseline, if any.
    #[must_use]
    pub const fn baseline_label(&self) -> Option<&'a str> {
        self.baseline_label
    }

    /// How many pins fit in `height` rows.
    #[must_use]
    pub fn visible_pins(&self, height: u16) -> usize {
        usize::from(height).min(self.pins.len())
    }

    /// Whether a pin had to be dropped for lack of room.
    ///
    /// §2.5 promises a pinned process stays visible, so a screen that sees `true`
    /// here owes the user a count rather than a silent truncation.
    #[must_use]
    pub fn is_truncated(&self, height: u16) -> bool {
        self.visible_pins(height) < self.pins.len()
    }

    /// The cells one row needs before the baseline label is considered.
    #[must_use]
    pub fn row_width() -> u16 {
        NAME_WIDTH
            .saturating_add(PID_WIDTH)
            .saturating_add(VALUE_WIDTH)
            .saturating_add(DELTA_WIDTH)
            .saturating_add(FIELD_GAP.saturating_mul(3))
    }

    /// Every rendered line, for assertions and snapshots.
    #[must_use]
    pub fn lines(&self, width: u16, height: u16) -> Vec<String> {
        self.pins
            .iter()
            .take(self.visible_pins(height))
            .enumerate()
            .map(|(index, pin)| {
                let mut row = pin.row(self.presentation, width);
                if index == 0
                    && let Some(label) = self.baseline_label
                {
                    self.append_baseline(&mut row, label);
                }
                row.padded_text()
            })
            .collect()
    }

    /// Appends the baseline label if the row has room left for it.
    fn append_baseline(&self, row: &mut RowBuilder, label: &str) {
        let needed = u16::try_from(display_width(label)).unwrap_or(u16::MAX);
        if row.remaining() >= needed.saturating_add(FIELD_GAP) {
            row.pad(FIELD_GAP);
            row.push(label, self.presentation.style(Token::Muted));
        }
    }
}

impl Widget for Pins<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut painter = Painter::new(buf, area);
        if painter.is_empty() {
            return;
        }
        let width = painter.width();
        let height = painter.height();
        for (index, pin) in self.pins.iter().take(self.visible_pins(height)).enumerate() {
            let Ok(y) = u16::try_from(index) else { break };
            if y >= height {
                break;
            }
            let mut row = pin.row(self.presentation, width);
            if index == 0
                && let Some(label) = self.baseline_label
            {
                self.append_baseline(&mut row, label);
            }
            painter.write_line(0, y, width, &row.finish());
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::UnavailableReason;

    use super::*;
    use crate::theme::{ColorDepth, ThemeId};

    /// The cell offset of `needle`, which is not its byte offset once a
    /// double-width process name is on the row.
    fn cell_of(line: &str, needle: &str) -> Option<usize> {
        let byte = line.find(needle)?;
        line.get(..byte).map(display_width)
    }

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

    fn identity(pid: u32) -> ProcessIdentity {
        ProcessIdentity::new(pid, u64::from(pid) * 11)
    }

    #[test]
    fn a_row_matches_the_shape_in_the_specification_mockup() {
        // §5.5: `rustc  PID 31842  CPU 287%  +42%`.
        let pin = PinRow::new(
            "rustc",
            identity(31_842),
            MetricState::Available(percent(287.0)),
        )
        .with_baseline(MetricState::Available(percent(245.0)));
        let line = pin.line(ascii(), 50);
        assert_eq!(display_width(&line), 50);
        assert!(line.starts_with("rustc"), "{line:?}");
        assert!(line.contains("PID 31842"), "{line:?}");
        assert!(line.contains("CPU  287%"), "{line:?}");
        assert!(line.contains("+42%"), "{line:?}");
    }

    #[test]
    fn a_row_occupies_exactly_its_width_at_every_size() {
        let pin = PinRow::new(
            "postgres",
            identity(1_221),
            MetricState::Available(percent(54.0)),
        )
        .with_baseline(MetricState::Available(percent(57.0)));
        for width in 0..=80u16 {
            let line = pin.line(ascii(), width);
            assert_eq!(display_width(&line), usize::from(width), "{line:?}");
        }
    }

    #[test]
    fn a_negative_delta_carries_its_sign_as_the_non_colour_cue() {
        let pin = PinRow::new(
            "postgres",
            identity(1_221),
            MetricState::Available(percent(54.0)),
        )
        .with_baseline(MetricState::Available(percent(57.0)));
        assert_eq!(pin.delta_points().map(f32::round), Some(-3.0));
        assert_eq!(pin.delta_text(DELTA_WIDTH, GlyphSet::ascii()), "-3.0%");
        let rising = PinRow::new("rustc", identity(2), MetricState::Available(percent(200.0)))
            .with_baseline(MetricState::Available(percent(54.0)));
        assert_eq!(rising.delta_text(DELTA_WIDTH, GlyphSet::ascii()), "+146%");
    }

    #[test]
    fn an_unchanged_value_shows_a_signed_zero_rather_than_a_blank() {
        // §4: "measured, and unchanged" must not look like "not measured".
        let pin = PinRow::new("idle", identity(3), MetricState::Available(percent(12.0)))
            .with_baseline(MetricState::Available(percent(12.0)));
        assert_eq!(pin.delta_text(DELTA_WIDTH, GlyphSet::ascii()), "+0%");
    }

    #[test]
    fn no_baseline_means_no_delta_rather_than_the_value_dressed_as_a_change() {
        let pin = PinRow::new("rustc", identity(4), MetricState::Available(percent(287.0)));
        assert_eq!(pin.delta_points(), None);
        assert_eq!(pin.delta_text(DELTA_WIDTH, GlyphSet::ascii()), "");
        let line = pin.line(ascii(), 50);
        assert!(!line.contains('+'), "{line:?}");
        assert!(
            line.contains("287%"),
            "the current value is still shown: {line:?}"
        );
    }

    #[test]
    fn a_warming_up_baseline_says_so_instead_of_computing_a_delta() {
        // §8.2: the first delta sample has no predecessor.
        let pin = PinRow::new("rustc", identity(5), MetricState::Available(percent(287.0)))
            .with_baseline(MetricState::WarmingUp);
        assert_eq!(pin.delta_points(), None);
        let text = pin.delta_text(DELTA_WIDTH, GlyphSet::ascii());
        assert_eq!(text, "n/a", "seven cells cannot hold `warming up`");
        assert!(!text.contains('0'), "{text:?}");
    }

    #[test]
    fn a_warming_up_value_produces_no_delta_either() {
        let pin = PinRow::new("rustc", identity(6), MetricState::WarmingUp)
            .with_baseline(MetricState::Available(percent(100.0)));
        assert_eq!(pin.delta_points(), None);
        let line = pin.line(ascii(), 50);
        assert!(line.contains("n/a"), "{line:?}");
        // The known baseline must not be printed where a change belongs.
        assert!(!line.contains("100%"), "{line:?}");
        assert!(!line.contains("0%"), "{line:?}");
    }

    #[test]
    fn a_stale_value_is_compared_but_marked_stale() {
        // §4 lets a retained value be displayed; the `~` symbol and the stale token
        // are what make the comparison honest.
        let presentation = ascii();
        let pin = PinRow::new(
            "rustc",
            identity(7),
            MetricState::Available(percent(120.0)).into_stale(Duration::from_secs(3)),
        )
        .with_baseline(MetricState::Available(percent(100.0)));
        assert_eq!(pin.delta_points().map(f32::round), Some(20.0));
        assert_eq!(pin.value_display().token(), Token::Stale);
        let line = pin.line(presentation, 50);
        assert!(line.contains('~'), "{line:?}");
    }

    #[test]
    fn an_exited_pin_stays_on_the_strip_and_says_why() {
        // §2.5: a pinned process remains visible. §14.1: exiting is expected.
        let pin = PinRow::exited("rustc", identity(8));
        let line = pin.line(ascii(), 60);
        assert!(line.starts_with("rustc"), "{line:?}");
        assert!(line.contains("PID 8"), "{line:?}");
        assert!(
            line.contains("process exited") || line.contains("n/a"),
            "{line:?}"
        );
        assert!(!line.contains("0%"), "{line:?}");
        assert_eq!(pin.value_display().symbol(), '?');
    }

    #[test]
    fn a_pin_is_keyed_on_identity_not_on_the_pid_it_displays() {
        // §2.5 and §26: PID reuse must not attach a pin to the wrong process.
        let original = ProcessIdentity::new(31_842, 900_100);
        let recycled = ProcessIdentity::new(31_842, 977_400);
        let pin = PinRow::new("rustc", original, MetricState::Available(percent(1.0)));
        assert_eq!(pin.identity(), original);
        assert_ne!(pin.identity(), recycled);
        assert!(recycled.is_reuse_of(&original));
        // The PID is shown because that is what a user types; it is not the key.
        assert!(pin.line(ascii(), 50).contains("PID 31842"));
    }

    #[test]
    fn the_compared_metric_can_be_renamed_for_a_memory_strip() {
        let pin = PinRow::new("rustc", identity(12), MetricState::Available(percent(8.1)))
            .with_metric_label("MEM")
            .with_baseline(MetricState::Available(percent(6.0)));
        let line = pin.line(ascii(), 60);
        assert!(line.contains("MEM  8.1%"), "{line:?}");
        assert!(!line.contains("CPU"), "{line:?}");
        assert_eq!(display_width(&line), 60);
    }

    #[test]
    fn a_pin_can_be_built_from_a_live_snapshot() {
        use monitrs_core::model::{ProcessIo, ProcessMemory, ProcessSnapshot, ProcessState};

        let snapshot = ProcessSnapshot {
            identity: identity(9),
            parent_pid: Some(1),
            name: "node".into(),
            command: "node server.js".into(),
            exe: None,
            user: MetricState::Unsupported,
            state: ProcessState::Running,
            cpu: MetricState::Available(percent(12.0)),
            memory: ProcessMemory::WARMING_UP,
            io: ProcessIo::UNSUPPORTED,
            threads: MetricState::Unsupported,
            age: MetricState::Unsupported,
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        };
        let pin = PinRow::from_process(&snapshot, Some(MetricState::Available(percent(8.0))));
        assert_eq!(pin.name(), "node");
        assert_eq!(pin.identity(), snapshot.identity);
        assert_eq!(pin.delta_points().map(f32::round), Some(4.0));
    }

    #[test]
    fn a_long_name_is_truncated_without_moving_the_columns() {
        // §5.4: the fields are reserved from geometry.
        let short = PinRow::new("node", identity(10), MetricState::Available(percent(1.0)));
        let long = PinRow::new(
            "a-very-long-process-name",
            identity(10),
            MetricState::Available(percent(1.0)),
        );
        let unicode_name = PinRow::new(
            "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d7}\u{30ed}\u{30bb}\u{30b9}",
            identity(10),
            MetricState::Available(percent(1.0)),
        );
        let position = |pin: &PinRow<'_>| cell_of(&pin.line(ascii(), 60), "PID");
        assert_eq!(position(&short), position(&long));
        assert_eq!(
            position(&short),
            position(&unicode_name),
            "a double-width name must not shift the PID column"
        );
        assert!(long.line(ascii(), 60).contains("..."));
        assert_eq!(display_width(&unicode_name.line(ascii(), 60)), 60);
    }

    #[test]
    fn the_value_column_does_not_move_as_the_value_changes() {
        let positions: Vec<Option<usize>> = [0.0f32, 9.9, 54.0, 287.0, 12_800.0]
            .into_iter()
            .map(|value| {
                PinRow::new("p", identity(11), MetricState::Available(percent(value)))
                    .line(ascii(), 60)
                    .find("CPU")
            })
            .collect();
        assert!(positions.windows(2).all(|pair| pair.first() == pair.get(1)));
    }

    #[test]
    fn the_strip_draws_one_row_per_pin_and_reports_truncation() {
        let pins = [
            PinRow::new("rustc", identity(1), MetricState::Available(percent(287.0))),
            PinRow::new(
                "postgres",
                identity(2),
                MetricState::Available(percent(54.0)),
            ),
            PinRow::new("node", identity(3), MetricState::Available(percent(12.0))),
        ];
        let strip = Pins::new(ascii(), &pins);
        assert_eq!(strip.visible_pins(3), 3);
        assert!(!strip.is_truncated(3));
        assert_eq!(strip.visible_pins(2), 2);
        assert!(strip.is_truncated(2), "§2.5 promises pins stay visible");
        assert_eq!(strip.lines(60, 3).len(), 3);
        assert_eq!(strip.lines(60, 0).len(), 0);
    }

    #[test]
    fn the_baseline_is_named_on_the_first_row_so_the_delta_means_something() {
        let pins = [
            PinRow::new("rustc", identity(1), MetricState::Available(percent(287.0)))
                .with_baseline(MetricState::Available(percent(245.0))),
            PinRow::new(
                "postgres",
                identity(2),
                MetricState::Available(percent(54.0)),
            )
            .with_baseline(MetricState::Available(percent(57.0))),
        ];
        let strip = Pins::new(ascii(), &pins).with_baseline_label("vs 30s");
        assert_eq!(strip.baseline_label(), Some("vs 30s"));
        let lines = strip.lines(60, 2);
        assert!(
            lines.first().is_some_and(|line| line.contains("vs 30s")),
            "{lines:?}"
        );
        assert!(
            lines.get(1).is_some_and(|line| !line.contains("vs 30s")),
            "the baseline is stated once: {lines:?}"
        );
    }

    #[test]
    fn a_row_too_narrow_for_the_baseline_label_drops_it_rather_than_the_delta() {
        let pins = [
            PinRow::new("rustc", identity(1), MetricState::Available(percent(287.0)))
                .with_baseline(MetricState::Available(percent(245.0))),
        ];
        let strip = Pins::new(ascii(), &pins).with_baseline_label("vs 30s");
        let narrow = strip.lines(Pins::row_width(), 1);
        assert!(narrow.first().is_some_and(|line| !line.contains("vs 30s")));
        assert!(narrow.first().is_some_and(|line| line.contains("+42%")));
    }

    #[test]
    fn the_reserved_row_width_holds_every_field() {
        let pin = PinRow::new(
            "kworker/2:1",
            ProcessIdentity::new(4_194_304, 1),
            MetricState::Available(percent(12_800.0)),
        )
        .with_baseline(MetricState::Available(Percent::ZERO));
        let line = pin.line(ascii(), Pins::row_width());
        assert_eq!(display_width(&line), usize::from(Pins::row_width()));
        assert!(line.contains("PID 4194304"), "{line:?}");
        assert!(line.contains("12800%"), "{line:?}");
    }

    #[test]
    fn an_empty_strip_draws_nothing() {
        let strip = Pins::new(ascii(), &[]);
        assert!(strip.lines(60, 4).is_empty());
        assert!(!strip.is_truncated(4));
        let area = Rect::new(0, 0, 60, 2);
        let mut buffer = Buffer::empty(area);
        strip.render(area, &mut buffer);
        assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
    }

    #[test]
    fn a_zero_area_strip_draws_nothing_without_panicking() {
        let pins = [PinRow::new(
            "rustc",
            identity(1),
            MetricState::Available(percent(1.0)),
        )];
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 60, 0),
            Rect::new(0, 0, 0, 3),
        ] {
            let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 3));
            Pins::new(ascii(), &pins)
                .with_baseline_label("vs 30s")
                .render(area, &mut buffer);
            assert!(buffer.content().iter().all(|cell| cell.symbol() == " "));
        }
    }

    #[test]
    fn the_strip_never_draws_more_rows_than_its_area_has() {
        let pins: Vec<PinRow<'_>> = (1..=8)
            .map(|pid| PinRow::new("p", identity(pid), MetricState::Available(percent(1.0))))
            .collect();
        let area = Rect::new(0, 0, 60, 3);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 60, 6));
        Pins::new(ascii(), &pins).render(area, &mut buffer);
        for y in 3..6u16 {
            let row: String = (0..60)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                .collect();
            assert_eq!(row.trim(), "", "row {y} escaped the area");
        }
    }

    #[test]
    fn the_delta_is_never_an_accent_colour() {
        // §5.2 caps a numeric row at one accent, and a rising CPU delta is not a
        // fault to be flagged.
        let presentation = ascii();
        let pin = PinRow::new("rustc", identity(1), MetricState::Available(percent(287.0)))
            .with_baseline(MetricState::Available(percent(1.0)));
        let row = pin.row(presentation, 60);
        let line = row.finish();
        let accents = Token::ALL.into_iter().filter(|token| token.is_accent());
        for token in accents {
            let style = presentation.style(token);
            assert!(
                !line.spans.iter().any(|span| span.style == style),
                "the pin row used the accent {}",
                token.name()
            );
        }
    }

    #[test]
    fn everything_the_strip_itself_draws_is_printable_seven_bit() {
        // §5.1 constrains the characters the *design system* emits — labels,
        // symbols, signs, placeholders. A process name is data, and is covered by
        // the next test.
        let pins = [
            PinRow::new("rustc", identity(1), MetricState::Available(percent(1.0)))
                .with_baseline(MetricState::Available(percent(2.0))),
            PinRow::exited("postgres", identity(2)),
            PinRow::new("p", identity(3), MetricState::PermissionDenied).with_baseline(
                MetricState::TemporarilyUnavailable(UnavailableReason::Timeout),
            ),
        ];
        for line in Pins::new(ascii(), &pins)
            .with_baseline_label("vs 30s")
            .lines(70, 4)
        {
            for byte in line.bytes() {
                assert!(
                    (0x20..=0x7e).contains(&byte),
                    "{line:?} has byte {byte:#04x}"
                );
            }
        }
    }

    #[test]
    fn a_unicode_process_name_survives_strict_ascii_mode_because_it_is_data() {
        // Transliterating a process's real name would be a worse lie than the
        // occasional wide glyph, so strict mode bounds the name's width without
        // rewriting it. Only the truncation marker changes.
        let name = "\u{65e5}\u{672c}\u{8a9e}";
        let pins = [PinRow::new(
            name,
            identity(1),
            MetricState::Available(percent(1.0)),
        )];
        let line = Pins::new(ascii(), &pins)
            .lines(60, 1)
            .first()
            .cloned()
            .expect("one row");
        assert!(line.contains(name), "{line:?}");
        assert_eq!(display_width(&line), 60);
    }
}

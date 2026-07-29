//! The process table of §5.5 and §7.2, driven by [`TableLayout`]'s column priority.
//!
//! Every width decision has already been made by [`crate::layout`]: which columns
//! fit, how wide each is, and where it starts. This widget's only job is to put
//! text in those cells, and its whole contract is that it *cannot* change them.
//! [`Column::reserved_width`] is a constant, so a process whose RSS crosses
//! `1023B -> 1.0KiB` cannot move the columns to its right (§5.4).
//!
//! # The three non-colour cues
//!
//! §5.2 forbids colour from being the only indicator and §7.2 requires zombie and
//! uninterruptible-sleep rows to be *visibly distinct*. Three separate mechanisms
//! carry that, and all three survive `--color off`:
//!
//! * the selected row is marked `>` in the marker column and reversed by
//!   [`crate::theme::Theme::selection_style`];
//! * a notable row is drawn in [`Token::Critical`], whose emphasis is bold plus
//!   underline rather than a colour;
//! * a notable row shows its state code — `Z` or `D` — in the marker column. That
//!   column is priority `0` and is therefore present at every width the table is,
//!   which matters because the `STATE` column itself is dropped in the Compact
//!   band (§5.7). Without this the character cue would disappear exactly when the
//!   terminal is smallest.
//!
//! [`Column::reserved_width`]: crate::layout::Column::reserved_width

use monitrs_core::model::{ProcessIdentity, ProcessSnapshot, UserIdentity};
use monitrs_core::units::{display_width, truncate_tail};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::widgets::Widget;

use crate::glyphs::GlyphSet;
use crate::layout::{Column, TableLayout};
use crate::theme::Token;
use crate::widgets::states::{self, MetricDisplay};
use crate::widgets::{Painter, Presentation, RowBuilder};

/// The owner of a process, as text and availability (§4).
///
/// A named helper so the cell text and the cell token cannot disagree about what
/// an unreadable owner looks like.
fn user_display(process: &ProcessSnapshot) -> MetricDisplay {
    states::describe(&process.user, UserIdentity::display_name)
}

/// One row of the table: a process plus the presentation state the screen owns.
///
/// The widget deliberately does not decide what is selected or pinned. Selection
/// is tracked by stable [`ProcessIdentity`] in the reducer (§7.2: "do not allow row
/// selection to jump unpredictably on each refresh"), and a widget that recomputed
/// it per frame would be the thing that made it jump.
#[derive(Clone, Copy, Debug)]
pub struct ProcessRow<'a> {
    process: &'a ProcessSnapshot,
    prefix: Option<&'a str>,
    selected: bool,
    pinned: bool,
}

impl<'a> ProcessRow<'a> {
    /// An unselected, unpinned, untreed row.
    #[must_use]
    pub const fn new(process: &'a ProcessSnapshot) -> Self {
        Self {
            process,
            prefix: None,
            selected: false,
            pinned: false,
        }
    }

    /// Attaches a tree prefix from [`crate::widgets::tree`].
    #[must_use]
    pub const fn with_prefix(mut self, prefix: &'a str) -> Self {
        self.prefix = Some(prefix);
        self
    }

    /// Marks this row as the selection.
    #[must_use]
    pub const fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Marks this row as pinned (§2.5).
    #[must_use]
    pub const fn pinned(mut self, pinned: bool) -> Self {
        self.pinned = pinned;
        self
    }

    /// The process this row shows.
    #[must_use]
    pub const fn process(&self) -> &'a ProcessSnapshot {
        self.process
    }

    /// Whether this row is the selection.
    #[must_use]
    pub const fn is_selected(&self) -> bool {
        self.selected
    }

    /// Whether this row is pinned.
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// Whether §7.2 requires this row to be rendered distinctly.
    #[must_use]
    pub const fn is_notable(&self) -> bool {
        self.process.state.is_notable()
    }

    /// The stable identity of the process on this row (§26).
    #[must_use]
    pub const fn identity(&self) -> ProcessIdentity {
        self.process.identity
    }
}

/// Builds rows for a slice of processes, marking the one with `selected` identity.
///
/// Keyed on the full [`ProcessIdentity`], so a reused PID selects nothing rather
/// than the wrong process (§26: a PID alone is not an identity).
#[must_use]
pub fn rows_from(
    processes: &[ProcessSnapshot],
    selected: Option<ProcessIdentity>,
) -> Vec<ProcessRow<'_>> {
    processes
        .iter()
        .map(|process| ProcessRow::new(process).selected(selected == Some(process.identity)))
        .collect()
}

/// The character shown in the one-cell marker column.
///
/// Selection wins over the notable-state cue: a selected row is already the one the
/// user is looking at, and its style and the detail overlay both say what state it
/// is in. An unselected notable row keeps its state code, which is the cue that has
/// to survive the `STATE` column being dropped (§5.7, §7.2).
#[must_use]
pub fn marker_for(row: &ProcessRow<'_>, glyphs: GlyphSet) -> String {
    if row.is_selected() {
        return glyphs.selection_marker().to_owned();
    }
    if row.is_notable() {
        return row.process().state.code().to_string();
    }
    if row.is_pinned() {
        return "*".to_owned();
    }
    glyphs.selection_blank().to_owned()
}

/// The process table.
#[derive(Clone, Debug)]
pub struct ProcessTable<'a> {
    presentation: Presentation<'a>,
    layout: &'a TableLayout,
    rows: &'a [ProcessRow<'a>],
    show_header: bool,
    scroll: usize,
}

impl<'a> ProcessTable<'a> {
    /// A table of `rows`, sized by `layout`.
    #[must_use]
    pub const fn new(
        presentation: Presentation<'a>,
        layout: &'a TableLayout,
        rows: &'a [ProcessRow<'a>],
    ) -> Self {
        Self {
            presentation,
            layout,
            rows,
            show_header: true,
            scroll: 0,
        }
    }

    /// Whether to draw the column-header row.
    #[must_use]
    pub const fn with_header(mut self, show_header: bool) -> Self {
        self.show_header = show_header;
        self
    }

    /// Skips the first `scroll` rows.
    ///
    /// The widget does not scroll itself: §7.2 requires selection to keep its visual
    /// position across refreshes, which only the reducer holding the identity can
    /// arrange.
    #[must_use]
    pub const fn with_scroll(mut self, scroll: usize) -> Self {
        self.scroll = scroll;
        self
    }

    /// How many data rows fit in `height`, allowing for the header.
    #[must_use]
    pub fn visible_rows(&self, height: u16) -> usize {
        let body = height.saturating_sub(u16::from(self.show_header));
        usize::from(body).min(self.rows.len().saturating_sub(self.scroll))
    }

    /// The header row.
    #[must_use]
    pub fn header_row(&self) -> RowBuilder {
        let mut row = RowBuilder::new(self.layout.width(), self.presentation.glyphs());
        let style = self.presentation.style(Token::Muted);
        for entry in self.layout.columns() {
            row.pad_to(entry.x);
            row.push_field(entry.column.header(), entry.width, entry.align, style);
        }
        row
    }

    /// The header as a plain string of exactly the layout's width.
    #[must_use]
    pub fn header_line(&self) -> String {
        self.header_row().padded_text()
    }

    /// The text for one cell, already fitted to its column.
    ///
    /// Truncation follows §5.4: a name loses its tail, a command loses its middle
    /// because the arguments at both ends carry information, and a numeric cell is
    /// right-aligned by the row builder rather than here.
    #[must_use]
    pub fn cell_text(&self, row: &ProcessRow<'_>, column: Column, width: u16) -> String {
        let glyphs = self.presentation.glyphs();
        let ellipsis = self.presentation.ellipsis();
        let units = self.presentation.units();
        let process = row.process();
        let budget = usize::from(width);

        match column {
            Column::Selection => marker_for(row, glyphs),
            Column::Pid => process.identity.pid.to_string(),
            Column::User => user_display(process).fitted(budget, glyphs),
            Column::State => process.state.code().to_string(),
            Column::CpuPercent => self.metric_cell(&states::describe_percent(&process.cpu), width),
            Column::MemoryPercent => self.metric_cell(
                &states::describe_percent(&process.memory.share_of_total),
                width,
            ),
            Column::Rss => self.metric_cell(
                &states::describe_bytes(&process.memory.rss_bytes, units),
                width,
            ),
            Column::VirtualMemory => self.metric_cell(
                &states::describe_bytes(&process.memory.virtual_bytes, units),
                width,
            ),
            Column::ReadRate => {
                self.metric_cell(&states::describe_byte_rate(&process.io.read, units), width)
            }
            Column::WriteRate => {
                self.metric_cell(&states::describe_byte_rate(&process.io.write, units), width)
            }
            Column::Threads => self.metric_cell(&states::describe_display(&process.threads), width),
            Column::Age => self.metric_cell(&states::describe_age(&process.age), width),
            Column::Name => {
                let prefix = row.prefix.unwrap_or("");
                let prefix_width = display_width(prefix).min(budget);
                let name =
                    states::fit_within(&process.name, budget.saturating_sub(prefix_width), glyphs);
                format!("{}{name}", truncate_tail(prefix, prefix_width, ellipsis))
            }
            Column::Command => states::fit_middle_within(process.command_or_name(), budget, glyphs),
        }
    }

    /// Fits a metric's text into `width`, degrading a placeholder rather than
    /// clipping it into something that reads like a value (§5.1).
    ///
    /// A retained value is prefixed with `~`. §4 requires a stale value to be
    /// *visibly* marked and §5.2 forbids the mark from being only a colour, and a
    /// six-cell `CPU%` column has no room for the age — so the cell carries the
    /// character and [`MetricDisplay::age`] carries the number for the detail
    /// overlay and the status line, which do have room for it.
    fn metric_cell(&self, display: &MetricDisplay, width: u16) -> String {
        let glyphs = self.presentation.glyphs();
        if display.age().is_some() && width > 1 {
            let value = display.fitted(usize::from(width) - 1, glyphs);
            return format!("{}{value}", display.symbol());
        }
        display.fitted(usize::from(width), glyphs)
    }

    /// The token one cell is drawn in.
    ///
    /// A notable row overrides everything: §7.2 requires the whole row to read as
    /// unusual, and [`Token::Critical`]'s bold-plus-underline emphasis does that
    /// without colour. Otherwise the cell follows its own metric's availability, so
    /// a permission-denied `READ/s` is flagged while the rest of the row is not.
    #[must_use]
    pub fn cell_token(&self, row: &ProcessRow<'_>, column: Column) -> Token {
        if row.is_notable() {
            return Token::Critical;
        }
        let process = row.process();
        let units = self.presentation.units();
        match column {
            // Never an accent: §5.2 caps a numeric row at one accent colour, and a
            // notable row above has already spent it.
            Column::Selection | Column::Name | Column::Command => Token::Text,
            Column::Pid | Column::State => Token::Muted,
            Column::User => user_display(process).token(),
            Column::CpuPercent => states::describe_percent(&process.cpu).token(),
            Column::MemoryPercent => {
                states::describe_percent(&process.memory.share_of_total).token()
            }
            Column::Rss => states::describe_bytes(&process.memory.rss_bytes, units).token(),
            Column::VirtualMemory => {
                states::describe_bytes(&process.memory.virtual_bytes, units).token()
            }
            Column::ReadRate => states::describe_byte_rate(&process.io.read, units).token(),
            Column::WriteRate => states::describe_byte_rate(&process.io.write, units).token(),
            Column::Threads => states::describe_display(&process.threads).token(),
            Column::Age => states::describe_age(&process.age).token(),
        }
    }

    /// One data row, assembled.
    #[must_use]
    pub fn data_row(&self, row: &ProcessRow<'_>) -> RowBuilder {
        let mut builder = RowBuilder::new(self.layout.width(), self.presentation.glyphs());
        for entry in self.layout.columns() {
            builder.pad_to(entry.x);
            let text = self.cell_text(row, entry.column, entry.width);
            let style = self.presentation.style(self.cell_token(row, entry.column));
            builder.push_field(&text, entry.width, entry.align, style);
        }
        builder
    }

    /// One data row as a plain string of exactly the layout's width.
    #[must_use]
    pub fn data_line(&self, row: &ProcessRow<'_>) -> String {
        self.data_row(row).padded_text()
    }

    /// Every line the table would draw in `height` rows, header included.
    #[must_use]
    pub fn lines(&self, height: u16) -> Vec<String> {
        let mut lines = Vec::new();
        if self.show_header && height > 0 {
            lines.push(self.header_line());
        }
        for row in self
            .rows
            .iter()
            .skip(self.scroll)
            .take(self.visible_rows(height))
        {
            lines.push(self.data_line(row));
        }
        lines
    }
}

impl Widget for ProcessTable<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let mut painter = Painter::new(buf, area);
        if painter.is_empty() || self.layout.columns().is_empty() {
            return;
        }
        let width = painter.width();
        let height = painter.height();
        let mut y = 0u16;
        if self.show_header {
            painter.write_line(0, 0, width, &self.header_row().finish());
            y = 1;
        }
        let selection = self.presentation.selection().into_style();
        for row in self
            .rows
            .iter()
            .skip(self.scroll)
            .take(self.visible_rows(height))
        {
            if y >= height {
                break;
            }
            if row.is_selected() {
                // The whole row, not only the cells that happened to hold text, so
                // the selection is unmistakable at every width (§5.2). Applied
                // *before* the text: the cell styles that follow set a foreground
                // and add modifiers without touching the background, so the row
                // keeps its selection background and each cell keeps its own
                // availability colour — and with colour off the `REVERSED`
                // modifier survives, because modifiers are inserted rather than
                // replaced.
                painter.style_row(y, selection);
            }
            painter.write_line(0, y, width, &self.data_row(row).finish());
            y = y.saturating_add(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

    use monitrs_core::model::{
        MetricState, ProcessIo, ProcessMemory, ProcessState, UnavailableReason,
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

    fn rate(per_second: f64) -> Rate {
        Rate::new(per_second).expect("a finite non-negative rate")
    }

    fn process(pid: u32, name: &str) -> ProcessSnapshot {
        ProcessSnapshot {
            identity: ProcessIdentity::new(pid, u64::from(pid) * 7),
            parent_pid: Some(1),
            name: name.into(),
            command: format!("/usr/bin/{name} --serve --port 8080 --verbose").into(),
            exe: Some(format!("/usr/bin/{name}").into()),
            user: MetricState::Available(UserIdentity {
                uid: 501,
                name: Some("gabor".into()),
            }),
            state: ProcessState::Running,
            cpu: MetricState::Available(percent(287.0)),
            memory: ProcessMemory {
                rss_bytes: MetricState::Available(2_814_509_056),
                virtual_bytes: MetricState::Available(16_000_000_000),
                share_of_total: MetricState::Available(percent(8.1)),
            },
            io: ProcessIo {
                read: MetricState::Available(rate(18.0 * 1024.0 * 1024.0)),
                write: MetricState::Available(rate(42.0 * 1024.0 * 1024.0)),
                read_total_bytes: MetricState::Unsupported,
                write_total_bytes: MetricState::Unsupported,
            },
            threads: MetricState::Available(9),
            age: MetricState::Available(Duration::from_secs(43)),
            started_at: MetricState::Unsupported,
            is_kernel_thread: false,
        }
    }

    #[test]
    fn the_header_is_exactly_the_layouts_width_and_right_aligns_numerics() {
        for width in 1..=200u16 {
            let layout = TableLayout::fit(width);
            let rows: Vec<ProcessRow<'_>> = Vec::new();
            let table = ProcessTable::new(ascii(), &layout, &rows);
            let header = table.header_line();
            assert_eq!(display_width(&header), usize::from(width), "{header:?}");
        }
        let layout = TableLayout::fit(140);
        let rows: Vec<ProcessRow<'_>> = Vec::new();
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let header = table.header_line();
        let pid = layout.column(Column::Pid).expect("PID at 140 cells");
        // "PID" right-aligned in seven cells ends at the column's right edge.
        let end = usize::from(pid.x + pid.width);
        assert_eq!(header.get(end - 3..end), Some("PID"), "{header:?}");
    }

    #[test]
    fn a_data_row_fills_exactly_the_layouts_width_at_every_size() {
        let snapshot = process(31_842, "rustc");
        let rows = [ProcessRow::new(&snapshot)];
        for width in 1..=200u16 {
            let layout = TableLayout::fit(width);
            let table = ProcessTable::new(ascii(), &layout, &rows);
            let line = table.data_line(&rows[0]);
            assert_eq!(display_width(&line), usize::from(width), "{line:?}");
        }
    }

    #[test]
    fn a_value_crossing_a_unit_boundary_never_moves_a_column() {
        // §5.4, the specific case: `1023B -> 1.0KiB` must not reflow the table.
        let layout = TableLayout::fit(140);
        let boundaries = [
            1_023u64,
            1_024,
            1_048_575,
            1_048_576,
            1_073_741_823,
            1_073_741_824,
            u64::MAX,
        ];
        let mut widths = Vec::new();
        for bytes in boundaries {
            let mut snapshot = process(1, "p");
            snapshot.memory.rss_bytes = MetricState::Available(bytes);
            let rows = [ProcessRow::new(&snapshot)];
            let table = ProcessTable::new(ascii(), &layout, &rows);
            let line = table.data_line(&rows[0]);
            widths.push(display_width(&line));
            // The AGE column, several columns to the right of RSS, stays put.
            let age = layout.column(Column::Age).expect("AGE at 140 cells");
            let cell = line
                .chars()
                .skip(usize::from(age.x))
                .take(usize::from(age.width))
                .collect::<String>();
            assert_eq!(cell.trim(), "00:43", "{bytes} shifted the AGE column");
        }
        assert!(widths.windows(2).all(|pair| pair.first() == pair.get(1)));
    }

    #[test]
    fn every_byte_magnitude_fits_the_reserved_column() {
        let layout = TableLayout::fit(140);
        let rss = layout.column(Column::Rss).expect("RSS at 140 cells");
        for bytes in [0u64, 1, 1_023, 1_024, 1_048_576, 1_073_741_824, u64::MAX] {
            let mut snapshot = process(1, "p");
            snapshot.memory.rss_bytes = MetricState::Available(bytes);
            let rows = [ProcessRow::new(&snapshot)];
            let table = ProcessTable::new(ascii(), &layout, &rows);
            let text = table.cell_text(&rows[0], Column::Rss, rss.width);
            assert!(
                display_width(&text) <= usize::from(rss.width),
                "{bytes} rendered {text:?}"
            );
            assert!(!text.trim().is_empty(), "{bytes} rendered nothing");
        }
    }

    #[test]
    fn si_units_are_honoured_when_configured() {
        let layout = TableLayout::fit(140);
        let mut snapshot = process(1, "p");
        snapshot.memory.rss_bytes = MetricState::Available(1_000);
        let rows = [ProcessRow::new(&snapshot)];
        let iec = ProcessTable::new(ascii(), &layout, &rows);
        let si = ProcessTable::new(ascii().with_units(ByteUnits::Si), &layout, &rows);
        assert_eq!(iec.cell_text(&rows[0], Column::Rss, 5), "1000B");
        assert_eq!(si.cell_text(&rows[0], Column::Rss, 5), "1.0K");
    }

    #[test]
    fn an_unavailable_metric_shows_its_placeholder_not_a_zero() {
        let layout = TableLayout::fit(140);
        let mut snapshot = process(1, "p");
        snapshot.cpu = MetricState::WarmingUp;
        snapshot.io.read = MetricState::PermissionDenied;
        snapshot.threads = MetricState::Unsupported;
        snapshot.age = MetricState::TemporarilyUnavailable(UnavailableReason::ProcessExited);
        let rows = [ProcessRow::new(&snapshot)];
        let table = ProcessTable::new(ascii(), &layout, &rows);
        for (column, forbidden) in [
            (Column::CpuPercent, "0%"),
            (Column::ReadRate, "0B/s"),
            (Column::Threads, "0"),
        ] {
            let entry = layout.column(column).expect("column at 140 cells");
            let text = table.cell_text(&rows[0], column, entry.width);
            assert_ne!(text.trim(), forbidden, "{column:?} rendered as zero");
            assert!(!text.trim().is_empty(), "{column:?} rendered nothing");
        }
        // The narrow columns degrade to `n/a`, and the token still flags them.
        assert_eq!(table.cell_token(&rows[0], Column::ReadRate), Token::Watch);
        assert_eq!(table.cell_token(&rows[0], Column::CpuPercent), Token::Muted);
        assert_eq!(table.cell_token(&rows[0], Column::Threads), Token::Muted);
    }

    #[test]
    fn a_warming_up_first_frame_never_reads_as_an_idle_machine() {
        // §8.2 and §26: the first delta sample is warming up, not zero.
        let layout = TableLayout::fit(100);
        let mut snapshot = process(1, "p");
        snapshot.cpu = MetricState::WarmingUp;
        snapshot.io = ProcessIo::WARMING_UP;
        let rows = [ProcessRow::new(&snapshot)];
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let line = table.data_line(&rows[0]);
        assert!(!line.contains("0%"), "{line:?}");
        assert!(!line.contains("0B/s"), "{line:?}");
    }

    #[test]
    fn a_stale_metric_is_marked_with_a_character_not_only_a_colour() {
        // §4 and §5.2: a retained value must be visibly marked, and the mark cannot
        // be a colour alone.
        let layout = TableLayout::fit(140);
        let mut snapshot = process(1, "p");
        snapshot.cpu = MetricState::Available(percent(54.0)).into_stale(Duration::from_secs(3));
        let rows = [ProcessRow::new(&snapshot)];
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert_eq!(table.cell_token(&rows[0], Column::CpuPercent), Token::Stale);
        let entry = layout
            .column(Column::CpuPercent)
            .expect("CPU% at 140 cells");
        assert_eq!(
            table.cell_text(&rows[0], Column::CpuPercent, entry.width),
            "~54%"
        );
        // Fresh values are not decorated, so the mark means something.
        let other = process(2, "q");
        let fresh = [ProcessRow::new(&other)];
        assert_eq!(
            table.cell_text(&fresh[0], Column::CpuPercent, entry.width),
            "287%"
        );
    }

    #[test]
    fn a_stale_mark_never_widens_its_column() {
        // The mark costs a cell of the *value*, never a cell of the table.
        let mut snapshot = process(1, "p");
        snapshot.cpu = MetricState::Available(percent(12_800.0)).into_stale(Duration::from_secs(3));
        let rows = [ProcessRow::new(&snapshot)];
        for width in 1..=200u16 {
            let layout = TableLayout::fit(width);
            let table = ProcessTable::new(ascii(), &layout, &rows);
            assert_eq!(
                display_width(&table.data_line(&rows[0])),
                usize::from(width),
                "width {width}"
            );
            if let Some(entry) = layout.column(Column::CpuPercent) {
                let cell = table.cell_text(&rows[0], Column::CpuPercent, entry.width);
                assert!(display_width(&cell) <= usize::from(entry.width), "{cell:?}");
                assert!(cell.starts_with('~'), "{cell:?}");
            }
        }
    }

    #[test]
    fn the_selected_row_is_marked_with_the_specified_character() {
        let snapshot = process(31_842, "rustc");
        let rows = [ProcessRow::new(&snapshot).selected(true)];
        let layout = TableLayout::fit(100);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert!(
            table.data_line(&rows[0]).starts_with('>'),
            "§5.1 selection is `>`"
        );
        let unselected = [ProcessRow::new(&snapshot)];
        assert!(!table.data_line(&unselected[0]).starts_with('>'));
    }

    #[test]
    fn a_notable_row_is_distinct_without_any_colour_at_all() {
        // §7.2: zombie and uninterruptible-sleep states visibly distinct. This is
        // the check that the distinction is not a colour.
        let plain = Presentation::default();
        assert_eq!(plain.depth(), ColorDepth::Off);
        let layout = TableLayout::fit(100);

        let normal = process(1, "p");
        for state in [ProcessState::Zombie, ProcessState::UninterruptibleSleep] {
            let mut odd = process(2, "q");
            odd.state = state;
            let odd_rows = [ProcessRow::new(&odd)];
            let normal_rows = [ProcessRow::new(&normal)];
            let table = ProcessTable::new(plain, &layout, &odd_rows);

            // 1. The marker column carries the state code.
            let line = table.data_line(&odd_rows[0]);
            assert!(
                line.starts_with(state.code()),
                "{state:?} lost its marker: {line:?}"
            );
            // 2. The row's style differs from a normal row's, with colour off.
            let odd_style = plain.style(table.cell_token(&odd_rows[0], Column::Name));
            let normal_style = plain.style(table.cell_token(&normal_rows[0], Column::Name));
            assert_ne!(odd_style, normal_style, "{state:?} is only a colour apart");
            assert_eq!(
                table.cell_token(&odd_rows[0], Column::Name),
                Token::Critical
            );
        }
    }

    #[test]
    fn the_notable_marker_survives_the_state_column_being_dropped() {
        // §5.7's Compact band hides low-priority columns; the cue must not go with
        // them.
        let mut zombie = process(2, "q");
        zombie.state = ProcessState::Zombie;
        let rows = [ProcessRow::new(&zombie)];
        let narrow = TableLayout::fit(20);
        assert!(
            !narrow.contains(Column::State),
            "the fixture must drop STATE"
        );
        assert!(narrow.contains(Column::Selection));
        let table = ProcessTable::new(ascii(), &narrow, &rows);
        assert!(table.data_line(&rows[0]).starts_with('Z'));
    }

    #[test]
    fn selection_outranks_the_notable_marker_in_the_one_cell_available() {
        let mut zombie = process(2, "q");
        zombie.state = ProcessState::Zombie;
        let rows = [ProcessRow::new(&zombie).selected(true)];
        let layout = TableLayout::fit(100);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert!(table.data_line(&rows[0]).starts_with('>'));
    }

    #[test]
    fn a_pinned_row_is_marked_when_it_is_neither_selected_nor_notable() {
        let snapshot = process(3, "r");
        let rows = [ProcessRow::new(&snapshot).pinned(true)];
        let layout = TableLayout::fit(100);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert!(table.data_line(&rows[0]).starts_with('*'));
    }

    #[test]
    fn a_long_unicode_name_is_truncated_without_overflowing_its_column() {
        let snapshot = process(
            4,
            "\u{65e5}\u{672c}\u{8a9e}\u{306e}\u{30d7}\u{30ed}\u{30bb}\u{30b9}\u{540d}",
        );
        let rows = [ProcessRow::new(&snapshot)];
        for width in 1..=200u16 {
            let layout = TableLayout::fit(width);
            let table = ProcessTable::new(ascii(), &layout, &rows);
            let line = table.data_line(&rows[0]);
            assert_eq!(display_width(&line), usize::from(width), "{line:?}");
            if let Some(name) = layout.column(Column::Name) {
                let text = table.cell_text(&rows[0], Column::Name, name.width);
                assert!(
                    display_width(&text) <= usize::from(name.width),
                    "width {width}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn a_name_is_tail_truncated_and_a_command_middle_truncated() {
        // §5.4: the name's head identifies the executable; a command's two ends
        // both carry information.
        let snapshot = process(5, "rustc-driver-longname");
        let rows = [ProcessRow::new(&snapshot)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let name = table.cell_text(&rows[0], Column::Name, 10);
        assert_eq!(name, "rustc-d...");
        let command = table.cell_text(&rows[0], Column::Command, 24);
        assert!(command.starts_with("/usr/bin"), "{command:?}");
        assert!(command.ends_with("verbose"), "{command:?}");
        assert!(command.contains("..."), "{command:?}");
    }

    #[test]
    fn a_tree_prefix_shares_the_name_column_without_overflowing_it() {
        let snapshot = process(6, "bash");
        let rows = [ProcessRow::new(&snapshot).with_prefix("| `- ")];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let name = layout.column(Column::Name).expect("NAME at 140 cells");
        let text = table.cell_text(&rows[0], Column::Name, name.width);
        assert!(text.starts_with("| `- "), "{text:?}");
        assert!(text.contains("bash"), "{text:?}");
        assert!(display_width(&text) <= usize::from(name.width));
        // A prefix wider than the column truncates rather than pushing the name out.
        let deep = [ProcessRow::new(&snapshot).with_prefix("| | | | | | | | | | `- ")];
        let squeezed = table.cell_text(&deep[0], Column::Name, 6);
        assert!(display_width(&squeezed) <= 6, "{squeezed:?}");
    }

    #[test]
    fn an_empty_table_draws_its_header_and_nothing_else() {
        let layout = TableLayout::fit(100);
        let rows: Vec<ProcessRow<'_>> = Vec::new();
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert_eq!(table.visible_rows(24), 0);
        let lines = table.lines(24);
        assert_eq!(lines.len(), 1);
        assert!(lines.first().is_some_and(|line| line.contains("PID")));

        let area = Rect::new(0, 0, 100, 6);
        let mut buffer = Buffer::empty(area);
        table.render(area, &mut buffer);
        for y in 1..6u16 {
            let row: String = (0..100)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                .collect();
            assert_eq!(row.trim(), "", "row {y} was drawn for an empty table");
        }
    }

    #[test]
    fn the_table_never_draws_more_rows_than_its_area_has() {
        let snapshots: Vec<ProcessSnapshot> = (1..=20).map(|pid| process(pid, "p")).collect();
        let rows = rows_from(&snapshots, None);
        let layout = TableLayout::fit(80);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert_eq!(table.visible_rows(6), 5, "one row goes to the header");
        let area = Rect::new(0, 0, 80, 6);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 10));
        table.render(area, &mut buffer);
        for y in 6..10u16 {
            let row: String = (0..80)
                .filter_map(|x| buffer.cell((x, y)).map(|cell| cell.symbol().to_owned()))
                .collect();
            assert_eq!(row.trim(), "", "row {y} escaped the area");
        }
    }

    #[test]
    fn scrolling_skips_rows_without_the_widget_choosing_to() {
        let snapshots: Vec<ProcessSnapshot> = (1..=10).map(|pid| process(pid, "p")).collect();
        let rows = rows_from(&snapshots, None);
        let layout = TableLayout::fit(80);
        let table = ProcessTable::new(ascii(), &layout, &rows).with_scroll(7);
        assert_eq!(table.visible_rows(24), 3);
        let lines = table.lines(24);
        assert_eq!(lines.len(), 4, "header plus three remaining rows");
        assert!(lines.get(1).is_some_and(|line| line.contains('8')));
    }

    #[test]
    fn rows_are_selected_by_identity_so_a_reused_pid_selects_nothing() {
        // §26: a PID alone is not an identity.
        let snapshots = vec![process(31_842, "rustc")];
        let real = snapshots.first().expect("one process").identity;
        assert_eq!(
            rows_from(&snapshots, Some(real))
                .first()
                .map(ProcessRow::is_selected),
            Some(true)
        );
        let recycled = ProcessIdentity::new(31_842, 999_999);
        assert!(recycled.is_reuse_of(&real));
        assert_eq!(
            rows_from(&snapshots, Some(recycled))
                .first()
                .map(ProcessRow::is_selected),
            Some(false)
        );
    }

    #[test]
    fn the_selected_row_is_styled_across_its_whole_width() {
        let snapshot = process(1, "p");
        let rows = [ProcessRow::new(&snapshot).selected(true)];
        let layout = TableLayout::fit(40);
        let presentation = ascii();
        let area = Rect::new(0, 0, 40, 2);
        let mut buffer = Buffer::empty(area);
        ProcessTable::new(presentation, &layout, &rows).render(area, &mut buffer);
        let expected = presentation.selection().into_style();
        for x in 0..40u16 {
            let cell = buffer.cell((x, 1u16)).expect("inside the buffer");
            assert_eq!(Some(cell.bg), expected.bg, "cell {x} lost the selection");
        }
    }

    #[test]
    fn a_zero_area_table_draws_nothing_without_panicking() {
        let snapshot = process(1, "p");
        let rows = [ProcessRow::new(&snapshot).selected(true)];
        for (width, height) in [(0u16, 0u16), (0, 24), (80, 0), (1, 1), (3, 2)] {
            let layout = TableLayout::fit(width);
            let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 24));
            ProcessTable::new(ascii(), &layout, &rows)
                .render(Rect::new(0, 0, width, height), &mut buffer);
        }
        // A zero-width layout has no columns, so there is nothing to draw at all.
        let layout = TableLayout::fit(0);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        assert!(table.header_line().is_empty());
        assert!(table.data_line(&rows[0]).is_empty());
    }

    #[test]
    fn every_column_is_driven_by_the_layout_rather_than_by_the_widget() {
        // The widget must not invent, reorder, or resize a column.
        let snapshot = process(31_842, "rustc");
        let rows = [ProcessRow::new(&snapshot)];
        for width in [20u16, 60, 80, 100, 140, 200] {
            let layout = TableLayout::fit(width);
            let table = ProcessTable::new(ascii(), &layout, &rows);
            let line = table.data_line(&rows[0]);
            for entry in layout.columns() {
                let cell: String = line
                    .chars()
                    .skip(usize::from(entry.x))
                    .take(usize::from(entry.width))
                    .collect();
                assert_eq!(
                    display_width(&cell),
                    usize::from(entry.width),
                    "width {width}, {:?}: {cell:?}",
                    entry.column
                );
            }
        }
    }

    #[test]
    fn numeric_cells_are_right_aligned_and_text_cells_are_not() {
        let snapshot = process(507, "rustc");
        let rows = [ProcessRow::new(&snapshot)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let line = table.data_line(&rows[0]);
        let cell = |column: Column| -> String {
            let entry = layout.column(column).expect("column at 140 cells");
            line.chars()
                .skip(usize::from(entry.x))
                .take(usize::from(entry.width))
                .collect()
        };
        // §5.4: numerics right-aligned, so the digits line up.
        let pid = cell(Column::Pid);
        assert!(pid.starts_with(' ') && pid.ends_with("507"), "{pid:?}");
        let user = cell(Column::User);
        assert!(user.starts_with("gabor"), "{user:?}");
    }

    #[test]
    fn a_process_with_no_command_line_shows_its_name_instead_of_a_blank() {
        let mut snapshot = process(2, "kworker/2:1");
        snapshot.command = "".into();
        let rows = [ProcessRow::new(&snapshot)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let command = layout
            .column(Column::Command)
            .expect("COMMAND at 140 cells");
        let text = table.cell_text(&rows[0], Column::Command, command.width);
        assert_eq!(text, "kworker/2:1");
    }

    #[test]
    fn a_user_whose_name_is_unreadable_shows_a_placeholder_not_a_blank() {
        let mut snapshot = process(3, "p");
        snapshot.user = MetricState::PermissionDenied;
        let rows = [ProcessRow::new(&snapshot)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let user = layout.column(Column::User).expect("USER at 140 cells");
        let text = table.cell_text(&rows[0], Column::User, user.width);
        assert!(!text.trim().is_empty(), "{text:?}");
        assert_eq!(text, "n/a");
        assert_eq!(table.cell_token(&rows[0], Column::User), Token::Watch);
    }

    #[test]
    fn a_user_id_without_a_name_still_identifies_the_owner() {
        let mut snapshot = process(4, "p");
        snapshot.user = MetricState::Available(UserIdentity {
            uid: 70,
            name: None,
        });
        let rows = [ProcessRow::new(&snapshot)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let text = table.cell_text(&rows[0], Column::User, 8);
        assert!(text.contains("70"), "{text:?}");
    }

    #[test]
    fn everything_the_table_itself_draws_is_printable_seven_bit() {
        // §5.1 constrains the characters the *design system* emits — the marker,
        // the headers, the placeholders, the truncation marker. A process name and
        // command line are data, and are covered by the next test.
        let mut snapshot = process(5, "rustc");
        snapshot.io.read = MetricState::PermissionDenied;
        snapshot.threads = MetricState::WarmingUp;
        let rows = [ProcessRow::new(&snapshot).selected(true)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        for line in table.lines(4) {
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
        // Rewriting a process's real name would be a worse lie than the occasional
        // wide glyph, so strict mode bounds its width without transliterating it.
        let name = "\u{65e5}\u{672c}\u{8a9e}";
        let snapshot = process(5, name);
        let rows = [ProcessRow::new(&snapshot)];
        let layout = TableLayout::fit(140);
        let table = ProcessTable::new(ascii(), &layout, &rows);
        let line = table.data_line(&rows[0]);
        assert!(line.contains(name), "{line:?}");
        assert_eq!(display_width(&line), 140);
    }

    #[test]
    fn no_numeric_row_carries_more_than_one_accent_colour() {
        // §5.2. A notable row is the interesting case: it is entirely `critical`,
        // which is one colour rather than several.
        let presentation = ascii();
        let layout = TableLayout::fit(140);
        let mut zombie = process(1, "p");
        zombie.state = ProcessState::Zombie;
        let mut mixed = process(2, "q");
        mixed.cpu = MetricState::PermissionDenied;
        mixed.io.read = MetricState::PermissionDenied;
        for snapshot in [zombie, mixed] {
            let rows = [ProcessRow::new(&snapshot)];
            let table = ProcessTable::new(presentation, &layout, &rows);
            let tokens: Vec<Token> = layout
                .columns()
                .iter()
                .map(|entry| table.cell_token(&rows[0], entry.column))
                .collect();
            let accents = presentation
                .theme()
                .accent_count(presentation.depth(), tokens.clone());
            assert!(accents <= 1, "{tokens:?} carries {accents} accents");
        }
    }
}

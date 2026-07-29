//! Responsive layout, breakpoints, and the process-table column engine
//! (§5.4, §5.7, §7.2).
//!
//! Two properties are structural rather than reviewed:
//!
//! * **No function panics on a zero-area rectangle.** Every split is written
//!   with saturating arithmetic and every returned rectangle is inside its
//!   parent. §5.7 makes this a hard requirement and M2 an acceptance criterion,
//!   so it is pinned by a property test over every terminal size up to 400×400.
//! * **Column widths come from panel geometry, never from the current values.**
//!   §5.4 forbids a value crossing a unit boundary from reflowing the table, so
//!   [`Column::reserved_width`] is a constant derived from
//!   `MAX_COMPACT_BYTES_WIDTH` and `MAX_BYTE_RATE_WIDTH`, and
//!   [`TableLayout::fit`] takes a width and nothing else.
//!
//! # Breakpoint precedence
//!
//! §5.7's four bands overlap, and as literally written some sizes match none of
//! them: 150×30 is too short for `Wide`, too wide for `Standard`'s stated
//! `100–139`, and outside `Compact`'s `80–99` / `20–27`. The rule implemented
//! here resolves the gaps without contradicting any size the specification does
//! pin down:
//!
//! 1. `width < 80 || height < 20` is `TooSmall`. This is checked *first* and
//!    absolutely: `Compact`'s `or` would otherwise claim a 300×10 terminal.
//! 2. Otherwise `width >= 140 && height >= 38` is `Wide`.
//! 3. Otherwise `width >= 100 && height >= 28` is `Standard`. The stated upper
//!    bound of 139 is treated as a consequence of rule 2 rather than a rule of
//!    its own, so 150×30 — which has all the room `Standard` needs — is
//!    `Standard` rather than nothing.
//! 4. Everything else is `Compact`, which is exactly `width 80..=99` at any
//!    usable height plus `height 20..=27` at any usable width.

use monitrs_core::units::{MAX_BYTE_RATE_WIDTH, MAX_COMPACT_BYTES_WIDTH};
use ratatui::layout::Rect;

/// The narrowest terminal that still gets a process list (§5.7).
pub const MINIMUM_WIDTH: u16 = 60;

/// The shortest terminal that still gets a process list (§5.7).
pub const MINIMUM_HEIGHT: u16 = 16;

/// Below this width the layout drops out of `Compact` (§5.7).
pub const COMPACT_MIN_WIDTH: u16 = 80;

/// Below this height the layout drops out of `Compact` (§5.7).
pub const COMPACT_MIN_HEIGHT: u16 = 20;

/// The narrowest `Standard` terminal (§5.7).
pub const STANDARD_MIN_WIDTH: u16 = 100;

/// The shortest `Standard` terminal (§5.7).
pub const STANDARD_MIN_HEIGHT: u16 = 28;

/// The narrowest `Wide` terminal (§5.7).
pub const WIDE_MIN_WIDTH: u16 = 140;

/// The shortest `Wide` terminal (§5.7).
pub const WIDE_MIN_HEIGHT: u16 = 38;

/// Which of §5.7's four layout bands applies.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Breakpoint {
    /// Too small for the dashboard: a minimal list, or a resize notice.
    TooSmall,
    /// Process table as the primary view, condensed header, hidden columns.
    Compact,
    /// Header meters, compact history, process table, one summary panel.
    Standard,
    /// The full dashboard: pressure and history side by side, pins and network.
    Wide,
}

impl Breakpoint {
    /// Resolves the band for a terminal size, using the precedence documented at
    /// the top of this module.
    #[must_use]
    pub const fn resolve(width: u16, height: u16) -> Self {
        // Rule 1: the hard gate, before any `or`-shaped rule can claim the size.
        if width < COMPACT_MIN_WIDTH || height < COMPACT_MIN_HEIGHT {
            return Self::TooSmall;
        }
        if width >= WIDE_MIN_WIDTH && height >= WIDE_MIN_HEIGHT {
            return Self::Wide;
        }
        if width >= STANDARD_MIN_WIDTH && height >= STANDARD_MIN_HEIGHT {
            return Self::Standard;
        }
        Self::Compact
    }

    /// Resolves the band for a rectangle.
    #[must_use]
    pub const fn of(area: Rect) -> Self {
        Self::resolve(area.width, area.height)
    }

    /// The name used in diagnostics and snapshot tests.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::TooSmall => "too-small",
            Self::Compact => "compact",
            Self::Standard => "standard",
            Self::Wide => "wide",
        }
    }
}

/// Rows reserved for the header meters in `Wide` and `Standard`.
const HEADER_ROWS: u16 = 3;

/// Rows reserved for the side-by-side pressure and history block in `Wide`.
const WIDE_MIDDLE_ROWS: u16 = 6;

/// Rows reserved for the pins and network footer panels in `Wide`.
const WIDE_FOOTER_ROWS: u16 = 4;

/// Rows reserved for the compact history strip in `Standard`.
const STANDARD_HISTORY_ROWS: u16 = 5;

/// Rows reserved for the focus-selected summary panel in `Standard`.
const STANDARD_SUMMARY_ROWS: u16 = 4;

/// Rows for the tab and status footer.
const STATUS_ROWS: u16 = 1;

/// The widest the pressure radar ever needs; §5.5's radar rows fit in 34 cells.
const PRESSURE_WIDTH: u16 = 34;

/// The panel rectangles for one frame.
///
/// A panel that this breakpoint does not show, or that received no space, is
/// `None` rather than a zero-area `Rect`: an empty rectangle is legal to render
/// but always a mistake to *choose*, and `None` makes the distinction impossible
/// to miss at the call site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Layout {
    /// Which band produced this geometry.
    pub breakpoint: Breakpoint,
    /// The full area the layout was resolved against.
    pub area: Rect,
    /// Title line plus the CPU and memory meters (§5.5).
    pub header: Option<Rect>,
    /// The Pressure Radar (§2.3).
    pub pressure: Option<Rect>,
    /// The history sparklines (§2.1).
    pub history: Option<Rect>,
    /// One lower summary panel, chosen by focus in `Standard`; the one-line
    /// CPU/memory/load strip in `Compact`.
    pub summary: Option<Rect>,
    /// The process table, which is the primary view at every band.
    pub processes: Option<Rect>,
    /// Pinned processes (§2.5).
    pub pins: Option<Rect>,
    /// The per-interface network footer (§7.4).
    pub network: Option<Rect>,
    /// Tabs, filter hint, and help hint.
    pub status: Option<Rect>,
    /// Where to draw the §5.7 resize notice, when the terminal is unusable.
    pub notice: Option<Rect>,
}

impl Layout {
    /// Resolves the geometry for `area`.
    ///
    /// The area is normalized through [`Rect::new`] first so that `x + width` and
    /// `y + height` are guaranteed to fit in a `u16`; every split below is then
    /// plain saturating arithmetic and cannot overflow or escape the parent.
    #[must_use]
    pub fn resolve(area: Rect) -> Self {
        let area = Rect::new(area.x, area.y, area.width, area.height);
        let breakpoint = Breakpoint::of(area);
        let mut layout = Self::vacant(breakpoint, area);
        if area.is_empty() {
            return layout;
        }
        match breakpoint {
            Breakpoint::Wide => layout.fill_wide(),
            Breakpoint::Standard => layout.fill_standard(),
            Breakpoint::Compact => layout.fill_compact(),
            Breakpoint::TooSmall => layout.fill_too_small(),
        }
        layout
    }

    const fn vacant(breakpoint: Breakpoint, area: Rect) -> Self {
        Self {
            breakpoint,
            area,
            header: None,
            pressure: None,
            history: None,
            summary: None,
            processes: None,
            pins: None,
            network: None,
            status: None,
            notice: None,
        }
    }

    fn fill_wide(&mut self) {
        let (header, rest) = take_top(self.area, HEADER_ROWS);
        let (middle, rest) = take_top(rest, WIDE_MIDDLE_ROWS);
        let (status, rest) = take_bottom(rest, STATUS_ROWS);
        let (footer, processes) = take_bottom(rest, WIDE_FOOTER_ROWS);

        // The radar has a fixed information width; the history takes the rest so
        // it gains resolution on wider terminals.
        let (pressure, history) = take_left(middle, PRESSURE_WIDTH.min(middle.width / 2));
        let (pins, network) = take_left(footer, footer.width / 2);

        self.header = non_empty(header);
        self.pressure = non_empty(pressure);
        self.history = non_empty(history);
        self.processes = non_empty(processes);
        self.pins = non_empty(pins);
        self.network = non_empty(network);
        self.status = non_empty(status);
    }

    fn fill_standard(&mut self) {
        let (header, rest) = take_top(self.area, HEADER_ROWS);
        let (history, rest) = take_top(rest, STANDARD_HISTORY_ROWS);
        let (status, rest) = take_bottom(rest, STATUS_ROWS);
        let (summary, processes) = take_bottom(rest, STANDARD_SUMMARY_ROWS);

        self.header = non_empty(header);
        self.history = non_empty(history);
        self.summary = non_empty(summary);
        self.processes = non_empty(processes);
        self.status = non_empty(status);
    }

    fn fill_compact(&mut self) {
        // A condensed one-line header, a one-line CPU/memory/load strip, and the
        // process table as the primary view (§5.7).
        let (header, rest) = take_top(self.area, 1);
        let (summary, rest) = take_top(rest, 1);
        let (status, processes) = take_bottom(rest, STATUS_ROWS);

        self.header = non_empty(header);
        self.summary = non_empty(summary);
        self.processes = non_empty(processes);
        self.status = non_empty(status);
    }

    fn fill_too_small(&mut self) {
        if self.area.width >= MINIMUM_WIDTH && self.area.height >= MINIMUM_HEIGHT {
            // §5.7: a *stable* minimal process list. Stability is why there is no
            // summary or footer here — nothing that could appear and disappear as
            // the terminal is dragged through this band.
            let (header, processes) = take_top(self.area, 1);
            self.header = non_empty(header);
            self.processes = non_empty(processes);
        } else {
            self.notice = non_empty(self.area);
        }
    }

    /// Whether the terminal is large enough to show any process data at all.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.processes.is_some()
    }

    /// Whether the §5.7 resize notice should be drawn instead of the interface.
    #[must_use]
    pub const fn shows_notice(&self) -> bool {
        self.notice.is_some()
    }

    /// Every panel with its name, for diagnostics and for the containment test.
    #[must_use]
    pub const fn named_panels(&self) -> [(&'static str, Option<Rect>); 9] {
        [
            ("header", self.header),
            ("pressure", self.pressure),
            ("history", self.history),
            ("summary", self.summary),
            ("processes", self.processes),
            ("pins", self.pins),
            ("network", self.network),
            ("status", self.status),
            ("notice", self.notice),
        ]
    }
}

/// The three lines §5.7 specifies for a terminal below 60×16.
///
/// The text is verbatim from the specification, including the lower-case `x`
/// between the dimensions, because it is a user-visible string that the snapshot
/// tests pin.
#[must_use]
pub fn unusable_notice(width: u16, height: u16) -> [String; 3] {
    [
        format!("monitrs needs at least {MINIMUM_WIDTH}x{MINIMUM_HEIGHT}"),
        format!("current terminal: {width}x{height}"),
        "resize or press q to quit".to_owned(),
    ]
}

/// Takes `height` rows from the top, returning `(taken, remainder)`.
///
/// Both rectangles are inside `area`, and asking for more rows than exist yields
/// all of them plus an empty remainder rather than panicking.
fn take_top(area: Rect, height: u16) -> (Rect, Rect) {
    let taken = height.min(area.height);
    let top = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: taken,
    };
    let rest = Rect {
        x: area.x,
        y: area.y.saturating_add(taken),
        width: area.width,
        height: area.height - taken,
    };
    (top, rest)
}

/// Takes `height` rows from the bottom, returning `(taken, remainder)`.
fn take_bottom(area: Rect, height: u16) -> (Rect, Rect) {
    let taken = height.min(area.height);
    let kept = area.height - taken;
    let bottom = Rect {
        x: area.x,
        y: area.y.saturating_add(kept),
        width: area.width,
        height: taken,
    };
    let rest = Rect {
        x: area.x,
        y: area.y,
        width: area.width,
        height: kept,
    };
    (bottom, rest)
}

/// Takes `width` columns from the left, returning `(taken, remainder)`.
fn take_left(area: Rect, width: u16) -> (Rect, Rect) {
    let taken = width.min(area.width);
    let left = Rect {
        x: area.x,
        y: area.y,
        width: taken,
        height: area.height,
    };
    let rest = Rect {
        x: area.x.saturating_add(taken),
        y: area.y,
        width: area.width - taken,
        height: area.height,
    };
    (left, rest)
}

/// Discards a rectangle that has no area, so callers cannot mistake "no room"
/// for "a panel to draw".
const fn non_empty(area: Rect) -> Option<Rect> {
    if area.is_empty() { None } else { Some(area) }
}

/// Horizontal alignment of a process-table column.
///
/// §5.4 requires every numeric column to be right-aligned so digits line up.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Align {
    /// Text columns.
    Left,
    /// Numeric columns.
    Right,
}

/// One process-table column (§7.2).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Column {
    /// The selected-row marker.
    Selection,
    /// Process id.
    Pid,
    /// Owning user.
    User,
    /// One-character process state code.
    State,
    /// CPU percentage, which may exceed 100% (§8.3).
    CpuPercent,
    /// Resident memory as a share of the machine.
    MemoryPercent,
    /// Resident set size.
    Rss,
    /// Virtual memory size.
    VirtualMemory,
    /// Read throughput.
    ReadRate,
    /// Write throughput.
    WriteRate,
    /// Thread count.
    Threads,
    /// Time since the process started.
    Age,
    /// Executable or process name.
    Name,
    /// Full command line with arguments.
    Command,
}

impl Column {
    /// The number of columns.
    pub const COUNT: usize = 14;

    /// Left-to-right display order, and the canonical list of every column.
    ///
    /// The identity columns come last so the variable-width text does not push
    /// the numeric columns around as it grows (§5.4).
    pub const DISPLAY_ORDER: [Self; Self::COUNT] = [
        Self::Selection,
        Self::Pid,
        Self::User,
        Self::State,
        Self::CpuPercent,
        Self::MemoryPercent,
        Self::Rss,
        Self::VirtualMemory,
        Self::ReadRate,
        Self::WriteRate,
        Self::Threads,
        Self::Age,
        Self::Name,
        Self::Command,
    ];

    /// The order columns are admitted as width becomes available.
    ///
    /// This is §7.2's priority list, highest first, with two deliberate
    /// decisions:
    ///
    /// * `Name` is admitted before `Selection`, so a terminal too narrow for
    ///   both still identifies its processes. `Selection` follows immediately,
    ///   so in practice both are present from 10 cells upwards.
    /// * `Command` is admitted last. §7.2 lists "process name/command" as a
    ///   single priority-1 field; the full command line is the widest possible
    ///   expansion of that field, so it is taken only once every other column
    ///   has been satisfied. Otherwise it would consume the width that PID and
    ///   CPU% need.
    pub const ADMISSION_ORDER: [Self; Self::COUNT] = [
        Self::Name,
        Self::Selection,
        Self::Pid,
        Self::CpuPercent,
        Self::Rss,
        Self::MemoryPercent,
        Self::User,
        Self::State,
        Self::ReadRate,
        Self::WriteRate,
        Self::Age,
        Self::Threads,
        Self::VirtualMemory,
        Self::Command,
    ];

    /// A stable position, used only to prove the two orderings are complete.
    /// Test-only, because it exists for that proof and for nothing else.
    #[cfg(test)]
    const fn index(self) -> usize {
        match self {
            Self::Selection => 0,
            Self::Pid => 1,
            Self::User => 2,
            Self::State => 3,
            Self::CpuPercent => 4,
            Self::MemoryPercent => 5,
            Self::Rss => 6,
            Self::VirtualMemory => 7,
            Self::ReadRate => 8,
            Self::WriteRate => 9,
            Self::Threads => 10,
            Self::Age => 11,
            Self::Name => 12,
            Self::Command => 13,
        }
    }

    /// The column header, which always fits [`Column::reserved_width`].
    #[must_use]
    pub const fn header(self) -> &'static str {
        match self {
            Self::Selection => " ",
            Self::Pid => "PID",
            Self::User => "USER",
            Self::State => "S",
            Self::CpuPercent => "CPU%",
            Self::MemoryPercent => "MEM%",
            Self::Rss => "RSS",
            Self::VirtualMemory => "VIRT",
            Self::ReadRate => "READ/s",
            Self::WriteRate => "WRITE/s",
            Self::Threads => "THR",
            Self::Age => "AGE",
            Self::Name => "NAME",
            Self::Command => "COMMAND",
        }
    }

    /// The column's place in §7.2's priority list, `1` being highest.
    ///
    /// The numbers are the specification's own, kept here so the mapping is
    /// checkable rather than implied by the order of a literal. `Selection` is
    /// `0`: §7.2 lists the selection indicator as a required column, and without
    /// it keyboard navigation has no visible anchor, so it outranks everything
    /// that can be read from a value.
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Selection => 0,
            // §7.2 priority 1 is the single "process name/command" field; both
            // columns render part of it.
            Self::Name | Self::Command => 1,
            Self::Pid => 2,
            Self::CpuPercent => 3,
            // §7.2 priority 4 is "RSS or memory%".
            Self::Rss | Self::MemoryPercent => 4,
            Self::User => 5,
            Self::State => 6,
            // §7.2 priority 7 is "read/write rate".
            Self::ReadRate | Self::WriteRate => 7,
            Self::Age => 8,
            Self::Threads => 9,
            Self::VirtualMemory => 10,
        }
    }

    /// The cells this column always occupies, or its minimum when flexible.
    ///
    /// Every value is a constant, never a function of the data on screen: §5.4
    /// requires reserving from panel geometry so that `1023B -> 1.0KiB` cannot
    /// shift the columns to its right.
    #[must_use]
    pub const fn reserved_width(self) -> u16 {
        match self {
            // The marker is one cell in both glyph modes.
            Self::Selection => 1,
            // Linux `pid_max` reaches 4194304, which is seven digits.
            Self::Pid => 7,
            Self::User => 8,
            // `ProcessState::code` is a single character.
            Self::State => 1,
            // A 100-core machine can legitimately show `12800%`.
            Self::CpuPercent => 6,
            // Bounded by 100, so `100%` and `9.9%` both fit in five with a sign
            // of headroom.
            Self::MemoryPercent => 5,
            Self::Rss | Self::VirtualMemory => MAX_COMPACT_BYTES_WIDTH,
            Self::ReadRate | Self::WriteRate => MAX_BYTE_RATE_WIDTH,
            // Four digits covers any realistic thread count.
            Self::Threads => 4,
            // `format_age`'s widest form is `03:12:44`.
            Self::Age => 8,
            Self::Name => NAME_MIN_WIDTH,
            Self::Command => COMMAND_MIN_WIDTH,
        }
    }

    /// Whether the column absorbs leftover width.
    ///
    /// Only the two identity columns do. Everything else is reserved, which is
    /// what keeps the numeric grid stable (§5.4).
    #[must_use]
    pub const fn is_flexible(self) -> bool {
        matches!(self, Self::Name | Self::Command)
    }

    /// The column's alignment.
    #[must_use]
    pub const fn align(self) -> Align {
        match self {
            Self::Pid
            | Self::CpuPercent
            | Self::MemoryPercent
            | Self::Rss
            | Self::VirtualMemory
            | Self::ReadRate
            | Self::WriteRate
            | Self::Threads
            | Self::Age => Align::Right,
            Self::Selection | Self::User | Self::State | Self::Name | Self::Command => Align::Left,
        }
    }
}

/// One cell of space between adjacent columns.
pub const COLUMN_GAP: u16 = 1;

/// The narrowest useful name column.
const NAME_MIN_WIDTH: u16 = 8;

/// The narrowest command column worth showing at all: below this, middle
/// truncation leaves nothing but the ellipsis and a few characters.
const COMMAND_MIN_WIDTH: u16 = 12;

/// The share of a wide panel the name column aims for before other columns
/// compete for the rest. A quarter keeps process names readable on wide
/// terminals without starving the numeric grid.
const NAME_TARGET_DIVISOR: u16 = 4;

/// The numerator of the name column's share of leftover width when the full
/// command is also shown; the command takes the remainder because it holds the
/// arguments that middle truncation is trying to preserve (§5.4).
const NAME_SHARE_NUMERATOR: u32 = 2;

/// The denominator of that share.
const NAME_SHARE_DENOMINATOR: u32 = 5;

/// One column's resolved position and width.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ColumnLayout {
    /// Which column.
    pub column: Column,
    /// Offset from the left edge of the table.
    pub x: u16,
    /// Cells available to this column, including padding.
    pub width: u16,
    /// How to align content within `width`.
    pub align: Align,
}

/// The columns that fit a given table width, in display order.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TableLayout {
    width: u16,
    columns: Vec<ColumnLayout>,
}

impl TableLayout {
    /// Chooses and sizes the columns for a table `width` cells wide.
    ///
    /// The result depends on nothing but `width`, which is what makes it immune
    /// to the reflow §5.4 forbids. Columns are dropped strictly in reverse
    /// priority order: once one does not fit, no lower-priority column is
    /// admitted either, so widening the terminal only ever *adds* columns and
    /// never rearranges them.
    #[must_use]
    pub fn fit(width: u16) -> Self {
        if width == 0 {
            return Self {
                width,
                columns: Vec::new(),
            };
        }

        // The identity column's target, held back from the greedy pass so that a
        // wide terminal does not spend all its space on low-priority numbers.
        let name_target = (width / NAME_TARGET_DIVISOR).max(NAME_MIN_WIDTH).min(width);
        let mut spend = name_target;
        let mut chosen = vec![Column::Name];

        for column in Column::ADMISSION_ORDER {
            if column == Column::Name {
                continue;
            }
            let need = COLUMN_GAP.saturating_add(column.reserved_width());
            if spend.saturating_add(need) > width {
                break;
            }
            spend += need;
            chosen.push(column);
        }

        // Gaps sit between columns, so there is one fewer gap than column.
        let gaps = u16::try_from(chosen.len().saturating_sub(1)).unwrap_or(u16::MAX);
        let fixed: u16 = chosen
            .iter()
            .filter(|column| !column.is_flexible())
            .map(|column| column.reserved_width())
            .sum();
        let flex_budget = width.saturating_sub(fixed.saturating_add(gaps));

        let shows_command = chosen.contains(&Column::Command);
        let (name_width, command_width) = if shows_command {
            // The admission pass only accepts `Command` when the name minimum and
            // the command minimum both still fit, so this range is non-empty.
            let share = u32::from(flex_budget) * NAME_SHARE_NUMERATOR / NAME_SHARE_DENOMINATOR;
            let upper = flex_budget.saturating_sub(COMMAND_MIN_WIDTH);
            let name = u16::try_from(share)
                .unwrap_or(u16::MAX)
                .clamp(NAME_MIN_WIDTH.min(upper), upper);
            (name, flex_budget.saturating_sub(name))
        } else {
            (flex_budget, 0)
        };

        let mut columns = Vec::with_capacity(chosen.len());
        let mut x = 0u16;
        for column in Column::DISPLAY_ORDER {
            if !chosen.contains(&column) {
                continue;
            }
            let column_width = match column {
                Column::Name => name_width,
                Column::Command => command_width,
                other => other.reserved_width(),
            };
            if !columns.is_empty() {
                x = x.saturating_add(COLUMN_GAP);
            }
            columns.push(ColumnLayout {
                column,
                x,
                width: column_width,
                align: column.align(),
            });
            x = x.saturating_add(column_width);
        }

        Self { width, columns }
    }

    /// Chooses and sizes the columns for a panel rectangle.
    #[must_use]
    pub fn for_area(area: Rect) -> Self {
        Self::fit(area.width)
    }

    /// The width the layout was fitted to.
    #[must_use]
    pub const fn width(&self) -> u16 {
        self.width
    }

    /// The chosen columns, left to right.
    #[must_use]
    pub fn columns(&self) -> &[ColumnLayout] {
        &self.columns
    }

    /// Whether `column` is shown at this width.
    #[must_use]
    pub fn contains(&self, column: Column) -> bool {
        self.columns.iter().any(|entry| entry.column == column)
    }

    /// The layout of one column, if it is shown.
    #[must_use]
    pub fn column(&self, column: Column) -> Option<&ColumnLayout> {
        self.columns.iter().find(|entry| entry.column == column)
    }

    /// Cells occupied by columns and the gaps between them.
    ///
    /// Never exceeds [`TableLayout::width`], and equals it whenever at least one
    /// column is shown, because the identity column absorbs the remainder.
    #[must_use]
    pub fn total_width(&self) -> u16 {
        let gaps = u16::try_from(self.columns.len().saturating_sub(1)).unwrap_or(u16::MAX);
        self.columns
            .iter()
            .map(|entry| entry.width)
            .fold(0u16, u16::saturating_add)
            .saturating_add(gaps)
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn contains_rect(parent: Rect, child: Rect) -> bool {
        child.x >= parent.x
            && child.y >= parent.y
            && u32::from(child.x) + u32::from(child.width)
                <= u32::from(parent.x) + u32::from(parent.width)
            && u32::from(child.y) + u32::from(child.height)
                <= u32::from(parent.y) + u32::from(parent.height)
    }

    #[test]
    fn breakpoints_match_every_threshold_the_specification_states() {
        let cases = [
            // Wide: width >= 140 and height >= 38.
            (140, 38, Breakpoint::Wide),
            (400, 400, Breakpoint::Wide),
            (200, 60, Breakpoint::Wide),
            // One short of wide in either dimension.
            (139, 38, Breakpoint::Standard),
            (140, 37, Breakpoint::Standard),
            (139, 37, Breakpoint::Standard),
            // Standard: width 100-139 and height >= 28.
            (100, 28, Breakpoint::Standard),
            (120, 30, Breakpoint::Standard),
            // Compact: width 80-99, or height 20-27.
            (99, 27, Breakpoint::Compact),
            (80, 20, Breakpoint::Compact),
            (80, 24, Breakpoint::Compact),
            (99, 400, Breakpoint::Compact),
            (400, 27, Breakpoint::Compact),
            (100, 27, Breakpoint::Compact),
            (99, 28, Breakpoint::Compact),
            // Too small: width < 80 or height < 20.
            (79, 19, Breakpoint::TooSmall),
            (79, 400, Breakpoint::TooSmall),
            (400, 19, Breakpoint::TooSmall),
            (0, 0, Breakpoint::TooSmall),
            (1, 1, Breakpoint::TooSmall),
            (59, 15, Breakpoint::TooSmall),
            (60, 16, Breakpoint::TooSmall),
        ];
        for (width, height, expected) in cases {
            assert_eq!(
                Breakpoint::resolve(width, height),
                expected,
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn the_ambiguous_overlaps_resolve_as_documented() {
        // §5.7's bands overlap; these are the sizes that match none of them as
        // literally written, and the module documents how they are decided.

        // Taller and wider than Standard's stated range but too short for Wide:
        // it has all the room Standard needs, so it is Standard.
        assert_eq!(Breakpoint::resolve(150, 30), Breakpoint::Standard);
        assert_eq!(Breakpoint::resolve(400, 28), Breakpoint::Standard);
        // Wide enough for Wide but only Compact-tall: the height decides.
        assert_eq!(Breakpoint::resolve(200, 25), Breakpoint::Compact);
        // Compact's `or` must not rescue a terminal that is below the hard gate.
        assert_eq!(Breakpoint::resolve(300, 10), Breakpoint::TooSmall);
        assert_eq!(Breakpoint::resolve(90, 400), Breakpoint::Compact);
        assert_eq!(Breakpoint::resolve(50, 25), Breakpoint::TooSmall);
    }

    #[test]
    fn breakpoints_change_only_at_the_stated_boundaries() {
        // Walk every boundary value and its neighbour in both dimensions.
        for (width, height, expected) in [
            (COMPACT_MIN_WIDTH - 1, 40, Breakpoint::TooSmall),
            (COMPACT_MIN_WIDTH, 40, Breakpoint::Compact),
            (140, COMPACT_MIN_HEIGHT - 1, Breakpoint::TooSmall),
            (140, COMPACT_MIN_HEIGHT, Breakpoint::Compact),
            (STANDARD_MIN_WIDTH - 1, 30, Breakpoint::Compact),
            (STANDARD_MIN_WIDTH, 30, Breakpoint::Standard),
            (120, STANDARD_MIN_HEIGHT - 1, Breakpoint::Compact),
            (120, STANDARD_MIN_HEIGHT, Breakpoint::Standard),
            (WIDE_MIN_WIDTH - 1, 40, Breakpoint::Standard),
            (WIDE_MIN_WIDTH, 40, Breakpoint::Wide),
            (160, WIDE_MIN_HEIGHT - 1, Breakpoint::Standard),
            (160, WIDE_MIN_HEIGHT, Breakpoint::Wide),
        ] {
            assert_eq!(
                Breakpoint::resolve(width, height),
                expected,
                "{width}x{height}"
            );
        }
    }

    #[test]
    fn the_wide_layout_shows_every_panel_the_specification_lists() {
        let layout = Layout::resolve(Rect::new(0, 0, 140, 38));
        assert_eq!(layout.breakpoint, Breakpoint::Wide);
        assert!(layout.header.is_some());
        assert!(
            layout.pressure.is_some(),
            "pressure and history side by side"
        );
        assert!(layout.history.is_some());
        assert!(layout.processes.is_some());
        assert!(layout.pins.is_some(), "pins footer panel");
        assert!(layout.network.is_some(), "network footer panel");
        assert!(layout.status.is_some());
        assert!(layout.notice.is_none());
        // Side by side means the same rows and adjacent columns.
        let pressure = layout.pressure.expect("pressure");
        let history = layout.history.expect("history");
        assert_eq!(pressure.y, history.y);
        assert_eq!(pressure.height, history.height);
        assert_eq!(pressure.x + pressure.width, history.x);
        // The two footer panels likewise.
        let pins = layout.pins.expect("pins");
        let network = layout.network.expect("network");
        assert_eq!(pins.y, network.y);
        assert_eq!(pins.x + pins.width, network.x);
    }

    #[test]
    fn the_standard_layout_drops_the_side_by_side_panels_for_one_summary() {
        let layout = Layout::resolve(Rect::new(0, 0, 100, 28));
        assert_eq!(layout.breakpoint, Breakpoint::Standard);
        assert!(layout.header.is_some());
        assert!(layout.history.is_some(), "compact history");
        assert!(layout.summary.is_some(), "one focus-selected summary panel");
        assert!(layout.processes.is_some());
        assert!(layout.status.is_some());
        assert!(layout.pressure.is_none());
        assert!(layout.pins.is_none());
        assert!(layout.network.is_none());
    }

    #[test]
    fn the_compact_layout_is_the_process_table_plus_one_summary_line() {
        let layout = Layout::resolve(Rect::new(0, 0, 80, 24));
        assert_eq!(layout.breakpoint, Breakpoint::Compact);
        assert_eq!(layout.header.expect("header").height, 1, "condensed header");
        assert_eq!(
            layout.summary.expect("summary").height,
            1,
            "one-line CPU/memory/load summary"
        );
        assert_eq!(layout.status.expect("status").height, STATUS_ROWS);
        // The table gets everything else, which is what makes it the primary view.
        assert_eq!(layout.processes.expect("processes").height, 24 - 3);
        assert!(layout.history.is_none());
        assert!(layout.pressure.is_none());
    }

    #[test]
    fn a_too_small_terminal_still_lists_processes_down_to_sixty_by_sixteen() {
        let layout = Layout::resolve(Rect::new(0, 0, 60, 16));
        assert_eq!(layout.breakpoint, Breakpoint::TooSmall);
        assert!(layout.is_usable(), "60x16 must still show a process list");
        assert!(!layout.shows_notice());
        assert_eq!(layout.processes.expect("processes").height, 15);
        // Nothing that could appear and disappear while resizing through here.
        assert!(layout.summary.is_none());
        assert!(layout.status.is_none());
    }

    #[test]
    fn below_sixty_by_sixteen_only_the_resize_notice_is_laid_out() {
        for (width, height) in [(59, 16), (60, 15), (52, 12), (1, 1)] {
            let layout = Layout::resolve(Rect::new(0, 0, width, height));
            assert!(layout.shows_notice(), "{width}x{height}");
            assert!(!layout.is_usable(), "{width}x{height}");
            assert_eq!(layout.notice, Some(Rect::new(0, 0, width, height)));
        }
    }

    #[test]
    fn the_resize_notice_is_verbatim_from_the_specification() {
        assert_eq!(
            unusable_notice(52, 12),
            [
                "monitrs needs at least 60x16".to_owned(),
                "current terminal: 52x12".to_owned(),
                "resize or press q to quit".to_owned(),
            ]
        );
        // Zero is a real terminal size during a resize storm.
        assert_eq!(unusable_notice(0, 0)[1], "current terminal: 0x0");
    }

    #[test]
    fn a_zero_area_terminal_yields_no_panels_and_does_not_panic() {
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 200, 0),
            Rect::new(0, 0, 0, 60),
            Rect::new(5, 7, 0, 0),
        ] {
            let layout = Layout::resolve(area);
            assert_eq!(layout.breakpoint, Breakpoint::TooSmall);
            for (name, rect) in layout.named_panels() {
                assert!(rect.is_none(), "{name} was laid out into an empty area");
            }
        }
    }

    #[test]
    fn every_panel_stays_inside_the_parent_at_the_named_sizes() {
        let sizes = [
            (0u16, 0u16),
            (1, 1),
            (59, 15),
            (60, 16),
            (79, 19),
            (80, 20),
            (80, 24),
            (99, 27),
            (100, 28),
            (139, 37),
            (140, 38),
            (400, 400),
        ];
        for (width, height) in sizes {
            let area = Rect::new(0, 0, width, height);
            let layout = Layout::resolve(area);
            for (name, rect) in layout.named_panels() {
                let Some(rect) = rect else { continue };
                assert!(
                    contains_rect(area, rect),
                    "{name} {rect:?} escaped {area:?} at {width}x{height}"
                );
                assert!(!rect.is_empty(), "{name} is present but empty");
            }
        }
    }

    #[test]
    fn panels_never_overlap_within_a_band() {
        for (width, height) in [(140u16, 38u16), (200, 60), (100, 28), (80, 24), (60, 16)] {
            let layout = Layout::resolve(Rect::new(0, 0, width, height));
            let panels: Vec<(&str, Rect)> = layout
                .named_panels()
                .into_iter()
                .filter_map(|(name, rect)| rect.map(|rect| (name, rect)))
                .collect();
            for (index, (name, rect)) in panels.iter().enumerate() {
                for (other_name, other) in panels.iter().skip(index + 1) {
                    assert!(
                        !rect.intersects(*other),
                        "{name} {rect:?} overlaps {other_name} {other:?} at {width}x{height}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_layout_is_resolved_relative_to_a_non_zero_origin() {
        // Ratatui gives the root frame area, but a nested render may not start at
        // the origin; the panels must follow the offset.
        let area = Rect::new(10, 5, 140, 38);
        let layout = Layout::resolve(area);
        for (name, rect) in layout.named_panels() {
            let Some(rect) = rect else { continue };
            assert!(
                contains_rect(area, rect),
                "{name} {rect:?} escaped {area:?}"
            );
        }
        assert_eq!(layout.header.expect("header").x, 10);
        assert_eq!(layout.header.expect("header").y, 5);
    }

    #[test]
    fn named_panels_reports_every_field() {
        // Guards against a new panel being added to `Layout` and forgotten here,
        // which would silently exclude it from the containment tests.
        let rect = Rect::new(0, 0, 1, 1);
        let layout = Layout {
            breakpoint: Breakpoint::Wide,
            area: rect,
            header: Some(rect),
            pressure: Some(rect),
            history: Some(rect),
            summary: Some(rect),
            processes: Some(rect),
            pins: Some(rect),
            network: Some(rect),
            status: Some(rect),
            notice: Some(rect),
        };
        assert_eq!(layout.named_panels().len(), 9);
        assert!(layout.named_panels().iter().all(|(_, r)| r.is_some()));
        let mut names: Vec<&str> = layout
            .named_panels()
            .iter()
            .map(|(name, _)| *name)
            .collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 9, "panel names must be unique");
    }

    #[test]
    fn column_orderings_list_every_column_exactly_once() {
        assert_eq!(Column::DISPLAY_ORDER.len(), Column::COUNT);
        for (position, column) in Column::DISPLAY_ORDER.iter().enumerate() {
            assert_eq!(
                column.index(),
                position,
                "{column:?} is missing from or misplaced in DISPLAY_ORDER"
            );
        }
        let mut admitted: Vec<usize> = Column::ADMISSION_ORDER
            .iter()
            .map(|column| column.index())
            .collect();
        admitted.sort_unstable();
        assert_eq!(
            admitted,
            (0..Column::COUNT).collect::<Vec<_>>(),
            "ADMISSION_ORDER must be a permutation of every column"
        );
    }

    #[test]
    fn admission_order_follows_the_priority_list_in_section_seven_two() {
        // Every reserved column is admitted in non-decreasing priority order, so
        // the widths that disappear first really are §7.2's lowest priorities.
        let mut previous = 0u8;
        for column in Column::ADMISSION_ORDER {
            if column.is_flexible() {
                continue;
            }
            assert!(
                column.priority() >= previous,
                "{column:?} (priority {}) is admitted after priority {previous}",
                column.priority()
            );
            previous = column.priority();
        }
        // The two documented deviations, both about the priority-1 identity field.
        assert_eq!(Column::ADMISSION_ORDER.first(), Some(&Column::Name));
        assert_eq!(Column::ADMISSION_ORDER.get(1), Some(&Column::Selection));
        assert_eq!(Column::ADMISSION_ORDER.last(), Some(&Column::Command));
        // The priority numbers are §7.2's, and every value 0..=10 is used.
        let mut used: Vec<u8> = Column::DISPLAY_ORDER
            .iter()
            .map(|column| column.priority())
            .collect();
        used.sort_unstable();
        used.dedup();
        assert_eq!(used, (0..=10).collect::<Vec<u8>>());
    }

    #[test]
    fn the_split_helpers_handle_zero_area_without_panicking() {
        // These are the primitives every panel is cut from; a zero dimension must
        // yield empty rectangles rather than wrap around (§5.7).
        for area in [
            Rect::new(0, 0, 0, 0),
            Rect::new(0, 0, 10, 0),
            Rect::new(0, 0, 0, 10),
            Rect::new(65_530, 65_530, 20, 20),
        ] {
            for amount in [0u16, 1, 5, u16::MAX] {
                let (taken, rest) = take_top(area, amount);
                assert!(contains_rect(area, taken));
                assert!(contains_rect(area, rest));
                assert_eq!(taken.height + rest.height, area.height);

                let (taken, rest) = take_bottom(area, amount);
                assert!(contains_rect(area, taken));
                assert!(contains_rect(area, rest));
                assert_eq!(taken.height + rest.height, area.height);

                let (taken, rest) = take_left(area, amount);
                assert!(contains_rect(area, taken));
                assert!(contains_rect(area, rest));
                assert_eq!(taken.width + rest.width, area.width);
            }
        }
        assert_eq!(non_empty(Rect::new(0, 0, 0, 5)), None);
        assert_eq!(non_empty(Rect::new(0, 0, 5, 0)), None);
        assert_eq!(
            non_empty(Rect::new(1, 2, 3, 4)),
            Some(Rect::new(1, 2, 3, 4))
        );
    }

    #[test]
    fn splits_are_adjacent_and_ordered() {
        let area = Rect::new(4, 6, 20, 10);
        let (top, rest) = take_top(area, 3);
        assert_eq!(top, Rect::new(4, 6, 20, 3));
        assert_eq!(rest, Rect::new(4, 9, 20, 7));

        let (bottom, rest) = take_bottom(area, 2);
        assert_eq!(bottom, Rect::new(4, 14, 20, 2));
        assert_eq!(rest, Rect::new(4, 6, 20, 8));

        let (left, rest) = take_left(area, 8);
        assert_eq!(left, Rect::new(4, 6, 8, 10));
        assert_eq!(rest, Rect::new(12, 6, 12, 10));
    }

    #[test]
    fn every_column_header_fits_its_reserved_width() {
        for column in Column::DISPLAY_ORDER {
            let header = column.header();
            assert!(
                monitrs_core::units::display_width(header) <= usize::from(column.reserved_width()),
                "{column:?} header {header:?} is wider than its {} reserved cells",
                column.reserved_width()
            );
        }
    }

    #[test]
    fn every_numeric_column_is_right_aligned() {
        // §5.4. The text columns are the only left-aligned ones.
        for column in Column::DISPLAY_ORDER {
            let expected = match column {
                Column::Selection
                | Column::User
                | Column::State
                | Column::Name
                | Column::Command => Align::Left,
                _ => Align::Right,
            };
            assert_eq!(column.align(), expected, "{column:?}");
        }
    }

    #[test]
    fn byte_columns_reserve_the_widths_the_formatters_guarantee() {
        // §5.4: reserve from geometry, and take the bound from the formatter that
        // will fill the cell rather than from a guess.
        assert_eq!(Column::Rss.reserved_width(), MAX_COMPACT_BYTES_WIDTH);
        assert_eq!(
            Column::VirtualMemory.reserved_width(),
            MAX_COMPACT_BYTES_WIDTH
        );
        assert_eq!(Column::ReadRate.reserved_width(), MAX_BYTE_RATE_WIDTH);
        assert_eq!(Column::WriteRate.reserved_width(), MAX_BYTE_RATE_WIDTH);
    }

    #[test]
    fn reserved_widths_hold_the_widest_value_each_formatter_can_produce() {
        use monitrs_core::units::{
            ByteUnits, Percent, Rate, display_width, format_age, format_byte_rate,
            format_bytes_compact,
        };

        assert!(
            display_width(&format_bytes_compact(u64::MAX, ByteUnits::Iec))
                <= usize::from(Column::Rss.reserved_width())
        );
        let rate = Rate::new(9.9e18).expect("finite");
        assert!(
            display_width(&format_byte_rate(rate, ByteUnits::Iec))
                <= usize::from(Column::ReadRate.reserved_width())
        );
        // The AGE column's widest form is hh:mm:ss just below one day.
        let almost_a_day = core::time::Duration::from_secs(86_399);
        assert!(
            display_width(&format_age(almost_a_day)) <= usize::from(Column::Age.reserved_width())
        );
        // A 100-core machine saturated by one process.
        let cpu = Percent::new(12_800.0).expect("finite");
        assert!(
            display_width(&cpu.to_string()) <= usize::from(Column::CpuPercent.reserved_width()),
            "{cpu} does not fit CPU%"
        );
        let memory = Percent::new(100.0).expect("finite");
        assert!(
            display_width(&memory.to_string())
                <= usize::from(Column::MemoryPercent.reserved_width())
        );
        // The PID column must hold Linux's maximum `pid_max`.
        assert!(display_width("4194304") <= usize::from(Column::Pid.reserved_width()));
    }

    #[test]
    fn a_zero_width_table_has_no_columns() {
        let layout = TableLayout::fit(0);
        assert!(layout.columns().is_empty());
        assert_eq!(layout.total_width(), 0);
        assert!(!layout.contains(Column::Name));
        assert_eq!(layout.column(Column::Pid), None);
        assert_eq!(
            TableLayout::for_area(Rect::new(0, 0, 0, 40))
                .columns()
                .len(),
            0
        );
    }

    #[test]
    fn the_identity_column_survives_even_a_single_cell() {
        // §7.2's highest priority is the process name; a table that cannot name
        // its rows is useless, so `Name` is admitted before anything else.
        for width in 1..=NAME_MIN_WIDTH {
            let layout = TableLayout::fit(width);
            assert!(layout.contains(Column::Name), "width {width}");
            assert_eq!(layout.total_width(), width, "width {width}");
        }
    }

    #[test]
    fn the_table_fills_its_width_exactly_at_every_size() {
        for width in 1..=400u16 {
            let layout = TableLayout::fit(width);
            assert_eq!(
                layout.total_width(),
                width,
                "width {width} produced {:?}",
                layout.columns()
            );
        }
    }

    #[test]
    fn columns_are_dropped_in_reverse_priority_order() {
        // Widening the terminal must only ever add columns.
        let mut previous: Vec<Column> = Vec::new();
        for width in 1..=400u16 {
            let current: Vec<Column> = TableLayout::fit(width)
                .columns()
                .iter()
                .map(|entry| entry.column)
                .collect();
            for column in &previous {
                assert!(
                    current.contains(column),
                    "{column:?} disappeared when widening to {width}"
                );
            }
            previous = current;
        }
    }

    #[test]
    fn the_highest_priority_columns_are_present_at_eighty_cells() {
        // §5.7's Compact band hides low-priority columns; §7.2 fixes which.
        let layout = TableLayout::fit(80);
        for column in [
            Column::Name,
            Column::Selection,
            Column::Pid,
            Column::CpuPercent,
            Column::Rss,
            Column::MemoryPercent,
            Column::User,
            Column::State,
        ] {
            assert!(layout.contains(column), "{column:?} missing at 80 cells");
        }
        for column in [Column::VirtualMemory, Column::Command] {
            assert!(
                !layout.contains(column),
                "{column:?} should be hidden at 80 cells"
            );
        }
    }

    #[test]
    fn a_wide_table_shows_every_column() {
        let layout = TableLayout::fit(WIDE_MIN_WIDTH);
        for column in Column::DISPLAY_ORDER {
            assert!(layout.contains(column), "{column:?} missing at 140 cells");
        }
        // The full command gets the larger share of the flexible space.
        let name = layout.column(Column::Name).expect("name").width;
        let command = layout.column(Column::Command).expect("command").width;
        assert!(
            command > name,
            "command {command} should exceed name {name}"
        );
        assert!(name >= NAME_MIN_WIDTH);
        assert!(command >= COMMAND_MIN_WIDTH);
    }

    #[test]
    fn columns_are_laid_out_left_to_right_without_gaps_or_overlaps() {
        for width in [1u16, 20, 60, 80, 100, 140, 400] {
            let layout = TableLayout::fit(width);
            let mut expected_x = 0u16;
            let mut first = true;
            for entry in layout.columns() {
                if !first {
                    expected_x += COLUMN_GAP;
                }
                assert_eq!(entry.x, expected_x, "width {width}, {:?}", entry.column);
                assert!(entry.width >= 1, "{:?} has no width", entry.column);
                assert!(
                    entry.x + entry.width <= width,
                    "{:?} runs past the table at width {width}",
                    entry.column
                );
                expected_x += entry.width;
                first = false;
            }
            assert_eq!(expected_x, width, "width {width} left cells unassigned");
        }
    }

    #[test]
    fn columns_appear_in_display_order_regardless_of_admission_order() {
        let layout = TableLayout::fit(200);
        let order: Vec<Column> = layout.columns().iter().map(|entry| entry.column).collect();
        let expected: Vec<Column> = Column::DISPLAY_ORDER
            .into_iter()
            .filter(|column| layout.contains(*column))
            .collect();
        assert_eq!(order, expected);
    }

    #[test]
    fn fixed_columns_keep_their_reserved_width_at_every_table_width() {
        // §5.4: a value crossing a unit boundary must not reflow the table, which
        // holds only if the reserved columns never change size.
        for width in 1..=400u16 {
            for entry in TableLayout::fit(width).columns() {
                if entry.column.is_flexible() {
                    continue;
                }
                assert_eq!(
                    entry.width,
                    entry.column.reserved_width(),
                    "{:?} was resized at table width {width}",
                    entry.column
                );
            }
        }
    }

    proptest! {
        /// §17.7: layout rectangles remain inside parent bounds.
        #[test]
        fn layout_rectangles_remain_inside_parent_bounds(
            width in 0u16..=400,
            height in 0u16..=400,
        ) {
            let area = Rect::new(0, 0, width, height);
            let layout = Layout::resolve(area);
            prop_assert_eq!(layout.area, area);
            for (name, rect) in layout.named_panels() {
                if let Some(rect) = rect {
                    prop_assert!(
                        contains_rect(area, rect),
                        "{} {:?} escaped {:?}", name, rect, area
                    );
                    prop_assert!(!rect.is_empty(), "{} is present but empty", name);
                }
            }
        }

        /// The same property with an arbitrary origin, since a nested render may
        /// not start at (0, 0).
        #[test]
        fn layout_rectangles_stay_inside_an_offset_parent(
            x in 0u16..=400,
            y in 0u16..=400,
            width in 0u16..=400,
            height in 0u16..=400,
        ) {
            let area = Rect::new(x, y, width, height);
            let layout = Layout::resolve(area);
            for (name, rect) in layout.named_panels() {
                if let Some(rect) = rect {
                    prop_assert!(
                        contains_rect(area, rect),
                        "{} {:?} escaped {:?}", name, rect, area
                    );
                }
            }
        }

        /// Column geometry never exceeds the panel it was fitted to.
        #[test]
        fn table_columns_never_exceed_the_panel_width(width in 0u16..=400) {
            let layout = TableLayout::fit(width);
            prop_assert!(layout.total_width() <= width);
            for entry in layout.columns() {
                prop_assert!(u32::from(entry.x) + u32::from(entry.width) <= u32::from(width));
            }
            if width > 0 {
                prop_assert_eq!(layout.total_width(), width);
                prop_assert!(layout.contains(Column::Name));
            }
        }

        /// Resolving a layout for any size at all must not panic, whatever the
        /// breakpoint decides. §5.7 makes this a hard requirement.
        #[test]
        fn resolving_any_terminal_size_never_panics(
            width in 0u16..=u16::MAX,
            height in 0u16..=u16::MAX,
        ) {
            let layout = Layout::resolve(Rect::new(0, 0, width, height));
            prop_assert_eq!(layout.breakpoint, Breakpoint::resolve(width, height));
            let _ = TableLayout::fit(width);
        }
    }
}
